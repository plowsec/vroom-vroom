//! PCI config space and the virtio-pci transport (common config, notify,
//! ISR, device config, MSI-X table) from a hostile guest, with a
//! virtio-rng behind it. BARs are programmed first; then anything goes.
#![no_main]
use std::sync::{Arc, Mutex};

use bulwark_devices::pci::{IntxRouter, MsiSender, PciRoot, SharedPciDevice};
use bulwark_devices::{BusDevice, NoIrq};
use bulwark_memory::{GuestAddress, GuestMemory};
use bulwark_virtio::devices::rng::Rng;
use bulwark_virtio::pci::VirtioPci;
use libfuzzer_sys::fuzz_target;

struct NoMsi;
impl MsiSender for NoMsi {
    fn send(&self, _a: u64, _d: u32) {}
}

const RAM: u64 = 0x4000_0000;
const RAM_SIZE: u64 = 64 << 10;
const WIN: u64 = 0x1000_0000;
const DEV: u64 = 1 << 15; // slot 1

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let k = (u16::from_le_bytes([data[0], data[1]]) as usize).min(data.len() - 2).min(RAM_SIZE as usize);
    let (ram, ops) = data[2..].split_at(k);
    let mem = GuestMemory::new(&[(GuestAddress(RAM), RAM_SIZE)]).unwrap();
    mem.write(GuestAddress(RAM), ram).unwrap();
    let r = IntxRouter::new(vec![Arc::new(NoIrq); 4]);
    let dev: SharedPciDevice = Arc::new(Mutex::new(VirtioPci::new(1, Box::new(Rng::new()), mem, r.pin(1, 1), Arc::new(NoMsi))));
    let root = PciRoot::new(vec![(1, dev)]);
    let (mut ecam, mut win) = (root.ecam_port(), root.window_port(WIN));
    // BAR0 (64-bit) at WIN, memory decoding + bus mastering on.
    ecam.write(DEV + 0x10, &(WIN as u32).to_le_bytes());
    ecam.write(DEV + 0x14, &0u32.to_le_bytes());
    ecam.write(DEV + 0x04, &6u16.to_le_bytes());
    for op in ops.chunks_exact(8) {
        let off = u16::from_le_bytes([op[1], op[2]]) as u64;
        let len = [1usize, 2, 4][op[3] as usize % 3];
        let v = &op[4..4 + len];
        match op[0] % 4 {
            0 => ecam.write(DEV + (off & 0xfff), v),
            1 => {
                let mut b = [0u8; 4];
                ecam.read(DEV + (off & 0xfff), &mut b[..len]);
            }
            2 => win.write(off & 0x7fff, v),
            _ => {
                let mut b = [0u8; 4];
                win.read(off & 0x7fff, &mut b[..len]);
            }
        }
    }
});
