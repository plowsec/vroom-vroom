//! Flash (UEFI variable store) command state machine from a hostile guest.
#![no_main]
use vroom_devices::pflash::PFlash;
use vroom_devices::BusDevice;
use libfuzzer_sys::fuzz_target;

const SIZE: u64 = 256 << 10;

fuzz_target!(|data: &[u8]| {
    let Ok(mut p) = PFlash::new(0x0400_0000, SIZE, vec![0xff; SIZE as usize], None) else { return };
    for op in data.chunks_exact(8) {
        let off = u32::from_le_bytes([op[1], op[2], op[3], 0]) as u64 % SIZE;
        let len = [1usize, 2, 4][op[4] as usize % 3];
        if op[0] & 1 == 0 {
            p.write(off, &op[5..5 + len.min(3)]);
        } else {
            let mut b = [0u8; 4];
            p.read(off, &mut b[..len]);
        }
    }
});
