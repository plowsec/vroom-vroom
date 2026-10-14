//! The VMM: builds a machine from a [`VmConfig`], runs it, and controls
//! its lifecycle (pause, resume, reset, snapshot, suspend, restore).
//!
//! One VMM process runs exactly one VM. Frontends (CLI, GUI) talk to it
//! through [`Vmm`] in-process or through the control socket.

pub mod agent;
mod display;
mod irq;
mod machine;
pub mod psci;
mod snapshot;
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
    viewer: Option<display::ViewerTx>,
    screen: Option<Arc<display::FbSink>>,
    /// Captured right after build; reset = restore these.
    pristine_devices: Vec<Vec<u8>>,
    pristine_irqchip: IrqChipState,
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
        let m = machine::build(&config, hv.as_ref(), opts.serial_out, opts.clipboard, ui, ev_tx)?;
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
            viewer: m.viewer,
            screen: m.screen,
            pristine_devices: Vec::new(),
            pristine_irqchip: IrqChipState::default(),
            boot: m.boot,
            state: RunState::Paused,
        };
        vmm.pristine_irqchip = vmm.vm.save_irqchip()?;
        for d in &vmm.devices {
            let s = d.lock().unwrap_or_else(|e| e.into_inner()).save()?;
            vmm.pristine_devices.push(s);
        }
        match opts.start {
            StartMode::Boot => {
                let regs = vmm.boot.load(&vmm.mem)?;
                vmm.vcpus[0].peer.send(VcpuCmd::PowerOn(regs))?;
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
            d.lock().unwrap_or_else(|e| e.into_inner()).restore(s)?;
        }
        self.mem.discard_all()?;
        self.vm.restore_irqchip(&self.pristine_irqchip)?;
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
        let r = snapshot::save(self, dir, description);
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
        snapshot::save(self, dir, "suspend")?;
        self.shutdown();
        Ok(())
    }

    /// Tear everything down. vCPUs are destroyed on their own threads.
    pub fn shutdown(&mut self) {
        self.agent.send(agent::AgentCmd::Stop);
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
