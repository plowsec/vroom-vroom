//! xHCI with a hostile driver: fixed bring-up (event ring, DCBAA, command
//! ring at known places), then guest RAM and a register-write stream from
//! the input. Doorbells make the controller walk whatever is in RAM.
//! A passthrough device (USB 2 port 3, USB 3 port 4) gets whatever
//! attach/detach and completion messages the input makes up, as from a
//! hostile helper.
#![no_main]
use std::sync::{mpsc, Arc};

use vroom_devices::pci::{IntxRouter, MsiSender, PciDevice};
use vroom_devices::usb::hid::HidEvent;
use vroom_devices::usb::xhci::Xhci;
use vroom_devices::NoIrq;
use vroom_memory::{GuestAddress, GuestMemory};
use vroom_usb::proto::{Done, Event, Request, Speed, Status};
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
    let (link, requests) = mpsc::channel::<Request>();
    let mut x = Xhci::new(4, mem, r.pin(4, 1), Arc::new(NoMsi)).with_passthrough(vec![link]);
    let mut sent: Vec<u32> = Vec::new();
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
        while let Ok(r) = requests.try_recv() {
            if let Request::Submit(s) = r {
                sent.push(s.id);
            }
        }
        match op[0] % 8 {
            0 => w(&mut x, off & !3, v, 4),
            1 => w(&mut x, off & !7, v, 8),
            2 => {
                let mut b = [0u8; 4];
                x.bar_read(0, off & !3, &mut b);
            }
            // Doorbell: slot from the value, target endpoint.
            3 => w(&mut x, 0x2000 + 4 * (op[1] as u64 % 9), v & 0xff, 4),
            4 => x.input(HidEvent::Key { code: op[1] as u16, down: op[2] & 1 != 0 }),
            5 => x.input(HidEvent::Pointer { x: op[1] as u16 * 128, y: op[2] as u16 * 128 }),
            6 => {
                let speed = [Speed::Low, Speed::Full, Speed::High, Speed::Super, Speed::SuperPlus][op[1] as usize % 5];
                x.usb_event(op[2] as usize % 2, if op[3] & 1 == 0 { Event::Attached { speed, vendor: 1, product: 2 } } else { Event::Detached });
            }
            _ => {
                // A completion for something asked (or not), any status,
                // any amount of data.
                let id = if sent.is_empty() || op[1] & 0x80 != 0 { op[2] as u32 } else { sent.remove(op[2] as usize % sent.len()) };
                let status = [Status::Ok, Status::Stall, Status::Error, Status::Babble, Status::Cancelled, Status::NoDevice][op[3] as usize % 6];
                let n = u16::from_le_bytes([op[4], op[5]]) as usize % 2048;
                let data = if op[6] & 1 == 0 { vec![op[7]; n] } else { Vec::new() };
                x.usb_event(0, Event::Done(Done { id, status, actual: n as u32, data }));
            }
        }
    }
    let _ = x.save();
});
