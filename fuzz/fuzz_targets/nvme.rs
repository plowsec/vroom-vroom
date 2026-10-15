//! NVMe with a hostile driver: admin queues at known places, then guest
//! RAM (commands, PRP lists) and a register/doorbell stream from the input.
#![no_main]
use std::sync::Arc;

use vroom_block::raw::RawImage;
use vroom_block::DiskImage;
use vroom_devices::nvme::{Namespace, Nvme};
use vroom_devices::pci::{IntxRouter, MsiSender, PciDevice};
use vroom_devices::NoIrq;
use vroom_memory::{GuestAddress, GuestMemory};
use libfuzzer_sys::fuzz_target;

struct NoMsi;
impl MsiSender for NoMsi {
    fn send(&self, _a: u64, _d: u32) {}
}

const RAM: u64 = 0x4000_0000;
const RAM_SIZE: u64 = 256 << 10;

fn w(n: &mut Nvme, off: u64, v: u64, len: usize) {
    n.bar_write(0, off, &v.to_le_bytes()[..len]);
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let k = (u16::from_le_bytes([data[0], data[1]]) as usize).min(data.len() - 2).min(RAM_SIZE as usize);
    let (ram, ops) = data[2..].split_at(k);
    let mem = GuestMemory::new(&[(GuestAddress(RAM), RAM_SIZE)]).unwrap();
    mem.write(GuestAddress(RAM), ram).unwrap();
    let f = tempfile::tempfile().unwrap();
    f.set_len(1 << 20).unwrap();
    let disk: Arc<dyn DiskImage> = Arc::new(RawImage::from_file(f, false).unwrap());
    let r = IntxRouter::new(vec![Arc::new(NoIrq); 4]);
    let ns = vec![Namespace { disk, read_only: false, block_size: 512 }];
    let mut n = Nvme::new(3, ns, mem, r.pin(3, 1), Arc::new(NoMsi));
    w(&mut n, 0x24, (15 << 16) | 15, 4);
    w(&mut n, 0x28, RAM, 8);
    w(&mut n, 0x30, RAM + 0x1000, 8);
    w(&mut n, 0x14, 1, 4);
    for op in ops.chunks_exact(11) {
        let off = u16::from_le_bytes([op[1], op[2]]) as u64 % 0x2000;
        let v = u64::from_le_bytes(op[3..11].try_into().unwrap());
        match op[0] % 4 {
            0 => w(&mut n, off & !3, v, 4),
            1 => w(&mut n, off & !7, v, 8),
            2 => {
                let mut b = [0u8; 4];
                n.bar_read(0, off & !3, &mut b);
            }
            // Doorbell: SQ tail / CQ head of queue op[1] % 9.
            _ => w(&mut n, 0x1000 + 4 * (op[1] as u64 % 18), v & 0xffff, 4),
        }
    }
    let _ = n.save();
});
