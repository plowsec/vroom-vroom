//! A downloaded qcow2 image: header, tables, compressed clusters.
#![no_main]
use std::io::Write;

use vroom_block::qcow2::Qcow2Image;
use vroom_block::DiskImage;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(data).unwrap();
    let Ok(q) = Qcow2Image::open(f.path(), true) else { return };
    let size = q.size();
    let mut buf = vec![0u8; 4096];
    for off in [0, size / 3, size.saturating_sub(4096)] {
        let n = 4096.min(size.saturating_sub(off)) as usize;
        let _ = q.read_at(&mut buf[..n], off);
    }
});
