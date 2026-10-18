//! KVM on aarch64: vGICv3, in-kernel PSCI, ONE_REG register access.

use std::sync::Arc;

use kvm_bindings::{
    kvm_create_device, kvm_device_attr, kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3, kvm_mp_state, kvm_vcpu_init,
    RegList, KVM_ARM_VCPU_POWER_OFF, KVM_ARM_VCPU_PSCI_0_2, KVM_DEV_ARM_VGIC_CTRL_INIT, KVM_DEV_ARM_VGIC_GRP_ADDR,
    KVM_DEV_ARM_VGIC_GRP_CTRL, KVM_DEV_ARM_VGIC_GRP_NR_IRQS, KVM_MP_STATE_RUNNABLE, KVM_MP_STATE_STOPPED,
    KVM_SYSTEM_EVENT_RESET, KVM_VGIC_V3_ADDR_TYPE_DIST, KVM_VGIC_V3_ADDR_TYPE_REDIST,
};
use kvm_ioctls::{Cap, DeviceFd, Kvm, VcpuExit as KExit, VcpuFd, VmFd};

use super::{kerr, set_thread_run, Chip, KvmKicker};
use crate::aarch64::PSTATE_EL1H_DAIF;
use crate::{BootRegs, Error, IrqChipConfig, IrqChipInfo, Result, Vcpu, VcpuExit, VcpuKicker, VcpuState, VmParams};

const BACKEND: &str = "kvm";
/// Interrupt ids the vGIC implements (SPIs 32..NR_IRQS). KVM caps this
/// at 1023 and wants a multiple of 32.
pub(super) const NR_IRQS: u32 = 992;
const IPA_BITS: u32 = 40;

const CORE_BASE: u64 = 0x6030_0000_0010_0000;
const SYSREG_BASE: u64 = 0x6030_0000_0013_0000;

fn core_reg(byte_off: u64) -> u64 {
    CORE_BASE | (byte_off / 4)
}
fn x_reg(n: u64) -> u64 {
    core_reg(8 * n)
}
const PC: u64 = CORE_BASE | (256 / 4);
const PSTATE: u64 = CORE_BASE | (264 / 4);

fn sysreg(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    SYSREG_BASE | op0 << 14 | op1 << 11 | crn << 7 | crm << 3 | op2
}

pub(super) fn irqchip_info() -> IrqChipInfo {
    IrqChipInfo {
        dist_size: 0x1_0000,
        redist_size_per_cpu: 0x2_0000,
        msi_size: 0x1_0000,
        spi_base: 32,
        spi_count: NR_IRQS - 32,
        msi_frame_in_kernel: false,
    }
}

fn set_attr(what: &'static str, dev: &DeviceFd, group: u32, attr: u64, addr: u64) -> Result<()> {
    let a = kvm_device_attr { group, attr, addr, flags: 0 };
    dev.set_device_attr(&a).map_err(kerr(what))
}

pub(super) fn create_vm(kvm: &Kvm, params: &VmParams) -> Result<(VmFd, Chip)> {
    let IrqChipConfig::GicV3 { dist_base, redist_base, .. } = params.irqchip else {
        return Err(Error::Unsupported("aarch64 KVM needs a GICv3 layout"));
    };
    let max_ipa = kvm.check_extension_int(Cap::ArmVmIPASize);
    let fd = if max_ipa >= IPA_BITS as i32 {
        kvm.create_vm_with_ipa_size(IPA_BITS).map_err(kerr("KVM_CREATE_VM"))?
    } else {
        kvm.create_vm().map_err(kerr("KVM_CREATE_VM"))?
    };
    let mut dev = kvm_create_device { type_: kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3, fd: 0, flags: 0 };
    let gic = fd.create_device(&mut dev).map_err(kerr("create vGICv3"))?;
    // The kernel reads the addresses through these pointers during the call.
    set_attr("vgic dist addr", &gic, KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_VGIC_V3_ADDR_TYPE_DIST as u64, &dist_base as *const u64 as u64)?;
    set_attr("vgic redist addr", &gic, KVM_DEV_ARM_VGIC_GRP_ADDR, KVM_VGIC_V3_ADDR_TYPE_REDIST as u64, &redist_base as *const u64 as u64)?;
    let nr: u32 = NR_IRQS;
    set_attr("vgic nr_irqs", &gic, KVM_DEV_ARM_VGIC_GRP_NR_IRQS, 0, &nr as *const u32 as u64)?;
    Ok((fd, Chip::Gic(gic)))
}

