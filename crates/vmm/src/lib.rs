//! The VMM: builds a machine from a [`VmConfig`], runs it, and controls
//! its lifecycle (pause, resume, reset, snapshot, suspend, restore).
//!
//! One VMM process runs exactly one VM. Frontends (CLI, GUI) talk to it
//! through [`Vmm`] in-process or through the control socket.

pub mod agent;
mod audio;
mod camera;
pub mod control;
mod debug;
mod disks;

/// Helper processes we started (swtpm, audio): the only processes the
/// sandboxed VMM may still signal (to stop them on exit).
static HELPERS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn note_helper(pid: u32) {
    HELPERS.lock().unwrap_or_else(|e| e.into_inner()).push(pid);
}
pub use vroom_state as state;
mod display;
mod firmware;
mod hotplug;
mod irq;
mod machine;
mod net;
pub use machine::gpu_in_process;
pub mod psci;
mod ramfb;
mod snapshot;
mod tpm;
mod usb;
mod vcpu;
mod video;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vroom_config::VmConfig;
use vroom_devices::pl011::Pl011;
use vroom_hv::{BootRegs, Hypervisor, IrqChipState, Vm};
#[cfg(feature = "research")]
use vroom_hv::VcpuState;
use vroom_memory::GuestMemory;
use vroom_snapshot::Snapshottable;

