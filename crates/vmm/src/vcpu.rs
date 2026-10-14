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

use bulwark_devices::Bus;
use bulwark_hv::{BootRegs, Vcpu, VcpuExit, VcpuKicker, VcpuState, Vm};

use crate::psci::{self, CpuTable, PsciOutcome};
use crate::{Error, Result};

/// Commands for a vCPU thread. Replies go back on the embedded channel.
pub enum VcpuCmd {
    /// Start executing (boot CPU, or PSCI CPU_ON target).
    PowerOn(BootRegs),
    Pause(Sender<()>),
    Resume,
    Save(Sender<Result<VcpuState>>),
    /// Load saved registers; `on` says whether the CPU should run on resume.
    Restore(Box<VcpuState>, bool, Sender<Result<()>>),
    /// Back to power-on state. Boot CPU gets `Some(regs)`.
    Reset(Option<BootRegs>, Sender<Result<()>>),
    Stop,
}

impl std::fmt::Debug for VcpuCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            VcpuCmd::PowerOn(_) => "PowerOn",
            VcpuCmd::Pause(_) => "Pause",
            VcpuCmd::Resume => "Resume",
            VcpuCmd::Save(_) => "Save",
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
    Shutdown,
    Reset,
    Crashed(String),
    /// Lifecycle request from the viewer window.
    Ui(bulwark_display::Command),
    ViewerClosed,
}

/// Shared by all vCPU threads.
pub struct VcpuShared {
    pub vm: Arc<dyn Vm>,
    pub mmio: Bus,
    pub cpus: Mutex<CpuTable>,
    pub events: Sender<VmEvent>,
    /// Filled once all threads are up; used for CPU_ON.
    pub peers: OnceLock<Vec<Peer>>,
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
pub fn spawn(index: u32, shared: Arc<VcpuShared>) -> Result<VcpuHandle> {
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
            let pristine = {
                let mut v = vcpu;
                match v.save_state() {
                    Ok(s) => {
                        let _ = ready_tx.send(Ok(v.kicker()));
                        (v, s)
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(Error::Hv(e)));
                        return;
                    }
                }
            };
            VcpuLoop { vcpu: pristine.0, pristine: pristine.1, shared, rx, running: false, paused: false }
                .run();
        })
        .map_err(|e| Error::Setup(format!("spawn vcpu thread: {e}")))?;
    let kicker = ready_rx.recv().map_err(|_| Error::VcpuGone)??;
    Ok(VcpuHandle { peer: Peer { tx, kicker }, thread: Some(thread) })
}

struct VcpuLoop {
    vcpu: Box<dyn Vcpu>,
    pristine: VcpuState,
    shared: Arc<VcpuShared>,
    rx: Receiver<VcpuCmd>,
    running: bool,
    paused: bool,
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
                let _ = self.shared.events.send(VmEvent::Crashed(e.to_string()));
                self.running = false;
            }
        }
    }

    /// Returns false when the thread should exit.
    fn command(&mut self, cmd: VcpuCmd) -> bool {
        match cmd {
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
                let r = self.vcpu.save_state().map(|mut s| {
                    s.powered_on = self.running;
                    s
                });
                let _ = reply.send(r.map_err(Error::Hv));
            }
            VcpuCmd::Restore(state, on, reply) => {
                let r = self.vcpu.restore_state(&state).map_err(Error::Hv);
                if r.is_ok() {
                    self.running = on;
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

    fn power_on(&mut self, regs: &BootRegs) -> Result<()> {
        // Fresh architectural state, then the entry point.
        self.vcpu.restore_state(&self.pristine)?;
        self.vcpu.set_boot_regs(regs)?;
        Ok(())
    }

    fn reset(&mut self, regs: Option<&BootRegs>) -> Result<()> {
        self.vcpu.restore_state(&self.pristine)?;
        self.running = false;
        if let Some(r) = regs {
            self.vcpu.set_boot_regs(r)?;
            self.running = true;
        }
        Ok(())
    }

    fn step(&mut self) -> Result<()> {
        match self.vcpu.run()? {
            VcpuExit::Kicked => {}
            VcpuExit::MmioRead { addr, len } => {
                let mut buf = [0u8; 8];
                let len = len.min(8);
                if !self.shared.mmio.read(addr, &mut buf[..len]) {
                    log::debug!("vcpu{}: read from unmapped {addr:#x}", self.index());
                }
                self.vcpu.complete_mmio_read(&buf[..len])?;
            }
            VcpuExit::MmioWrite { addr, len, data } => {
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
                        bulwark_hv::aarch64::psci::INTERNAL_FAILURE
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

