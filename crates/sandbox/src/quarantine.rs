//! `com.apple.quarantine` on files the guest supplied (drag out, `vroom guest pull`):
//! Gatekeeper then checks a guest-made app or script before it runs. No-op off macOS.

use std::io;
use std::os::fd::AsRawFd;

/// The value macOS writes for a download: flags; hex unix time; agent; (no event UUID).
pub fn value(unix_time: u64) -> String {
    format!("0081;{unix_time:08x};vroom;")
}

/// Mark the open file or directory `fd`.
#[cfg(target_os = "macos")]
pub fn mark(fd: &impl AsRawFd) -> io::Result<()> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let v = value(now);
    // SAFETY: NUL-terminated name, `v` valid for `v.len()` bytes; fd is open (borrowed).
    let r = unsafe {
        libc::fsetxattr(fd.as_raw_fd(), c"com.apple.quarantine".as_ptr(), v.as_ptr().cast(), v.len(), 0, 0)
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn mark(_fd: &impl AsRawFd) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn marks_files_and_dirs() {
        let t = tempfile::tempdir().unwrap();
        let f = std::fs::File::create(t.path().join("a")).unwrap();
        super::mark(&f).unwrap();
        let d = std::fs::File::open(t.path()).unwrap();
        super::mark(&d).unwrap();
        for p in [t.path().join("a"), t.path().to_path_buf()] {
            let o = std::process::Command::new("xattr").args(["-p", "com.apple.quarantine"]).arg(&p).output().unwrap();
            let s = String::from_utf8(o.stdout).unwrap();
            assert!(s.starts_with("0081;") && s.trim_end().ends_with(";vroom;"), "{p:?}: {s:?}");
        }
        assert_eq!(super::value(0x6af0565f), "0081;6af0565f;vroom;");
    }
}