pub(super) fn init_gic(dev: &DeviceFd) -> Result<()> {
    set_attr("vgic init", dev, KVM_DEV_ARM_VGIC_GRP_CTRL, KVM_DEV_ARM_VGIC_CTRL_INIT as u64, 0)
}

pub struct KvmVcpu {
    fd: VcpuFd,
    index: u32,
    mpidr: u64,
    kicker: Arc<KvmKicker>,
    pending_read: Option<usize>,
    debug_slots: usize,
    watch_slots: usize,
    guest_debug: bool,
}

impl std::fmt::Debug for KvmVcpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvmVcpu").field("index", &self.index).finish()
    }
}

impl KvmVcpu {
    pub(super) fn new(vm: &VmFd, index: u32) -> Result<Self> {
        let mut fd = vm.create_vcpu(index as u64).map_err(kerr("KVM_CREATE_VCPU"))?;
        let mut kvi = kvm_vcpu_init::default();
        vm.get_preferred_target(&mut kvi).map_err(kerr("KVM_ARM_PREFERRED_TARGET"))?;
        kvi.features[0] |= 1 << KVM_ARM_VCPU_PSCI_0_2;
        if index > 0 {
            // Secondaries wait in the kernel for PSCI CPU_ON.
            kvi.features[0] |= 1 << KVM_ARM_VCPU_POWER_OFF;
        }
        fd.vcpu_init(&kvi).map_err(kerr("KVM_ARM_VCPU_INIT"))?;
        set_thread_run(fd.get_kvm_run() as *mut _);
        let kvm = Kvm::new().map_err(kerr("open KVM capabilities"))?;
        let mut v = KvmVcpu { fd, index, mpidr: 0, kicker: KvmKicker::current(), pending_read: None,
            debug_slots: kvm.check_extension_int(Cap::DebugHwBps).max(0) as usize,
            watch_slots: kvm.check_extension_int(Cap::DebugHwWps).max(0) as usize,
            guest_debug: kvm.check_extension(Cap::SetGuestDebug),
        };
        v.mpidr = v.get(sysreg(3, 0, 0, 0, 5))?;
        Ok(v)
    }

    fn get(&self, id: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.fd.get_one_reg(id, &mut b).map_err(kerr("KVM_GET_ONE_REG"))?;
        Ok(u64::from_le_bytes(b))
    }

    fn set(&self, id: u64, v: u64) -> Result<()> {
        self.fd.set_one_reg(id, &v.to_le_bytes()).map(|_| ()).map_err(kerr("KVM_SET_ONE_REG"))
    }
}

impl Vcpu for KvmVcpu {
    fn debug_write_register(&mut self, register: crate::debug::Register, value: u64) -> Result<()> {
        use crate::debug::Register::*;
        register.validate(value)?;
        let id = match register {
            X(n) => x_reg(n as u64), Pc => PC, Pstate => PSTATE,
            Sp => core_reg(if self.get(PSTATE)? & 1 == 0 { 248 } else { 272 }),
            Ttbr0 => sysreg(3,0,2,0,0), Ttbr1 => sysreg(3,0,2,0,1),
            Tcr => sysreg(3,0,2,0,2), Sctlr => sysreg(3,0,1,0,0), Vbar => sysreg(3,0,12,0,0),
        };
        self.set(id, value)
    }

    fn configure_debug(&mut self, config: &crate::debug::DebugConfig) -> Result<()> {
        if !self.guest_debug {
            return if *config == Default::default() { Ok(()) }
                else { Err(Error::Unsupported("KVM_CAP_SET_GUEST_DEBUG")) };
        }
        let debug = debug_config(config, self.debug_slots, self.watch_slots)?;
        self.fd.set_guest_debug(&debug).map_err(kerr("KVM_SET_GUEST_DEBUG"))
    }

    fn index(&self) -> u32 {
        self.index
    }

