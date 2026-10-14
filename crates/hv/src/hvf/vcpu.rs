use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::ffi::{self, check};
use super::BACKEND;
use crate::aarch64::{self, DataAbort, ExceptionClass};
use crate::{BootRegs, Error, Result, Vcpu, VcpuExit, VcpuKicker, VcpuState};

/// How long a WFI sleep may last before we re-check for SGIs sent by other
/// vCPUs. HVF delivers those in-kernel and gives us no wakeup signal.
const WFI_POLL_SLICE: Duration = Duration::from_millis(1);

#[derive(Debug)]
pub struct HvfKicker {
    vcpu: u64,
    kicked: AtomicBool,
    wake_flag: Mutex<bool>,
    cv: Condvar,
}

impl HvfKicker {
    fn take_kick(&self) -> bool {
        self.kicked.swap(false, Ordering::AcqRel)
    }
}

impl VcpuKicker for HvfKicker {
    fn kick(&self) {
        self.kicked.store(true, Ordering::Release);
        // SAFETY: hv_vcpus_exit is thread-safe and takes a vcpu id array.
        unsafe { ffi::hv_vcpus_exit(&self.vcpu, 1) };
        self.wake();
    }

    fn wake(&self) {
        let mut f = self.wake_flag.lock().unwrap_or_else(|e| e.into_inner());
        *f = true;
        self.cv.notify_one();
    }
}

#[derive(Debug)]
pub struct HvfVcpu {
    id: u64,
    index: u32,
    mpidr: u64,
    exit: *const ffi::hv_vcpu_exit_t,
    kicker: Arc<HvfKicker>,
    pending_read: Option<DataAbort>,
    vtimer_masked: bool,
    vtimer_offset: Arc<AtomicU64>,
}

impl HvfVcpu {
    pub(super) fn new(index: u32, vtimer_offset: Arc<AtomicU64>) -> Result<Self> {
        let mut id = 0u64;
        let mut exit: *const ffi::hv_vcpu_exit_t = std::ptr::null();
        // SAFETY: out pointers valid; default config.
        check("hv_vcpu_create", unsafe {
            ffi::hv_vcpu_create(&mut id, &mut exit, std::ptr::null_mut())
        })?;
        let mpidr = aarch64::mpidr_for(index);
        let v = HvfVcpu {
            id,
            index,
            mpidr,
            exit,
            kicker: Arc::new(HvfKicker {
                vcpu: id,
                kicked: AtomicBool::new(false),
                wake_flag: Mutex::new(false),
                cv: Condvar::new(),
            }),
            pending_read: None,
            vtimer_masked: false,
            vtimer_offset,
        };
        v.set_sys(ffi::HV_SYS_REG_MPIDR_EL1, mpidr)?;
        v.apply_vm_clock()?;
        Ok(v)
    }

    pub(super) fn hvf_kicker(&self) -> Arc<HvfKicker> {
        self.kicker.clone()
    }

    fn get(&self, reg: u32) -> Result<u64> {
        let mut v = 0;
        // SAFETY: own vcpu on own thread; valid out pointer.
        check("hv_vcpu_get_reg", unsafe { ffi::hv_vcpu_get_reg(self.id, reg, &mut v) })?;
        Ok(v)
    }

    fn set(&self, reg: u32, v: u64) -> Result<()> {
        // SAFETY: own vcpu on own thread.
        check("hv_vcpu_set_reg", unsafe { ffi::hv_vcpu_set_reg(self.id, reg, v) })
    }

    fn get_sys(&self, reg: u16) -> Result<u64> {
        let mut v = 0;
        // SAFETY: as above.
        check("hv_vcpu_get_sys_reg", unsafe { ffi::hv_vcpu_get_sys_reg(self.id, reg, &mut v) })?;
        Ok(v)
    }

    fn set_sys(&self, reg: u16, v: u64) -> Result<()> {
        // SAFETY: as above.
        check("hv_vcpu_set_sys_reg", unsafe { ffi::hv_vcpu_set_sys_reg(self.id, reg, v) })
    }

    /// Read Xn, where 31 means XZR.
    fn get_x(&self, n: u8) -> Result<u64> {
        if n == 31 {
            Ok(0)
        } else {
            self.get(ffi::HV_REG_X0 + n as u32)
        }
    }

    fn set_x(&self, n: u8, v: u64) -> Result<()> {
        if n == 31 {
            Ok(())
        } else {
            self.set(ffi::HV_REG_X0 + n as u32, v)
        }
    }

