//! Linux KVM backend.
//!
//! - aarch64: vGICv3 in kernel, PSCI in kernel, same board as HVF.
//! - x86_64: in-kernel LAPIC/IOAPIC/PIT (see `x86_64.rs`).
//!
//! vCPUs are kicked out of KVM_RUN with a thread-directed signal; the
//! handler sets `immediate_exit` so a kick racing with KVM_RUN entry is
//! never lost.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
mod gic;
#[cfg(target_arch = "x86_64")]
mod x86_64;

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};

use vroom_memory::{GuestMemory, MmapRegion};
use kvm_bindings::{kvm_run, kvm_userspace_memory_region, KVM_MEM_READONLY};
use kvm_ioctls::{Kvm as KvmSys, VmFd};

use crate::{Arch, Error, Hypervisor, IrqChipInfo, Result, Vm, VmParams, VcpuKicker};

pub(crate) fn kerr(call: &'static str) -> impl Fn(kvm_ioctls::Error) -> Error {
    move |e| Error::Backend { call, code: e.errno() as i64 }
}

#[derive(Debug)]
pub struct Kvm {
    kvm: KvmSys,
}

impl Kvm {
    pub fn new() -> Result<Self> {
        let kvm = KvmSys::new().map_err(kerr("open /dev/kvm"))?;
        Ok(Kvm { kvm })
    }
}

impl Hypervisor for Kvm {
    fn name(&self) -> &'static str {
        "kvm"
    }

    fn arch(&self) -> Arch {
        Arch::host()
    }

    fn irqchip_info(&self) -> Result<IrqChipInfo> {
        #[cfg(target_arch = "aarch64")]
        {
            Ok(aarch64::irqchip_info())
        }
        #[cfg(target_arch = "x86_64")]
        {
            Ok(x86_64::irqchip_info())
        }
    }

    fn create_vm(&self, params: &VmParams) -> Result<Arc<dyn Vm>> {
        #[cfg(target_arch = "aarch64")]
        let (fd, chip) = aarch64::create_vm(&self.kvm, params)?;
        #[cfg(target_arch = "x86_64")]
        let (fd, chip) = x86_64::create_vm(&self.kvm, params)?;
        Ok(Arc::new(KvmVm {
            fd: Arc::new(fd),
            chip,
            slots: Arc::new(Mutex::new(Slots::default())),
            vcpus: params.vcpus,
            finalized: AtomicBool::new(false),
        }))
    }
}

#[derive(Default)]
struct Slots {
    next: u32,
    memory: Vec<GuestMemory>,
    roms: Vec<Arc<MmapRegion>>,
    /// Host buffer mappings (GPU): gpa -> slot, and slots to reuse.
    host: std::collections::HashMap<u64, u32>,
    free: Vec<u32>,
    #[cfg(target_arch = "x86_64")]
    smm_setup: crate::smm::Setup,
    #[cfg(target_arch = "x86_64")]
    smm: Option<crate::smm::Memory>,
}

/// Arch-specific irqchip handle (vGIC device fd on ARM).
pub(crate) enum Chip {
    #[cfg(target_arch = "aarch64")]
    Gic(kvm_ioctls::DeviceFd),
    #[cfg(target_arch = "x86_64")]
    Ioapic,
}

pub struct KvmVm {
    fd: Arc<VmFd>,
    chip: Chip,
    // SMM vCPUs retain this Arc: a VcpuFd keeps the kernel VM alive even
    // after KvmVm drops. The fd fields must drop before these backing owners.
    slots: Arc<Mutex<Slots>>,
    vcpus: u32,
    finalized: AtomicBool,
}

impl std::fmt::Debug for KvmVm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvmVm").field("vcpus", &self.vcpus).finish()
    }
}

