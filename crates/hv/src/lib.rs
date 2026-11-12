//! Hypervisor abstraction.
//!
//! The VMM talks to [`Hypervisor`], [`Vm`] and [`Vcpu`] only. Backends:
//! - `hvf`: Apple Hypervisor.framework (macOS, arm64), with the in-kernel GICv3.
//! - `kvm`: Linux KVM (x86_64 and aarch64).
//!
//! Backends stay thin. Policy (PSCI, device emulation, boot protocol) lives
//! above this crate so it is shared and testable without hardware.

pub mod aarch64;
mod x86_64;
// Private, binding-independent SMM mapping transaction; also tested on macOS.
#[path = "kvm/smm.rs"]
mod smm;
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
    /// Guest-physical address of one page the guest never uses (no RAM,
    /// no device): the backend may map code of its own there (HVF: the
    /// stage-1 TLB flush a register restore runs).
    pub helper_page: u64,
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
/// What the idle skip sees (see [`Vm::set_idle_skip`]).
#[derive(Debug, Clone, Default)]
pub struct IdleSkipState {
    /// vCPUs in WFI, with their timer's compare value if armed.
    pub waiting: Vec<(u32, Option<u64>)>,
    /// vCPUs in all.
    pub cpus: usize,
    /// Devices have work in flight (no jump).
    pub busy: bool,
    /// Times the VM-wide counter offset moved (jumps and rebases).
    pub moves: u64,
    /// Jumps alone.
    pub jumps: u64,
    /// A rebase waits for a vCPU to enter the guest.
    pub rebase_pending: bool,
}

pub trait Vm: Send + Sync {
    /// Restricted x86 SMM fixture: own a private 64 KiB image at the default
    /// SMBASE (0x30000), mapped in KVM address space 1 only. Call before any
    /// memory mapping or vCPU creation attempt. Common RAM/ROM mapped later
    /// is mirrored into both address spaces; topology freezes at vCPU creation.
    /// No host-buffer maps, SMRAM relocation, chipset or firmware board.
    /// The backend copies `initial`; no device-facing GuestMemory owns SMRAM.
    fn configure_x86_smm(&self, _initial: &[u8]) -> Result<X86SmmCapabilities> {
        Err(Error::Unsupported("restricted x86 SMM memory"))
    }

    /// Fixed one-CPU Q35 SMRAM prerequisite: nominal low RAM ends at 512 MiB,
    /// with private default-SMBASE 0x30000..0x50000 and 8 MiB TSEG immediately
    /// below that low-RAM end. Both are initially CPU-visible in AS0 and AS1;
    /// neither ever belongs to device-facing GuestMemory. Configure once on a
    /// fresh unmapped VM. No chipset/firmware profile or runtime reset implied.
    fn configure_x86_q35_smram(&self) -> Result<X86SmmCapabilities> {
        Err(Error::Unsupported("fixed Q35 SMRAM memory"))
    }

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

    /// Free `len` bytes of guest RAM at `gpa` while the VM runs (pages the
    /// guest handed over): the guest reads zeros there after. Multiples of
    /// the host page size. False if not now (writes logged, execute
    /// patches, watches, shared RAM): nothing changed then.
    fn release_memory(&self, mem: &GuestMemory, gpa: u64, len: u64) -> Result<bool> {
        mem.release(vroom_memory::GuestAddress(gpa), len).map_err(|e| Error::Invalid(format!("release: {e}")))
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

    /// Make the guest execute `word` at `gpa` (4-byte aligned RAM) instead
    /// of RAM's instruction, while its reads and writes there still see RAM
    /// unchanged; None executes RAM's again. Not while logging writes.
    /// vCPUs must be stopped.
    fn set_execute_patch(&self, _gpa: u64, _word: Option<[u8; 4]>) -> Result<()> {
        Err(Error::Unsupported("execute patches"))
    }

    /// Replace the memory watches: any number of RAM ranges whose guest
    /// reads or writes stop the vCPU before the access with a Watchpoint
    /// whose syndrome is the data abort's; run again, it passes. Not while
    /// logging writes, nor on pages with execute patches. vCPUs must be
    /// stopped.
    fn set_memory_watches(&self, watches: &[debug::MemoryWatch]) -> Result<()> {
        if watches.is_empty() {
            return Ok(());
        }
        Err(Error::Unsupported("memory watches"))
    }

    /// Memory watch faults so far: (stops, passes). A pass is an access to
    /// a watched page outside the watched ranges or kinds: the cost of
    /// watching that page.
    fn memory_watch_counts(&self) -> (u64, u64) {
        (0, 0)
    }

    /// Catch execute patches up with RAM written behind their back (device
    /// DMA). Returns the patched addresses whose RAM word changed since the
    /// last call: their patches are gone. Safe while vCPUs run.
    fn refresh_execute_patches(&self) -> Result<Vec<u64>> {
        Ok(Vec::new())
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
    /// Takes effect on each vCPU at its next state restore, or (HVF) the
    /// next time it enters the guest.
    fn set_clock(&self, value: u64);
    /// Nanoseconds in `ticks` of [`Vm::clock`].
    fn clock_ns(&self, ticks: u64) -> u64 {
        ticks
    }
    /// While every vCPU waits for an interrupt and `busy` (devices with
    /// work in flight) says no, move the guest counter to the earliest
    /// armed timer rather than wait for it. None: off.
    fn set_idle_skip(&self, _busy: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync>>) {}
    /// The idle skip right now (diagnostics); None while it is off.
    fn idle_skip_state(&self) -> Option<IdleSkipState> {
        None
    }

    /// Called once every vCPU exists (KVM initialises its vGIC here).
    fn finalize(&self) -> Result<()> {
        Ok(())
    }

    /// Save / restore the interrupt controller (VM must be paused).
    fn save_irqchip(&self) -> Result<IrqChipState>;
    fn restore_irqchip(&self, s: &IrqChipState) -> Result<()>;

    /// Every readable shared interrupt controller register (GIC
    /// distributor), as (register, value), loading `restore` first when
    /// given. VM paused. See `Vcpu::gic_cpu_registers`.
    fn gic_shared_registers(&self, _restore: Option<&[(u32, u64)]>) -> Result<Vec<(u32, u64)>> {
        Err(Error::Unsupported("per-register interrupt controller access"))
    }
}

/// `HVC #IDLE_HVC` waits like WFI. HVF handles a guest's WFI itself, so
/// a VMM that must see the guest idle (the idle skip) rewrites them to it.
pub const IDLE_HVC: u16 = 0x7669;

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
    /// Repeated string I/O: every element accesses the same port.
    PioInBatch { port: u16, size: usize, count: usize },
    PioOutBatch { port: u16, size: usize, data: Vec<u8> },
    /// An x86 I/O exit with KVM_RUN_X86_SMM set in that same successful RUN.
    /// Must never enter ordinary device/GuestMemory routing. Only an explicit
    /// SMM-aware board may handle it; existing VMM profiles reject this exit.
    X86SmmIo(X86IoExit),
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

/// Bounded x86 I/O, with the same scalar widths/completion rules as VcpuExit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum X86IoExit {
    MmioRead { addr: u64, len: usize },
    MmioWrite { addr: u64, len: usize, data: [u8; 8] },
    PioIn { port: u16, len: usize },
    PioOut { port: u16, len: usize, data: [u8; 4] },
}

/// Successful restricted SMM configuration, not a protected-firmware claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X86SmmCapabilities {
    /// VM-level KVM_CAP_X86_SMM query succeeded.
    pub vm_smm: bool,
    /// Numeric system-level KVM_CAP_MULTI_ADDRESS_SPACE result (at least 2).
    pub system_address_spaces: u32,
}

