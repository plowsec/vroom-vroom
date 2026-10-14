//! Apple Hypervisor.framework backend (macOS 15+, Apple silicon).
//!
//! Notes:
//! - HVF allows one VM per process. That fits our model: one VM, one
//!   sandboxed process.
//! - vCPUs are bound to the thread that created them.
//! - We use the in-kernel GICv3 (`hv_gic_*`), so distributor/redistributor
//!   MMIO never exits to us.

mod ffi;
mod vcpu;

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bulwark_memory::{GuestMemory, MmapRegion};

use crate::{
    Arch, Error, Hypervisor, IrqChipConfig, IrqChipInfo, IrqChipState, Result, Vcpu, VcpuKicker, Vm,
    VmParams,
};
use ffi::check;
pub use vcpu::HvfVcpu;
use vcpu::HvfKicker;

const BACKEND: &str = "hvf";
/// HVF supports exactly one VM per process.
static VM_EXISTS: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub struct Hvf {
    _priv: (),
}

impl Hvf {
    pub fn new() -> Result<Self> {
        Ok(Hvf { _priv: () })
    }
}

fn query(call: &'static str, f: unsafe extern "C" fn(*mut usize) -> i32) -> Result<u64> {
    let mut v = 0usize;
    // SAFETY: f writes one usize to a valid pointer.
    check(call, unsafe { f(&mut v) })?;
    Ok(v as u64)
}

impl Hypervisor for Hvf {
    fn name(&self) -> &'static str {
        BACKEND
    }

    fn arch(&self) -> Arch {
        Arch::Aarch64
    }

    fn irqchip_info(&self) -> Result<IrqChipInfo> {
        let (mut base, mut count) = (0u32, 0u32);
        // SAFETY: valid out pointers.
        check("hv_gic_get_spi_interrupt_range", unsafe {
            ffi::hv_gic_get_spi_interrupt_range(&mut base, &mut count)
        })?;
        Ok(IrqChipInfo {
            dist_size: query("hv_gic_get_distributor_size", ffi::hv_gic_get_distributor_size)?,
            redist_size_per_cpu: query(
                "hv_gic_get_redistributor_size",
                ffi::hv_gic_get_redistributor_size,
            )?,
            msi_size: query("hv_gic_get_msi_region_size", ffi::hv_gic_get_msi_region_size)?,
            spi_base: base,
            spi_count: count,
        })
    }

    fn create_vm(&self, params: &VmParams) -> Result<Arc<dyn Vm>> {
        if VM_EXISTS.swap(true, Ordering::SeqCst) {
            return Err(Error::Unsupported("HVF allows one VM per process"));
        }
        match HvfVm::create(params) {
            Ok(vm) => Ok(Arc::new(vm)),
            Err(e) => {
                VM_EXISTS.store(false, Ordering::SeqCst);
                Err(e)
            }
        }
    }
}

pub struct HvfVm {
    /// Keeps RAM alive while mapped.
    memory: Mutex<Vec<GuestMemory>>,
    roms: Mutex<Vec<Arc<MmapRegion>>>,
    kickers: Mutex<Vec<Arc<HvfKicker>>>,
    /// Host ticks minus guest counter. Shared by all vCPUs.
    vtimer_offset: Arc<AtomicU64>,
    spi_base: u32,
    spi_count: u32,
    msi: Option<crate::GicMsiConfig>,
}

impl std::fmt::Debug for HvfVm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HvfVm").field("spi_base", &self.spi_base).finish()
    }
}