impl KvmVm {
    #[cfg(target_arch = "x86_64")]
    fn smm_capabilities(&self) -> Result<(crate::X86SmmCapabilities, usize)> {
        use kvm_ioctls::Cap;
        let kvm = KvmSys::new().map_err(kerr("open SMM capabilities"))?;
        if !kvm.check_extension(Cap::CheckExtensionVm) || !self.fd.check_extension(Cap::X86Smm) {
            return Err(Error::Unsupported("VM-level KVM_CAP_X86_SMM"));
        }
        // VmFd's pinned API exposes bool only, so query the numeric count
        // with the existing raw system extension wrapper (not a new ioctl).
        let count = kvm.check_extension_raw(kvm_bindings::KVM_CAP_MULTI_ADDRESS_SPACE.into());
        if count < 2 { return Err(Error::Unsupported("KVM_CAP_MULTI_ADDRESS_SPACE must be at least 2")); }
        Ok((crate::X86SmmCapabilities { vm_smm: true, system_address_spaces: count as u32 }, kvm.get_nr_memslots()))
    }

    fn add_slot(&self, s: &mut Slots, gpa: u64, host: *mut u8, size: u64, flags: u32) -> Result<u32> {
        #[cfg(target_arch = "x86_64")]
        x86_64::check_memory_range(gpa, size)?;
        let slot = s.free.pop().unwrap_or(s.next);
        let region = kvm_userspace_memory_region { slot, flags, guest_phys_addr: gpa, memory_size: size, userspace_addr: host as u64 };
        // SAFETY: host..host+size is a live mapping: RAM/ROMs are kept in
        // `slots` for the VM's lifetime, host buffers until unmap_host.
        unsafe { self.fd.set_user_memory_region(region) }.map_err(kerr("KVM_SET_USER_MEMORY_REGION"))?;
        if slot == s.next {
            s.next += 1;
        }
        Ok(slot)
    }
}

#[cfg(target_arch = "x86_64")]
fn install_smm_slot(fd: &VmFd, mapping: crate::smm::Mapping) -> Result<()> {
    let region = kvm_userspace_memory_region { slot: mapping.slot,
        flags: if mapping.readonly { KVM_MEM_READONLY } else { 0 },
        guest_phys_addr: mapping.gpa, memory_size: mapping.size, userspace_addr: mapping.host };
    // SAFETY: the SMM transaction retains the full backing owner BEFORE this
    // call, including failed/partial transactions; all live vCPUs retain it.
    unsafe { fd.set_user_memory_region(region) }.map_err(kerr("KVM_SET_USER_MEMORY_REGION for SMM"))
}

#[cfg(target_arch = "x86_64")]
impl Slots {
    fn smm_memory(&mut self) -> Result<&mut crate::smm::Memory> {
        self.smm.as_mut().ok_or_else(|| Error::Invalid("failed SMM configuration; recreate the VM".into()))
    }
}

impl Vm for KvmVm {
    #[cfg(target_arch = "x86_64")]
    fn configure_x86_smm(&self, initial: &[u8]) -> Result<crate::X86SmmCapabilities> {
        let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        s.smm_setup.begin(self.finalized.load(Ordering::SeqCst))?;
        let (capabilities, slots) = self.smm_capabilities()?;
        s.smm = Some(crate::smm::Memory::new(initial, slots)?);
        s.smm_memory()?.install_private(|mapping| install_smm_slot(&self.fd, mapping))?;
        Ok(capabilities)
    }

    #[cfg(target_arch = "x86_64")]
    fn configure_x86_q35_smram(&self) -> Result<crate::X86SmmCapabilities> {
        let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        s.smm_setup.begin(self.finalized.load(Ordering::SeqCst))?;
        if self.vcpus != 1 { return Err(Error::Unsupported("fixed Q35 SMRAM requires exactly one vCPU")); }
        let (capabilities, slots) = self.smm_capabilities()?;
        s.smm = Some(crate::smm::Memory::new_q35(slots)?);
        s.smm_memory()?.install_private(|mapping| install_smm_slot(&self.fd, mapping))?;
        Ok(capabilities)
    }

