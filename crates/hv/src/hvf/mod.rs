//! Apple Hypervisor.framework backend (macOS 15+, Apple silicon).
//!
//! Notes:
//! - HVF allows one VM per process. That fits our model: one VM, one
//!   sandboxed process.
//! - vCPUs are bound to the thread that created them.
//! - We use the in-kernel GICv3 (`hv_gic_*`), so distributor/redistributor
//!   MMIO never exits to us.

mod dirty;
mod execute;
mod ffi;
mod vcpu;
mod watch;

/// GIC registers that change at run time are restored by value, except
/// set/clear pairs (the clear register sits 0x80 above the set one): only
/// the bits that differ are cleared or set.
fn restore_gic_registers(
    saved: &[(u32, u64)],
    pair: impl Fn(u32) -> bool,
    get: impl Fn(u32) -> Result<u64>,
    set: impl Fn(u32, u64) -> Result<()>,
) -> Result<()> {
    for &(reg, value) in saved {
        let now = get(reg)?;
        if now == value {
            continue;
        }
        if pair(reg) {
            if now & !value != 0 {
                set(reg + 0x80, now & !value)?;
            }
            if value & !now != 0 {
                set(reg, value & !now)?;
            }
        } else {
            set(reg, value)?;
        }
    }
    Ok(())
}

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vroom_memory::{GuestMemory, MmapRegion};

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
            msi_frame_in_kernel: true,
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
    /// Stage-2 page size: the host's, or 4 KiB with `small_pages`.
    granule: u64,
    /// Keeps RAM alive while mapped.
    memory: Mutex<Vec<GuestMemory>>,
    roms: Mutex<Vec<Arc<MmapRegion>>>,
    kickers: Mutex<Vec<Arc<HvfKicker>>>,
    /// Host ticks minus guest counter. Shared by all vCPUs.
    vtimer_offset: Arc<AtomicU64>,
    spi_base: u32,
    spi_count: u32,
    msi: Option<crate::GicMsiConfig>,
    el2: bool,
    /// Guest writes to RAM, when a snapshot fuzzer logs them.
    dirty: Arc<dirty::DirtyLog>,
    execute: Arc<execute::ExecutePatches>,
    watches: Arc<watch::MemoryWatches>,
}

impl std::fmt::Debug for HvfVm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HvfVm").field("spi_base", &self.spi_base).finish()
    }
}

