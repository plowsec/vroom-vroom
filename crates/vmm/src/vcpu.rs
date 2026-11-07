//! vCPU threads.
//!
//! Each vCPU lives on its own thread for its whole life (HVF requires it).
//! Other threads talk to it with [`VcpuCmd`] messages, then kick it out of
//! guest mode so it notices. Anything that touches registers (snapshot,
//! reset) therefore runs on the owning thread.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use vroom_devices::Bus;
use vroom_hv::{BootRegs, Vcpu, VcpuExit, VcpuKicker, VcpuState, Vm};

use crate::psci::{self, CpuTable, PsciOutcome};
use crate::{Error, Result};
use crate::lifecycle::LifecyclePolicy;

/// Commands for a vCPU thread. Replies go back on the embedded channel.
pub enum VcpuCmd {
    WriteRegister(vroom_hv::debug::Register, u64, Sender<Result<()>>),
    Debug(vroom_hv::debug::DebugConfig, Sender<Result<()>>),
    /// Once-only initial boot, acknowledged before any CPU is resumed.
    Initialize(Option<BootRegs>, Sender<Result<()>>),
    /// Runtime PSCI CPU_ON, using pristine state on resettable boards.
    PowerOn(BootRegs),
    Pause(Sender<()>),
    Resume,
    Save(Sender<Result<VcpuState>>),
    /// This CPU's interrupt controller registers; load the given ones first.
    #[cfg(feature = "research")]
    GicRegisters(Option<Vec<(u32, u64)>>, Sender<Result<Vec<(u32, u64)>>>),
    /// Load saved registers; `on` says whether the CPU should run on resume.
    Restore(Box<VcpuState>, bool, Sender<Result<()>>),
    /// Back to power-on state. Boot CPU gets `Some(regs)`.
    Reset(Option<BootRegs>, Sender<Result<()>>),
    Stop,
}

impl std::fmt::Debug for VcpuCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            VcpuCmd::WriteRegister(..) => "WriteRegister",
            VcpuCmd::Debug(..) => "Debug",
            VcpuCmd::PowerOn(_) => "PowerOn",
            VcpuCmd::Initialize(..) => "Initialize",
            VcpuCmd::Pause(_) => "Pause",
            VcpuCmd::Resume => "Resume",
            VcpuCmd::Save(_) => "Save",
            #[cfg(feature = "research")]
            VcpuCmd::GicRegisters(..) => "GicRegisters",
            VcpuCmd::Restore(..) => "Restore",
            VcpuCmd::Reset(..) => "Reset",
            VcpuCmd::Stop => "Stop",
        };
        f.write_str(s)
    }
}

/// Things vCPU threads tell the VMM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmEvent {
    /// The originating CPU is parked. The frontend must pause the VM
    /// before inspecting RAM or other CPUs.
    Debug(vroom_hv::debug::DebugStop),
    Shutdown,
    Reset,
    Crashed(String),
    /// Lifecycle request from the viewer window.
    Ui(vroom_display::Command),
    /// The viewer went away without asking for anything (it crashed).
    ViewerClosed,
    /// A viewer (re)connected through the state dir's socket.
    ViewerAttached,
}

/// Shared by all vCPU threads.
pub struct VcpuShared {
    pub vm: Arc<dyn Vm>,
    pub mmio: Bus,
    pub cpus: Mutex<CpuTable>,
    pub events: Sender<VmEvent>,
    /// Filled once all threads are up; used for CPU_ON.
    pub peers: OnceLock<Vec<Peer>>,
    /// MMIO accesses vCPUs passed to devices (research resets skip the
    /// device restore when none happened and the devices stayed paused).
    pub device_exits: std::sync::atomic::AtomicU64,
    /// A debug stop parks every vCPU at once (research: the guest clock
    /// stands still from there). `halting`: a stop is parking them,
    /// `parked`: how many are, `halted_at`: the guest clock once all are
    /// (+1; 0: not yet).
    pub halt_on_stop: std::sync::atomic::AtomicBool,
    pub halting: std::sync::atomic::AtomicBool,
    pub parked: std::sync::atomic::AtomicUsize,
    pub halted_at: std::sync::atomic::AtomicU64,
}