/// CPU visibility of the fixed Q35 TSEG; Locked cannot be downgraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X86TsegState { Open, Closed, Locked }

/// Applied backend state, separate from chipset register emulation. The MCH
/// may publish matching readbacks only after its mapping operation succeeds.
/// Neither lock can be cleared without recreating the complete VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X86SmramState {
    pub tseg: X86TsegState,
    pub default_smbase_locked: bool,
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
    /// 64-bit Linux boot protocol: RIP, RSI = boot_params, identity-mapped
    /// page tables at CR3. `gdt` names four entries: null, unused, 64-bit
    /// kernel code (selector 0x10), flat kernel data (selector 0x18).
    /// The caller supplies that GDT and maps the entry, parameters and GDT.
    /// Interrupts start disabled; this does not describe a saved CPU state.
    X86_64 { rip: u64, rsi: u64, cr3: u64, gdt: u64 },
    /// Firmware entry in the architectural x86 reset state.
    X86Reset,
    /// Enter the x86 firmware reset vector on a fresh, never-run vCPU only.
    /// The backend verifies its power-on state; this is not a runtime reset.
    /// The caller must supply the ROM mapping and board. No SMM, firmware
    /// security or Windows support is implied by this entry mechanism.
    X86ResetVector,
}

/// A virtual CPU. Not `Send`: lives and dies on its own thread.
pub trait Vcpu {
    /// Apply fixed-Q35 CPU visibility on the owning thread at the current
    /// successful PIO-out/MMIO-write exit. Consumes that grant even on error
    /// or an idempotent request. Pending reads and every other exit reject it.
    /// Partial mapping failure poisons execution/completion; never retry RUN.
    fn apply_x86_smram(&mut self, _target: X86SmramState) -> Result<X86SmramState> {
        Err(Error::Unsupported("fixed Q35 SMRAM transition"))
    }
    /// Inject SMI on the owning thread into an initialized, explicitly SMM-
    /// configured x86 vCPU. Pending I/O reads must be completed first.
    fn inject_smi(&mut self) -> Result<()> {
        Err(Error::Unsupported("x86 SMI injection"))
    }
    fn debug_write_register(&mut self, _register: debug::Register, _value: u64) -> Result<()> {
        Err(Error::Unsupported("register editing on this backend"))
    }
    /// Configure host debugging on the owning thread, while stopped.
    fn configure_debug(&mut self, _config: &debug::DebugConfig) -> Result<()> {
        Err(Error::Unsupported("host debugging on this backend"))
    }
    /// This CPU's interrupt controller registers that change at run time
    /// (GIC redistributor: groups, enables, pending, active, priorities,
    /// configuration), as (register, value). With `restore`, load those
    /// first. On the owning thread, while stopped. A cheaper alternative to
    /// `Vm::restore_irqchip` for snapshot fuzzing resets.
    fn gic_cpu_registers(&mut self, _restore: Option<&[(u32, u64)]>) -> Result<Vec<(u32, u64)>> {
        Err(Error::Unsupported("per-register interrupt controller access"))
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
