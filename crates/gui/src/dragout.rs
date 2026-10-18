//! Files dragged out of a VM window onto the Mac (guest -> host).
//!
//! When the mouse leaves the guest screen with the guest's button held,
//! we ask the guest whether it drags files (`ToVmm::DragOut`). If it
//! does, we start a drag on the Mac with file promises (dragout_mac.rs):
//! nothing is copied until something takes the drop. Then Finder (or the
//! app) says where each item goes, and we fetch it from the guest
//! through the VMM (`ToVmm::DragOutGet` with a pipe) and write it there,
//! here in the app: the sandboxed VMM writes no file of the user's.
//!
//! The guest chooses names and sizes, so they are checked again here
//! (vroom-proto's TreeCheck: only the item asked for, each entry inside a
//! folder announced before it, no more bytes than announced). Files and
//! folders are only ever created new (never through a link, never over
//! something that is there), a name that is taken gets a number, and the
//! copy stops before the Mac's disk gets too full.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use vroom_proto::{drop_name_ok, Decoder, Message, TreeCheck, DROP_CHUNK, DROP_PATH_MAX};

use crate::dnd::Progress;

/// Free space a copy leaves on the Mac's disk, at least.
pub const KEEP_FREE: u64 = 2 << 30;

/// A name in `dir` that isn't taken, the way Finder makes them:
/// "photo.jpg", "photo 2.jpg", "photo 3.jpg".
pub fn free_name(dir: &Path, name: &str, is_dir: bool) -> PathBuf {
    let p = dir.join(name);
    if std::fs::symlink_metadata(&p).is_err() {
        return p;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..10_000).map(|n| dir.join(format!("{stem} {n}{ext}"))).find(|p| std::fs::symlink_metadata(p).is_err()).unwrap_or(p)
}

/// Bytes free for us on the disk holding `p`.
fn free_space(p: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills the zeroed struct for a valid C string.
    #[allow(unsafe_code)]
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(c.as_ptr(), &mut s) == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
    }
}

/// Read item `name` (a folder if `dir`) of drag `id` from `r` and write
/// it into `dest`, under a free name. Ok: where it went. On failure
/// nothing of it stays.
pub fn receive(r: &mut impl Read, id: u32, name: &str, dir: bool, dest: &Path, p: &Progress, keep_free: u64) -> Result<PathBuf, String> {
    if !drop_name_ok(name) {
        return Err("a name the Mac can't take".into());
    }
    let top = free_name(dest, name, dir);
    let mut made = false;
    let r = write_item(r, id, name, dir, &top, &mut made, p, keep_free);
    if r.is_err() && made {
        let _ = if dir { std::fs::remove_dir_all(&top) } else { std::fs::remove_file(&top) };
    }
    r.map(|_| top)
}