    fn run(&mut self) -> Result<VcpuExit> {
        loop {
            if self.kicker.take() {
                return Ok(VcpuExit::Kicked);
            }
            let exit = match self.fd.run() {
                Ok(KExit::MmioRead(addr, data)) => {
                    let len = data.len();
                    self.pending_read = Some(len);
                    VcpuExit::MmioRead { addr, len }
                }
                Ok(KExit::MmioWrite(addr, data)) => {
                    let mut d = [0u8; 8];
                    let n = data.len().min(8);
                    d[..n].copy_from_slice(&data[..n]);
                    VcpuExit::MmioWrite { addr, len: n, data: d }
                }
                Ok(KExit::Debug(debug)) => {
                    let syndrome = (debug.hsr as u64) | ((debug.hsr_high as u64) << 32);
                    let reason = crate::debug::arm64_reason(syndrome)
                        .ok_or_else(|| Error::GuestFault(format!("unknown KVM debug syndrome {syndrome:#x}")))?;
                    VcpuExit::Debug(crate::debug::DebugStop { cpu: self.index, pc: self.get(PC)?, syndrome, reason })
                }
                Ok(KExit::SystemEvent(ty, _)) => {
                    if ty == KVM_SYSTEM_EVENT_RESET {
                        VcpuExit::SystemReset
                    } else {
                        VcpuExit::SystemOff
                    }
                }
                Ok(KExit::Hlt) | Ok(KExit::Intr) => continue,
                Ok(other) => return Err(Error::GuestFault(format!("unexpected KVM exit {other:?}"))),
                Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => {
                    self.fd.set_kvm_immediate_exit(0);
                    continue;
                }
                Err(e) => return Err(Error::Backend { call: "KVM_RUN", code: e.errno() as i64 }),
            };
            return Ok(exit);
        }
    }

    fn complete_mmio_read(&mut self, data: &[u8]) -> Result<()> {
        let len = self.pending_read.take().ok_or_else(|| Error::Invalid("no MMIO read pending".into()))?;
        let run = self.fd.get_kvm_run();
        // SAFETY: the last exit was KVM_EXIT_MMIO, so the mmio arm is active.
        let mmio = unsafe { &mut run.__bindgen_anon_1.mmio };
        let n = len.min(data.len()).min(8);
        mmio.data[..n].copy_from_slice(&data[..n]);
        Ok(())
    }

    fn complete_pio_in(&mut self, _data: &[u8]) -> Result<()> {
        Err(Error::Unsupported("no port I/O on arm64"))
    }

    fn complete_hypercall(&mut self, _ret: [u64; 4]) -> Result<()> {
        // PSCI runs in the kernel; no hypercalls reach us.
        Ok(())
    }

    fn kicker(&self) -> Arc<dyn VcpuKicker> {
        self.kicker.clone()
    }

    fn in_kernel_psci(&self) -> bool {
        true
    }

    fn set_boot_regs(&mut self, regs: &BootRegs) -> Result<()> {
        let BootRegs::Aarch64 { pc, x } = regs else {
            return Err(Error::Invalid("x86 boot regs on arm64".into()));
        };
        for (i, v) in x.iter().enumerate() {
            self.set(x_reg(i as u64), *v)?;
        }
        self.set(PC, *pc)?;
        self.set(PSTATE, PSTATE_EL1H_DAIF)?;
        let mp = kvm_mp_state { mp_state: KVM_MP_STATE_RUNNABLE };
        self.fd.set_mp_state(mp).map_err(kerr("KVM_SET_MP_STATE"))
    }

    fn arch_id(&self) -> u64 {
        self.mpidr
    }

    fn save_state(&mut self) -> Result<VcpuState> {
        let mut list = RegList::new(500).map_err(|_| Error::Invalid("reg list".into()))?;
        self.fd.get_reg_list(&mut list).map_err(kerr("KVM_GET_REG_LIST"))?;
        let mut raw = Vec::new();
        for &id in list.as_slice() {
            let size = 1usize << ((id >> 52) & 0xf);
            if size > 256 {
                continue;
            }
            let mut b = vec![0u8; size];
            if self.fd.get_one_reg(id, &mut b).is_ok() {
                raw.push((id, b));
            }
        }
        let mp = self.fd.get_mp_state().map_err(kerr("KVM_GET_MP_STATE"))?;
        Ok(VcpuState {
            backend: BACKEND.into(),
            index: self.index,
            powered_on: mp.mp_state != KVM_MP_STATE_STOPPED,
            raw,
            ..Default::default()
        })
    }