impl HvfVm {
    fn create(params: &VmParams) -> Result<Self> {
        let IrqChipConfig::GicV3 { dist_base, redist_base, msi } = params.irqchip else {
            return Err(Error::Unsupported("HVF needs a GICv3 layout"));
        };
        // SAFETY: plain constructor; we own the returned object.
        let cfg = ffi::OsObject(unsafe { ffi::hv_vm_config_create() });
        let mut max_ipa = 0u32;
        // SAFETY: valid out pointer.
        check("hv_vm_config_get_max_ipa_size", unsafe {
            ffi::hv_vm_config_get_max_ipa_size(&mut max_ipa)
        })?;
        // 40 bits = 1 TiB of guest physical space; plenty, and widely supported.
        let ipa = max_ipa.min(40);
        // SAFETY: cfg is a valid config object.
        check("hv_vm_config_set_ipa_size", unsafe { ffi::hv_vm_config_set_ipa_size(cfg.0, ipa) })?;
        // SAFETY: cfg valid; one VM per process enforced by caller.
        check("hv_vm_create", unsafe { ffi::hv_vm_create(cfg.0) })?;

        let vm_guard = VmDestroyGuard;
        let hv = Hvf { _priv: () };
        let info = hv.irqchip_info()?;

        let align = |call, f| -> Result<u64> { query(call, f) };
        let da = align("dist align", ffi::hv_gic_get_distributor_base_alignment)?;
        let ra = align("redist align", ffi::hv_gic_get_redistributor_base_alignment)?;
        if dist_base % da != 0 || redist_base % ra != 0 {
            return Err(Error::Invalid(format!(
                "GIC bases misaligned (dist {dist_base:#x}/{da:#x}, redist {redist_base:#x}/{ra:#x})"
            )));
        }

        // SAFETY: constructor, owned.
        let gcfg = ffi::OsObject(unsafe { ffi::hv_gic_config_create() });
        // SAFETY: gcfg is valid for all calls below.
        unsafe {
            check("gic dist base", ffi::hv_gic_config_set_distributor_base(gcfg.0, dist_base))?;
            check("gic redist base", ffi::hv_gic_config_set_redistributor_base(gcfg.0, redist_base))?;
            if let Some(m) = msi {
                let ma = align("msi align", ffi::hv_gic_get_msi_region_base_alignment)?;
                if m.base % ma != 0 {
                    return Err(Error::Invalid(format!("MSI base {:#x} misaligned", m.base)));
                }
                check("gic msi base", ffi::hv_gic_config_set_msi_region_base(gcfg.0, m.base))?;
                check(
                    "gic msi range",
                    ffi::hv_gic_config_set_msi_interrupt_range(gcfg.0, m.intid_base, m.count),
                )?;
            }
            check("hv_gic_create", ffi::hv_gic_create(gcfg.0))?;
        }
        std::mem::forget(vm_guard);
        log::info!(
            "hvf: vm created, ipa {ipa} bits, SPIs {}..{}",
            info.spi_base,
            info.spi_base + info.spi_count
        );
        Ok(HvfVm {
            memory: Mutex::new(Vec::new()),
            roms: Mutex::new(Vec::new()),
            kickers: Mutex::new(Vec::new()),
            // Guest counter starts at 0 when the VM is created.
            // SAFETY: no preconditions.
            vtimer_offset: Arc::new(AtomicU64::new(unsafe { ffi::mach_absolute_time() })),
            spi_base: info.spi_base,
            spi_count: info.spi_count,
            msi,
        })
    }

    fn map(&self, ptr: *mut u8, gpa: u64, size: usize, flags: u64) -> Result<()> {
        // SAFETY: ptr..ptr+size is a live host mapping owned by an object
        // we keep in `memory`/`roms` for the VM's lifetime.
        check("hv_vm_map", unsafe { ffi::hv_vm_map(ptr as *mut c_void, gpa, size, flags) })
    }

    fn wake_all(&self) {
        for k in self.kickers.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            k.wake();
        }
    }
}

/// Destroys the VM if creation fails half way.
struct VmDestroyGuard;
impl Drop for VmDestroyGuard {
    fn drop(&mut self) {
        // SAFETY: VM was created in this process.
        unsafe { ffi::hv_vm_destroy() };
        VM_EXISTS.store(false, Ordering::SeqCst);
    }
}

impl Drop for HvfVm {
    fn drop(&mut self) {
        // SAFETY: we own the process-wide VM.
        unsafe { ffi::hv_vm_destroy() };
        VM_EXISTS.store(false, Ordering::SeqCst);
    }
}