impl VcpuShared {
    /// The halt is over (the VM resumes, or one CPU runs alone): the clock
    /// it recorded no longer holds.
    pub fn end_halt(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        self.halting.store(false, SeqCst);
        self.parked.store(0, SeqCst);
        self.halted_at.store(0, SeqCst);
    }
}

impl std::fmt::Debug for VcpuShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VcpuShared").finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct Peer {
    pub tx: Sender<VcpuCmd>,
    pub kicker: Arc<dyn VcpuKicker>,
}

impl std::fmt::Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Peer")
    }
}

impl Peer {
    pub fn send(&self, cmd: VcpuCmd) -> Result<()> {
        self.tx.send(cmd).map_err(|_| Error::VcpuGone)?;
        self.kicker.kick();
        Ok(())
    }
}

#[derive(Debug)]
pub struct VcpuHandle {
    pub peer: Peer,
    thread: Option<JoinHandle<()>>,
}

impl VcpuHandle {
    pub fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Spawn vCPU `index`. Returns once the vCPU exists (or failed to).
pub fn spawn(index: u32, shared: Arc<VcpuShared>, lifecycle: LifecyclePolicy) -> Result<VcpuHandle> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name(format!("vcpu{index}"))
        .spawn(move || {
            let vcpu = match shared.vm.create_vcpu(index) {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready_tx.send(Err(Error::Hv(e)));
                    return;
                }
            };
            let mut vcpu = vcpu;
            let pristine = if lifecycle == LifecyclePolicy::Resettable {
                match vcpu.save_state() {
                    Ok(state) => Some(state),
                    Err(error) => {
                        let _ = ready_tx.send(Err(Error::Hv(error)));
                        return;
                    }
                }
            } else {
                None
            };
            let _ = ready_tx.send(Ok(vcpu.kicker()));
            // Wait for acknowledged Initialize; KVM's irqchip is finalized first.
            VcpuLoop { vcpu, pristine, lifecycle, initialized: false, shared, rx,
                running: false, paused: false, stopped_by: None }.run();
        })
        .map_err(|e| Error::Setup(format!("spawn vcpu thread: {e}")))?;
    let kicker = ready_rx.recv().map_err(|_| Error::VcpuGone)??;
    Ok(VcpuHandle { peer: Peer { tx, kicker }, thread: Some(thread) })
}

struct VcpuLoop {
    vcpu: Box<dyn Vcpu>,
    pristine: Option<VcpuState>,
    lifecycle: LifecyclePolicy,
    initialized: bool,
    shared: Arc<VcpuShared>,
    rx: Receiver<VcpuCmd>,
    running: bool,
    paused: bool,
    /// Why this CPU stopped on a fault it can't go past: it is not saved
    /// then (a snapshot would bring the VM back without it).
    stopped_by: Option<String>,
}

impl VcpuLoop {
    fn index(&self) -> usize {
        self.vcpu.index() as usize
    }

