//! aarch64 "virt"-style board.
//!
//! Addresses match QEMU's `virt` machine where it matters, so unmodified
//! EDK2 `ArmVirtQemu` firmware finds its devices.

pub mod fdt;
pub mod layout;
pub mod loader;

pub use fdt::{create_fdt, FdtConfig, FdtDevice};
pub use loader::{load_initrd, load_kernel, place_boot_blobs, BootPlan, KernelInfo};