    fn advance_pc(&self) -> Result<()> {
        let pc = self.get(ffi::HV_REG_PC)?;
        self.set(ffi::HV_REG_PC, pc.wrapping_add(4))
    }

    fn gicr(&self, reg: u32) -> Result<u64> {
        let mut v = 0;
        // SAFETY: owning thread; valid out pointer.
        check("hv_gic_get_redistributor_reg", unsafe {
            ffi::hv_gic_get_redistributor_reg(self.id, reg, &mut v)
        })?;
        Ok(v)
    }

    fn set_gicr(&self, reg: u32, v: u64) -> Result<()> {
        // SAFETY: owning thread.
        check("hv_gic_set_redistributor_reg", unsafe {
            ffi::hv_gic_set_redistributor_reg(self.id, reg, v)
        })
    }

    /// Use the VM-wide counter offset on this vCPU.
    fn apply_vm_clock(&self) -> Result<()> {
        let off = self.vtimer_offset.load(Ordering::SeqCst);
        // SAFETY: owning thread.
        check("hv_vcpu_set_vtimer_offset", unsafe { ffi::hv_vcpu_set_vtimer_offset(self.id, off) })
    }

    /// Guest virtual counter now.
    fn vcounter(&self) -> Result<u64> {
        let mut off = 0;
        // SAFETY: owning thread; valid out pointer.
        check("hv_vcpu_get_vtimer_offset", unsafe {
            ffi::hv_vcpu_get_vtimer_offset(self.id, &mut off)
        })?;
        // SAFETY: no preconditions.
        Ok(unsafe { ffi::mach_absolute_time() }.wrapping_sub(off))
    }

    /// The guest timer fires: is the line asserted right now?
    fn vtimer_asserted(&self) -> Result<bool> {
        let ctl = self.get_sys(ffi::HV_SYS_REG_CNTV_CTL_EL0)?;
        // ENABLE=1, IMASK=0, ISTATUS=1
        Ok(ctl & 0b111 == 0b101)
    }

    /// Level-triggered timer emulation: once the guest has handled the
    /// timer (condition gone) we drop the PPI and let HVF signal again.
    fn sync_vtimer(&mut self) -> Result<()> {
        if self.vtimer_masked && !self.vtimer_asserted()? {
            self.set_gicr(ffi::GICR_ICPENDR0, 1 << ffi::HV_GIC_INT_EL1_VIRTUAL_TIMER)?;
            // SAFETY: owning thread.
            check("hv_vcpu_set_vtimer_mask", unsafe { ffi::hv_vcpu_set_vtimer_mask(self.id, false) })?;
            self.vtimer_masked = false;
        }
        Ok(())
    }

    /// Nanoseconds until the guest timer fires, if armed.
    fn vtimer_deadline_ns(&self) -> Result<Option<u64>> {
        let ctl = self.get_sys(ffi::HV_SYS_REG_CNTV_CTL_EL0)?;
        if ctl & 1 == 0 || ctl & 2 != 0 {
            return Ok(None);
        }
        let cval = self.get_sys(ffi::HV_SYS_REG_CNTV_CVAL_EL0)?;
        let now = self.vcounter()?;
        Ok(Some(if cval <= now { 0 } else { ffi::ticks_to_ns(cval - now) }))
    }

    /// SGI/PPI pending and enabled for this vCPU.
    fn local_irq_pending(&self) -> Result<bool> {
        Ok(self.gicr(ffi::GICR_ISPENDR0)? & self.gicr(ffi::GICR_ISENABLER0)? != 0)
    }

    /// Emulate WFI. Returns true if we were kicked.
    fn wait_for_interrupt(&mut self) -> Result<bool> {
        loop {
            if self.kicker.kicked.load(Ordering::Acquire) {
                return Ok(true);
            }
            if self.local_irq_pending()? {
                return Ok(false);
            }
            let deadline = self.vtimer_deadline_ns()?;
            if deadline == Some(0) {
                return Ok(false);
            }
            let slice = deadline
                .map(|ns| Duration::from_nanos(ns).min(WFI_POLL_SLICE))
                .unwrap_or(WFI_POLL_SLICE);
            let mut f = self.kicker.wake_flag.lock().unwrap_or_else(|e| e.into_inner());
            if !*f {
                f = self.kicker.cv.wait_timeout(f, slice).unwrap_or_else(|e| e.into_inner()).0;
            }
            if std::mem::take(&mut *f) {
                return Ok(self.kicker.kicked.load(Ordering::Acquire));
            }
        }
    }