    fn run(mut self) {
        loop {
            let cmd = if self.running && !self.paused {
                match self.rx.try_recv() {
                    Ok(c) => Some(c),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            } else {
                // Parked: block, but wake now and then in case the VMM died.
                match self.rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            if let Some(cmd) = cmd {
                if !self.command(cmd) {
                    return;
                }
                continue;
            }
            if let Err(e) = self.step() {
                log::error!("vcpu{}: {e}", self.index());
                self.stopped_by = Some(e.to_string());
                let _ = self.shared.events.send(VmEvent::Crashed(e.to_string()));
                self.running = false;
            }
        }
    }

    /// Returns false when the thread should exit.
    fn command(&mut self, cmd: VcpuCmd) -> bool {
        match cmd {
            #[cfg(feature = "research")]
            VcpuCmd::GicRegisters(restore, reply) => {
                let result = if self.paused {
                    self.vcpu.gic_cpu_registers(restore.as_deref()).map_err(Error::Hv)
                } else {
                    Err(Error::Setup("pause before touching interrupt registers".into()))
                };
                let _ = reply.send(result);
            }
            VcpuCmd::WriteRegister(register, value, reply) => {
                let result = if self.paused {
                    self.vcpu.debug_write_register(register, value).map_err(Error::Hv)
                } else { Err(Error::Setup("pause before editing registers".into())) };
                let _ = reply.send(result);
            }
            VcpuCmd::Debug(config, reply) => {
                let r = if self.paused {
                    self.vcpu.configure_debug(&config).map_err(Error::Hv)
                } else {
                    Err(Error::Setup("pause the CPU before configuring debug".into()))
                };
                let _ = reply.send(r);
            }
            VcpuCmd::Initialize(regs, reply) => {
                let result = self.initialize(regs.as_ref());
                let _ = reply.send(result);
            }
            VcpuCmd::PowerOn(regs) => match self.power_on(&regs) {
                Ok(()) => self.running = true,
                Err(e) => log::error!("vcpu{}: power on: {e}", self.index()),
            },
            VcpuCmd::Pause(ack) => {
                self.paused = true;
                let _ = ack.send(());
            }
            VcpuCmd::Resume => self.paused = false,
            VcpuCmd::Save(reply) => {
                if let Err(error) = self.lifecycle.require_state("snapshot") {
                    let _ = reply.send(Err(error));
                    return true;
                }
                if let Some(why) = &self.stopped_by {
                    let _ = reply.send(Err(Error::CantSave(format!("CPU {} stopped: {why}", self.index()))));
                    return true;
                }
                let kernel_psci = self.vcpu.in_kernel_psci();
                let r = self.vcpu.save_state().map(|mut s| {
                    if !kernel_psci { s.powered_on = self.running; }
                    s
                });
                let _ = reply.send(r.map_err(Error::Hv));
            }
            VcpuCmd::Restore(state, on, reply) => {
                if let Err(error) = self.lifecycle.require_state("restore") {
                    let _ = reply.send(Err(error));
                    return true;
                }
                let r = self.vcpu.restore_state(&state).map_err(Error::Hv);
                if r.is_ok() {
                    // In-kernel PSCI: off CPUs wait inside KVM_RUN.
                    self.running = on || self.vcpu.in_kernel_psci();
                }
                let _ = reply.send(r);
            }
            VcpuCmd::Reset(regs, reply) => {
                let r = self.reset(regs.as_ref());
                let _ = reply.send(r);
            }
            VcpuCmd::Stop => return false,
        }
        true
    }

    /// Stopped (a debug stop, or another CPU's): wait for commands. The
    /// last CPU to park during a halt records the guest clock.
    fn park_for_halt(&mut self) {
        use std::sync::atomic::Ordering::SeqCst;
        self.paused = true;
        if !self.shared.halting.load(SeqCst) {
            return;
        }
        let n = self.shared.parked.fetch_add(1, SeqCst) + 1;
        if n == self.shared.peers.get().map_or(usize::MAX, Vec::len) {
            self.shared.halted_at.store(self.shared.vm.clock() + 1, SeqCst);
        }
    }

    fn initialize(&mut self, regs: Option<&BootRegs>) -> Result<()> {
        if self.initialized {
            return Err(Error::Setup("vCPU initialization was already attempted".into()));
        }
        // A failed register write can leave partial state. Never retry a cold start.
        self.initialized = true;
        self.paused = true;
        if let Some(regs) = regs {
            if self.lifecycle == LifecyclePolicy::Resettable {
                self.power_on(regs)?;
            } else {
                self.vcpu.set_boot_regs(regs)?;
            }
        }
        self.running = regs.is_some() || self.vcpu.in_kernel_psci();
        Ok(())
    }

    fn power_on(&mut self, regs: &BootRegs) -> Result<()> {
        self.lifecycle.require_state("CPU power-on reset")?;
        // Fresh architectural state, then the entry point.
        let pristine = self.pristine.as_ref().ok_or_else(|| Error::Setup("missing pristine CPU state".into()))?;
        self.vcpu.restore_state(pristine)?;
        self.vcpu.set_boot_regs(regs)?;
        Ok(())
    }

    fn reset(&mut self, regs: Option<&BootRegs>) -> Result<()> {
        self.lifecycle.require_state("reset")?;
        let pristine = self.pristine.as_ref().ok_or_else(|| Error::Setup("missing pristine CPU state".into()))?;
        self.vcpu.restore_state(pristine)?;
        self.stopped_by = None;
        self.running = self.vcpu.in_kernel_psci() && self.index() != 0;
        if let Some(r) = regs {
            self.vcpu.set_boot_regs(r)?;
            self.running = true;
        }
        Ok(())
    }

    fn step(&mut self) -> Result<()> {
        match self.vcpu.run()? {
            VcpuExit::Debug(stop) => {
                use std::sync::atomic::Ordering::SeqCst;
                if self.shared.halt_on_stop.load(SeqCst) && !self.shared.halting.swap(true, SeqCst) {
                    let me = self.index();
                    for (i, peer) in self.shared.peers.get().into_iter().flatten().enumerate() {
                        if i != me {
                            peer.kicker.kick();
                        }
                    }
                }
                self.park_for_halt();
                let _ = self.shared.events.send(VmEvent::Debug(stop));
            }
            VcpuExit::Kicked => {
                if self.shared.halting.load(std::sync::atomic::Ordering::SeqCst) {
                    self.park_for_halt();
                }
            }
            VcpuExit::MmioRead { addr, len } => {
                self.shared.device_exits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut buf = [0u8; 8];
                let len = len.min(8);
                if !self.shared.mmio.read(addr, &mut buf[..len]) {
                    log::debug!("vcpu{}: read from unmapped {addr:#x}", self.index());
                }
                self.vcpu.complete_mmio_read(&buf[..len])?;
            }
            VcpuExit::MmioWrite { addr, len, data } => {
                self.shared.device_exits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if !self.shared.mmio.write(addr, &data[..len.min(8)]) {
                    log::debug!("vcpu{}: write to unmapped {addr:#x}", self.index());
                }
            }
            VcpuExit::Hypercall { func, args } => self.hypercall(func, args)?,
            VcpuExit::SystemOff => {
                self.running = false;
                let _ = self.shared.events.send(VmEvent::Shutdown);
            }
            VcpuExit::SystemReset => {
                self.running = false;
                let _ = self.shared.events.send(VmEvent::Reset);
            }
            VcpuExit::Halted => self.running = false,
            VcpuExit::PioIn { port, .. } | VcpuExit::PioOut { port, .. } => {
                return Err(Error::Setup(format!("port I/O {port:#x} on a board without ports")));
            }
        }
        Ok(())
    }

    fn hypercall(&mut self, func: u64, args: [u64; 3]) -> Result<()> {
        let me = self.index();
        let outcome = {
            let mut t = self.shared.cpus.lock().unwrap_or_else(|e| e.into_inner());
            psci::handle(func, args, me, &mut t)
        };
        let ret = match outcome {
            PsciOutcome::Return(v) => v,
            PsciOutcome::CpuOn { target, entry, ctx } => {
                let regs = BootRegs::Aarch64 { pc: entry, x: [ctx, 0, 0, 0] };
                match self.shared.peers.get().and_then(|p| p.get(target)) {
                    Some(peer) if peer.send(VcpuCmd::PowerOn(regs)).is_ok() => 0,
                    _ => {
                        self.shared.cpus.lock().unwrap_or_else(|e| e.into_inner()).set_on(target, false);
                        vroom_hv::aarch64::psci::INTERNAL_FAILURE
                    }
                }
            }
            PsciOutcome::CpuOff => {
                self.running = false;
                return Ok(());
            }
            PsciOutcome::SystemOff => {
                self.running = false;
                let _ = self.shared.events.send(VmEvent::Shutdown);
                return Ok(());
            }
            PsciOutcome::SystemReset => {
                self.running = false;
                let _ = self.shared.events.send(VmEvent::Reset);
                return Ok(());
            }
        };
        self.vcpu.complete_hypercall([ret as u64, 0, 0, 0])?;
        Ok(())
    }
}

