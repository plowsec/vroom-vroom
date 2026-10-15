//! HD Audio controller with a hostile driver: guest RAM (CORB verbs,
//! buffer descriptor lists, sample buffers) from the input, then a stream
//! of register writes and reads of any width, config space writes (MSI)
//! and ticks at made-up times.
#![no_main]
use std::sync::Arc;
use std::time::{Duration, Instant};

use vroom_devices::audio::{AudioSink, AudioSource};
use vroom_devices::hda::{Hda, HdaIds};
use vroom_devices::pci::{IntxRouter, MsiSender, PciDevice};
use vroom_devices::NoIrq;
use vroom_memory::{GuestAddress, GuestMemory};
use libfuzzer_sys::fuzz_target;

struct NoMsi;
impl MsiSender for NoMsi {
    fn send(&self, _a: u64, _d: u32) {}
}

struct Sink;
impl AudioSink for Sink {
    fn start(&mut self, rate: u32, ch: u8) {
        assert!((1..=2).contains(&ch) && rate > 0);
    }
    fn write(&mut self, pcm: &[u8]) {
        std::hint::black_box(pcm.len());
    }
    fn stop(&mut self) {}
}

struct Mic;
impl AudioSource for Mic {
    fn start(&mut self, rate: u32, ch: u8) {
        assert!((1..=2).contains(&ch) && rate > 0);
    }
    fn read(&mut self, out: &mut [u8]) {
        assert_eq!(out.len() % 2, 0);
        out.fill(0x7f);
    }
    fn stop(&mut self) {}
}

const RAM: u64 = 0x4000_0000;
const RAM_SIZE: u64 = 256 << 10;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let k = (u16::from_le_bytes([data[0], data[1]]) as usize).min(data.len() - 2).min(RAM_SIZE as usize);
    let (ram, ops) = data[2..].split_at(k);
    let mem = GuestMemory::new(&[(GuestAddress(RAM), RAM_SIZE)]).unwrap();
    mem.write(GuestAddress(RAM), ram).unwrap();
    let r = IntxRouter::new(vec![Arc::new(NoIrq); 4]);
    let mut h = Hda::new(5, mem, r.pin(5, 1), Arc::new(NoMsi), HdaIds::default(), true, Box::new(Sink), Box::new(Mic));
    h.config_mut().write(4, &6u16.to_le_bytes());
    // Out of reset, rings in RAM, so the stream of ops gets somewhere.
    for (off, v) in [(0x08u64, 1u32), (0x40, RAM as u32), (0x4e, 2), (0x50, RAM as u32 + 0x800), (0x5e, 2), (0x5c, 3), (0x4c, 2)] {
        h.bar_write(0, off, &v.to_le_bytes());
    }
    let t0 = Instant::now();
    for op in ops.chunks_exact(8) {
        let off = u16::from_le_bytes([op[1], op[2]]) as u64 & 0x3fff;
        let v = u32::from_le_bytes(op[3..7].try_into().unwrap());
        let len = [1, 2, 4, 8][(op[0] >> 2) as usize % 4];
        let mut buf = [0u8; 8];
        buf[..4].copy_from_slice(&v.to_le_bytes());
        buf[4..].copy_from_slice(&v.rotate_left(op[7] as u32).to_le_bytes());
        match op[0] % 4 {
            0 => h.bar_write(0, off, &buf[..len]),
            1 => h.bar_read(0, off, &mut buf[..len]),
            2 => {
                // Config space (MSI capability, command register).
                let o = (off & 0xff) as usize;
                h.config_mut().write(o, &buf[..len]);
                h.config_written(o, len);
            }
            _ => {
                h.tick(t0 + Duration::from_micros(v as u64 % 3_000_000));
            }
        }
    }
    h.pause();
    let s = h.save().unwrap();
    h.resume();
    h.restore(&s).unwrap();
});
