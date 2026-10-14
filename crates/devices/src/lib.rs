//! Device model core.
//!
//! Everything in here parses guest-controlled input, so the crate forbids
//! `unsafe`. A malformed access must never panic the VMM: return 0 on
//! reads, drop bad writes, log once.
#![forbid(unsafe_code)]

mod bus;
mod irq;
pub mod fw_cfg;
pub mod nvme;
pub mod pci;
pub mod pflash;
pub mod tpm;
pub mod pl011;
pub mod pl031;
pub mod usb;

pub use bus::{Bus, BusBuilder, BusDevice, BusRange, SharedDevice};
pub use irq::{IrqLine, NoIrq};

/// A device that sits on the bus and can be snapshotted.
pub trait Device: BusDevice + bulwark_snapshot::Snapshottable + Send {}
impl<T: BusDevice + bulwark_snapshot::Snapshottable + Send> Device for T {}

/// Little-endian register read helper: copy `val` into `data` (len 1..8).
pub fn put_le(data: &mut [u8], val: u64) {
    let b = val.to_le_bytes();
    let n = data.len().min(8);
    data[..n].copy_from_slice(&b[..n]);
}

/// Little-endian register write helper.
pub fn get_le(data: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    let n = data.len().min(8);
    b[..n].copy_from_slice(&data[..n]);
    u64::from_le_bytes(b)
}
