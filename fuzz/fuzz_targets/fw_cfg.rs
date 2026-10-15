//! fw_cfg from a hostile guest: selector, data port and the DMA interface
//! (descriptors in guest RAM), including a guest-writable file.
#![no_main]
use vroom_devices::fw_cfg::FwCfg;
use vroom_devices::BusDevice;
use vroom_memory::{GuestAddress, GuestMemory};
use libfuzzer_sys::fuzz_target;

const RAM: u64 = 0x4000_0000;
const RAM_SIZE: u64 = 64 << 10;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let k = (u16::from_le_bytes([data[0], data[1]]) as usize).min(data.len() - 2).min(RAM_SIZE as usize);
    let (ram, ops) = data[2..].split_at(k);
    let mem = GuestMemory::new(&[(GuestAddress(RAM), RAM_SIZE)]).unwrap();
    mem.write(GuestAddress(RAM), ram).unwrap();
    let mut f = FwCfg::new(0x0902_0000, mem, 4, 1 << 30);
    f.add_file("etc/ramfb", vec![0; 28], Some(Box::new(|d: &[u8]| {
        std::hint::black_box(d.len());
    })))
    .unwrap();
    f.add_file("etc/table", vec![7; 300], None).unwrap();
    for op in ops.chunks_exact(10) {
        let v = u64::from_le_bytes(op[2..10].try_into().unwrap());
        match op[0] % 6 {
            0 => f.write(8, &(v as u16).to_be_bytes()),
            1 => {
                let mut b = [0u8; 8];
                f.read(0, &mut b[..1 + op[1] as usize % 8]);
            }
            2 => f.write(0, &op[2..3 + op[1] as usize % 7]),
            // DMA: point at a descriptor somewhere in RAM.
            3 => f.write(16, &(RAM + (v % RAM_SIZE)).to_be_bytes()),
            4 => {
                f.write(16, &((RAM + (v % RAM_SIZE)) >> 32).to_be_bytes()[4..]);
                f.write(20, &((RAM + (v % RAM_SIZE)) as u32).to_be_bytes());
            }
            _ => {
                let mut b = [0u8; 8];
                f.read(16 + (op[1] as u64 % 8), &mut b[..4]);
            }
        }
    }
});