    fn map_memory(&self, mem: &GuestMemory) -> Result<()> {
        let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(target_arch = "x86_64")]
        { s.smm_setup.mapping_attempted = true; }
        #[cfg(target_arch = "x86_64")]
        if s.smm_setup.requested {
            return s.smm_memory()?.map_memory(mem, |mapping| install_smm_slot(&self.fd, mapping));
        }
        for r in mem.regions() {
            self.add_slot(&mut s, r.start().raw(), r.host_ptr(), r.len(), 0)?;
        }
        s.memory.push(mem.clone());
        Ok(())
    }

    fn map_readonly(&self, gpa: u64, region: Arc<MmapRegion>) -> Result<()> {
        let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(target_arch = "x86_64")]
        { s.smm_setup.mapping_attempted = true; }
        #[cfg(target_arch = "x86_64")]
        if s.smm_setup.requested {
            return s.smm_memory()?.map_readonly(gpa, region, |mapping| install_smm_slot(&self.fd, mapping));
        }
        self.add_slot(&mut s, gpa, region.as_ptr(), region.size() as u64, KVM_MEM_READONLY)?;
        s.roms.push(region);
        Ok(())
    }

    unsafe fn map_host(&self, gpa: u64, ptr: *mut u8, size: u64) -> Result<()> {
        let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(target_arch = "x86_64")]
        { s.smm_setup.mapping_attempted = true; }
        #[cfg(target_arch = "x86_64")]
        if s.smm_setup.requested { return Err(Error::Unsupported("host-buffer maps in restricted SMM mode")); }
        let slot = self.add_slot(&mut s, gpa, ptr, size, 0)?;
        s.host.insert(gpa, slot);
        Ok(())
    }

    fn unmap_host(&self, gpa: u64, _size: u64) -> Result<()> {
        let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(target_arch = "x86_64")]
        if s.smm_setup.requested { return Err(Error::Unsupported("host-buffer maps in restricted SMM mode")); }
        let slot = s.host.remove(&gpa).ok_or_else(|| Error::Invalid(format!("no host mapping at {gpa:#x}")))?;
        let region = kvm_userspace_memory_region { slot, flags: 0, guest_phys_addr: gpa, memory_size: 0, userspace_addr: 0 };
        // SAFETY: size 0 deletes the slot.
        unsafe { self.fd.set_user_memory_region(region) }.map_err(kerr("KVM_SET_USER_MEMORY_REGION"))?;
        s.free.push(slot);
        Ok(())
    }

    fn create_vcpu(&self, index: u32) -> Result<Box<dyn crate::Vcpu>> {
        install_kick_handler();
        #[cfg(target_arch = "aarch64")]
        {
            Ok(Box::new(aarch64::KvmVcpu::new(&self.fd, index)?))
        }
        #[cfg(target_arch = "x86_64")]
        {
            // Configuration, all mapping ioctls and this attempt share one
            // lock. Seal before validation/CREATE_VCPU, even if either fails.
            let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            s.smm_setup.vcpu_attempted = true;
            let owners = if s.smm_setup.requested {
                s.smm_memory()?.seal()?;
                Some(self.slots.clone())
            } else { None };
            Ok(Box::new(x86_64::KvmVcpu::new(&self.fd, index, self.vcpus, owners)?))
        }
    }

    fn finalize(&self) -> Result<()> {
        // Serialize the fresh-SMM check with finalize as well as map/create.
        // ARM's GIC finalization path is unchanged.
        #[cfg(target_arch = "x86_64")]
        let _topology = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if self.finalized.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        match &self.chip {
            #[cfg(target_arch = "aarch64")]
            Chip::Gic(dev) => aarch64::init_gic(dev),
            #[cfg(target_arch = "x86_64")]
            Chip::Ioapic => Ok(()),
        }
    }

    fn set_irq_line(&self, irq: u32, level: bool) -> Result<()> {
        #[cfg(target_arch = "aarch64")]
        let line = (1 << 24) | (32 + irq); // SPI
        #[cfg(target_arch = "x86_64")]
        let line = irq;
        self.fd.set_irq_line(line, level).map_err(kerr("KVM_IRQ_LINE"))
    }

    fn signal_msi(&self, addr: u64, data: u32) -> Result<()> {
        #[cfg(target_arch = "aarch64")]
        {
            // No in-kernel v2m frame: the "message" is just an SPI number.
            let _ = addr;
            if data < 32 {
                return Err(Error::Invalid(format!("MSI intid {data}")));
            }
            self.set_irq_line(data - 32, true)?;
            self.set_irq_line(data - 32, false)
        }
        #[cfg(target_arch = "x86_64")]
        {
            let msi = kvm_bindings::kvm_msi { address_lo: addr as u32, address_hi: (addr >> 32) as u32, data, ..Default::default() };
            self.fd.signal_msi(msi).map(|_| ()).map_err(kerr("KVM_SIGNAL_MSI"))
        }
    }

    fn clock(&self) -> u64 {
        0
    }

    fn set_clock(&self, _value: u64) {}

    fn save_irqchip(&self) -> Result<crate::IrqChipState> {
        match &self.chip {
            #[cfg(target_arch = "aarch64")]
            Chip::Gic(dev) => {
                Ok(crate::IrqChipState { backend: "kvm-vgicv3".into(), blob: gic::save(dev, self.vcpus, aarch64::NR_IRQS)? })
            }
            #[cfg(target_arch = "x86_64")]
            Chip::Ioapic => Err(Error::Unsupported("x86 irqchip save")),
        }
    }

    fn restore_irqchip(&self, s: &crate::IrqChipState) -> Result<()> {
        #[cfg(target_arch = "x86_64")]
        let _ = s;
        match &self.chip {
            #[cfg(target_arch = "aarch64")]
            Chip::Gic(dev) => {
                if s.backend != "kvm-vgicv3" {
                    return Err(Error::Invalid(format!("irqchip state from {}", s.backend)));
                }
                gic::restore(dev, &s.blob)
            }
            #[cfg(target_arch = "x86_64")]
            Chip::Ioapic => Err(Error::Unsupported("x86 irqchip restore")),
        }
    }
}

