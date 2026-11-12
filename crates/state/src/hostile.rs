//! Files in a state dir, which the sandboxed VMM can write: whatever runs
//! outside the sandbox (the next VMM start, the app, the CLI) treats them
//! as planted. No links followed, no FIFOs opened blocking, sizes capped.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

fn invalid(p: &Path, what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {what}", p.display()))
}

/// Open `p` itself (not what a link there points at) without blocking.
fn open_nofollow(p: &Path, write: bool) -> io::Result<File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true).write(write).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let f = o.open(p)?;
    let m = f.metadata()?;
    // A hard link makes another file of the user's look like ours.
    if !m.is_file() || m.nlink() != 1 {
        return Err(invalid(p, "not a plain file"));
    }
    Ok(f)
}

/// A small file, read whole: a regular file, at most `max` bytes.
pub fn read_small(p: &Path, max: u64) -> io::Result<Vec<u8>> {
    let f = open_nofollow(p, false)?;
    let mut v = Vec::new();
    // take(): the file can grow after the size check.
    f.take(max + 1).read_to_end(&mut v)?;
    if v.len() as u64 > max {
        return Err(invalid(p, "too large"));
    }
    Ok(v)
}

/// An existing regular file opened read-write (blocking I/O from here on).
pub fn open_rw(p: &Path) -> io::Result<File> {
    let f = open_nofollow(p, true)?;
    // SAFETY: valid fd we own; F_GETFL/F_SETFL take and return plain ints.
    let fl = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) };
    // SAFETY: as above.
    if fl < 0 || unsafe { libc::fcntl(f.as_raw_fd(), libc::F_SETFL, fl & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(f)
}

/// Replace `p` with `data`: a new file next to it (never an existing one,
/// never through a link), then renamed over `p` (a link at `p` is replaced,
/// not followed).
pub fn write_atomic(p: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let name = p.file_name().ok_or_else(|| invalid(p, "no file name"))?;
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), t.subsec_nanos()));
    let tmp = p.with_file_name(tmp_name);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true).mode(mode).custom_flags(libc::O_NOFOLLOW);
    let mut f = o.open(&tmp)?;
    let r = f.write_all(data).and_then(|()| f.sync_all()).and_then(|()| std::fs::rename(&tmp, p));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// `p` is a directory itself, not a link to one somewhere else.
pub fn real_dir(p: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(p)?.is_dir() {
        Ok(())
    } else {
        Err(invalid(p, "not a directory"))
    }
}

/// `real_dir`, made (mode `mode`) if missing. Its parent must exist.
pub fn ensure_dir(p: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(mode).create(p) {
        Ok(()) => Ok(()),
        // mkdir doesn't follow a link at `p`: it says it exists.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => real_dir(p),
        Err(e) => Err(e),
    }
}

/// `p` is a plain file: regular, not a link, one name.
pub fn plain_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && m.nlink() == 1)
}

/// Where this user's VM sockets go: `$XDG_RUNTIME_DIR/vroom` on Linux when
/// set, else `<temp>/vroom`. Made if missing; must be a real directory of
/// ours, mode 0700 (on Linux `/tmp/vroom` is anyone's to create first).
pub fn socket_dir() -> io::Result<PathBuf> {
    let base = match std::env::var_os("XDG_RUNTIME_DIR").filter(|_| cfg!(target_os = "linux")) {
        Some(d) if Path::new(&d).is_absolute() => PathBuf::from(d),
        _ => std::env::temp_dir(),
    };
    let dir = base.join("vroom");
    ensure_dir(&dir, 0o700)?;
    let m = std::fs::symlink_metadata(&dir)?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if m.uid() != me || m.mode() & 0o777 != 0o700 {
        return Err(invalid(&dir, "not ours (owner and mode 0700)"));
    }
    Ok(dir)
}

/// The socket a VM on state dir `state` listens on for `ext` ("sock": the
/// viewer, "ctl": `vroom guest`). Derived from the state dir, never read
/// from it.
pub fn socket_path(state: &Path, ext: &str) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let real = std::fs::canonicalize(state).unwrap_or_else(|_| state.to_path_buf());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in real.as_os_str().as_bytes() {
        h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
    }
    Ok(socket_dir()?.join(format!("{h:016x}.{ext}")))
}

/// The socket a running VMM wrote to `state/<file>`, if it is the one
/// [`socket_path`] gives and a socket is there.
pub fn running_socket(state: &Path, file: &str, ext: &str) -> Option<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    let said = read_small(&state.join(file), 4096).ok()?;
    let want = socket_path(state, ext).ok()?;
    if std::str::from_utf8(&said).ok()?.trim() != want.to_str()? {
        return None;
    }
    std::fs::symlink_metadata(&want).ok()?.file_type().is_socket().then_some(want)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planted_links_and_fifos() {
        let t = tempfile::tempdir().unwrap();
        let victim = t.path().join("victim");
        std::fs::write(&victim, b"secret").unwrap();
        let p = t.path().join("uuid");
        std::os::unix::fs::symlink(&victim, &p).unwrap();
        assert!(read_small(&p, 64).is_err(), "followed a link");
        assert!(open_rw(&p).is_err());
        write_atomic(&p, b"new", 0o600).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"secret", "wrote through a link");
        assert_eq!(read_small(&p, 64).unwrap(), b"new");
        // Hard link to another file.
        let h = t.path().join("hard");
        std::fs::hard_link(&victim, &h).unwrap();
        assert!(read_small(&h, 64).is_err());
        assert!(!plain_file(&h));
        // FIFO: no hang.
        let fifo = t.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(read_small(&fifo, 64).is_err());
        assert!(open_rw(&fifo).is_err());
        // Too big.
        std::fs::write(t.path().join("big"), [0u8; 100]).unwrap();
        assert!(read_small(&t.path().join("big"), 64).is_err());
    }

    #[test]
    fn dirs_are_real() {
        let t = tempfile::tempdir().unwrap();
        let elsewhere = t.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let d = t.path().join("disks");
        std::os::unix::fs::symlink(&elsewhere, &d).unwrap();
        assert!(ensure_dir(&d, 0o700).is_err());
        assert!(real_dir(&d).is_err());
        std::fs::remove_file(&d).unwrap();
        ensure_dir(&d, 0o700).unwrap();
        ensure_dir(&d, 0o700).unwrap();
    }

    #[test]
    fn sockets_come_from_the_state_dir_name() {
        let t = tempfile::tempdir().unwrap();
        let s = t.path().join("vm.state");
        std::fs::create_dir(&s).unwrap();
        // A file naming another socket (ssh-agent, say) is not believed.
        std::fs::write(s.join("control"), "/tmp/agent.sock").unwrap();
        assert_eq!(running_socket(&s, "control", "ctl"), None);
        let want = socket_path(&s, "ctl").unwrap();
        assert!(want.starts_with(socket_dir().unwrap()));
        std::fs::write(s.join("control"), want.to_str().unwrap()).unwrap();
        // Right name, nothing listening there.
        assert_eq!(running_socket(&s, "control", "ctl"), None);
        let _l = std::os::unix::net::UnixListener::bind(&want).unwrap();
        assert_eq!(running_socket(&s, "control", "ctl"), Some(want.clone()));
        std::fs::remove_file(&want).unwrap();
    }
}