impl HvfVm {
    /// The host address of the stage-2 page of RAM at `page`.
    fn ram_page(&self, page: u64) -> Option<*mut u8> {
        let memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        let ram = memory
            .iter()
            .flat_map(|m| m.regions())
            .find(|r| page >= r.start().raw() && page + self.granule <= r.start().raw() + r.len())
            .map(|r| r.host_ptr().wrapping_add((page - r.start().raw()) as usize));
        ram
    }

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
        if params.nested {
            let mut ok = false;
            // SAFETY: valid out pointer.
            check("hv_vm_config_get_el2_supported", unsafe { ffi::hv_vm_config_get_el2_supported(&mut ok) })?;
            if !ok {
                return Err(Error::Unsupported("nested virtualization needs an M3+ Mac with macOS 15+"));
            }
            // SAFETY: cfg is a valid config object.
            check("hv_vm_config_set_el2_enabled", unsafe { ffi::hv_vm_config_set_el2_enabled(cfg.0, true) })?;
        }
        let host_page = vroom_memory::page_size() as u64;
        let granule = match params.small_pages && host_page > 4096 {
            true => match set_ipa_granule(&cfg, 4096) {
                Ok(()) => 4096,
                Err(e) => {
                    // Still runs; Vulkan then needs a 16 KiB-page guest kernel.
                    log::warn!("hvf: no 4 KiB guest pages ({e}): Vulkan needs a 16 KiB-page guest kernel");
                    host_page
                }
            },
            false => host_page,
        };
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
            "hvf: vm created, ipa {ipa} bits, {} KiB pages, SPIs {}..{}",
            granule >> 10,
            info.spi_base,
            info.spi_base + info.spi_count
        );
        Ok(HvfVm {
            granule,
            memory: Mutex::new(Vec::new()),
            roms: Mutex::new(Vec::new()),
            kickers: Mutex::new(Vec::new()),
            // Guest counter starts at 0 when the VM is created.
            // SAFETY: no preconditions.
            vtimer_offset: Arc::new(AtomicU64::new(unsafe { ffi::mach_absolute_time() })),
            spi_base: info.spi_base,
            spi_count: info.spi_count,
            msi,
            el2: params.nested,
            dirty: Arc::new(dirty::DirtyLog::new(granule)),
            execute: Arc::new(execute::ExecutePatches::new(granule)),
            watches: Arc::new(watch::MemoryWatches::new(granule)),
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

/// Stage-2 pages of `bytes` (4096). macOS 15 has this only as a private
/// call taking the size; macOS 26 makes it public, taking `hv_ipa_granule_t`
/// instead, and drops the private one (26.0 exports only the public name),
/// so try the public call first and keep the private one for macOS 15.
fn set_ipa_granule(cfg: &ffi::OsObject, bytes: u32) -> Result<()> {
    type SetGranule = unsafe extern "C" fn(*mut c_void, u32) -> ffi::hv_return_t;
    // SAFETY: dlsym with a valid C string; RTLD_DEFAULT searches loaded images.
    let public = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"hv_vm_config_set_ipa_granule".as_ptr()) };
    if !public.is_null() {
        // hv_ipa_granule_t: HV_IPA_GRANULE_4KB, then HV_IPA_GRANULE_16KB.
        let granule = match bytes {
            4096 => 0,
            16384 => 1,
            _ => return Err(Error::Unsupported("guest pages: 4 KiB or 16 KiB")),
        };
        // SAFETY: the symbol takes (config, granule enum) and both are
        // passed by value; cfg is a live config object.
        return check("hv_vm_config_set_ipa_granule", unsafe {
            std::mem::transmute::<*mut c_void, SetGranule>(public)(cfg.0, granule)
        });
    }
    // SAFETY: dlsym with a valid C string; RTLD_DEFAULT searches loaded images.
    let private = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"_hv_vm_config_set_ipa_granule".as_ptr()) };
    if private.is_null() {
        return Err(Error::Unsupported("4 KiB guest pages: this macOS has no way to set them"));
    }
    // SAFETY: the symbol has this signature (config, granule in bytes);
    // cfg is a live config object.
    check("hv_vm_config_set_ipa_granule", unsafe {
        std::mem::transmute::<*mut c_void, SetGranule>(private)(cfg.0, bytes)
    })
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

    unsafe fn map_host(&self, gpa: u64, ptr: *mut u8, size: u64) -> Result<()> {
        // HVF wants host pointer, IPA and size aligned to its granule.
        let ps = self.granule;
        if !gpa.is_multiple_of(ps) || !(ptr as u64).is_multiple_of(ps) || !size.is_multiple_of(ps) || size == 0 {
            return Err(Error::Invalid(format!("unaligned host map {gpa:#x} {ptr:p} {size:#x}")));
        }
        self.map(ptr, gpa, size as usize, ffi::HV_MEMORY_READ | ffi::HV_MEMORY_WRITE)
    }

    fn map_granule(&self) -> u64 {
        self.granule
    }

    fn unmap_host(&self, gpa: u64, size: u64) -> Result<()> {
        // SAFETY: removes a mapping we made; the guest just faults there now.
        check("hv_vm_unmap", unsafe { ffi::hv_vm_unmap(gpa, size as usize) })
    }

    fn zero_memory(&self, mem: &GuestMemory) -> Result<()> {
        self.replace_memory(mem, &mut || mem.discard_all().map_err(|e| Error::Invalid(format!("discard: {e}"))))
    }

    fn replace_memory(&self, mem: &GuestMemory, replace: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        // HVF keeps the pages it mapped: replacing them under it (discard
        // and COW resets remap) would leave the guest on the old contents.
        // Unmap first, map the new pages after.
        let flags = ffi::HV_MEMORY_READ | ffi::HV_MEMORY_WRITE | ffi::HV_MEMORY_EXEC;
        for r in mem.regions() {
            // SAFETY: unmapping a range we mapped; vCPUs are stopped.
            check("hv_vm_unmap", unsafe { ffi::hv_vm_unmap(r.start().raw(), r.len() as usize) })?;
        }
        let replaced = replace();
        for r in mem.regions() {
            self.map(r.host_ptr(), r.start().raw(), r.len() as usize, flags)?;
        }
        self.execute.remap()?;
        self.watches.reprotect()?;
        replaced
    }

    fn start_dirty_log(&self, mem: &GuestMemory) -> Result<()> {
        if !self.execute.is_empty() {
            return Err(Error::Unsupported("logging writes to RAM with execute patches"));
        }
        if !self.watches.is_empty() {
            return Err(Error::Unsupported("logging writes to RAM with memory watches"));
        }
        self.dirty.start(mem)
    }

    fn set_execute_patch(&self, gpa: u64, word: Option<[u8; 4]>) -> Result<()> {
        if self.dirty.is_on() {
            return Err(Error::Unsupported("execute patches while logging writes to RAM"));
        }
        if word.is_some() && self.watches.covers(gpa) {
            return Err(Error::Unsupported("execute patches on a memory-watched page"));
        }
        let ram = self.ram_page(self.execute.page(gpa)).ok_or_else(|| Error::Invalid(format!("execute patch outside RAM at {gpa:#x}")))?;
        self.execute.set(gpa, ram, word)
    }

    fn set_memory_watches(&self, watches: &[crate::debug::MemoryWatch]) -> Result<()> {
        if self.dirty.is_on() && !watches.is_empty() {
            return Err(Error::Unsupported("memory watches while logging writes to RAM"));
        }
        if let Some(w) = watches.iter().find(|w| {
            let last = w.gpa.saturating_add(w.length.saturating_sub(1));
            (self.watches.page(w.gpa)..=last).step_by(self.granule as usize).any(|page| self.execute.covers(page))
        }) {
            return Err(Error::Invalid(format!(
                "memory watch at {:#x}: its page has execute patches (software tracepoints)",
                w.gpa
            )));
        }
        self.watches.set(watches, |page| self.ram_page(page))
    }

    fn memory_watch_counts(&self) -> (u64, u64) {
        (self.watches.stops.load(Ordering::Relaxed), self.watches.passes.load(Ordering::Relaxed))
    }

    fn refresh_execute_patches(&self) -> Result<Vec<u64>> {
        self.execute.refresh()
    }

    fn take_dirty_log(&self) -> Result<Vec<(u64, u64)>> {
        self.dirty.take()
    }

    fn map_readonly(&self, gpa: u64, region: Arc<MmapRegion>) -> Result<()> {
        self.map(region.as_ptr(), gpa, region.size(), ffi::HV_MEMORY_READ | ffi::HV_MEMORY_EXEC)?;
        self.roms.lock().unwrap_or_else(|e| e.into_inner()).push(region);
        Ok(())
    }

    fn create_vcpu(&self, index: u32) -> Result<Box<dyn Vcpu>> {
        let v = HvfVcpu::new(index, self.vtimer_offset.clone(), self.el2, self.dirty.clone(), self.execute.clone(), self.watches.clone())?;
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

    fn gic_shared_registers(&self, restore: Option<&[(u32, u64)]>) -> Result<Vec<(u32, u64)>> {
        // GICD_ISENABLER/ISPENDR/ISACTIVER: set registers with a clear
        // register 0x80 above, left out of the list (they read the same).
        let pair = |reg: u32| matches!(reg, 0x100..=0x17f | 0x200..=0x27f | 0x300..=0x37f);
        let clear = |reg: u32| matches!(reg, 0x180..=0x1ff | 0x280..=0x2ff | 0x380..=0x3ff);
        let get = |reg: u32| {
            let mut v = 0;
            // SAFETY: valid out pointer; the GIC exists once vCPUs do.
            check("hv_gic_get_distributor_reg", unsafe { ffi::hv_gic_get_distributor_reg(reg, &mut v) })?;
            Ok(v)
        };
        if let Some(saved) = restore {
            restore_gic_registers(saved, pair, get, |reg, v| {
                // SAFETY: plain register write; vCPUs are stopped.
                check("hv_gic_set_distributor_reg", unsafe { ffi::hv_gic_set_distributor_reg(reg, v) })
            })?;
            return Ok(saved.to_vec());
        }
        // 32-bit registers, the 64-bit IROUTERs, the ID registers. HVF
        // refuses offsets it doesn't implement and write-only ones.
        let offsets = (0..0x6100u32)
            .step_by(4)
            .chain((0x6100..0x8000).step_by(8))
            .chain((0xffd0..0x10000).step_by(4))
            .filter(|reg| !clear(*reg));
        let mut out = Vec::new();
        for reg in offsets {
            let mut v = 0;
            // SAFETY: valid out pointer.
            if unsafe { ffi::hv_gic_get_distributor_reg(reg, &mut v) } == 0 {
                out.push((reg, v));
            }
        }
        Ok(out)
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
