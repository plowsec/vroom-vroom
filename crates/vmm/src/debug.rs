//! Debugger access requires an acknowledged all-CPU/device pause.
use crate::{Error, Result, RunState, VcpuCmd, Vmm, PAUSE_TIMEOUT};
use std::sync::mpsc;
use vroom_hv::{debug::DebugConfig, VcpuState};
use vroom_memory::GuestAddress;

impl Vmm {
    pub fn debug_write_register(
        &self,
        cpu: usize,
        register: vroom_hv::debug::Register,
        value: u64,
    ) -> Result<()> {
        self.require_debug_pause()?;
        register.validate(value)?;
        let v = self
            .vcpus
            .get(cpu)
            .ok_or_else(|| Error::Setup("invalid CPU index".into()))?;
        let (tx, rx) = mpsc::channel();
        v.peer.send(VcpuCmd::WriteRegister(register, value, tx))?;
        rx.recv_timeout(PAUSE_TIMEOUT).map_err(|_| Error::Timeout)?
    }

    /// Validate every RAM span before changing any byte; MMIO is never edited.
    pub fn debug_write_physical(&self, spans: &[(u64, &[u8])]) -> Result<()> {
        self.require_debug_pause()?;
        for (address, bytes) in spans {
            if !self
                .mem
                .check_range(GuestAddress(*address), bytes.len() as u64)
            {
                return Err(Error::Setup(format!("write outside RAM at {address:#x}")));
            }
        }
        for (address, bytes) in spans {
            self.mem.write(GuestAddress(*address), bytes)?;
        }
        // Guest executable RAM shares these host physical pages. Synchronize
        // the instruction cache after patches, before any vCPU can resume.
        for (address, bytes) in spans {
            for (pointer, length) in self
                .mem
                .host_ranges(GuestAddress(*address), bytes.len() as u64)
                .expect("validated RAM")
            {
                // SAFETY: each range is mapped, stable guest RAM and all vCPUs are paused.
                unsafe {
                    sync_instruction_cache(pointer, length);
                }
            }
        }
        Ok(())
    }

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

    /// Resume only the selected CPU: the other CPUs and the device
    /// workers stay parked (a snapshot fuzzer running one thread without
    /// the rest of the guest). The CPU can still access devices; a research
    /// reset then restores them.
    pub fn debug_resume_cpu(&mut self, cpu: usize) -> Result<()> {
        self.require_debug_pause()?;
        let v = self
            .vcpus
            .get(cpu)
            .ok_or_else(|| Error::Setup("invalid CPU index".into()))?;
        v.peer.send(VcpuCmd::Resume)?;
        self.state = RunState::Running;
        Ok(())
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

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) unsafe fn sync_instruction_cache(pointer: *mut u8, length: usize) {
    unsafe extern "C" {
        fn sys_icache_invalidate(start: *mut std::ffi::c_void, length: usize);
    }
    // SAFETY: caller guarantees a live mapped range.
    unsafe {
        sys_icache_invalidate(pointer.cast(), length);
    }
}
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub(crate) unsafe fn sync_instruction_cache(pointer: *mut u8, length: usize) {
    let ctr: u64;
    // SAFETY: ARM64 EL0 cache-maintenance sequence over the caller's mapped range.
    unsafe {
        std::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack));
        let data_line = 4usize << ((ctr >> 16) & 15);
        let instruction_line = 4usize << (ctr & 15);
        let end = pointer as usize + length;
        for address in ((pointer as usize & !(data_line - 1))..end).step_by(data_line) {
            std::arch::asm!("dc cvau, {}", in(reg) address, options(nostack));
        }
        std::arch::asm!("dsb ish", options(nostack));
        for address in ((pointer as usize & !(instruction_line - 1))..end).step_by(instruction_line)
        {
            std::arch::asm!("ic ivau, {}", in(reg) address, options(nostack));
        }
        std::arch::asm!("dsb ish", "isb", options(nostack));
    }
}
#[cfg(not(target_arch = "aarch64"))]
pub(crate) unsafe fn sync_instruction_cache(_pointer: *mut u8, _length: usize) {}
