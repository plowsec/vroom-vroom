//! Direct Linux64 boot and a separate fixed-Q35 firmware topology.
//!
//! This prepares RAM and entry registers, not a complete machine: the
//! caller still supplies the irqchip/devices and a fresh, stopped vCPU.
//! The firmware module supplies metadata, not a firmware machine or security
//! implementation. The direct Linux loader and its memory map remain separate.

pub mod firmware;
pub mod layout;
pub mod loader;

pub use firmware::Q35FirmwareLayout;
pub use loader::{prepare_boot, BootConfig, BootInfo, BootPlan, BootRegion, PreparedBoot};