    fn handle_exception(&mut self, esr: u64, ipa: u64) -> Result<Option<VcpuExit>> {
        match aarch64::exception_class(esr) {
            ExceptionClass::DataAbortLower => {
                let Some(da) = aarch64::decode_data_abort(esr) else {
                    return Err(Error::GuestFault(format!(
                        "undecodable MMIO access at {ipa:#x} (esr {esr:#x})"
                    )));
                };
                self.advance_pc()?;
                if da.write {
                    let v = self.get_x(da.reg)?;
                    let mut data = [0u8; 8];
                    data.copy_from_slice(&v.to_le_bytes());
                    Ok(Some(VcpuExit::MmioWrite { addr: ipa, len: da.len, data }))
                } else {
                    self.pending_read = Some(da);
                    Ok(Some(VcpuExit::MmioRead { addr: ipa, len: da.len }))
                }
            }
            ExceptionClass::WfxTrap => {
                self.advance_pc()?;
                if esr & 1 == 1 {
                    // WFE: just yield.
                    std::thread::yield_now();
                    return Ok(None);
                }
                if self.wait_for_interrupt()? {
                    self.kicker.take_kick();
                    return Ok(Some(VcpuExit::Kicked));
                }
                Ok(None)
            }
            ExceptionClass::Hvc64 => self.hypercall_exit(),
            ExceptionClass::Smc64 => {
                // SMC returns to the SMC itself; step over it.
                self.advance_pc()?;
                self.hypercall_exit()
            }
            ExceptionClass::SysReg => {
                let s = aarch64::decode_sysreg(esr);
                // Debug/PMU/OS-lock registers HVF doesn't handle: RAZ/WI.
                log::debug!("vcpu{}: sysreg {:#x} {} RAZ/WI", self.index, s.key(), if s.read { "read" } else { "write" });
                if s.read {
                    self.set_x(s.reg, 0)?;
                }
                self.advance_pc()?;
                Ok(None)
            }
            other => {
                let pc = self.get(ffi::HV_REG_PC)?;
                Err(Error::GuestFault(format!(
                    "unhandled exception {other:?} esr={esr:#x} ipa={ipa:#x} pc={pc:#x}"
                )))
            }
        }
    }

    fn hypercall_exit(&mut self) -> Result<Option<VcpuExit>> {
        let func = self.get_x(0)?;
        let args = [self.get_x(1)?, self.get_x(2)?, self.get_x(3)?];
        Ok(Some(VcpuExit::Hypercall { func, args }))
    }
}

impl Drop for HvfVcpu {
    fn drop(&mut self) {
        // SAFETY: destroyed on the owning thread (we are !Send).
        unsafe { ffi::hv_vcpu_destroy(self.id) };
    }
}

impl Vcpu for HvfVcpu {
    fn index(&self) -> u32 {
        self.index
    }

    fn run(&mut self) -> Result<VcpuExit> {
        loop {
            if self.kicker.take_kick() {
                return Ok(VcpuExit::Kicked);
            }
            self.sync_vtimer()?;
            // SAFETY: own vcpu, own thread.
            check("hv_vcpu_run", unsafe { ffi::hv_vcpu_run(self.id) })?;
            // SAFETY: HVF keeps `exit` valid for the vcpu's lifetime and
            // fills it before hv_vcpu_run returns.
            let exit = unsafe { *self.exit };
            match exit.reason {
                ffi::HV_EXIT_REASON_CANCELED => {
                    if self.kicker.take_kick() {
                        return Ok(VcpuExit::Kicked);
                    }
                }
                ffi::HV_EXIT_REASON_VTIMER_ACTIVATED => {
                    self.set_gicr(ffi::GICR_ISPENDR0, 1 << ffi::HV_GIC_INT_EL1_VIRTUAL_TIMER)?;
                    self.vtimer_masked = true;
                }
                ffi::HV_EXIT_REASON_EXCEPTION => {
                    let e = exit.exception;
                    if let Some(x) = self.handle_exception(e.syndrome, e.physical_address)? {
                        return Ok(x);
                    }
                }
                r => return Err(Error::GuestFault(format!("unknown exit reason {r}"))),
            }
        }
    }

