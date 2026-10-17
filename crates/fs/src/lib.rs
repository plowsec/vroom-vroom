//! Host side of a shared folder: a directory the guest may use, and
//! nothing outside it.
//!
//! Paths come from the guest as components below the share root. Each is
//! walked one component at a time with `openat(O_NOFOLLOW)` from an fd on
//! the root: a symlink anywhere on the way (planted by the guest, or swapped
//! in on the host while we walk) ends the walk, so nothing outside the root
//! can be reached. Names with `/`, NUL, `.` or `..` are refused. Only
//! directories and regular files are ever opened (no FIFOs, devices or
//! sockets). A read-only share refuses every change.
//!
//! All unsafe code is in the thin libc wrappers at the bottom.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

/// A path below the share root, as components (empty: the root).
pub type Rel = [OsString];

/// Longest name we accept (both host file systems allow 255 bytes).
pub const NAME_MAX: usize = 255;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub blksize: u32,
    pub atime: (i64, u32),
    pub mtime: (i64, u32),
    pub ctime: (i64, u32),
}

impl Stat {
    pub fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32
    }
    pub fn is_file(&self) -> bool {
        self.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FsStat {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
}

/// One directory entry. `kind` uses the DT_* numbering (same on Linux
/// and macOS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: OsString,
    pub ino: u64,
    pub kind: u32,
}

#[derive(Debug)]
pub struct Share {
    /// None: an empty share (a removable share with nothing in it).
    root: Option<OwnedFd>,
    readonly: bool,
}

/// What an empty share's root looks like: a read-only directory, no
/// time stamps.
const EMPTY_ROOT: Stat = Stat {
    dev: 0,
    ino: 1,
    size: 0,
    blocks: 0,
    mode: libc::S_IFDIR as u32 | 0o555,
    nlink: 2,
    uid: 0,
    gid: 0,
    blksize: 4096,
    atime: (0, 0),
    mtime: (0, 0),
    ctime: (0, 0),
};

/// Can the guest use this as one path component?
pub fn valid_name(n: &OsStr) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= NAME_MAX && b != b"." && b != b".." && !b.contains(&b'/') && !b.contains(&0)
}

