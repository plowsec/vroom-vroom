//! Debugger access requires an acknowledged all-CPU/device pause.
use crate::{Error, Result, RunState, VcpuCmd, Vmm, PAUSE_TIMEOUT};
use std::sync::mpsc;
use vroom_hv::{debug::DebugConfig, VcpuState};
use vroom_memory::GuestAddress;

impl Vmm {
    fn require_debug_pause(&self) -> Result<()> {
        if self.state() != RunState::Paused {
            return Err(Error::Setup(
                "pause every CPU and device before inspecting guest state".into(),
            ));
        }
        Ok(())
    }

    pub fn debug_registers(&self, cpu: usize) -> Result<VcpuState> {
        self.require_debug_pause()?;
        let v = self
            .vcpus
            .get(cpu)
            .ok_or_else(|| Error::Setup("invalid CPU index".into()))?;
        let (tx, rx) = mpsc::channel();
        v.peer.send(VcpuCmd::Save(tx))?;
        rx.recv_timeout(PAUSE_TIMEOUT).map_err(|_| Error::Timeout)?
    }

    pub fn debug_configure(&self, cpu: usize, config: DebugConfig) -> Result<()> {
        self.require_debug_pause()?;
        let v = self
            .vcpus
            .get(cpu)
            .ok_or_else(|| Error::Setup("invalid CPU index".into()))?;
        let (tx, rx) = mpsc::channel();
        v.peer.send(VcpuCmd::Debug(config, tx))?;
        rx.recv_timeout(PAUSE_TIMEOUT).map_err(|_| Error::Timeout)?
    }

    pub fn debug_read_physical(&self, address: u64, buf: &mut [u8]) -> Result<()> {
        self.require_debug_pause()?;
        self.mem.read(GuestAddress(address), buf)?;
        Ok(())
    }

    pub fn debug_memory_regions(&self) -> Result<Vec<(u64, u64)>> {
        self.require_debug_pause()?;
        Ok(self.mem.regions().map(|r| (r.start().0, r.len())).collect())
    }

    /// Run only the selected CPU; all other CPUs and device workers stay parked.
    /// The caller consumes the Debug event, then calls pause() to acknowledge
    /// the completed stop before reading memory again.
    pub fn debug_step(&mut self, cpu: usize, mut config: DebugConfig) -> Result<()> {
        self.require_debug_pause()?;
        if !self.debug_registers(cpu)?.powered_on {
            return Err(Error::Setup("cannot step a powered-off CPU".into()));
        }
        config.enabled = true;
        config.single_step = true;
        self.debug_configure(cpu, config)?;
        self.vcpus[cpu].peer.send(VcpuCmd::Resume)?;
        self.state = RunState::Running;
        Ok(())
    }
}
