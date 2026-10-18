//! Hypervisor abstraction.
//!
//! The VMM talks to [`Hypervisor`], [`Vm`] and [`Vcpu`] only. Backends:
//! - `hvf`: Apple Hypervisor.framework (macOS, arm64), with the in-kernel GICv3.
//! - `kvm`: Linux KVM (x86_64 and aarch64).
//!
//! Backends stay thin. Policy (PSCI, device emulation, boot protocol) lives
//! above this crate so it is shared and testable without hardware.

pub mod aarch64;
mod error;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod hvf;
#[cfg(target_os = "linux")]
pub mod kvm;
mod state;
pub mod debug;

use std::sync::Arc;

use vroom_memory::GuestMemory;

pub use error::{Error, Result};
pub use state::{IrqChipState, VcpuState};

/// Guest CPU architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Arch {
    Aarch64,
    X86_64,
}

impl Arch {
    pub const fn host() -> Arch {
        if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            Arch::X86_64
        }
    }
}

/// Where the interrupt controller lives in guest physical space.
/// Picked by the arch layer, honoured by the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqChipConfig {
    /// ARM GICv3.
    GicV3 {
        dist_base: u64,
        redist_base: u64,
        /// Optional MSI frame (GICv2m-style) for PCI MSI.
        msi: Option<GicMsiConfig>,
    },
    /// x86 in-kernel IOAPIC + LAPIC.
    Ioapic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GicMsiConfig {
    pub base: u64,
    /// First SPI intid reserved for MSIs.
    pub intid_base: u32,
    pub count: u32,
}

/// Facts about the interrupt controller the arch layer needs for the FDT/ACPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqChipInfo {
    pub dist_size: u64,
    pub redist_size_per_cpu: u64,
    pub msi_size: u64,
    /// SPI intids the backend lets us drive: [base, base+count).
    pub spi_base: u32,
    pub spi_count: u32,
    /// The backend emulates the MSI frame itself (HVF). If false, the VMM
    /// provides an MMIO frame that turns writes into SPIs (KVM).
    pub msi_frame_in_kernel: bool,
}

#[derive(Debug, Clone)]
pub struct VmParams {
    pub vcpus: u32,
    pub irqchip: IrqChipConfig,
    /// Run the guest at EL2 so it can host its own VMs (ARM nested virt).
    pub nested: bool,
    /// Map guest memory in 4 KiB steps, not the host's 16 KiB (macOS on
    /// Apple Silicon): a 4 KiB-page guest kernel can then put GPU buffers
    /// anywhere it likes in the shared window (Vulkan). Ignored where the
    /// host already maps 4 KiB.
    pub small_pages: bool,
}

/// Entry point to a hypervisor backend.
pub trait Hypervisor: Send + Sync {
    fn name(&self) -> &'static str;
    fn arch(&self) -> Arch;
    /// Static irqchip facts, available before the VM exists (needed for layout).
    fn irqchip_info(&self) -> Result<IrqChipInfo>;
    fn create_vm(&self, params: &VmParams) -> Result<Arc<dyn Vm>>;
}

/// A VM: address space + interrupt controller. Shared by all vCPU threads.
pub trait Vm: Send + Sync {
    /// Map all guest RAM. The VM keeps a clone so the backing outlives the mapping.
    fn map_memory(&self, mem: &GuestMemory) -> Result<()>;

    /// Map a read-only region (firmware flash). Guest writes trap as MMIO.
    fn map_readonly(&self, gpa: u64, region: Arc<vroom_memory::MmapRegion>) -> Result<()>;

    /// Map memory a native library owns (GPU buffers) at `gpa`, read/write.
    ///
    /// # Safety
    /// `ptr..ptr+size` must stay mapped until `unmap_host(gpa, size)`.
    unsafe fn map_host(&self, _gpa: u64, _ptr: *mut u8, _size: u64) -> Result<()> {
        Err(Error::Unsupported("mapping host buffers into the guest"))
    }

    fn unmap_host(&self, _gpa: u64, _size: u64) -> Result<()> {
        Err(Error::Unsupported("mapping host buffers into the guest"))
    }

    /// The step `map_host` works in: addresses and sizes are multiples.
    fn map_granule(&self) -> u64 {
        vroom_memory::page_size() as u64
    }

    /// Zero (and free) all of `mem`, already mapped with `map_memory`.
    /// vCPUs must be stopped. The default suits backends that follow host
    /// page table changes (KVM, via MMU notifiers).
    fn zero_memory(&self, mem: &GuestMemory) -> Result<()> {
        mem.discard_all().map_err(|e| Error::Invalid(format!("discard: {e}")))
    }

    /// Run `replace`, which swaps the host pages behind `mem` (remaps it),
    /// so the guest sees the new pages afterwards. vCPUs must be stopped.
    /// The default suits backends that follow host page table changes.
    fn replace_memory(&self, _mem: &GuestMemory, replace: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        replace()
    }

    /// Start logging which pages of `mem` the guest writes (snapshot
    /// resets copy back only those). vCPUs must be stopped.
    fn start_dirty_log(&self, _mem: &GuestMemory) -> Result<()> {
        Err(Error::Unsupported("logging guest writes to RAM"))
    }

