//! FUSE requests from a hostile guest against a share holding a few
//! files, a directory and escaping symlinks. The first byte picks the
//! client (Linux or Windows INIT flags) and read-only or writable.
//! Length-prefixed requests; replies must stay within the room given,
//! and nothing next to the share may change.
#![no_main]
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use vroom_fs::Share;
use vroom_virtio::devices::fs::FuseServer;
use libfuzzer_sys::fuzz_target;

/// (read-only share, writable share, the directory next to them)
fn dirs() -> &'static (PathBuf, PathBuf, PathBuf) {
    static R: OnceLock<(PathBuf, PathBuf, PathBuf)> = OnceLock::new();
    R.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("vroom-fuzz-fs-{}", std::process::id()));
        let (ro, rw, out) = (base.join("ro"), base.join("rw"), base.join("outside"));
        for d in [&ro, &rw] {
            let _ = std::fs::create_dir_all(d.join("dir/deeper"));
            let _ = std::fs::write(d.join("file"), vec![7u8; 10000]);
            let _ = std::fs::write(d.join("dir/deeper/x"), b"x");
            let _ = std::os::unix::fs::symlink("/etc", d.join("escape"));
            let _ = std::os::unix::fs::symlink(&out, d.join("next"));
        }
        let _ = std::fs::create_dir_all(&out);
        let _ = std::fs::write(out.join("secret"), b"secret");
        (ro, rw, out)
    })
}

fn untouched(out: &Path) -> bool {
    std::fs::read(out.join("secret")).is_ok_and(|b| b == b"secret")
        && std::fs::read_dir(out).is_ok_and(|d| d.count() == 1)
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, mut rest)) = data.split_first() else { return };
    let (ro, rw, out) = dirs();
    let writable = mode & 2 != 0;
    let mut s = FuseServer::new(Share::open(if writable { rw } else { ro }, !writable).unwrap());
    // Always start with a valid INIT so the fuzzer reaches the ops.
    // Linux asks for async reads; virtiofs.exe for READDIRPLUS and max_pages.
    let flags: u32 = if mode & 1 != 0 { (1 << 13) | (1 << 22) } else { 1 | (1 << 5) };
    let mut init = vec![0u8; 40 + 16];
    init[0..4].copy_from_slice(&56u32.to_le_bytes());
    init[4..8].copy_from_slice(&26u32.to_le_bytes());
    init[40..44].copy_from_slice(&7u32.to_le_bytes());
    init[44..48].copy_from_slice(&31u32.to_le_bytes());
    init[52..56].copy_from_slice(&flags.to_le_bytes());
    s.handle(&init, 4096);
    while rest.len() > 4 {
        let n = (u16::from_le_bytes([rest[0], rest[1]]) as usize).min(rest.len() - 4);
        let room = u16::from_le_bytes([rest[2], rest[3]]) as usize;
        if let Some(r) = s.handle(&rest[4..4 + n], room) {
            assert!(r.len() <= 16 + room.max(128) + 64, "reply {} for room {room}", r.len());
        }
        rest = &rest[4 + n..];
    }
    assert!(untouched(out), "something changed outside the share");
});
