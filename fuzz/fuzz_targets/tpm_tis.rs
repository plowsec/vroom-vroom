//! TPM TIS registers from a hostile guest: locality, status, FIFO with
//! any lengths; the backend echoes a well-formed reply.
#![no_main]
use bulwark_devices::tpm::{Tpm, TpmBackend};
use bulwark_devices::BusDevice;
use libfuzzer_sys::fuzz_target;

struct Echo;
impl TpmBackend for Echo {
    fn execute(&mut self, cmd: &[u8]) -> Vec<u8> {
        assert!(cmd.len() <= bulwark_devices::tpm::MAX_BUFFER);
        let mut r = vec![0x80, 0x01, 0, 0, 0, 10, 0, 0, 0, 0];
        r.extend_from_slice(&cmd[..cmd.len().min(64)]);
        let n = r.len() as u32;
        r[2..6].copy_from_slice(&n.to_be_bytes());
        r
    }
    fn reset(&mut self) {}
}

fuzz_target!(|data: &[u8]| {
    let mut t = Tpm::new(0x0c00_0000, Box::new(Echo));
    for op in data.chunks_exact(8) {
        let off = u16::from_le_bytes([op[1], op[2]]) as u64 % 0x5000;
        let len = [1usize, 2, 4][op[3] as usize % 3];
        if op[0] & 1 == 0 {
            t.write(off, &op[4..4 + len]);
        } else {
            let mut b = [0u8; 4];
            t.read(off, &mut b[..len]);
        }
    }
});
