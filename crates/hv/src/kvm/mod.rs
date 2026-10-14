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
#[cfg(target_arch = "x86_64")]
mod x86_64;

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};

use bulwark_memory::{GuestMemory, MmapRegion};
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
            fd,
            chip,
            slots: Mutex::new(Slots::default()),
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
}

/// Arch-specific irqchip handle (vGIC device fd on ARM).
pub(crate) enum Chip {
    #[cfg(target_arch = "aarch64")]
    Gic(kvm_ioctls::DeviceFd),
    #[cfg(target_arch = "x86_64")]
    Ioapic,
}

pub struct KvmVm {
    fd: VmFd,
    chip: Chip,
    slots: Mutex<Slots>,
    vcpus: u32,
    finalized: AtomicBool,
}

impl std::fmt::Debug for KvmVm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvmVm").field("vcpus", &self.vcpus).finish()
    }
}

impl KvmVm {
    fn add_slot(&self, gpa: u64, host: *mut u8, size: u64, flags: u32) -> Result<()> {
        let mut s = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let region = kvm_userspace_memory_region {
            slot: s.next,
            flags,
            guest_phys_addr: gpa,
            memory_size: size,
            userspace_addr: host as u64,
        };
        // SAFETY: host..host+size is a live mapping kept alive in `slots`
        // for the VM's lifetime.
        unsafe { self.fd.set_user_memory_region(region) }.map_err(kerr("KVM_SET_USER_MEMORY_REGION"))?;
        s.next += 1;
        Ok(())
    }
}

impl Vm for KvmVm {
    fn map_memory(&self, mem: &GuestMemory) -> Result<()> {
        for r in mem.regions() {
            self.add_slot(r.start().raw(), r.host_ptr(), r.len(), 0)?;
        }
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).memory.push(mem.clone());
        Ok(())
    }

    fn map_readonly(&self, gpa: u64, region: Arc<MmapRegion>) -> Result<()> {
        self.add_slot(gpa, region.as_ptr(), region.size() as u64, KVM_MEM_READONLY)?;
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).roms.push(region);
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
            Ok(Box::new(x86_64::KvmVcpu::new(&self.fd, index, self.vcpus)?))
        }
    }

    fn finalize(&self) -> Result<()> {
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
        Err(Error::Unsupported("snapshots on KVM need irqchip save (not done yet)"))
    }

    fn restore_irqchip(&self, _s: &crate::IrqChipState) -> Result<()> {
        Err(Error::Unsupported("snapshots on KVM need irqchip restore (not done yet)"))
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

#[derive(Debug)]
pub(crate) struct KvmKicker {
    thread: libc::pthread_t,
    pub kicked: AtomicBool,
}

// SAFETY: pthread_t is an opaque thread id (a pointer on musl); we only
// pass it to pthread_kill, which is thread-safe.
unsafe impl Send for KvmKicker {}
// SAFETY: see above.
unsafe impl Sync for KvmKicker {}

impl KvmKicker {
    pub fn current() -> Arc<Self> {
        // SAFETY: pthread_self has no preconditions.
        Arc::new(KvmKicker { thread: unsafe { libc::pthread_self() }, kicked: AtomicBool::new(false) })
    }

    pub fn take(&self) -> bool {
        self.kicked.swap(false, Ordering::AcqRel)
    }
}

impl VcpuKicker for KvmKicker {
    fn kick(&self) {
        self.kicked.store(true, Ordering::Release);
        // SAFETY: the vCPU thread lives as long as the vCPU (and kicker users).
        unsafe { libc::pthread_kill(self.thread, kick_signal()) };
    }

    fn wake(&self) {
        // KVM wakes vCPUs itself when interrupts arrive.
    }
}