    fn restore_state(&mut self, s: &VcpuState) -> Result<()> {
        if s.backend != BACKEND {
            return Err(Error::Invalid(format!("vcpu state from backend {}", s.backend)));
        }
        for (id, b) in &s.raw {
            if b.len() != 1usize << ((id >> 52) & 0xf) {
                return Err(Error::Invalid(format!("reg {id:#x}: bad size")));
            }
            // Some registers are read-only / invariant; only core regs must stick.
            if let Err(e) = self.fd.set_one_reg(*id, b) {
                if id & 0xffff_0000 == CORE_BASE & 0xffff_0000 {
                    return Err(Error::Backend { call: "KVM_SET_ONE_REG", code: e.errno() as i64 });
                }
            }
        }
        let st = if s.powered_on { KVM_MP_STATE_RUNNABLE } else { KVM_MP_STATE_STOPPED };
        self.fd.set_mp_state(kvm_mp_state { mp_state: st }).map_err(kerr("KVM_SET_MP_STATE"))
    }
}

// KVM owns the guest/host debug-register switch and forwards guest BRKs
// unless USE_SW_BP is explicitly requested. No guest register emulation here.
fn debug_config(config: &crate::debug::DebugConfig, bps: usize, wps: usize) -> Result<kvm_bindings::kvm_guest_debug> {
    use kvm_bindings::*;
    if config.breakpoints.len() > bps.min(16) || config.watchpoints.len() > wps.min(16)
        || config.breakpoints.iter().any(|a| a & 3 != 0) {
        return Err(Error::Invalid("KVM debug slot limit or unaligned breakpoint".into()));
    }
    if config.trap_brk_immediate.is_some() {
        // KVM traps every BRK or none; handing the others back to the
        // guest from user space isn't implemented.
        return Err(Error::Unsupported("trapping one BRK immediate"));
    }
    if !config.enabled && (config.single_step || !config.breakpoints.is_empty()
        || !config.watchpoints.is_empty() || config.trap_software_breakpoints) {
        return Err(Error::Invalid("debug controls require enabled=true".into()));
    }
    let mut out = kvm_guest_debug::default();
    if config.enabled { out.control |= KVM_GUESTDBG_ENABLE; }
    if config.single_step { out.control |= KVM_GUESTDBG_SINGLESTEP; }
    if config.trap_software_breakpoints { out.control |= KVM_GUESTDBG_USE_SW_BP; }
    if !config.breakpoints.is_empty() || !config.watchpoints.is_empty() { out.control |= KVM_GUESTDBG_USE_HW; }
    for (i, address) in config.breakpoints.iter().enumerate() {
        out.arch.dbg_bvr[i] = *address;
        out.arch.dbg_bcr[i] = 1 | (3 << 1) | (15 << 5);
    }
    for (i, watch) in config.watchpoints.iter().enumerate() {
        out.arch.dbg_wvr[i] = watch.address & !7;
        out.arch.dbg_wcr[i] = watch.arm64_control()?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_controls_use_host_slots_and_leave_guest_brks_alone() {
        use crate::debug::*;
        let mut c = DebugConfig { enabled: true, single_step: true,
            breakpoints: vec![0x1000], watchpoints: vec![Watchpoint {address: 0x2003, size: 2, read: false, write: true}],
            ..Default::default() };
        let d = debug_config(&c, 4, 2).unwrap();
        assert_eq!(d.control, kvm_bindings::KVM_GUESTDBG_ENABLE | kvm_bindings::KVM_GUESTDBG_SINGLESTEP | kvm_bindings::KVM_GUESTDBG_USE_HW);
        assert_eq!(d.arch.dbg_bvr[0], 0x1000);
        assert_eq!(d.arch.dbg_wvr[0], 0x2000);
        assert_eq!(d.arch.dbg_wcr[0], c.watchpoints[0].arm64_control().unwrap());
        assert!(debug_config(&c, 0, 2).is_err());
        c.enabled = false;
        assert!(debug_config(&c, 4, 2).is_err());
        assert_eq!(debug_config(&Default::default(), 4, 2).unwrap().control, 0);
    }

    #[test]
    fn reg_ids() {
        assert_eq!(x_reg(0), 0x6030_0000_0010_0000);
        assert_eq!(x_reg(1), 0x6030_0000_0010_0002);
        assert_eq!(PC, 0x6030_0000_0010_0040);
        // MPIDR_EL1 = op0 3, op1 0, crn 0, crm 0, op2 5
        assert_eq!(sysreg(3, 0, 0, 0, 5), 0x6030_0000_0013_c005);
    }
}
