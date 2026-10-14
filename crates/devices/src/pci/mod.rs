//! PCI(e): config space, MSI-X, INTx routing and the ECAM root complex.

mod config;
pub mod msix;
mod root;

use std::sync::{Arc, Mutex};

pub use config::{BarSpec, ConfigSpace, PciId, CFG_SIZE};
pub use msix::{Msix, MsiSender};
pub use root::{Intx, IntxRouter, PciRoot};

use crate::IrqLine;

/// A PCI function on the root bus.
pub trait PciDevice: Send {
    fn config(&self) -> &ConfigSpace;
    fn config_mut(&mut self) -> &mut ConfigSpace;
    /// Called after a guest config write (enable bits, MSI-X control...).
    fn config_written(&mut self, _off: usize, _len: usize) {}
    fn bar_read(&mut self, bar: usize, off: u64, data: &mut [u8]);
    fn bar_write(&mut self, bar: usize, off: u64, data: &[u8]);
    /// Snapshot id (unique per VM).
    fn id(&self) -> String;
    fn save(&mut self) -> bulwark_snapshot::Result<Vec<u8>>;
    fn restore(&mut self, data: &[u8]) -> bulwark_snapshot::Result<()>;
    fn pause(&mut self) {}
    fn resume(&mut self) {}
}

pub type SharedPciDevice = Arc<Mutex<dyn PciDevice>>;

/// Interrupt delivery for a device: MSI-X if the driver enabled it,
/// otherwise the legacy INTx line.
#[derive(Clone)]
pub struct PciIrq {
    pub intx: Arc<Intx>,
}

impl std::fmt::Debug for PciIrq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PciIrq")
    }
}

impl IrqLine for Intx {
    fn set_level(&self, level: bool) {
        self.set(level)
    }
}
