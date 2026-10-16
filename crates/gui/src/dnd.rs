//! Files dragged from the Mac onto a VM window.
//!
//! While they hover we send only their names (the guest gets ready: on
//! Windows it starts a drag of its own under the pointer, which we drive
//! with the mouse button and the pointer). On the drop we read the files
//! here, in the app (the sandboxed VMM can't, and shouldn't), and write
//! them into a pipe the VMM forwards to the guest agent, as agent frames.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use vroom_proto::{Message, DROP_CHUNK, DROP_PATH_MAX};

/// Most files and folders one drop may carry (the walk stops there).
const MAX_ENTRIES: usize = 200_000;

/// A name the guest takes (see `vroom_proto::fix_name`).
pub fn guest_name(n: &str) -> String {
    vroom_proto::fix_name(n)
}

/// Top-level names and whether each is a folder.
pub fn items(paths: &[PathBuf]) -> Vec<(String, bool)> {
    paths
        .iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy();
            Some((guest_name(&name), p.is_dir()))
        })
        .collect()
}

/// One file or folder to send: its path in the guest, where it is here.
struct Entry {
    rel: String,
    path: PathBuf,
    dir: bool,
    size: u64,
}

/// Everything under the dropped paths, folders before what's in them.
/// Links and special files are left out (a link could lead anywhere,
/// a loop forever).
fn walk(paths: &[PathBuf]) -> Vec<Entry> {
    let mut out = Vec::new();
    for p in paths {
        let Some(name) = p.file_name() else { continue };
        add(&mut out, p, guest_name(&name.to_string_lossy()));
    }
    out
}

fn add(out: &mut Vec<Entry>, path: &Path, rel: String) {
    if out.len() >= MAX_ENTRIES || rel.len() > DROP_PATH_MAX {
        return;
    }
    let Ok(m) = std::fs::symlink_metadata(path) else { return };
    if m.is_file() {
        out.push(Entry { rel, path: path.to_path_buf(), dir: false, size: m.len() });
    } else if m.is_dir() {
        out.push(Entry { rel: rel.clone(), path: path.to_path_buf(), dir: true, size: 0 });
        let Ok(rd) = std::fs::read_dir(path) else { return };
        let mut kids: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        kids.sort();
        for k in kids {
            if let Some(n) = k.file_name() {
                add(out, &k, format!("{rel}/{}", guest_name(&n.to_string_lossy())));
            }
        }
    }
}

/// How a drop is going, shared with the window.
#[derive(Debug, Default)]
pub struct Progress {
    pub total: AtomicU64,
    pub sent: AtomicU64,
    pub files: AtomicU64,
    /// Stop now (the window closed).
    pub stop: AtomicBool,
}

/// Write the drop into `pipe`, then end it. Runs on its own thread. A
/// file that changes size underway is sent as announced (cut or padded):
/// the guest checks sizes.
pub fn send(id: u32, paths: Vec<PathBuf>, mut pipe: UnixStream, p: Arc<Progress>) {
    let entries = walk(&paths);
    p.total.store(entries.iter().map(|e| e.size).sum(), Ordering::Relaxed);
    let mut buf = vec![0u8; DROP_CHUNK];
    let r = (|| -> std::io::Result<()> {
        for e in &entries {
            if p.stop.load(Ordering::Relaxed) {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            pipe.write_all(&Message::DropEntry { id, path: e.rel.clone(), dir: e.dir, size: e.size }.encode())?;
            if e.dir {
                continue;
            }
            p.files.fetch_add(1, Ordering::Relaxed);
            let mut f = std::fs::File::open(&e.path).ok();
            let mut left = e.size;
            while left > 0 {
                if p.stop.load(Ordering::Relaxed) {
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                let want = left.min(buf.len() as u64) as usize;
                let n = match f.as_mut().map(|f| f.read(&mut buf[..want])) {
                    Some(Ok(n)) if n > 0 => n,
                    // Unreadable or shorter than it was: zeros.
                    _ => {
                        f = None;
                        buf[..want].fill(0);
                        want
                    }
                };
                pipe.write_all(&Message::DropData { id, data: buf[..n].to_vec() }.encode())?;
                left -= n as u64;
                p.sent.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
        pipe.write_all(&Message::DropEnd { id }.encode())
    })();
    if let Err(e) = r {
        log::info!("drop {id}: stopped ({e})");
    }
}

/// Where the mouse is on the screen, in points from the top left of the
/// main display (macOS). Windows get no mouse events during a drag.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn mouse() -> Option<(f64, f64)> {
    #[repr(C)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
        fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPoint;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: *mut std::ffi::c_void);
    }
    // SAFETY: an event with no source just carries the current mouse
    // position; we release what we created.
    unsafe {
        let e = CGEventCreate(std::ptr::null());
        if e.is_null() {
            return None;
        }
        let p = CGEventGetLocation(e);
        CFRelease(e);
        Some((p.x, p.y))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn mouse() -> Option<(f64, f64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use vroom_proto::Decoder;

    #[test]
    fn names_become_ones_the_guest_takes() {
        assert_eq!(guest_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(guest_name(".."), "_");
        assert_eq!(guest_name("x\ny"), "xy");
        assert!(vroom_proto::drop_name_ok(&guest_name(&"é".repeat(300))));
    }

    #[test]
    fn a_folder_goes_over_as_frames() {
        let d = std::env::temp_dir().join(format!("vroom-dnd-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("top/sub")).unwrap();
        std::fs::write(d.join("top/a.txt"), b"hello").unwrap();
        std::fs::write(d.join("top/sub/b:c"), vec![7u8; DROP_CHUNK + 10]).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", d.join("top/link")).unwrap();
        let (a, mut b) = UnixStream::pair().unwrap();
        let p = Arc::new(Progress::default());
        let p2 = p.clone();
        let paths = vec![d.join("top")];
        assert_eq!(items(&paths), vec![("top".to_string(), true)]);
        let t = std::thread::spawn(move || send(5, paths, a, p2));
        let mut raw = Vec::new();
        b.read_to_end(&mut raw).unwrap();
        t.join().unwrap();
        let mut dec = Decoder::new(DROP_CHUNK + 4096);
        dec.push(&raw);
        let msgs: Vec<Message> = std::iter::from_fn(|| dec.next().unwrap()).collect();
        let entries: Vec<(String, bool, u64)> = msgs
            .iter()
            .filter_map(|m| if let Message::DropEntry { path, dir, size, .. } = m { Some((path.clone(), *dir, *size)) } else { None })
            .collect();
        assert_eq!(
            entries,
            vec![("top".into(), true, 0), ("top/a.txt".into(), false, 5), ("top/sub".into(), true, 0), ("top/sub/b_c".into(), false, DROP_CHUNK as u64 + 10)]
        );
        let data: usize = msgs.iter().map(|m| if let Message::DropData { data, .. } = m { data.len() } else { 0 }).sum();
        assert_eq!(data, 5 + DROP_CHUNK + 10);
        assert_eq!(msgs.last(), Some(&Message::DropEnd { id: 5 }));
        assert_eq!(p.sent.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
        let _ = std::fs::remove_dir_all(&d);
    }
}
