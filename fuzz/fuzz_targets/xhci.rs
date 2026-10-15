//! xHCI with a hostile driver: fixed bring-up (event ring, DCBAA, command
//! ring at known places), then guest RAM and a register-write stream from
//! the input. Doorbells make the controller walk whatever is in RAM.
#![no_main]
use std::sync::Arc;

use vroom_devices::pci::{IntxRouter, MsiSender, PciDevice};
use vroom_devices::usb::hid::HidEvent;
use vroom_devices::usb::xhci::Xhci;
use vroom_devices::NoIrq;
use vroom_memory::{GuestAddress, GuestMemory};
use libfuzzer_sys::fuzz_target;

struct NoMsi;
impl MsiSender for NoMsi {
    fn send(&self, _a: u64, _d: u32) {}
}

const RAM: u64 = 0x4000_0000;
const RAM_SIZE: u64 = 256 << 10;

fn w(x: &mut Xhci, off: u64, v: u64, len: usize) {
    x.bar_write(0, off, &v.to_le_bytes()[..len]);
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let k = (u16::from_le_bytes([data[0], data[1]]) as usize).min(data.len() - 2).min(RAM_SIZE as usize);
    let (ram, ops) = data[2..].split_at(k);
    let mem = GuestMemory::new(&[(GuestAddress(RAM), RAM_SIZE)]).unwrap();
    mem.write(GuestAddress(RAM), ram).unwrap();
    let r = IntxRouter::new(vec![Arc::new(NoIrq); 4]);
    let mut x = Xhci::new(4, mem, r.pin(4, 1), Arc::new(NoMsi));
    // ERST at RAM+0x2000 (entry contents come from the input), DCBAA at
    // RAM, command ring at RAM+0x1000, running.
    w(&mut x, 0x1000 + 0x28, 1, 4);
    w(&mut x, 0x1000 + 0x30, RAM + 0x2000, 8);
    w(&mut x, 0x1000 + 0x38, RAM + 0x3000, 8);
    w(&mut x, 0x40 + 0x30, RAM, 8);
    w(&mut x, 0x40 + 0x18, RAM + 0x1000 | 1, 8);
    w(&mut x, 0x40, 5, 4);
    for op in ops.chunks_exact(11) {
        let off = u16::from_le_bytes([op[1], op[2]]) as u64 % 0x4000;
        let v = u64::from_le_bytes(op[3..11].try_into().unwrap());
        match op[0] % 6 {
            0 => w(&mut x, off & !3, v, 4),
            1 => w(&mut x, off & !7, v, 8),
            2 => {
                let mut b = [0u8; 4];
                x.bar_read(0, off & !3, &mut b);
            }
            // Doorbell: slot from the value, target endpoint.
            3 => w(&mut x, 0x2000 + 4 * (op[1] as u64 % 9), v & 0xff, 4),
            4 => x.input(HidEvent::Key { code: op[1] as u16, down: op[2] & 1 != 0 }),
            _ => x.input(HidEvent::Pointer { x: op[1] as u16 * 128, y: op[2] as u16 * 128 }),
        }
    }
    let _ = x.save();
});