// ---- kicking ----

thread_local! {
    static KVM_RUN: Cell<*mut kvm_run> = const { Cell::new(std::ptr::null_mut()) };
}

fn kick_signal() -> libc::c_int {
    libc::SIGRTMIN()
}

extern "C" fn on_kick(_sig: libc::c_int) {
    let p = KVM_RUN.with(|c| c.get());
    if !p.is_null() {
        // SAFETY: p is this thread's kvm_run mapping, set while its vCPU lives.
        unsafe { (*p).immediate_exit = 1 };
    }
}

fn install_kick_handler() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: plain sigaction with a handler that only touches
        // async-signal-safe thread-local memory.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_kick as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(kick_signal(), &sa, std::ptr::null_mut());
        }
    });
}

/// Register this thread's kvm_run for the kick handler.
pub(crate) fn set_thread_run(p: *mut kvm_run) {
    KVM_RUN.with(|c| c.set(p));
}

/// A different live vCPU may currently own this thread's signal binding.
pub(crate) fn clear_thread_run(p: *mut kvm_run) {
    KVM_RUN.with(|c| {
        if c.get() == p {
            c.set(std::ptr::null_mut());
        }
    });
}

#[cfg(test)]
mod signal_binding_tests {
    use super::*;

    #[test]
    fn clearing_one_vcpu_keeps_another_active_binding() {
        let mut first = kvm_run::default();
        let mut second = kvm_run::default();
        let first = &mut first as *mut _;
        let second = &mut second as *mut _;
        set_thread_run(first);
        set_thread_run(second);
        clear_thread_run(first);
        assert_eq!(KVM_RUN.with(|c| c.get()), second);
        set_thread_run(first); // the next RUN rebinds its own mapping
        clear_thread_run(second);
        assert_eq!(KVM_RUN.with(|c| c.get()), first);
        clear_thread_run(first);
        assert!(KVM_RUN.with(|c| c.get()).is_null());
    }
}

#[derive(Debug)]
pub(crate) struct KvmKicker {
    /// Kernel thread id of the vCPU thread (we signal it with tgkill on
    /// our own pid, which is all the seccomp filter lets through).
    tid: libc::pid_t,
    pub kicked: AtomicBool,
}

impl KvmKicker {
    pub fn current() -> Arc<Self> {
        // SAFETY: gettid has no preconditions.
        Arc::new(KvmKicker { tid: unsafe { libc::gettid() }, kicked: AtomicBool::new(false) })
    }

    pub fn take(&self) -> bool {
        self.kicked.swap(false, Ordering::AcqRel)
    }
}

impl VcpuKicker for KvmKicker {
    fn kick(&self) {
        self.kicked.store(true, Ordering::Release);
        // SAFETY: plain syscall; the target is a thread of this process.
        unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), self.tid, kick_signal()) };
    }

    fn wake(&self) {
        // KVM wakes vCPUs itself when interrupts arrive.
    }
}