    fn complete_mmio_read(&mut self, data: &[u8]) -> Result<()> {
        let da = self
            .pending_read
            .take()
            .ok_or_else(|| Error::Invalid("no MMIO read pending".into()))?;
        self.set_x(da.reg, da.read_value(data))
    }

    fn complete_pio_in(&mut self, _data: &[u8]) -> Result<()> {
        Err(Error::Unsupported("no port I/O on arm64"))
    }

    fn complete_hypercall(&mut self, ret: [u64; 4]) -> Result<()> {
        for (i, v) in ret.iter().enumerate() {
            self.set_x(i as u8, *v)?;
        }
        Ok(())
    }

    fn kicker(&self) -> Arc<dyn VcpuKicker> {
        self.kicker.clone()
    }

    fn set_boot_regs(&mut self, regs: &BootRegs) -> Result<()> {
        let BootRegs::Aarch64 { pc, x } = regs else {
            return Err(Error::Invalid("x86 boot regs on arm64".into()));
        };
        for i in 0..31u8 {
            self.set_x(i, if (i as usize) < x.len() { x[i as usize] } else { 0 })?;
        }
        self.set(ffi::HV_REG_PC, *pc)?;
        self.set(ffi::HV_REG_CPSR, aarch64::PSTATE_EL1H_DAIF)?;
        Ok(())
    }

    fn arch_id(&self) -> u64 {
        self.mpidr
    }

    fn save_state(&mut self) -> Result<VcpuState> {
        let mut s = VcpuState { backend: BACKEND.into(), index: self.index, ..Default::default() };
        for r in 0..=ffi::HV_REG_CPSR {
            s.regs.push((r, self.get(r)?));
        }
        for &r in ffi::SNAPSHOT_SYS_REGS {
            s.regs.push((0x1_0000 | r as u32, self.get_sys(r)?));
        }
        for q in 0..32u32 {
            let mut v = [0u8; 16];
            // SAFETY: owning thread; v is 16 writable bytes.
            check("hv_vcpu_get_simd_fp_reg", unsafe {
                ffi::bw_hv_vcpu_get_simd_fp_reg(self.id, q, v.as_mut_ptr())
            })?;
            s.vregs.push((q, v));
        }
        for &r in ffi::SNAPSHOT_ICC_REGS {
            let mut v = 0;
            // SAFETY: owning thread, valid out pointer.
            check("hv_gic_get_icc_reg", unsafe { ffi::hv_gic_get_icc_reg(self.id, r, &mut v) })?;
            s.extra.push((r as u32, v));
        }
        s.powered_on = true;
        Ok(s)
    }

    fn restore_state(&mut self, s: &VcpuState) -> Result<()> {
        if s.backend != BACKEND {
            return Err(Error::Invalid(format!("vcpu state from backend {}", s.backend)));
        }
        for &(id, v) in &s.regs {
            if id & 0x1_0000 != 0 {
                let r = (id & 0xffff) as u16;
                if !ffi::SNAPSHOT_SYS_REGS.contains(&r) {
                    return Err(Error::Invalid(format!("unknown sysreg {r:#x} in snapshot")));
                }
                self.set_sys(r, v)?;
            } else if id <= ffi::HV_REG_CPSR {
                self.set(id, v)?;
            } else {
                return Err(Error::Invalid(format!("unknown reg {id} in snapshot")));
            }
        }
        for &(q, v) in &s.vregs {
            if q >= 32 {
                return Err(Error::Invalid(format!("bad simd reg {q}")));
            }
            // SAFETY: owning thread; v is 16 readable bytes.
            check("hv_vcpu_set_simd_fp_reg", unsafe { ffi::bw_hv_vcpu_set_simd_fp_reg(self.id, q, v.as_ptr()) })?;
        }
        for &(r, v) in &s.extra {
            let r = r as u16;
            if !ffi::SNAPSHOT_ICC_REGS.contains(&r) {
                return Err(Error::Invalid(format!("unknown icc reg {r:#x}")));
            }
            // SAFETY: owning thread.
            check("hv_gic_set_icc_reg", unsafe { ffi::hv_gic_set_icc_reg(self.id, r, v) })?;
        }
        // Counter is VM-wide; the VMM sets it via Vm::set_clock before this.
        self.apply_vm_clock()?;
        self.mpidr = self.get_sys(ffi::HV_SYS_REG_MPIDR_EL1)?;
        self.pending_read = None;
        Ok(())
    }
}
