//! FUSE requests from a hostile guest against a read-only share holding a
//! few files, a directory and an escaping symlink. Length-prefixed
//! requests; replies must stay within the room given.
#![no_main]
use std::path::PathBuf;
use std::sync::OnceLock;

use vroom_fs::Share;
use vroom_virtio::devices::fs::FuseServer;
use libfuzzer_sys::fuzz_target;

fn root() -> &'static PathBuf {
    static R: OnceLock<PathBuf> = OnceLock::new();
    R.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("vroom-fuzz-fs-{}", std::process::id()));
        let _ = std::fs::create_dir_all(d.join("dir/deeper"));
        let _ = std::fs::write(d.join("file"), vec![7u8; 10000]);
        let _ = std::fs::write(d.join("dir/deeper/x"), b"x");
        let _ = std::os::unix::fs::symlink("/etc", d.join("escape"));
        d
    })
}

fuzz_target!(|data: &[u8]| {
    let mut s = FuseServer::new(Share::open(root(), true).unwrap());
    // Always start with a valid INIT so the fuzzer reaches the ops.
    let mut init = vec![0u8; 40 + 16];
    init[0..4].copy_from_slice(&56u32.to_le_bytes());
    init[4..8].copy_from_slice(&26u32.to_le_bytes());
    init[40..44].copy_from_slice(&7u32.to_le_bytes());
    init[44..48].copy_from_slice(&31u32.to_le_bytes());
    s.handle(&init, 4096);
    let mut rest = data;
    while rest.len() > 4 {
        let n = (u16::from_le_bytes([rest[0], rest[1]]) as usize).min(rest.len() - 4);
        let room = u16::from_le_bytes([rest[2], rest[3]]) as usize;
        if let Some(r) = s.handle(&rest[4..4 + n], room) {
            assert!(r.len() <= 16 + room.max(128) + 64, "reply {} for room {room}", r.len());
        }
        rest = &rest[4 + n..];
    }
});
