//! PCI(e): config space, MSI-X, INTx routing and the ECAM root complex.

mod config;
pub mod msi;
pub mod msix;
mod root;

use std::sync::{Arc, Mutex};

pub use config::{BarSpec, ConfigSpace, PciId, VendorIds, CFG_SIZE};
pub use msi::Msi;
pub use msix::{Msix, MsiSender};
pub use root::{Intx, IntxRouter, PciRoot};

use crate::IrqLine;

/// A PCI function on the root bus.
pub trait PciDevice: Send {
    fn config(&self) -> &ConfigSpace;
    fn config_mut(&mut self) -> &mut ConfigSpace;
    /// Config read; override for side-effecting windows (virtio pci_cfg).
    fn config_read(&mut self, off: usize, data: &mut [u8]) {
        self.config().read(off, data);
    }
    /// Called after a guest config write (enable bits, MSI-X control...).
    fn config_written(&mut self, _off: usize, _len: usize) {}
    fn bar_read(&mut self, bar: usize, off: u64, data: &mut [u8]);
    fn bar_write(&mut self, bar: usize, off: u64, data: &[u8]);
    /// Snapshot id (unique per VM).
    fn id(&self) -> String;
    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>>;
    fn restore(&mut self, data: &[u8]) -> vroom_snapshot::Result<()>;
    fn pause(&mut self) {}
    fn resume(&mut self) {}
}

pub type SharedPciDevice = Arc<Mutex<dyn PciDevice>>;

impl IrqLine for Intx {
    fn set_level(&self, level: bool) {
        self.set(level)
    }
}