#[allow(clippy::too_many_arguments)]
fn write_item(r: &mut impl Read, id: u32, name: &str, dir: bool, top: &Path, made: &mut bool, p: &Progress, keep_free: u64) -> Result<(), String> {
    let mut check = TreeCheck::new(id, name, dir);
    let mut dec = Decoder::new(DROP_CHUNK + DROP_PATH_MAX + 64);
    let mut buf = vec![0u8; DROP_CHUNK];
    let mut file: Option<File> = None;
    let dest = top.parent().unwrap_or(Path::new("/"));
    loop {
        if p.stop.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        let n = r.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the VM stopped sending".into());
        }
        dec.push(&buf[..n]);
        while let Some(m) = dec.next().map_err(|e| format!("bad data from the VM: {e}"))? {
            if let Message::DragOutFail { why, .. } = &m {
                return Err(if why.is_empty() { "the guest couldn't send it".into() } else { why.clone() });
            }
            let end = check.check(&m).map_err(|e| format!("the guest sent a bad file list ({e})"))?;
            match m {
                Message::DropEntry { path, dir: is_dir, size, .. } => {
                    file = None;
                    // Checked: the item's name, then names under it.
                    let at = match path.split_once('/') {
                        None => top.to_path_buf(),
                        Some((_, rest)) => rest.split('/').fold(top.to_path_buf(), |a, n| a.join(n)),
                    };
                    if path == name {
                        // A folder's size is its files' total (for the
                        // progress bar only).
                        p.total.fetch_add(size, Ordering::Relaxed);
                    }
                    if is_dir {
                        std::fs::create_dir(&at).map_err(|e| format!("{}: {e}", at.display()))?;
                        *made |= path == name;
                    } else {
                        if free_space(dest).is_some_and(|f| f < size.saturating_add(keep_free)) {
                            return Err("not enough free space on the Mac".into());
                        }
                        let f = OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .custom_flags(libc::O_NOFOLLOW)
                            .open(&at)
                            .map_err(|e| format!("{}: {e}", at.display()))?;
                        *made |= path == name;
                        p.files.fetch_add(1, Ordering::Relaxed);
                        file = Some(f);
                    }
                }
                Message::DropData { data, .. } => {
                    let f = file.as_mut().ok_or("data without a file")?;
                    f.write_all(&data).map_err(|e| e.to_string())?;
                    p.sent.fetch_add(data.len() as u64, Ordering::Relaxed);
                }
                _ => {}
            }
            if end {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vroom-dragout-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn bytes(ms: &[Message]) -> Vec<u8> {
        ms.iter().flat_map(|m| m.encode()).collect()
    }

    fn e(path: &str, dir: bool, size: u64) -> Message {
        Message::DropEntry { id: 3, path: path.into(), dir, size }
    }

    #[test]
    fn a_folder_lands_under_a_free_name() {
        let d = tmp("tree");
        std::fs::create_dir(d.join("Photos")).unwrap();
        let ms = [
            e("Photos", true, 5),
            e("Photos/a.txt", false, 5),
            Message::DropData { id: 3, data: b"hel".to_vec() },
            Message::DropData { id: 3, data: b"lo".to_vec() },
            e("Photos/s", true, 0),
            e("Photos/s/empty", false, 0),
            Message::DropEnd { id: 3 },
        ];
        let p = Progress::default();
        let got = receive(&mut bytes(&ms).as_slice(), 3, "Photos", true, &d, &p, 0).unwrap();
        assert_eq!(got, d.join("Photos 2"));
        assert_eq!(std::fs::read(got.join("a.txt")).unwrap(), b"hello");
        assert!(got.join("s/empty").is_file());
        assert_eq!((p.sent.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed), p.files.load(Ordering::Relaxed)), (5, 5, 2));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn failures_leave_nothing() {
        let d = tmp("fail");
        let p = Progress::default();
        // Short, then the stream ends.
        let ms = [e("a.bin", false, 9), Message::DropData { id: 3, data: vec![1; 4] }];
        assert!(receive(&mut bytes(&ms).as_slice(), 3, "a.bin", false, &d, &p, 0).is_err());
        // The guest's own reason.
        let ms = [e("F", true, 0), e("F/x", false, 1), Message::DragOutFail { stream: 3, why: "in use".into() }];
        assert_eq!(receive(&mut bytes(&ms).as_slice(), 3, "F", true, &d, &p, 0), Err("in use".into()));
        // A file where the folder's tree doesn't hold.
        let ms = [e("F", true, 0), e("G/x", false, 0)];
        assert!(receive(&mut bytes(&ms).as_slice(), 3, "F", true, &d, &p, 0).is_err());
        // Not enough room on the disk.
        let ms = [e("big", false, 1 << 20), Message::DropData { id: 3, data: vec![0; 10] }];
        assert_eq!(receive(&mut bytes(&ms).as_slice(), 3, "big", false, &d, &p, u64::MAX / 2), Err("not enough free space on the Mac".into()));
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn names_are_finder_style() {
        let d = tmp("names");
        std::fs::write(d.join("a.txt"), b"").unwrap();
        std::fs::write(d.join("a 2.txt"), b"").unwrap();
        std::fs::create_dir(d.join("v1.0")).unwrap();
        assert_eq!(free_name(&d, "a.txt", false), d.join("a 3.txt"));
        assert_eq!(free_name(&d, "v1.0", true), d.join("v1.0 2"));
        // A dangling link counts as taken: never written through.
        std::os::unix::fs::symlink(d.join("nowhere"), d.join("l")).unwrap();
        assert_eq!(free_name(&d, "l", false), d.join("l 2"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
