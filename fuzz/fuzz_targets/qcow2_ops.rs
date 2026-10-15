//! Differential: random writes, discards, zero writes and reopens on a
//! qcow2 over a patterned backing file, against a plain byte model.
//! Catches allocator / free-list / zero-cluster bugs that corrupt data
//! without crashing.
#![no_main]
use std::path::Path;

use bulwark_block::qcow2::Qcow2Image;
use bulwark_block::{DiskImage, Format};
use libfuzzer_sys::fuzz_target;

const C: u64 = 64 << 10;
const SIZE: u64 = 24 * C;

fn open(p: &Path) -> Qcow2Image {
    Qcow2Image::open(p, false).unwrap()
}

fuzz_target!(|data: &[u8]| {
    let td = tempfile::tempdir().unwrap();
    let (base, top) = (td.path().join("base.raw"), td.path().join("top.qcow2"));
    let mut model: Vec<u8> = (0..SIZE).map(|i| (i / 512) as u8 | 1).collect();
    std::fs::write(&base, &model).unwrap();
    Qcow2Image::create(&top, SIZE, Some((&base, Format::Raw))).unwrap();
    let mut q = open(&top);
    for op in data.chunks_exact(6).take(64) {
        let off = u32::from_le_bytes([op[1], op[2], op[3], 0]) as u64 % SIZE;
        let len = (u16::from_le_bytes([op[4], op[5]]) as u64 * 37).min(SIZE - off);
        match op[0] % 6 {
            0 | 1 => {
                let b = op[1] ^ op[4];
                q.write_at(&vec![b; len as usize], off).unwrap();
                model[off as usize..(off + len) as usize].fill(b);
            }
            2 => {
                q.discard(off, len).unwrap();
                // Whole clusters inside the range read as zeros after.
                let (s, e) = (off.div_ceil(C) * C, (off + len) / C * C);
                if e > s {
                    model[s as usize..e as usize].fill(0);
                }
            }
            3 => {
                q.write_zeroes(off, len).unwrap();
                model[off as usize..(off + len) as usize].fill(0);
            }
            4 => {
                drop(q);
                q = open(&top);
            }
            _ => {
                drop(q);
                bulwark_block::qcow2::compact(&top, &Default::default()).unwrap();
                q = open(&top);
            }
        }
    }
    let check = |q: &Qcow2Image| {
        let mut got = vec![0u8; SIZE as usize];
        q.read_at(&mut got, 0).unwrap();
        if got != model {
            let i = got.iter().zip(&model).position(|(a, b)| a != b).unwrap();
            panic!("mismatch at {i}: got {} want {}", got[i], model[i]);
        }
    };
    check(&q);
    drop(q);
    let q = Qcow2Image::open(&top, true).unwrap();
    check(&q);
});