impl Vm for HvfVm {
    fn map_memory(&self, mem: &GuestMemory) -> Result<()> {
        let flags = ffi::HV_MEMORY_READ | ffi::HV_MEMORY_WRITE | ffi::HV_MEMORY_EXEC;
        for r in mem.regions() {
            self.map(r.host_ptr(), r.start().raw(), r.len() as usize, flags)?;
        }
        self.memory.lock().unwrap_or_else(|e| e.into_inner()).push(mem.clone());
        Ok(())
    }

    fn map_readonly(&self, gpa: u64, region: Arc<MmapRegion>) -> Result<()> {
        self.map(region.as_ptr(), gpa, region.size(), ffi::HV_MEMORY_READ | ffi::HV_MEMORY_EXEC)?;
        self.roms.lock().unwrap_or_else(|e| e.into_inner()).push(region);
        Ok(())
    }

    fn create_vcpu(&self, index: u32) -> Result<Box<dyn Vcpu>> {
        let v = HvfVcpu::new(index, self.vtimer_offset.clone())?;
        self.kickers.lock().unwrap_or_else(|e| e.into_inner()).push(v.hvf_kicker());
        Ok(Box::new(v))
    }

    fn set_irq_line(&self, irq: u32, level: bool) -> Result<()> {
        if irq >= self.spi_count {
            return Err(Error::Invalid(format!("SPI {irq} out of range")));
        }
        // SAFETY: intid validated against the backend's SPI range.
        check("hv_gic_set_spi", unsafe { ffi::hv_gic_set_spi(self.spi_base + irq, level) })?;
        if level {
            self.wake_all();
        }
        Ok(())
    }

    fn signal_msi(&self, addr: u64, data: u32) -> Result<()> {
        let Some(m) = self.msi else {
            return Err(Error::Unsupported("no MSI frame configured"));
        };
        if data < m.intid_base || data >= m.intid_base + m.count {
            return Err(Error::Invalid(format!("MSI intid {data} out of range")));
        }
        // SAFETY: plain call; arguments validated.
        check("hv_gic_send_msi", unsafe { ffi::hv_gic_send_msi(addr, data) })?;
        self.wake_all();
        Ok(())
    }

    fn clock(&self) -> u64 {
        // SAFETY: no preconditions.
        unsafe { ffi::mach_absolute_time() }.wrapping_sub(self.vtimer_offset.load(Ordering::SeqCst))
    }

    fn set_clock(&self, value: u64) {
        // SAFETY: no preconditions.
        let now = unsafe { ffi::mach_absolute_time() };
        self.vtimer_offset.store(now.wrapping_sub(value), Ordering::SeqCst);
    }

    fn save_irqchip(&self) -> Result<IrqChipState> {
        // SAFETY: constructor; owned.
        let st = ffi::OsObject(unsafe { ffi::hv_gic_state_create() });
        if st.0.is_null() {
            return Err(Error::Backend { call: "hv_gic_state_create", code: -1 });
        }
        let mut sz = 0usize;
        // SAFETY: st valid, out pointer valid.
        check("hv_gic_state_get_size", unsafe { ffi::hv_gic_state_get_size(st.0, &mut sz) })?;
        let mut blob = vec![0u8; sz];
        // SAFETY: blob has exactly the size HVF asked for.
        check("hv_gic_state_get_data", unsafe {
            ffi::hv_gic_state_get_data(st.0, blob.as_mut_ptr() as *mut c_void)
        })?;
        Ok(IrqChipState { backend: BACKEND.into(), blob })
    }

    fn restore_irqchip(&self, s: &IrqChipState) -> Result<()> {
        if s.backend != BACKEND {
            return Err(Error::Invalid(format!("irqchip state from backend {}", s.backend)));
        }
        // SAFETY: HVF validates the blob; pointer/len describe our buffer.
        check("hv_gic_set_state", unsafe {
            ffi::hv_gic_set_state(s.blob.as_ptr() as *const c_void, s.blob.len())
        })
    }
}