fn os(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

fn cname(n: &OsStr) -> io::Result<CString> {
    if !valid_name(n) {
        return Err(os(libc::EINVAL));
    }
    CString::new(n.as_bytes()).map_err(|_| os(libc::EINVAL))
}

fn split(p: &Rel) -> Option<(&Rel, &OsStr)> {
    p.split_last().map(|(last, parent)| (parent, last.as_os_str()))
}

impl Share {
    /// `root` comes from the user's config; it may itself be a symlink.
    pub fn open(root: &Path, readonly: bool) -> io::Result<Share> {
        let c = CString::new(root.as_os_str().as_bytes()).map_err(|_| os(libc::EINVAL))?;
        let fd = sys::open_dir(&c)?;
        if !sys::fstat(fd.as_raw_fd())?.is_dir() {
            return Err(os(libc::ENOTDIR));
        }
        Ok(Share { root: Some(fd), readonly })
    }

    /// A directory someone else opened for us (the app, for a USB disk's
    /// volume): served the same way, below that fd.
    pub fn from_fd(fd: OwnedFd, readonly: bool) -> io::Result<Share> {
        if !sys::fstat(fd.as_raw_fd())?.is_dir() {
            return Err(os(libc::ENOTDIR));
        }
        Ok(Share { root: Some(fd), readonly })
    }

    /// Nothing: an empty, read-only directory (a removable share with
    /// nothing plugged in).
    pub fn empty() -> Share {
        Share { root: None, readonly: true }
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn readonly(&self) -> bool {
        self.readonly
    }

    fn root(&self) -> io::Result<&OwnedFd> {
        self.root.as_ref().ok_or_else(|| os(libc::ENOENT))
    }

    fn writable(&self) -> io::Result<()> {
        if self.readonly {
            Err(os(libc::EROFS))
        } else {
            Ok(())
        }
    }

    /// Open directory `dir`, never following a symlink.
    fn dir(&self, dir: &Rel) -> io::Result<OwnedFd> {
        let mut cur = self.root()?.try_clone()?;
        for c in dir {
            cur = sys::openat(cur.as_raw_fd(), &cname(c)?, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        }
        Ok(cur)
    }

    /// Attributes of `p` itself (a symlink is reported as one).
    pub fn stat(&self, p: &Rel) -> io::Result<Stat> {
        match split(p) {
            None => match &self.root {
                Some(r) => sys::fstat(r.as_raw_fd()),
                None => Ok(EMPTY_ROOT),
            },
            Some((parent, name)) => sys::fstatat(self.dir(parent)?.as_raw_fd(), &cname(name)?),
        }
    }

    /// Attributes of the entries `names` of directory `dir` (None: gone
    /// or not a valid name). The directory is walked to once.
    pub fn stat_children(&self, dir: &Rel, names: &[&OsStr]) -> io::Result<Vec<Option<Stat>>> {
        let d = self.dir(dir)?;
        Ok(names.iter().map(|n| cname(n).ok().and_then(|c| sys::fstatat(d.as_raw_fd(), &c).ok())).collect())
    }

    /// Open a regular file.
    pub fn open_file(&self, p: &Rel, write: bool) -> io::Result<File> {
        if write {
            self.writable()?;
        }
        let (parent, name) = split(p).ok_or_else(|| os(libc::EISDIR))?;
        let d = self.dir(parent)?;
        // O_NONBLOCK: a FIFO must not hang us before we see what it is.
        let mode = if write { libc::O_RDWR } else { libc::O_RDONLY };
        let fd = sys::openat(d.as_raw_fd(), &cname(name)?, mode | libc::O_NONBLOCK, 0)?;
        let st = sys::fstat(fd.as_raw_fd())?;
        if st.is_dir() {
            return Err(os(libc::EISDIR));
        }
        if !st.is_file() {
            return Err(os(libc::EACCES));
        }
        Ok(File::from(fd))
    }

    /// Entries of directory `p` (without `.` and `..`), at most `max`.
    pub fn read_dir(&self, p: &Rel, max: usize) -> io::Result<Vec<DirEntry>> {
        if self.root.is_none() && p.is_empty() {
            return Ok(Vec::new());
        }
        sys::read_dir(self.dir(p)?, max)
    }

    pub fn read_link(&self, p: &Rel) -> io::Result<OsString> {
        let (parent, name) = split(p).ok_or_else(|| os(libc::EINVAL))?;
        sys::readlinkat(self.dir(parent)?.as_raw_fd(), &cname(name)?)
    }

    /// Create (or with `excl` false, open) a regular file.
    pub fn create(&self, parent: &Rel, name: &OsStr, mode: u32, excl: bool) -> io::Result<File> {
        self.writable()?;
        let d = self.dir(parent)?;
        let flags = libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK | if excl { libc::O_EXCL } else { 0 };
        let fd = sys::openat(d.as_raw_fd(), &cname(name)?, flags, mode & 0o777)?;
        if !sys::fstat(fd.as_raw_fd())?.is_file() {
            return Err(os(libc::EACCES));
        }
        Ok(File::from(fd))
    }

    pub fn mkdir(&self, parent: &Rel, name: &OsStr, mode: u32) -> io::Result<()> {
        self.writable()?;
        sys::mkdirat(self.dir(parent)?.as_raw_fd(), &cname(name)?, mode & 0o777)
    }

    /// Remove a file (or an empty directory, with `dir`).
    pub fn remove(&self, parent: &Rel, name: &OsStr, dir: bool) -> io::Result<()> {
        self.writable()?;
        sys::unlinkat(self.dir(parent)?.as_raw_fd(), &cname(name)?, dir)
    }

    pub fn rename(&self, from: &Rel, from_name: &OsStr, to: &Rel, to_name: &OsStr, no_replace: bool) -> io::Result<()> {
        self.writable()?;
        let (fd, td) = (self.dir(from)?, self.dir(to)?);
        let (fname, tname) = (cname(from_name)?, cname(to_name)?);
        if no_replace {
            if let Ok(t) = sys::fstatat(td.as_raw_fd(), &tname) {
                // On a case-insensitive host "a" -> "A" finds the file
                // itself: that's a case change, not a replace.
                let same = from == to
                    && same_ignoring_case(from_name, to_name)
                    && sys::fstatat(fd.as_raw_fd(), &fname).is_ok_and(|f| (f.dev, f.ino) == (t.dev, t.ino));
                if !same {
                    return Err(os(libc::EEXIST));
                }
            }
        }
        sys::renameat(fd.as_raw_fd(), &fname, td.as_raw_fd(), &tname)
    }

    pub fn set_len(&self, p: &Rel, len: u64) -> io::Result<()> {
        self.open_file(p, true)?.set_len(len)
    }

    /// Permission bits of a file or directory (symlinks: refused).
    pub fn set_mode(&self, p: &Rel, mode: u32) -> io::Result<()> {
        self.writable()?;
        let fd = match split(p) {
            None => self.root()?.try_clone()?,
            Some((parent, name)) => {
                let d = self.dir(parent)?;
                let n = cname(name)?;
                match sys::openat(d.as_raw_fd(), &n, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
                    Ok(fd) => fd,
                    // Write-only files: try the directory form.
                    Err(_) => sys::openat(d.as_raw_fd(), &n, libc::O_RDONLY | libc::O_DIRECTORY, 0)?,
                }
            }
        };
        let st = sys::fstat(fd.as_raw_fd())?;
        if !st.is_file() && !st.is_dir() {
            return Err(os(libc::EPERM));
        }
        sys::fchmod(fd.as_raw_fd(), mode & 0o7777)
    }

    /// Access / modification times; None leaves one alone.
    pub fn set_times(&self, p: &Rel, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> io::Result<()> {
        self.writable()?;
        match split(p) {
            None => sys::futimens(self.root()?.as_raw_fd(), atime, mtime),
            Some((parent, name)) => sys::utimensat(self.dir(parent)?.as_raw_fd(), &cname(name)?, atime, mtime),
        }
    }

    pub fn statfs(&self) -> io::Result<FsStat> {
        match &self.root {
            Some(r) => sys::fstatvfs(r.as_raw_fd()),
            None => Ok(FsStat { bsize: 4096, ..Default::default() }),
        }
    }
}

fn same_ignoring_case(a: &OsStr, b: &OsStr) -> bool {
    match (a.to_str(), b.to_str()) {
        (Some(a), Some(b)) => a.to_lowercase() == b.to_lowercase(),
        _ => a.as_bytes().eq_ignore_ascii_case(b.as_bytes()),
    }
}

/// Host error -> Linux errno (what FUSE clients expect). macOS numbers
/// differ for many codes; anything unexpected becomes EIO.
pub fn linux_errno(e: &io::Error) -> i32 {
    let Some(c) = e.raw_os_error() else { return 5 };
    match c {
        libc::EPERM => 1,
        libc::ENOENT => 2,
        libc::EIO => 5,
        libc::EBADF => 9,
        libc::ENOMEM => 12,
        libc::EACCES => 13,
        libc::EBUSY => 16,
        libc::EEXIST => 17,
        libc::EXDEV => 18,
        libc::ENOTDIR => 20,
        libc::EISDIR => 21,
        libc::EINVAL => 22,
        libc::ENFILE => 23,
        libc::EMFILE => 24,
        libc::ETXTBSY => 26,
        libc::EFBIG => 27,
        libc::ENOSPC => 28,
        libc::EROFS => 30,
        libc::EMLINK => 31,
        libc::ERANGE => 34,
        libc::ENAMETOOLONG => 36,
        libc::ENOSYS => 38,
        libc::ENOTEMPTY => 39,
        libc::ELOOP => 40,
        libc::ENOTSUP => 95,
        libc::EDQUOT => 122,
        _ => 5,
    }
}

#[allow(unsafe_code)]
// Field types differ between Linux and macOS: the casts are not no-ops
// everywhere.
#[allow(clippy::unnecessary_cast)]
mod sys {
    use super::*;

    fn last() -> io::Error {
        io::Error::last_os_error()
    }

    fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
        if fd < 0 {
            return Err(last());
        }
        // SAFETY: a fresh fd returned by the kernel, owned by nobody else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn open_dir(path: &CStr) -> io::Result<OwnedFd> {
        // SAFETY: NUL-terminated path; plain flags.
        owned(unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) })
    }

    /// openat that never follows a symlink in the last component.
    pub fn openat(dir: RawFd, name: &CStr, flags: libc::c_int, mode: u32) -> io::Result<OwnedFd> {
        let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: valid dir fd and NUL-terminated name; mode is only read
        // with O_CREAT.
        owned(unsafe { libc::openat(dir, name.as_ptr(), flags, mode as libc::c_uint) })
    }

    fn conv(s: &libc::stat) -> Stat {
        let (a, m, c) = ((s.st_atime, s.st_atime_nsec), (s.st_mtime, s.st_mtime_nsec), (s.st_ctime, s.st_ctime_nsec));
        Stat {
            dev: s.st_dev as u64,
            ino: s.st_ino as u64,
            size: s.st_size as u64,
            blocks: s.st_blocks as u64,
            mode: s.st_mode as u32,
            nlink: s.st_nlink as u32,
            uid: s.st_uid,
            gid: s.st_gid,
            blksize: s.st_blksize as u32,
            atime: (a.0 as i64, a.1 as u32),
            mtime: (m.0 as i64, m.1 as u32),
            ctime: (c.0 as i64, c.1 as u32),
        }
    }

    pub fn fstat(fd: RawFd) -> io::Result<Stat> {
        // SAFETY: zeroed stat is valid plain data; fd is valid.
        let mut s: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd and out pointer.
        if unsafe { libc::fstat(fd, &mut s) } != 0 {
            return Err(last());
        }
        Ok(conv(&s))
    }

    pub fn fstatat(dir: RawFd, name: &CStr) -> io::Result<Stat> {
        // SAFETY: zeroed stat is valid plain data.
        let mut s: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid dir fd, NUL-terminated name, out pointer.
        if unsafe { libc::fstatat(dir, name.as_ptr(), &mut s, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(last());
        }
        Ok(conv(&s))
    }

    pub fn read_dir(dir: OwnedFd, max: usize) -> io::Result<Vec<DirEntry>> {
        // The root's fd is a dup of the share's: it shares the read
        // position with every earlier listing. Start from the top.
        // SAFETY: valid fd.
        if unsafe { libc::lseek(dir.as_raw_fd(), 0, libc::SEEK_SET) } < 0 {
            return Err(last());
        }
        let raw = dir.into_raw_fd();
        // SAFETY: fdopendir takes ownership of `raw` on success.
        let d = unsafe { libc::fdopendir(raw) };
        if d.is_null() {
            let e = last();
            // SAFETY: still ours: fdopendir failed.
            unsafe { libc::close(raw) };
            return Err(e);
        }
        let mut out = Vec::new();
        while out.len() < max {
            // SAFETY: valid DIR*; the returned entry stays valid until the
            // next readdir/closedir on it, and we copy what we need now.
            let e = unsafe { libc::readdir(d) };
            if e.is_null() {
                break;
            }
            // SAFETY: d_name is NUL-terminated within the entry.
            let (name, ino, kind) = unsafe { (CStr::from_ptr((*e).d_name.as_ptr()), (*e).d_ino as u64, (*e).d_type as u32) };
            let name = OsStr::from_bytes(name.to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            out.push(DirEntry { name: name.to_os_string(), ino, kind });
        }
        // SAFETY: valid DIR*, closed once (closes the fd too).
        unsafe { libc::closedir(d) };
        Ok(out)
    }

    pub fn readlinkat(dir: RawFd, name: &CStr) -> io::Result<OsString> {
        let mut buf = vec![0u8; 4096];
        // SAFETY: valid dir fd, name, and a buffer of buf.len() bytes.
        let n = unsafe { libc::readlinkat(dir, name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(last());
        }
        buf.truncate(n as usize);
        Ok(OsString::from_vec(buf))
    }

    pub fn mkdirat(dir: RawFd, name: &CStr, mode: u32) -> io::Result<()> {
        // SAFETY: valid dir fd and NUL-terminated name.
        if unsafe { libc::mkdirat(dir, name.as_ptr(), mode as libc::mode_t) } != 0 {
            return Err(last());
        }
        Ok(())
    }

    pub fn unlinkat(dir: RawFd, name: &CStr, is_dir: bool) -> io::Result<()> {
        let flags = if is_dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: valid dir fd and NUL-terminated name.
        if unsafe { libc::unlinkat(dir, name.as_ptr(), flags) } != 0 {
            return Err(last());
        }
        Ok(())
    }

    pub fn renameat(fd: RawFd, from: &CStr, td: RawFd, to: &CStr) -> io::Result<()> {
        // SAFETY: valid dir fds and NUL-terminated names.
        if unsafe { libc::renameat(fd, from.as_ptr(), td, to.as_ptr()) } != 0 {
            return Err(last());
        }
        Ok(())
    }

    pub fn fchmod(fd: RawFd, mode: u32) -> io::Result<()> {
        // SAFETY: valid fd.
        if unsafe { libc::fchmod(fd, mode as libc::mode_t) } != 0 {
            return Err(last());
        }
        Ok(())
    }

    fn times(a: Option<(i64, u32)>, m: Option<(i64, u32)>) -> [libc::timespec; 2] {
        let t = |v: Option<(i64, u32)>| match v {
            Some((s, ns)) => libc::timespec { tv_sec: s as _, tv_nsec: ns.min(999_999_999) as _ },
            None => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT as _ },
        };
        [t(a), t(m)]
    }

    pub fn utimensat(dir: RawFd, name: &CStr, a: Option<(i64, u32)>, m: Option<(i64, u32)>) -> io::Result<()> {
        let ts = times(a, m);
        // SAFETY: valid dir fd, name and a 2-element timespec array.
        if unsafe { libc::utimensat(dir, name.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(last());
        }
        Ok(())
    }

    pub fn futimens(fd: RawFd, a: Option<(i64, u32)>, m: Option<(i64, u32)>) -> io::Result<()> {
        let ts = times(a, m);
        // SAFETY: valid fd and a 2-element timespec array.
        if unsafe { libc::futimens(fd, ts.as_ptr()) } != 0 {
            return Err(last());
        }
        Ok(())
    }

    pub fn fstatvfs(fd: RawFd) -> io::Result<FsStat> {
        // SAFETY: zeroed statvfs is valid plain data.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd and out pointer.
        if unsafe { libc::fstatvfs(fd, &mut s) } != 0 {
            return Err(last());
        }
        Ok(FsStat {
            blocks: s.f_blocks as u64,
            bfree: s.f_bfree as u64,
            bavail: s.f_bavail as u64,
            files: s.f_files as u64,
            ffree: s.f_ffree as u64,
            bsize: s.f_frsize as u32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn p(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let share = td.path().join("share");
        std::fs::create_dir_all(share.join("sub")).unwrap();
        std::fs::write(share.join("sub/hello.txt"), b"hi").unwrap();
        std::fs::write(td.path().join("secret.txt"), b"outside").unwrap();
        // Escape attempts planted inside the share.
        std::os::unix::fs::symlink(td.path().join("secret.txt"), share.join("link-file")).unwrap();
        std::os::unix::fs::symlink(td.path(), share.join("link-dir")).unwrap();
        (td, share)
    }

    #[test]
    fn reads_inside_and_never_follows_symlinks() {
        let (_td, root) = fixture();
        let s = Share::open(&root, true).unwrap();
        let mut b = String::new();
        s.open_file(&p(&["sub", "hello.txt"]), false).unwrap().read_to_string(&mut b).unwrap();
        assert_eq!(b, "hi");
        // Symlinks are visible as symlinks, never followed.
        assert!(s.open_file(&p(&["link-file"]), false).is_err());
        assert!(s.stat(&p(&["link-dir", "secret.txt"])).is_err());
        assert!(s.read_dir(&p(&["link-dir"]), 100).is_err());
        assert_eq!(s.stat(&p(&["link-file"])).unwrap().mode & libc::S_IFMT as u32, libc::S_IFLNK as u32);
        assert!(s.read_link(&p(&["link-file"])).unwrap().as_bytes().ends_with(b"secret.txt"));
        // Bad names never reach the host.
        for bad in ["..", ".", "", "a/b"] {
            assert!(s.stat(&p(&[bad])).is_err(), "{bad:?}");
        }
        let mut names: Vec<_> = s.read_dir(&p(&[]), 100).unwrap().into_iter().map(|e| e.name).collect();
        names.sort();
        assert_eq!(names, p(&["link-dir", "link-file", "sub"]));
    }

    #[test]
    fn from_an_fd_and_empty() {
        let (_td, root) = fixture();
        let fd = OwnedFd::from(File::open(&root).unwrap());
        let s = Share::from_fd(fd, true).unwrap();
        assert_eq!(s.stat(&p(&["sub", "hello.txt"])).unwrap().size, 2);
        assert!(s.stat(&p(&["link-dir", "secret.txt"])).is_err());
        assert!(Share::from_fd(OwnedFd::from(File::open(root.join("sub/hello.txt")).unwrap()), true).is_err(), "not a directory");
        let e = Share::empty();
        assert!(e.is_empty() && e.readonly());
        assert!(e.stat(&p(&[])).unwrap().is_dir());
        assert!(e.read_dir(&p(&[]), 10).unwrap().is_empty());
        assert_eq!(e.stat(&p(&["x"])).unwrap_err().raw_os_error(), Some(libc::ENOENT));
        assert!(e.open_file(&p(&["x"]), false).is_err());
        assert_eq!(e.mkdir(&p(&[]), OsStr::new("x"), 0o755).unwrap_err().raw_os_error(), Some(libc::EROFS));
        assert!(e.statfs().is_ok());
    }

    #[test]
    fn read_only_refuses_changes() {
        let (_td, root) = fixture();
        let s = Share::open(&root, true).unwrap();
        let ro = |r: io::Result<()>| assert_eq!(r.unwrap_err().raw_os_error(), Some(libc::EROFS));
        ro(s.mkdir(&p(&[]), OsStr::new("x"), 0o755));
        ro(s.remove(&p(&["sub"]), OsStr::new("hello.txt"), false));
        ro(s.create(&p(&[]), OsStr::new("n"), 0o644, true).map(|_| ()));
        ro(s.open_file(&p(&["sub", "hello.txt"]), true).map(|_| ()));
        ro(s.rename(&p(&["sub"]), OsStr::new("hello.txt"), &p(&[]), OsStr::new("y"), false));
    }

    #[test]
    fn read_write_stays_inside() {
        let (td, root) = fixture();
        let s = Share::open(&root, false).unwrap();
        s.mkdir(&p(&[]), OsStr::new("new"), 0o755).unwrap();
        let mut f = s.create(&p(&["new"]), OsStr::new("f.txt"), 0o644, true).unwrap();
        f.write_all(b"data").unwrap();
        s.rename(&p(&["new"]), OsStr::new("f.txt"), &p(&[]), OsStr::new("g.txt"), false).unwrap();
        assert_eq!(std::fs::read(root.join("g.txt")).unwrap(), b"data");
        s.set_len(&p(&["g.txt"]), 2).unwrap();
        s.set_mode(&p(&["g.txt"]), 0o600).unwrap();
        assert_eq!(s.stat(&p(&["g.txt"])).unwrap().size, 2);
        s.remove(&p(&[]), OsStr::new("g.txt"), false).unwrap();
        s.remove(&p(&[]), OsStr::new("new"), true).unwrap();
        // Writing "through" a symlink creates nothing outside and changes nothing.
        assert!(s.create(&p(&["link-dir"]), OsStr::new("evil"), 0o644, true).is_err());
        assert!(s.open_file(&p(&["link-file"]), true).is_err());
        assert!(s.set_mode(&p(&["link-file"]), 0o777).is_err());
        assert!(!td.path().join("evil").exists());
        assert_eq!(std::fs::read(td.path().join("secret.txt")).unwrap(), b"outside");
        // Replacing an existing name with no_replace.
        s.create(&p(&[]), OsStr::new("a"), 0o644, true).unwrap();
        s.create(&p(&[]), OsStr::new("b"), 0o644, true).unwrap();
        assert!(s.rename(&p(&[]), OsStr::new("a"), &p(&[]), OsStr::new("b"), true).is_err());
    }

    #[test]
    fn fifos_are_not_opened() {
        let (_td, root) = fixture();
        let c = CString::new(root.join("fifo").as_os_str().as_bytes()).unwrap();
        #[allow(unsafe_code)]
        // SAFETY: NUL-terminated path.
        let r = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
        assert_eq!(r, 0);
        let s = Share::open(&root, false).unwrap();
        // Must return, not block, and must refuse.
        assert!(s.open_file(&p(&["fifo"]), false).is_err());
    }

    #[test]
    fn listing_twice_lists_twice() {
        let (_td, root) = fixture();
        let s = Share::open(&root, true).unwrap();
        // The root fd is shared: a second listing must not start at its end.
        assert_eq!(s.read_dir(&p(&[]), 100).unwrap().len(), 3);
        assert_eq!(s.read_dir(&p(&[]), 100).unwrap().len(), 3);
        let st = s.stat_children(&p(&["sub"]), &[OsStr::new("hello.txt"), OsStr::new("gone"), OsStr::new("..")]).unwrap();
        assert_eq!(st[0].map(|s| s.size), Some(2));
        assert!(st[1].is_none() && st[2].is_none());
    }

    #[test]
    fn rename_no_replace_allows_case_changes_only() {
        let (_td, root) = fixture();
        let s = Share::open(&root, false).unwrap();
        s.create(&p(&[]), OsStr::new("a.txt"), 0o644, true).unwrap();
        // Case change: fine on case-insensitive hosts (the target "exists")
        // and on case-sensitive ones (it doesn't).
        s.rename(&p(&[]), OsStr::new("a.txt"), &p(&[]), OsStr::new("A.TXT"), true).unwrap();
        assert!(s.read_dir(&p(&[]), 100).unwrap().iter().any(|e| e.name == "A.TXT"));
        // Hard links to the same file under other names are still refused.
        std::fs::hard_link(root.join("A.TXT"), root.join("b.txt")).unwrap();
        let e = s.rename(&p(&[]), OsStr::new("A.TXT"), &p(&[]), OsStr::new("b.txt"), true).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EEXIST));
        // Same name, other directory: refused.
        s.create(&p(&["sub"]), OsStr::new("a.txt"), 0o644, true).unwrap();
        let e = s.rename(&p(&[]), OsStr::new("A.TXT"), &p(&["sub"]), OsStr::new("a.txt"), true).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EEXIST));
    }

    #[test]
    fn errnos_are_linux_numbers() {
        assert_eq!(linux_errno(&os(libc::ENOTEMPTY)), 39);
        assert_eq!(linux_errno(&os(libc::ENAMETOOLONG)), 36);
        assert_eq!(linux_errno(&io::Error::other("x")), 5);
    }
}
