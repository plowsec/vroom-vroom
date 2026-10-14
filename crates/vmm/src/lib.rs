//! The VMM: builds a machine from a [`VmConfig`], runs it, and controls
//! its lifecycle (pause, resume, reset, snapshot, suspend, restore).
//!
//! One VMM process runs exactly one VM. Frontends (CLI, GUI) talk to it
//! through [`Vmm`] in-process or through the control socket.

pub mod agent;
mod disks;
mod display;
mod firmware;
mod irq;
mod machine;
pub mod psci;
mod ramfb;
mod snapshot;
mod tpm;
mod vcpu;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bulwark_config::VmConfig;
use bulwark_devices::pl011::Pl011;
use bulwark_hv::{BootRegs, Hypervisor, IrqChipState, Vm};
use bulwark_memory::GuestMemory;
use bulwark_snapshot::Snapshottable;

pub use vcpu::VmEvent;
use vcpu::{VcpuCmd, VcpuHandle, VcpuShared};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("hypervisor: {0}")]
    Hv(#[from] bulwark_hv::Error),
    #[error("memory: {0}")]
    Memory(#[from] bulwark_memory::Error),
    #[error("boot: {0}")]
    Arch(#[from] bulwark_arch::Error),
    #[error("config: {0}")]
    Config(#[from] bulwark_config::Error),
    #[error("snapshot: {0}")]
    Snapshot(#[from] bulwark_snapshot::Error),
    #[error("setup: {0}")]
    Setup(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("a vCPU thread is gone")]
    VcpuGone,
    #[error("timed out waiting for vCPUs")]
    Timeout,
    #[error("virtio: {0}")]
    Virtio(#[from] bulwark_virtio::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// How the VM should come up.
#[derive(Debug, Clone)]
pub enum StartMode {
    /// Cold boot from the configured kernel/firmware.
    Boot,
    /// Resume from a snapshot directory.
    Restore(PathBuf),
}

/// Host-side wiring the frontend provides.
pub struct VmmOptions {
    pub serial_out: Box<dyn Write + Send>,
    pub start: StartMode,
    /// Host clipboard, if the frontend has one.
    pub clipboard: Option<Box<dyn agent::ClipboardProvider>>,
    /// Connection to a viewer process (GUI). Overrides `clipboard`.
    pub ui: Option<std::os::unix::net::UnixStream>,
    /// Per-VM state directory (disk layers live in `disks/`).
    pub state_dir: PathBuf,
}

impl std::fmt::Debug for VmmOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmmOptions").field("start", &self.start).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running,
    Paused,
    Stopped,
}

/// A component that is part of snapshots and reset.
pub(crate) type StatefulDevice = Arc<Mutex<dyn Snapshottable + Send>>;

/// A running VM.
pub struct Vmm {
    config: VmConfig,
    hv: Arc<dyn Hypervisor>,
    vm: Arc<dyn Vm>,
    mem: GuestMemory,
    shared: Arc<VcpuShared>,
    vcpus: Vec<VcpuHandle>,
    events: Receiver<VmEvent>,
    devices: Vec<StatefulDevice>,
    uart: Arc<Mutex<Pl011>>,
    agent: agent::Agent,
    disks: disks::DiskSet,
    viewer: Option<display::ViewerTx>,
    screen: Option<Arc<display::FbSink>>,
    /// Captured right after build; reset = restore these.
    pristine_devices: Vec<Vec<u8>>,
    /// None when the backend can't save its irqchip (KVM for now).
    pristine_irqchip: Option<IrqChipState>,
    boot: machine::BootImage,
    state: RunState,
}

impl std::fmt::Debug for Vmm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vmm").field("name", &self.config.name).field("state", &self.state).finish()
    }
}

/// How long we wait for every vCPU to acknowledge a pause.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(5);

impl Vmm {
    /// Build the machine and start it.
    pub fn start(config: VmConfig, hv: Arc<dyn Hypervisor>, opts: VmmOptions) -> Result<Vmm> {
        config.validate()?;
        let (ev_tx, ev_rx) = mpsc::channel();
        let ui = match opts.ui {
            Some(s) => Some(display::DisplayServer::new(s)?),
            None => None,
        };
        let layers = match &opts.start {
            StartMode::Restore(dir) => snapshot::disk_plan(dir)?,
            StartMode::Boot => Default::default(),
        };
        let disks = disks::DiskSet::open(&config, &opts.state_dir, &layers)?;
        let uuid = machine_uuid(&opts.state_dir)?;
        let env = machine::BuildEnv {
            uuid,
            state_dir: opts.state_dir.clone(),
            resuming: matches!(opts.start, StartMode::Restore(_)),
        };
        let m = machine::build(&config, hv.as_ref(), opts.serial_out, opts.clipboard, ui, &disks, ev_tx, &env)?;
        let mut vmm = Vmm {
            config,
            hv,
            vm: m.vm,
            mem: m.mem,
            shared: m.shared,
            vcpus: m.vcpus,
            events: ev_rx,
            devices: m.devices,
            uart: m.uart,
            agent: m.agent,
            disks,
            viewer: m.viewer,
            screen: m.screen,
            pristine_devices: Vec::new(),
            pristine_irqchip: None,
            boot: m.boot,
            state: RunState::Paused,
        };
        vmm.pristine_irqchip = match vmm.vm.save_irqchip() {
            Ok(s) => Some(s),
            Err(bulwark_hv::Error::Unsupported(why)) => {
                log::info!("irqchip state not saveable ({why}); reset won't restore it");
                None
            }
            Err(e) => return Err(e.into()),
        };
        for d in &vmm.devices {
            let s = d.lock().unwrap_or_else(|e| e.into_inner()).save()?;
            vmm.pristine_devices.push(s);
        }
        match opts.start {
            StartMode::Boot => {
                let regs = vmm.boot.load(&vmm.mem)?;
                vmm.vcpus[0].peer.send(VcpuCmd::PowerOn(regs))?;
                // In-kernel PSCI: secondaries wait inside KVM_RUN for CPU_ON.
                // Only now: irqchip state access needs them out of KVM_RUN.
                for v in vmm.vcpus.iter().skip(1).filter(|v| v.in_kernel_psci) {
                    v.peer.send(VcpuCmd::Start)?;
                }
            }
            StartMode::Restore(dir) => {
                snapshot::restore(&mut vmm, &dir)?;
                // Restored workers start parked; let them go with the vCPUs.
                for d in &vmm.devices {
                    d.lock().unwrap_or_else(|e| e.into_inner()).resume();
                }
                // Guest wall clock is stuck at snapshot time; fix it.
                vmm.agent.send(agent::AgentCmd::SyncTime);
            }
        }
        vmm.state = RunState::Running;
        Ok(vmm)
    }

    pub fn config(&self) -> &VmConfig {
        &self.config
    }

    pub fn state(&self) -> RunState {
        self.state
    }

    /// Save the guest screen as PNG. Returns the mode.
    pub fn screendump(&self, path: &Path) -> Result<(u32, u32)> {
        match &self.screen {
            Some(s) => Ok(s.screendump(path)?),
            None => Err(Error::Setup("VM has no display".into())),
        }
    }

    /// Tell the viewer something (status line).
    pub fn viewer_status(&self, text: &str) {
        if let Some(v) = &self.viewer {
            v.send(&bulwark_display::ToViewer::Status(text.to_string()));
        }
    }

    /// Guest agent channel (clipboard, time sync, clean shutdown).
    pub fn agent(&self) -> &agent::Agent {
        &self.agent
    }

    /// Keyboard input for the serial console.
    pub fn serial_input(&self, bytes: &[u8]) {
        self.uart.lock().unwrap_or_else(|e| e.into_inner()).queue_input(bytes);
    }

    /// Wait for the next event from the guest.
    pub fn next_event(&self, timeout: Duration) -> Option<VmEvent> {
        match self.events.recv_timeout(timeout) {
            Ok(e) => Some(e),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(VmEvent::Crashed("vcpus gone".into())),
        }
    }

    /// Stop every vCPU at an instruction boundary.
    pub fn pause(&mut self) -> Result<()> {
        if self.state != RunState::Running {
            return Ok(());
        }
        let mut acks = Vec::new();
        for v in &self.vcpus {
            let (tx, rx) = mpsc::channel();
            v.peer.send(VcpuCmd::Pause(tx))?;
            acks.push(rx);
        }
        for rx in acks {
            rx.recv_timeout(PAUSE_TIMEOUT).map_err(|_| Error::Timeout)?;
        }
        // vCPUs first (no new kicks), then device workers (no DMA).
        for d in &self.devices {
            d.lock().unwrap_or_else(|e| e.into_inner()).pause();
        }
        self.state = RunState::Paused;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
        if self.state != RunState::Paused {
            return Ok(());
        }
        for d in &self.devices {
            d.lock().unwrap_or_else(|e| e.into_inner()).resume();
        }
        for v in &self.vcpus {
            v.peer.send(VcpuCmd::Resume)?;
        }
        self.state = RunState::Running;
        Ok(())
    }

    /// Hard reset: like pressing the reset button.
    pub fn reset(&mut self) -> Result<()> {
        let was_running = self.state == RunState::Running;
        self.pause()?;
        for (d, s) in self.devices.iter().zip(&self.pristine_devices) {
            d.lock().unwrap_or_else(|e| e.into_inner()).reset(s)?;
        }
        self.vm.zero_memory(&self.mem)?;
        if let Some(s) = &self.pristine_irqchip {
            self.vm.restore_irqchip(s)?;
        }
        self.shared.cpus.lock().unwrap_or_else(|e| e.into_inner()).reset();
        let regs = self.boot.load(&self.mem)?;
        for (i, v) in self.vcpus.iter().enumerate() {
            let (tx, rx) = mpsc::channel();
            let r: Option<BootRegs> = if i == 0 { Some(regs.clone()) } else { None };
            v.peer.send(VcpuCmd::Reset(r, tx))?;
            rx.recv_timeout(PAUSE_TIMEOUT).map_err(|_| Error::Timeout)??;
        }
        if was_running {
            self.resume()?;
        }
        Ok(())
    }

    /// Save a full snapshot. The VM keeps running afterwards.
    pub fn snapshot(&mut self, dir: &Path, description: &str) -> Result<()> {
        let was_running = self.state == RunState::Running;
        self.pause()?;
        let r = snapshot::save(self, dir, description, snapshot::Kind::Snapshot);
        if was_running {
            self.resume()?;
        }
        r
    }

    /// Snapshot then stop: the VM can be resumed later with
    /// [`StartMode::Restore`]. This is "suspend to disk".
    pub fn suspend(mut self, dir: &Path) -> Result<()> {
        self.suspend_in_place(dir)
    }

    /// Like [`suspend`], for callers that keep the handle. On error the
    /// VM is left paused, not stopped, so nothing is lost.
    pub fn suspend_in_place(&mut self, dir: &Path) -> Result<()> {
        self.pause()?;
        snapshot::save(self, dir, "suspend", snapshot::Kind::Suspend)?;
        self.shutdown();
        Ok(())
    }

    /// Tear everything down. vCPUs are destroyed on their own threads.
    pub fn shutdown(&mut self) {
        self.agent.send(agent::AgentCmd::Stop);
        self.disks.flush_all();
        if let Some(v) = &self.viewer {
            v.send(&bulwark_display::ToViewer::Closed);
        }
        for v in &self.vcpus {
            let _ = v.peer.send(VcpuCmd::Stop);
        }
        for v in &mut self.vcpus {
            v.join();
        }
        self.state = RunState::Stopped;
    }
}

impl Drop for Vmm {
    fn drop(&mut self) {
        if self.state != RunState::Stopped {
            self.shutdown();
        }
    }
}

/// The VM's hardware UUID (SMBIOS): random once, then kept in the state
/// dir. Guests key activation and hardware ids on it.
fn machine_uuid(state_dir: &Path) -> Result<[u8; 16]> {
    let path = state_dir.join("uuid");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() == 32 {
            let mut u = [0u8; 16];
            for (i, b) in u.iter_mut().enumerate() {
                *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0);
            }
            return Ok(u);
        }
    }
    let mut u = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut u))?;
    u[6] = (u[6] & 0x0f) | 0x40; // version 4
    u[8] = (u[8] & 0x3f) | 0x80; // variant 1
    std::fs::create_dir_all(state_dir)?;
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&path, format!("{}-{}-{}-{}-{}\n", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]))?;
    Ok(u)
}
