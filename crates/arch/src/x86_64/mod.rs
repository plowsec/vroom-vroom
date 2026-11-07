//! Direct Linux64 boot for the initial one-CPU, 512 MiB x86 board.
//!
//! This prepares RAM and entry registers, not a complete machine: the
//! caller still supplies the irqchip/devices and a fresh, stopped vCPU.
//! There is no firmware, ACPI, Secure Boot or snapshot implementation here.

pub mod layout;
pub mod loader;

pub use loader::{prepare_boot, BootConfig, BootInfo, BootPlan, BootRegion, PreparedBoot};