pub use vcpu::VmEvent;
use vcpu::{VcpuCmd, VcpuHandle, VcpuShared};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("hypervisor: {0}")]
    Hv(#[from] vroom_hv::Error),
    #[error("memory: {0}")]
    Memory(#[from] vroom_memory::Error),
    #[error("boot: {0}")]
    Arch(#[from] vroom_arch::Error),
    #[error("config: {0}")]
    Config(#[from] vroom_config::Error),
    #[error("snapshot: {0}")]
    Snapshot(#[from] vroom_snapshot::Error),
    #[error("setup: {0}")]
    Setup(String),
    #[error("{0}")]
    State(#[from] vroom_state::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("a vCPU thread is gone")]
    VcpuGone,
    #[error("timed out waiting for vCPUs")]
    Timeout,
    #[error("virtio: {0}")]
    Virtio(#[from] vroom_virtio::Error),
    /// The guest isn't in a state we can save, in words for people.
    #[error("{0}")]
    CantSave(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// How the VM should come up.
#[derive(Debug, Clone)]
pub enum StartMode {
    /// Cold boot from the configured kernel/firmware.
    Boot,
    /// Cold boot with CPUs parked before the first guest instruction.
    BootPaused,
    /// Resume from a snapshot directory.
    Restore(PathBuf),
    /// Restore without executing guest instructions until explicitly resumed.
    RestorePaused(PathBuf),
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
    /// The frontend sandboxes the VMM once it is built (what a network
    /// adapter changed later may still reach).
    pub sandbox: bool,
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
    /// One VMM per state dir: two would write the same disk layers.
    _lock: state::StateLock,
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
    gpus: Vec<vroom_virtio::devices::gpu::GpuControl>,
    disks: disks::DiskSet,
    viewer: Option<display::ViewerTx>,
    /// Ends the viewer threads at shutdown.
    display: Option<display::DisplayStop>,
    screens: Vec<Arc<display::FbSink>>,
    /// Captured right after build; reset = restore these.
    pristine_devices: Vec<Vec<u8>>,
    /// None when the backend can't save its irqchip (KVM for now).
    pristine_irqchip: Option<IrqChipState>,
    boot: machine::BootImage,
    state: RunState,
    /// The network adapters have a control queue (what a save records).
    net_ctrl: bool,
    /// Removable shares for USB disks' files (what a save records).
    usb_file_slots: usize,
    /// Windows' 3D GPU has memory in BAR 0 (see `machine::GPU3D_VRAM`).
    gpu3d_vram: bool,
    /// The network adapters now (a save records them).
    net_ports: Arc<net::NetPorts>,
    /// Hot plug slots (a VM on PCI).
    hotplug: Option<Arc<hotplug::Hotplug>>,
    #[cfg(feature = "research")]
    research_baseline: Option<ResearchBaseline>,
}

/// Interrupt controller registers as (register, value).
#[cfg(feature = "research")]
type GicRegisters = Vec<(u32, u64)>;

#[cfg(feature = "research")]
struct ResearchBaseline {
    devices: Vec<Vec<u8>>,
    vcpus: Vec<VcpuState>,
    irqchip: Option<IrqChipState>,
    /// Run-time interrupt controller registers, shared then per CPU: what
    /// a reset restores when the devices stayed idle.
    gic: Option<(GicRegisters, Vec<GicRegisters>)>,
    clock: u64,
    /// The hypervisor logs guest writes and RAM logs host writes: a reset
    /// copies back only those pages instead of remapping all RAM.
    logged: bool,
    /// `device_exits` when the devices last held `devices`.
    device_exits: u64,
    /// Device workers resumed since then.
    devices_ran: bool,
    /// Devices whose saved state changes by itself (`VROOM_COW_VERIFY`).
    drifting: Vec<bool>,
}

impl std::fmt::Debug for Vmm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vmm").field("name", &self.config.name).field("state", &self.state).finish()
    }
}

/// How long we wait for every vCPU to acknowledge a pause.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long Windows gets to turn its 3D display off before a save, and
/// to settle on its basic display afterwards.
const PARK_TIMEOUT: Duration = Duration::from_secs(20);
/// Saved 1.5 s after its 3D display went off, Windows showed nothing on
/// it after the first resume after a boot (4 of 4); 3 s and more, never.
const PARK_SETTLE: Duration = Duration::from_secs(3);
/// Once the device is off, how long its driver's Direct3D contexts get to
/// go before the save drops them (see `GpuControl::set_driver_stopped`).
const PARK_LEFTOVERS: Duration = Duration::from_secs(3);

/// A snapshot comes back with the hardware it was taken with: its devices
/// and their state are in it. From the VM file now only what the host does
/// around them: network attachments (planned against the saved adapters),
/// clipboard, serial, sandbox and the window's size.
fn restore_hardware(now: VmConfig, saved: &VmConfig) -> VmConfig {
    VmConfig {
        name: now.name,
        nets: now.nets,
        clipboard: now.clipboard,
        serial: now.serial,
        security: now.security,
        os: now.os,
        display: vroom_config::Display { width: now.display.width, height: now.display.height, hidpi: now.display.hidpi, ..saved.display.clone() },
        ..saved.clone()
    }
}

impl Vmm {
    /// Build the machine and start it.
    pub fn start(config: VmConfig, hv: Arc<dyn Hypervisor>, opts: VmmOptions) -> Result<Vmm> {
        config.validate()?;
        let saved = match &opts.start {
            StartMode::Restore(dir) | StartMode::RestorePaused(dir) => Some(snapshot::saved_config(dir)?),
            StartMode::Boot | StartMode::BootPaused => None,
        };
        let config = match &saved {
            Some((s, _)) => restore_hardware(config, s),
            None => config,
        };
        let (ev_tx, ev_rx) = mpsc::channel();
        let ui = match opts.ui {
            Some(s) => Some(display::DisplayServer::new(s)?),
            None => None,
        };
        let lock = state::StateLock::acquire(&opts.state_dir)?;
        let layers = match &opts.start {
            StartMode::Restore(dir) | StartMode::RestorePaused(dir) => snapshot::disk_plan(dir)?,
            StartMode::Boot | StartMode::BootPaused => Default::default(),
        };
        let disks = disks::DiskSet::open(&config, &opts.state_dir, &layers)?;
        let uuid = machine_uuid(&opts.state_dir)?;
        let net_ctrl = saved.as_ref().is_none_or(|s| s.1);
        // USB disks' files go to removable shares a VM started on a Mac
        // has; a resumed one has those it was saved with.
        let usb_file_slots = match &opts.start {
            StartMode::Restore(dir) | StartMode::RestorePaused(dir) => snapshot::saved_usb_file_slots(dir),
            StartMode::Boot | StartMode::BootPaused if cfg!(target_os = "macos") => usb::FILE_SLOTS,
            StartMode::Boot | StartMode::BootPaused => 0,
        };
        let saved = saved.map(|s| s.0);
        let hot_nets = match &opts.start {
            StartMode::Restore(dir) | StartMode::RestorePaused(dir) => snapshot::saved_hot_nets(dir)?,
            StartMode::Boot | StartMode::BootPaused => Default::default(),
        };
        // Windows' 3D GPU has memory in BAR 0; saves from before have not.
        let gpu3d_vram = match &opts.start {
            StartMode::Restore(dir) | StartMode::RestorePaused(dir) => snapshot::saved_gpu3d_vram(dir),
            StartMode::Boot | StartMode::BootPaused => true,
        };
        let env = machine::BuildEnv {
            uuid,
            state_dir: opts.state_dir.clone(),
            resuming: matches!(opts.start, StartMode::Restore(_) | StartMode::RestorePaused(_)),
            saved,
            net_ctrl,
            usb_file_slots,
            sandbox: opts.sandbox && config.security.sandbox,
            hot_nets,
            gpu3d_vram,
        };
        let m = machine::build(&config, hv.as_ref(), opts.serial_out, opts.clipboard, ui, &disks, ev_tx, &env)?;
        // A save records the hardware the guest has: a resumed VM keeps
        // the network adapters it was saved with until it restarts.
        let mut config = config;
        config.nets = m.nets;
        let mut vmm = Vmm {
            _lock: lock,
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
            gpus: m.gpus,
            disks,
            viewer: m.viewer,
            display: m.display,
            screens: m.screens,
            pristine_devices: Vec::new(),
            pristine_irqchip: None,
            boot: m.boot,
            state: RunState::Paused,
            net_ctrl,
            usb_file_slots,
            gpu3d_vram,
            net_ports: m.net_ports,
            hotplug: m.hotplug,
            #[cfg(feature = "research")]
            research_baseline: None,
        };
        vmm.pristine_irqchip = match vmm.vm.save_irqchip() {
            Ok(s) => Some(s),
            Err(vroom_hv::Error::Unsupported(why)) => {
                log::info!("irqchip state not saveable ({why}); reset won't restore it");
                None
            }
            Err(e) => return Err(e.into()),
        };
        for d in &vmm.devices {
            let s = d.lock().unwrap_or_else(|e| e.into_inner()).save()?;
            vmm.pristine_devices.push(s);
        }
        let start_paused = matches!(opts.start, StartMode::BootPaused | StartMode::RestorePaused(_));
        // Restore marks each CPU runnable. Park every CPU before loading any
        // state so none can execute against partially restored RAM/devices.
        vmm.state = RunState::Running;
        vmm.pause()?;
        match opts.start {
            StartMode::Boot | StartMode::BootPaused => {
                let regs = vmm.boot.load(&vmm.mem)?;
                vmm.vcpus[0].peer.send(VcpuCmd::PowerOn(regs))?;
                // In-kernel PSCI: secondaries wait inside KVM_RUN for CPU_ON.
                // Only now: irqchip state access needs them out of KVM_RUN.
                for v in vmm.vcpus.iter().skip(1).filter(|v| v.in_kernel_psci) {
                    v.peer.send(VcpuCmd::Start)?;
                }
            }
            StartMode::Restore(dir) | StartMode::RestorePaused(dir) => {
                snapshot::restore(&mut vmm, &dir)?;
                // Guest wall clock is stuck at snapshot time; fix it.
                vmm.agent.send(agent::AgentCmd::SyncTime);
            }
        }
        if !start_paused { vmm.resume()?; }
        Ok(vmm)
    }

    pub fn config(&self) -> &VmConfig {
        &self.config
    }

    pub fn state(&self) -> RunState {
        self.state
    }

    #[cfg(feature = "research")]
    /// Capture the paused machine for high-rate fuzz iterations.
    pub fn research_arm(&mut self) -> Result<()> {
        if self.state != RunState::Paused {
            return Err(Error::Setup("research baseline requires a paused VM".into()));
        }
        if self.research_baseline.is_some() {
            return Err(Error::Setup("research baseline is already armed".into()));
        }
        let verify = std::env::var_os("VROOM_COW_VERIFY").is_some();
        let mut devices = Vec::with_capacity(self.devices.len());
        for d in &self.devices {
            devices.push(d.lock().unwrap_or_else(|e| e.into_inner()).save()?);
        }
        let vcpus = ask_vcpus(&self.vcpus, VcpuCmd::Save)?;
        let irqchip = match self.vm.save_irqchip() {
            Ok(s) => Some(s),
            Err(vroom_hv::Error::Unsupported(_)) => None,
            Err(e) => return Err(e.into()),
        };
        let gic = match self.vm.gic_shared_registers(None) {
            Ok(shared) => Some((shared, ask_vcpus(&self.vcpus, |tx| VcpuCmd::GicRegisters(None, tx))?)),
            Err(vroom_hv::Error::Unsupported(_)) => None,
            Err(e) => return Err(e.into()),
        };
        let clock = self.vm.clock();
        let mem = &self.mem;
        self.vm.replace_memory(mem, &mut || {
            mem.snapshot_cow().map_err(|e| vroom_hv::Error::Invalid(format!("COW snapshot: {e}")))
        })?;
        let logged = match self.vm.start_dirty_log(mem) {
            Ok(()) => {
                mem.log_host_writes(true);
                true
            }
            Err(vroom_hv::Error::Unsupported(why)) => {
                log::info!("research resets remap all RAM: no {why}");
                false
            }
            Err(e) => return Err(e.into()),
        };
        // RAM goes back to here, so the page cache will: the disks must too.
        self.disks.arm_rollback()?;
        // A device whose saved state moves on its own (the TPM's clock)
        // can't be compared after a reset.
        let mut drifting = vec![false; devices.len()];
        if verify {
            std::thread::sleep(std::time::Duration::from_millis(20));
            for ((d, state), drifting) in self.devices.iter().zip(&devices).zip(&mut drifting) {
                *drifting = &d.lock().unwrap_or_else(|e| e.into_inner()).save()? != state;
            }
        }
        let device_exits = self.shared.device_exits.load(std::sync::atomic::Ordering::Relaxed);
        self.research_baseline = Some(ResearchBaseline {
            devices,
            vcpus,
            irqchip,
            gic,
            clock,
            logged,
            device_exits,
            devices_ran: false,
            drifting,
        });
        Ok(())
    }

    #[cfg(feature = "research")]
    /// Whether `len` bytes at `gpa` read the same in live RAM and in the
    /// armed baseline (false when nothing is armed). VM paused.
    pub fn research_matches_baseline(&self, gpa: u64, len: usize) -> Result<bool> {
        if self.state != RunState::Paused {
            return Err(Error::Setup("research check requires a paused VM".into()));
        }
        if self.research_baseline.is_none() {
            return Ok(false);
        }
        let addr = vroom_memory::GuestAddress(gpa);
        let (mut live, mut base) = (vec![0u8; len], vec![0u8; len]);
        self.mem.read(addr, &mut live)?;
        self.mem.read_baseline(addr, &mut base)?;
        Ok(live == base)
    }

    #[cfg(feature = "research")]
    /// Write guest RAM so that research resets keep the change: into the
    /// armed baseline too (a fuzzer's code breakpoints, planted or taken
    /// out). Before `research_arm` it is a plain write. VM paused.
    pub fn research_patch(&self, gpa: u64, bytes: &[u8]) -> Result<()> {
        if self.state != RunState::Paused {
            return Err(Error::Setup("research patch requires a paused VM".into()));
        }
        let addr = vroom_memory::GuestAddress(gpa);
        if !self.mem.check_range(addr, bytes.len() as u64) {
            return Err(Error::Setup(format!("patch outside RAM at {gpa:#x}")));
        }
        if self.research_baseline.is_some() {
            self.mem.write_baseline(addr, bytes)?;
        } else {
            self.mem.write(addr, bytes)?;
        }
        for (pointer, length) in self.mem.host_ranges(addr, bytes.len() as u64).expect("checked RAM") {
            // SAFETY: mapped guest RAM; every vCPU is paused.
            unsafe { debug::sync_instruction_cache(pointer, length) };
        }
        Ok(())
    }

    #[cfg(feature = "research")]
    pub(crate) fn research_devices_ran(&mut self) {
        if let Some(baseline) = &mut self.research_baseline {
            baseline.devices_ran = true;
        }
    }

    #[cfg(feature = "research")]
    /// The disk overlays this VM started on its snapshot are scratch:
    /// delete them when the VM goes away (a fuzzer that restores the same
    /// snapshot again would otherwise leave one behind per restore).
    pub fn research_discard_disk_layers(&mut self) {
        self.disks.discard_fresh_layers();
    }

    #[cfg(feature = "research")]
    /// Restore the armed baseline without rebuilding the VM or its mappings.
    ///
    /// When the devices stayed paused (`debug_resume_cpu`) and no vCPU
    /// touched them, they still hold the baseline and nothing raised a
    /// shared interrupt: their restore is skipped (it costs ~5 ms on the
    /// HEVD VM, mostly PCI and the TPM) and the interrupt controller is put
    /// back register by register (~0.1 ms instead of ~1.7 ms).
    /// `VROOM_COW_VERIFY=1` checks RAM, those devices and the interrupt
    /// controller registers against the baseline after every reset.
    pub fn research_reset(&mut self) -> Result<()> {
        if self.state != RunState::Paused {
            return Err(Error::Setup("research reset requires a paused VM".into()));
        }
        let verify = std::env::var_os("VROOM_COW_VERIFY").is_some();
        let device_exits = self.shared.device_exits.load(std::sync::atomic::Ordering::Relaxed);
        let Some(baseline) = &mut self.research_baseline else {
            return Err(Error::Setup("research baseline is not armed".into()));
        };
        let devices_idle = !baseline.devices_ran && baseline.device_exits == device_exits;
        baseline.devices_ran = false;
        baseline.device_exits = device_exits;
        let baseline = &*baseline;
        let mem = &self.mem;
        if baseline.logged {
            // Copy back only the pages written since the last reset: by the
            // guest (the hypervisor logs them) or by host code (RAM does).
            // Remapping all RAM costs ~130 ms for 8 GiB, and every page the
            // guest touches next faults in again.
            let mut written: Vec<(vroom_memory::GuestAddress, u64)> =
                self.vm.take_dirty_log()?.into_iter().map(|(gpa, len)| (vroom_memory::GuestAddress(gpa), len)).collect();
            written.extend(mem.take_host_writes());
            mem.restore_cow_copy(&written)?;
            for &(gpa, len) in &written {
                if let Some(r) = mem.regions().find(|r| gpa >= r.start() && gpa.0 < r.end().0) {
                    // SAFETY: inside that region's mapping; every CPU is paused.
                    unsafe { debug::sync_instruction_cache(r.host_ptr().add((gpa.0 - r.start().0) as usize), len as usize) };
                }
            }
        } else {
            self.vm.replace_memory(mem, &mut || {
                mem.restore_cow().map_err(|e| vroom_hv::Error::Invalid(format!("COW restore: {e}")))
            })?;
        }
        if verify {
            let differ = mem.cow_verify()?;
            if !differ.is_empty() {
                let first: Vec<String> = differ.iter().take(8).map(|a| format!("{:#x}", a.0)).collect();
                return Err(Error::Setup(format!(
                    "COW reset left {} pages off the baseline: {}",
                    differ.len(),
                    first.join(" ")
                )));
            }
        }
        // Paused devices have no request running.
        let dropped = self.disks.rollback()?;
        if dropped > 0 {
            log::trace!("research reset: dropped {dropped} bytes of disk writes");
        }
        let light_gic = baseline.gic.as_ref().filter(|_| devices_idle);
        if let Some((shared, _)) = light_gic {
            self.vm.gic_shared_registers(Some(shared))?;
        } else if let Some(irqchip) = &baseline.irqchip {
            self.vm.restore_irqchip(irqchip)?;
        }
        self.vm.set_clock(baseline.clock);
        if devices_idle {
            if verify {
                for ((d, state), _) in self.devices.iter().zip(&baseline.devices).zip(&baseline.drifting).filter(|(_, drifting)| !**drifting) {
                    let mut d = d.lock().unwrap_or_else(|e| e.into_inner());
                    if &d.save()? != state {
                        return Err(Error::Setup(format!("device {} changed without being accessed", d.snapshot_id())));
                    }
                }
            }
        } else {
            for (d, state) in self.devices.iter().zip(&baseline.devices) {
                d.lock().unwrap_or_else(|e| e.into_inner()).restore(state)?;
            }
        }
        let mut cpus = self.shared.cpus.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for (v, state) in self.vcpus.iter().zip(&baseline.vcpus) {
            cpus.set_on(state.index as usize, state.powered_on);
            let (tx, rx) = mpsc::channel();
            v.peer.send(VcpuCmd::Restore(Box::new(state.clone()), state.powered_on, tx))?;
            rx.recv_timeout(PAUSE_TIMEOUT).map_err(|_| Error::Timeout)??;
        }
        if let Some((_, gic_cpus)) = light_gic {
            for (v, saved) in self.vcpus.iter().zip(gic_cpus) {
                let (tx, rx) = mpsc::channel();
                v.peer.send(VcpuCmd::GicRegisters(Some(saved.clone()), tx))?;
                rx.recv_timeout(PAUSE_TIMEOUT).map_err(|_| Error::Timeout)??;
            }
        }
        *self.shared.cpus.lock().unwrap_or_else(|e| e.into_inner()) = cpus;
        if verify {
            // The irqchip blob can't be compared: reading registers reorders
            // HVF's internal state. Every architectural register can.
            if let Some((shared, gic_cpus)) = &baseline.gic {
                let mut differ: Vec<String> = Vec::new();
                let now = self.vm.gic_shared_registers(None)?;
                differ.extend(now.iter().zip(shared).filter(|(a, b)| a != b).map(|(a, b)| format!("GICD+{:#x} {:#x}->{:#x}", a.0, b.1, a.1)));
                let now = ask_vcpus(&self.vcpus, |tx| VcpuCmd::GicRegisters(None, tx))?;
                for (cpu, (now, saved)) in now.iter().zip(gic_cpus).enumerate() {
                    differ.extend(now.iter().zip(saved).filter(|(a, b)| a != b).map(|(a, b)| format!("cpu{cpu} GICR+{:#x} {:#x}->{:#x}", a.0, b.1, a.1)));
                }
                if !differ.is_empty() {
                    return Err(Error::Setup(format!("interrupt controller off the baseline: {}", differ.join(", "))));
                }
            }
        }
        Ok(())
    }

    /// Save the guest screen as PNG. Returns the mode. Other monitors
    /// that are on go next to it, as `<name>-2.png` and so on.
    pub fn screendump(&self, path: &Path) -> Result<(u32, u32)> {
        let Some(first) = self.screens.first() else { return Err(Error::Setup("VM has no display".into())) };
        for (i, s) in self.screens.iter().enumerate().skip(1) {
            if s.mode().0 > 0 {
                let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let _ = s.screendump(&path.with_file_name(format!("{stem}-{}.png", i + 1)));
            }
        }
        Ok(first.screendump(path)?)
    }

    /// Tell the viewer something (status line).
    pub fn viewer_status(&self, text: &str) {
        if let Some(v) = &self.viewer {
            v.send(&vroom_display::ToViewer::Status(text.to_string()));
        }
    }

    /// Pids of the helper processes this VM started.
    pub fn helper_pids(&self) -> Vec<u32> {
        HELPERS.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Free space where this VM's disk layers live.
    pub fn host_free_bytes(&self) -> std::io::Result<u64> {
        let state = self._lock.free_bytes()?;
        Ok(self.disks.min_free().map_or(state, |d| d.min(state)))
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
        #[cfg(feature = "research")]
        self.research_devices_ran();
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
        let parked = self.park_direct3d()?;
        self.pause()?;
        let r = snapshot::save(self, dir, description, snapshot::Kind::Snapshot);
        if parked {
            self.unpark_direct3d();
        }
        if was_running {
            self.resume()?;
        }
        r
    }

    /// Ask for the 3D display back (delivered once the VM runs).
    fn unpark_direct3d(&mut self) {
        for g in &self.gpus {
            g.set_driver_stopped(false);
        }
        self.agent.send(agent::AgentCmd::Gpu3d(true));
    }

    /// Windows' Direct3D state lives on the host GPU and can't be saved.
    /// Have the guest agent turn the 3D display device off first, as in
    /// Device Manager: Windows goes on with its basic display and drops
    /// every Direct3D object; the agent turns it back on when it next
    /// connects, or right after a snapshot. Whether it was parked.
    fn park_direct3d(&mut self) -> Result<bool> {
        let gpus = self.gpus.clone();
        let direct3d = || gpus.iter().any(|g| g.direct3d());
        if !direct3d() || self.state == RunState::Stopped {
            return Ok(false);
        }
        let s = self.agent.status();
        if !s.connected || s.features & vroom_proto::features::GPU3D == 0 {
            return Err(Error::CantSave(
                "Windows draws with Direct3D, and its vroom agent can't turn 3D off for the save (not running, or too old). Update the vroom tools in Windows".into(),
            ));
        }
        // A paused guest can't do it: let it run for that (the caller
        // pauses it again to save).
        let paused = self.state == RunState::Paused;
        if paused {
            self.resume()?;
        }
        log::info!("turning the guest's 3D display off for the save");
        self.agent.send(agent::AgentCmd::Gpu3d(false));
        let deadline = Instant::now() + PARK_TIMEOUT;
        let mut off_since = None;
        let mut dropping = false;
        loop {
            let off = self.agent.status().gpu3d == Some(false);
            if off && !direct3d() {
                break;
            }
            // Off, the driver is stopped and resets the GPU when it starts
            // again: what it didn't destroy is nobody's. Windows destroys a
            // process's contexts after the driver can no longer send that
            // (a suspend waited for them in vain), and the process may be
            // stuck waiting on them: drop them, which tells it so.
            if off && !dropping && off_since.get_or_insert_with(Instant::now).elapsed() > PARK_LEFTOVERS {
                log::info!("the guest's 3D driver stopped with Direct3D contexts left: dropping them");
                for g in &self.gpus {
                    g.set_driver_stopped(true);
                }
                dropping = true;
            }
            if Instant::now() > deadline {
                self.unpark_direct3d();
                if paused {
                    self.pause()?;
                }
                return Err(Error::CantSave("Windows didn't turn its 3D display off in time".into()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // Let Windows finish moving to its basic display before we freeze it.
        std::thread::sleep(PARK_SETTLE);
        Ok(true)
    }

    /// Snapshot then stop: the VM can be resumed later with
    /// [`StartMode::Restore`]. This is "suspend to disk".
    pub fn suspend(mut self, dir: &Path) -> Result<()> {
        self.suspend_in_place(dir)
    }

    /// Like [`suspend`], for callers that keep the handle. On error the
    /// VM is left paused, not stopped, so nothing is lost.
    pub fn suspend_in_place(&mut self, dir: &Path) -> Result<()> {
        let parked = self.park_direct3d()?;
        self.pause()?;
        if let Err(e) = snapshot::save(self, dir, "suspend", snapshot::Kind::Suspend) {
            if parked {
                self.unpark_direct3d();
            }
            return Err(e);
        }
        self.shutdown();
        Ok(())
    }

    /// Tear everything down. vCPUs are destroyed on their own threads.
    pub fn shutdown(&mut self) {
        self.agent.send(agent::AgentCmd::Stop);
        self.disks.flush_all();
        if let Some(v) = &self.viewer {
            v.send(&vroom_display::ToViewer::Closed);
        }
        for v in &self.vcpus {
            let _ = v.peer.send(VcpuCmd::Stop);
        }
        for v in &mut self.vcpus {
            v.join();
        }
        if let Some(d) = &self.display {
            d.stop();
        }
        self.state = RunState::Stopped;
    }
}

/// Whether a VM can start from snapshot `dir`: its manifest and saved
/// VM file read, and its disk layers are as they were saved.
pub fn check_snapshot(dir: &Path) -> Result<()> {
    snapshot::saved_config(dir)?;
    snapshot::disk_plan(dir)?;
    Ok(())
}

impl Drop for Vmm {
    fn drop(&mut self) {
        if self.state != RunState::Stopped {
            self.shutdown();
        }
    }
}

#[cfg(feature = "research")]
/// Send every vCPU a command and collect the answers in order.
fn ask_vcpus<T>(vcpus: &[vcpu::VcpuHandle], command: impl Fn(mpsc::Sender<Result<T>>) -> VcpuCmd) -> Result<Vec<T>> {
    let mut out = Vec::with_capacity(vcpus.len());
    for v in vcpus {
        let (tx, rx) = mpsc::channel();
        v.peer.send(command(tx))?;
        out.push(rx.recv_timeout(PAUSE_TIMEOUT).map_err(|_| Error::Timeout)??);
    }
    Ok(out)
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