    /// Guest pages (address, length) written since the log started or was
    /// last taken; logging goes on from here. vCPUs must be stopped.
    fn take_dirty_log(&self) -> Result<Vec<(u64, u64)>> {
        Err(Error::Unsupported("logging guest writes to RAM"))
    }

    /// Create the vCPU `index`. On HVF this MUST run on the thread that will
    /// drive the vCPU forever; the returned object is deliberately `!Send`.
    fn create_vcpu(&self, index: u32) -> Result<Box<dyn Vcpu>>;

    /// Drive a shared interrupt line (SPI on ARM, GSI on x86). `irq` is the
    /// 0-based line number; the backend adds the arch offset.
    fn set_irq_line(&self, irq: u32, level: bool) -> Result<()>;

    /// Deliver a message-signalled interrupt.
    fn signal_msi(&self, addr: u64, data: u32) -> Result<()>;

    /// Guest-visible counter (CNTVCT on ARM). One clock for the whole VM.
    fn clock(&self) -> u64;
    /// Make the guest counter continue from `value` (snapshot restore).
    /// Takes effect on each vCPU at its next state restore.
    fn set_clock(&self, value: u64);

    /// Called once every vCPU exists (KVM initialises its vGIC here).
    fn finalize(&self) -> Result<()> {
        Ok(())
    }

    /// Save / restore the interrupt controller (VM must be paused).
    fn save_irqchip(&self) -> Result<IrqChipState>;
    fn restore_irqchip(&self, s: &IrqChipState) -> Result<()>;
}

/// Why the vCPU stopped running guest code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VcpuExit {
    /// An architectural debug stop. PC still names the stopped instruction.
    Debug(debug::DebugStop),
    /// Guest read device memory. Call [`Vcpu::complete_mmio_read`] next.
    MmioRead { addr: u64, len: usize },
    MmioWrite { addr: u64, len: usize, data: [u8; 8] },
    /// x86 port I/O.
    PioIn { port: u16, len: usize },
    PioOut { port: u16, len: usize, data: [u8; 4] },
    /// SMCCC call (PSCI etc) the backend did not handle in-kernel.
    /// Call [`Vcpu::complete_hypercall`] with the result.
    Hypercall { func: u64, args: [u64; 3] },
    /// Guest asked to power off / reboot (in-kernel PSCI or x86 reset).
    SystemOff,
    SystemReset,
    /// Another thread asked us to stop via [`VcpuKicker::kick`].
    Kicked,
    /// Halted with no wakeup source; the VMM decides.
    Halted,
}

/// Handle another thread uses to bounce a vCPU out of guest mode.
pub trait VcpuKicker: Send + Sync {
    fn kick(&self);
    /// Wake the vCPU if it sleeps in WFI/HLT (an interrupt may be pending).
    fn wake(&self);
}

/// Initial register state for a boot. Arch-specific.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootRegs {
    /// PC, and X0..X3 (Linux arm64 boot protocol: X0 = FDT).
    Aarch64 { pc: u64, x: [u64; 4] },
    /// 64-bit Linux boot protocol: RIP, RSI = boot_params, page tables.
    X86_64 { rip: u64, rsi: u64, cr3: u64, gdt: u64 },
}

/// A virtual CPU. Not `Send`: lives and dies on its own thread.
pub trait Vcpu {
    fn debug_write_register(&mut self, _register: debug::Register, _value: u64) -> Result<()> {
        Err(Error::Unsupported("register editing on this backend"))
    }
    /// Configure host debugging on the owning thread, while stopped.
    fn configure_debug(&mut self, _config: &debug::DebugConfig) -> Result<()> {
        Err(Error::Unsupported("host debugging on this backend"))
    }
    fn index(&self) -> u32;
    fn run(&mut self) -> Result<VcpuExit>;
    fn complete_mmio_read(&mut self, data: &[u8]) -> Result<()>;
    fn complete_pio_in(&mut self, data: &[u8]) -> Result<()>;
    fn complete_hypercall(&mut self, ret: [u64; 4]) -> Result<()>;
    fn kicker(&self) -> Arc<dyn VcpuKicker>;
    /// PSCI / power state handled by the kernel (KVM): every vCPU thread
    /// just runs; secondaries stay parked in the kernel until CPU_ON.
    fn in_kernel_psci(&self) -> bool {
        false
    }
    fn set_boot_regs(&mut self, regs: &BootRegs) -> Result<()>;
    /// Arch identity (MPIDR on ARM, APIC id on x86).
    fn arch_id(&self) -> u64;
    fn save_state(&mut self) -> Result<VcpuState>;
    fn restore_state(&mut self, s: &VcpuState) -> Result<()>;
}

/// Pick the best backend for this host.
pub fn default_hypervisor() -> Result<Arc<dyn Hypervisor>> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Ok(Arc::new(hvf::Hvf::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Arc::new(kvm::Kvm::new()?))
    }
    #[cfg(not(any(all(target_os = "macos", target_arch = "aarch64"), target_os = "linux")))]
    {
        Err(Error::Unsupported("no hypervisor backend for this host"))
    }
}
