//! Files the host puts in the guest and gets from it (`FilePut`,
//! `FileGet`), as the agent's user. The disk work runs on threads of
//! their own; a file going out waits for each piece to be written to the
//! host (the link sets the pace, nothing piles up).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use vroom_proto::{Message, FILE_CHUNK};

/// Writes a frame to the host, once it's out.
pub type Send = Arc<dyn Fn(Message) -> io::Result<()> + std::marker::Send + Sync>;

/// A file coming in that stops this long is given up.
const IDLE: Duration = if cfg!(test) { Duration::from_millis(500) } else { Duration::from_secs(60) };

#[derive(Default)]
pub struct Files {
    /// Files coming in: their writer, and the bytes still to come.
    puts: HashMap<u32, (SyncSender<Vec<u8>>, u64)>,
}

impl std::fmt::Debug for Files {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Files").field("puts", &self.puts.len()).finish()
    }
}

fn failed(id: u32, why: String) -> Message {
    Message::FileDone { id, ok: false, size: 0, sha256: [0; 32], why }
}

impl Files {
    /// A `FilePut`, `FileGet` or `FileData` from the host.
    pub fn handle(&mut self, m: Message, send: &Send) {
        match m {
            Message::FilePut { id, size, path } => {
                log::info!("file {id}: {path} from the host, {size} bytes");
                let (tx, rx) = sync_channel::<Vec<u8>>(16);
                let send = send.clone();
                std::thread::spawn(move || {
                    let r = match receive(&path, size, |t| rx.recv_timeout(t)) {
                        Ok(sha256) => Message::FileDone { id, ok: true, size, sha256, why: String::new() },
                        Err(e) => failed(id, format!("{path}: {e}")),
                    };
                    let _ = send(r);
                });
                // Nothing to wait for: the writer finishes alone.
                if size > 0 {
                    self.puts.insert(id, (tx, size));
                }
            }
            Message::FileData { id, data } => {
                let Some((tx, left)) = self.puts.get_mut(&id) else { return };
                let n = data.len() as u64;
                // More than it said: the writer, cut off, says so.
                if n > *left || tx.send(data).is_err() {
                    self.puts.remove(&id);
                    return;
                }
                *left -= n;
                if *left == 0 {
                    self.puts.remove(&id);
                }
            }
            Message::FileGet { id, path } => {
                log::info!("file {id}: {path} to the host");
                let send = send.clone();
                std::thread::spawn(move || {
                    if let Err(e) = give(id, &path, &send) {
                        let _ = send(failed(id, format!("{path}: {e}")));
                    }
                });
            }
            _ => {}
        }
    }
}

/// Write `size` bytes from `next` to `path` (through a file beside it,
/// so a file cut short never replaces one). Their SHA-256.
///
/// We run as root/SYSTEM, often in folders a guest user may write: the
/// folders on the way are opened one by one without following links
/// (`at`), and the part file is made, renamed and removed relative to its
/// folder's handle, so no folder swapped for a link meanwhile sends the
/// write elsewhere. A part file there already (or a link planted as one)
/// is removed, never written through; the new one is made new,
/// owner-only.
fn receive(path: &str, size: u64, next: impl Fn(Duration) -> Result<Vec<u8>, RecvTimeoutError>) -> io::Result<[u8; 32]> {
    let (dir, name) = at::Dir::parent_of(std::path::Path::new(path))?;
    let mut part = name.clone();
    part.push(".vroom-part");
    let mut f = dir.create_part(&part)?;
    let r = (|| {
        let mut h = Sha256::new();
        let mut got = 0u64;
        while got < size {
            let b = next(IDLE).map_err(|e| match e {
                RecvTimeoutError::Timeout => io::Error::other("the host stopped sending it"),
                RecvTimeoutError::Disconnected => io::Error::other("the host sent more or less than it said"),
            })?;
            f.write_all(&b)?;
            h.update(&b);
            got += b.len() as u64;
        }
        f.flush()?;
        // Whole: readable like any file root makes (umask 022).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        }
        dir.rename(&f, &part, &name)?;
        Ok(h.finalize().into())
    })();
    // Only ours: one we couldn't make isn't ours to remove.
    if r.is_err() {
        dir.discard(f, &part);
    }
    r
}

/// Unix: folders opened with `openat(O_NOFOLLOW)`, one at a time.
///
/// A link on the way is refused, unless the user can't have planted it:
/// the link and the folder holding it are root's, and that folder is not
/// group- or world-writable (system links like `/var/run`, `/tmp` or
/// `/home` on some distros; `/var` on macOS). Only root could have made
/// or replaced such a link, so following it (ourselves, at most
/// `MAX_LINKS`) goes where root meant.
#[cfg(unix)]
#[allow(unsafe_code)]
mod at {
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Path};

    const MAX_LINKS: usize = 8;

    /// Whether to follow a link owned by `link_uid` in a folder owned by
    /// `dir_uid`, writable by group or others (`shared`) or not.
    pub fn trusted(link_uid: u32, dir_uid: u32, shared: bool) -> bool {
        link_uid == 0 && dir_uid == 0 && !shared
    }

    pub struct Dir(OwnedFd);

    fn cstr(s: &OsStr) -> io::Result<CString> {
        CString::new(s.as_bytes()).map_err(|_| io::Error::other("a NUL in the path"))
    }

    fn last(what: &OsStr) -> io::Error {
        let e = io::Error::last_os_error();
        io::Error::new(e.kind(), format!("{}: {e}", what.to_string_lossy()))
    }

    fn fd(r: libc::c_int, what: &OsStr) -> io::Result<OwnedFd> {
        if r < 0 {
            return Err(last(what));
        }
        // SAFETY: a new fd, ours alone.
        Ok(unsafe { OwnedFd::from_raw_fd(r) })
    }

    fn open_dir(at: libc::c_int, name: &CStr, what: &OsStr) -> io::Result<OwnedFd> {
        // SAFETY: valid fd (or AT_FDCWD) and NUL-terminated name.
        fd(unsafe { libc::openat(at, name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) }, what)
    }

    fn lstat_at(dir: &OwnedFd, name: &CStr) -> Option<libc::stat> {
        // SAFETY: zeroed stat is valid plain data; fstatat fills it.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd, name and out pointer.
        (unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0).then_some(st)
    }

    fn stat(dir: &OwnedFd) -> io::Result<libc::stat> {
        // SAFETY: zeroed stat is valid plain data; fstat fills it.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd and out pointer.
        if unsafe { libc::fstat(dir.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    fn read_link(dir: &OwnedFd, name: &CStr, what: &OsStr) -> io::Result<OsString> {
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: valid fd and name; buf is writable for its length.
        let n = unsafe { libc::readlinkat(dir.as_raw_fd(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(last(what));
        }
        let n = n as usize;
        if n >= buf.len() {
            return Err(io::Error::other(format!("{}: link too long", what.to_string_lossy())));
        }
        buf.truncate(n);
        Ok(OsString::from_vec(buf))
    }

    /// Path pieces, last first (a work list): `/` for the root.
    fn pieces(p: &Path, out: &mut Vec<OsString>) {
        let mut v: Vec<OsString> = p
            .components()
            .filter_map(|c| match c {
                Component::RootDir => Some("/".into()),
                Component::ParentDir => Some("..".into()),
                Component::Normal(n) => Some(n.to_os_string()),
                Component::CurDir | Component::Prefix(_) => None,
            })
            .collect();
        v.reverse();
        out.extend(v);
    }

    impl Dir {
        /// The folder `path` is in, opened without following a link the
        /// user may have planted, and the file's name in it.
        pub fn parent_of(path: &Path) -> io::Result<(Dir, OsString)> {
            let mut todo = Vec::new();
            pieces(path, &mut todo);
            let name = match todo.first() {
                Some(n) if n != "/" && n != ".." => n.clone(),
                _ => return Err(io::Error::other("no file name")),
            };
            todo.remove(0);
            let mut dir = open_dir(libc::AT_FDCWD, c".", OsStr::new("."))?;
            let mut links = 0;
            while let Some(c) = todo.pop() {
                if c == "/" {
                    dir = open_dir(libc::AT_FDCWD, c"/", &c)?;
                    continue;
                }
                let cn = cstr(&c)?;
                let e = match open_dir(dir.as_raw_fd(), &cn, &c) {
                    Ok(d) => {
                        dir = d;
                        continue;
                    }
                    Err(e) => e,
                };
                // A link? Followed only if root's, in a folder only root writes.
                let Some(st) = lstat_at(&dir, &cn).filter(|st| st.st_mode & libc::S_IFMT == libc::S_IFLNK) else { return Err(e) };
                let d = stat(&dir)?;
                if !trusted(st.st_uid, d.st_uid, d.st_mode & 0o022 != 0) {
                    return Err(io::Error::other(format!("{}: a link, not followed", c.to_string_lossy())));
                }
                links += 1;
                if links > MAX_LINKS {
                    return Err(io::Error::other(format!("{}: too many links", c.to_string_lossy())));
                }
                pieces(Path::new(&read_link(&dir, &cn, &c)?), &mut todo);
            }
            Ok((Dir(dir), name))
        }

        /// `part`, new, owner-only; whatever was there (not a folder) removed.
        pub fn create_part(&self, part: &OsStr) -> io::Result<std::fs::File> {
            let p = cstr(part)?;
            if let Some(st) = lstat_at(&self.0, &p) {
                if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
                    return Err(io::Error::other(format!("{} is a folder", part.to_string_lossy())));
                }
                // SAFETY: valid fd and name.
                if unsafe { libc::unlinkat(self.0.as_raw_fd(), p.as_ptr(), 0) } != 0 {
                    return Err(last(part));
                }
            }
            let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            // SAFETY: valid fd and name; mode for O_CREAT.
            let f = fd(unsafe { libc::openat(self.0.as_raw_fd(), p.as_ptr(), flags, 0o600 as libc::c_uint) }, part)?;
            Ok(f.into())
        }

        pub fn rename(&self, _f: &std::fs::File, part: &OsStr, name: &OsStr) -> io::Result<()> {
            let (p, n) = (cstr(part)?, cstr(name)?);
            let d = self.0.as_raw_fd();
            // SAFETY: valid fds and names.
            if unsafe { libc::renameat(d, p.as_ptr(), d, n.as_ptr()) } != 0 {
                return Err(last(name));
            }
            Ok(())
        }

        pub fn discard(&self, f: std::fs::File, part: &OsStr) {
            drop(f);
            if let Ok(p) = cstr(part) {
                // SAFETY: valid fd and name.
                unsafe { libc::unlinkat(self.0.as_raw_fd(), p.as_ptr(), 0) };
            }
        }
    }
}

/// Windows: folders opened one by one relative to the last one's handle
/// (`NtCreateFile` with a root directory and `FILE_OPEN_REPARSE_POINT`),
/// the part file made and renamed relative to its folder's handle. Each
/// step names one entry in a folder we hold: nothing is looked up by
/// path again, so a folder swapped for a junction meanwhile changes
/// nothing. Links on the way (symlinks, junctions: name surrogates) are
/// refused, all of them, system ones too: telling one users can't have
/// made would need the folder's ACL judged for every user. Other
/// reparse points (OneDrive, dedup) are the folder itself: walked.
/// `..` is taken off by name, as Windows does.
#[cfg(windows)]
#[allow(unsafe_code)]
mod at {
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Component, Path};

    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        NtCreateFile, FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    };
    use windows_sys::Win32::Foundation::{RtlNtStatusToDosError, HANDLE, UNICODE_STRING};
    use windows_sys::Win32::Storage::FileSystem::{
        FileAttributeTagInfo, FileDispositionInfo, FileRenameInfo, GetFileInformationByHandleEx, SetFileInformationByHandle, DELETE,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    const OBJ_CASE_INSENSITIVE: u32 = 0x40;
    /// Symlinks, junctions: reparse points that name another place.
    const NAME_SURROGATE: u32 = 0x2000_0000;
    const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

    pub struct Dir(OwnedHandle);

    /// Entry `name` of folder `root`, never through a reparse point.
    fn nt_open(root: HANDLE, name: &OsStr, access: u32, share: u32, disposition: u32, options: u32) -> io::Result<OwnedHandle> {
        let mut w: Vec<u16> = name.encode_wide().collect();
        let len = u16::try_from(w.len() * 2).map_err(|_| io::Error::other("name too long"))?;
        let us = UNICODE_STRING { Length: len, MaximumLength: len, Buffer: w.as_mut_ptr() };
        let oa = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: root,
            ObjectName: &us,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut h: HANDLE = std::ptr::null_mut();
        // SAFETY: zeroed IO_STATUS_BLOCK is valid plain data.
        let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        // SAFETY: valid out pointers; `oa`, `us` and `w` outlive the call.
        let st = unsafe {
            NtCreateFile(
                &mut h,
                access | SYNCHRONIZE,
                &oa,
                &mut iosb,
                std::ptr::null(),
                FILE_ATTRIBUTE_NORMAL,
                share,
                disposition,
                options | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
                std::ptr::null(),
                0,
            )
        };
        if st < 0 {
            // SAFETY: plain call.
            let e = io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(st) } as i32);
            return Err(io::Error::new(e.kind(), format!("{}: {e}", name.to_string_lossy())));
        }
        // SAFETY: a new handle, ours alone.
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    fn tag(h: &OwnedHandle) -> io::Result<FILE_ATTRIBUTE_TAG_INFO> {
        let mut t = FILE_ATTRIBUTE_TAG_INFO { FileAttributes: 0, ReparseTag: 0 };
        // SAFETY: valid handle; out buffer of the size given.
        if unsafe { GetFileInformationByHandleEx(h.as_raw_handle(), FileAttributeTagInfo, (&mut t as *mut FILE_ATTRIBUTE_TAG_INFO).cast(), std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(t)
    }

    fn delete(h: HANDLE) -> io::Result<()> {
        let d = FILE_DISPOSITION_INFO { DeleteFile: 1 };
        // SAFETY: valid handle opened with DELETE; in buffer of the size given.
        if unsafe { SetFileInformationByHandle(h, FileDispositionInfo, (&d as *const FILE_DISPOSITION_INFO).cast(), std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    impl Dir {
        pub fn parent_of(path: &Path) -> io::Result<(Dir, OsString)> {
            let mut comps: Vec<Component> = path.components().collect();
            let name = match comps.pop() {
                Some(Component::Normal(n)) => n.to_os_string(),
                _ => return Err(io::Error::other("no file name")),
            };
            // Where to start: the drive or share's root ("C:" "\\"), or ours.
            let skip = comps.iter().take_while(|c| matches!(c, Component::Prefix(_) | Component::RootDir)).count();
            let (head, rest) = comps.split_at(skip);
            let start: OsString = if head.is_empty() {
                ".".into()
            } else if !head.contains(&Component::RootDir) {
                return Err(io::Error::other("a drive without a folder (C:x)"));
            } else {
                head.iter().map(|c| c.as_os_str()).collect()
            };
            let root = std::fs::OpenOptions::new()
                .access_mode(FILE_LIST_DIRECTORY | FILE_TRAVERSE | SYNCHRONIZE)
                .share_mode(SHARE_ALL)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(&start)
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", start.to_string_lossy())))?;
            let mut stack = vec![OwnedHandle::from(root)];
            for c in rest {
                match c {
                    Component::Normal(n) => {
                        let Some(top) = stack.last() else { return Err(io::Error::other("no folder")) };
                        let h = nt_open(top.as_raw_handle(), n, FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, FILE_DIRECTORY_FILE)?;
                        let t = tag(&h)?;
                        if t.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && t.ReparseTag & NAME_SURROGATE != 0 {
                            return Err(io::Error::other(format!("{}: a link, not followed", n.to_string_lossy())));
                        }
                        stack.push(h);
                    }
                    Component::ParentDir if stack.len() > 1 => {
                        stack.pop();
                    }
                    _ => {}
                }
            }
            let dir = stack.pop().ok_or_else(|| io::Error::other("no folder"))?;
            Ok((Dir(dir), name))
        }

        pub fn create_part(&self, part: &OsStr) -> io::Result<std::fs::File> {
            let d = self.0.as_raw_handle();
            match nt_open(d, part, DELETE | FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, 0) {
                Ok(h) => {
                    let t = tag(&h)?;
                    if t.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 && t.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                        return Err(io::Error::other(format!("{} is a folder", part.to_string_lossy())));
                    }
                    delete(h.as_raw_handle())?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            // Not shared: nobody opens, renames or removes it under us.
            let h = nt_open(d, part, FILE_GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES, 0, FILE_CREATE, FILE_NON_DIRECTORY_FILE)?;
            Ok(h.into())
        }

        /// `f` (the part file) renamed to `name` in this folder, by handle.
        pub fn rename(&self, f: &std::fs::File, _part: &OsStr, name: &OsStr) -> io::Result<()> {
            let w: Vec<u16> = name.encode_wide().collect();
            let off = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
            let size = off + (w.len() + 1) * 2;
            let bytes = u32::try_from(w.len() * 2).map_err(|_| io::Error::other("name too long"))?;
            // u64s: aligned for FILE_RENAME_INFO; zeroed, so NUL-terminated.
            let mut buf = vec![0u64; size.div_ceil(8)];
            let p = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            // SAFETY: buf is aligned and big enough for the struct and the name.
            unsafe {
                (*p).Anonymous.ReplaceIfExists = 1;
                (*p).RootDirectory = self.0.as_raw_handle();
                (*p).FileNameLength = bytes;
                std::ptr::copy_nonoverlapping(w.as_ptr(), p.cast::<u8>().add(off).cast::<u16>(), w.len());
            }
            // SAFETY: valid handle opened with DELETE; buffer of the size given.
            if unsafe { SetFileInformationByHandle(f.as_raw_handle(), FileRenameInfo, p.cast(), size as u32) } == 0 {
                let e = io::Error::last_os_error();
                return Err(io::Error::new(e.kind(), format!("{}: {e}", name.to_string_lossy())));
            }
            Ok(())
        }

        pub fn discard(&self, f: std::fs::File, _part: &OsStr) {
            let _ = delete(f.as_raw_handle());
        }
    }
}

/// Send the file at `path`, then its size and SHA-256.
fn give(id: u32, path: &str, send: &Send) -> io::Result<()> {
    let mut f = std::fs::File::open(path)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::other("not a file"));
    }
    let mut h = Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; FILE_CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        size += n as u64;
        // The host gone: stop, nobody to tell.
        if send(Message::FileData { id, data: buf[..n].to_vec() }).is_err() {
            return Ok(());
        }
    }
    let _ = send(Message::FileDone { id, ok: true, size, sha256: h.finalize().into(), why: String::new() });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::path::Path;
    use std::sync::Mutex;

    fn sink() -> (Send, Arc<Mutex<Vec<Message>>>) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        (Arc::new(move |m| {
            g.lock().unwrap().push(m);
            Ok(())
        }), got)
    }

    fn wait_done(got: &Arc<Mutex<Vec<Message>>>) -> Message {
        for _ in 0..400 {
            if let Some(m) = got.lock().unwrap().iter().find(|m| matches!(m, Message::FileDone { .. })) {
                return m.clone();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no answer");
    }

    #[test]
    fn a_file_comes_in_whole_or_not_at_all() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("a.bin").to_string_lossy().into_owned();
        let data: Vec<u8> = (0..FILE_CHUNK * 2 + 5).map(|i| i as u8).collect();
        let (send, got) = sink();
        let mut f = Files::default();
        f.handle(Message::FilePut { id: 1, size: data.len() as u64, path: path.clone() }, &send);
        for c in data.chunks(FILE_CHUNK) {
            f.handle(Message::FileData { id: 1, data: c.to_vec() }, &send);
        }
        let sha256: [u8; 32] = Sha256::digest(&data).into();
        assert_eq!(wait_done(&got), Message::FileDone { id: 1, ok: true, size: data.len() as u64, sha256, why: String::new() });
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert!(f.puts.is_empty());
        // Too much: refused, the old file stays.
        let (send, got) = sink();
        f.handle(Message::FilePut { id: 2, size: 2, path: path.clone() }, &send);
        f.handle(Message::FileData { id: 2, data: vec![1, 2, 3] }, &send);
        assert!(matches!(wait_done(&got), Message::FileDone { id: 2, ok: false, .. }));
        // Too little: given up after a while.
        let (send, got) = sink();
        f.handle(Message::FilePut { id: 3, size: 2, path: path.clone() }, &send);
        f.handle(Message::FileData { id: 3, data: vec![1] }, &send);
        assert!(matches!(wait_done(&got), Message::FileDone { id: 3, ok: false, .. }));
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 1, "no parts left");
        // Empty.
        let (send, got) = sink();
        f.handle(Message::FilePut { id: 4, size: 0, path: path.clone() }, &send);
        assert!(matches!(wait_done(&got), Message::FileDone { id: 4, ok: true, size: 0, .. }));
        assert!(std::fs::read(&path).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_part_file_is_never_written_through() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let victim = t.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let send = |path: &str, data: &[u8]| {
            let (tx, rx) = sync_channel::<Vec<u8>>(1);
            tx.send(data.to_vec()).unwrap();
            receive(path, data.len() as u64, |d| rx.recv_timeout(d))
        };
        let links: [fn(&Path, &Path) -> io::Result<()>; 2] = [|a, b| std::os::unix::fs::symlink(a, b), |a, b| std::fs::hard_link(a, b)];
        for (i, link) in links.into_iter().enumerate() {
            let path = t.path().join(format!("a{i}.bin")).to_string_lossy().into_owned();
            link(&victim, Path::new(&format!("{path}.vroom-part"))).unwrap();
            send(&path, b"new").unwrap();
            assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
            assert_eq!(std::fs::read(&path).unwrap(), b"new");
            let m = std::fs::symlink_metadata(&path).unwrap();
            assert!(m.is_file() && m.permissions().mode() & 0o777 == 0o644);
            assert!(!Path::new(&format!("{path}.vroom-part")).exists());
        }
        // A folder there: refused, left alone.
        let path = t.path().join("c.bin").to_string_lossy().into_owned();
        std::fs::create_dir(format!("{path}.vroom-part")).unwrap();
        assert!(send(&path, b"x").is_err());
        assert!(Path::new(&format!("{path}.vroom-part")).is_dir() && !Path::new(&path).exists());
    }

    #[cfg(unix)]
    fn put(path: &Path, data: &[u8]) -> io::Result<[u8; 32]> {
        let (tx, rx) = sync_channel::<Vec<u8>>(1);
        tx.send(data.to_vec()).unwrap();
        receive(&path.to_string_lossy(), data.len() as u64, |d| rx.recv_timeout(d))
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_folder_on_the_way_is_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real");
        std::fs::create_dir_all(t.path().join("d")).unwrap();
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, t.path().join("link")).unwrap();
        std::os::unix::fs::symlink(&real, t.path().join("d/link")).unwrap();
        std::os::unix::fs::symlink("../real", t.path().join("d/rel")).unwrap();
        for p in ["link/a.bin", "d/link/a.bin", "d/rel/a.bin", "d/rel/../d/a.bin"] {
            let e = put(&t.path().join(p), b"x").unwrap_err();
            assert!(e.to_string().contains("a link, not followed"), "{p}: {e}");
        }
        assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0, "nothing written through");
        assert!(!t.path().join("d/a.bin").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_nested_path_still_works() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("x/y")).unwrap();
        for (p, data) in [("x/y/z.bin", b"one"), ("x/./y/../y/w.bin", b"two")] {
            put(&t.path().join(p), data).unwrap();
        }
        assert_eq!(std::fs::read(t.path().join("x/y/z.bin")).unwrap(), b"one");
        assert_eq!(std::fs::read(t.path().join("x/y/w.bin")).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(t.path().join("x/y")).unwrap().count(), 2, "no parts left");
        // A missing folder: an error naming it.
        let e = put(&t.path().join("x/nope/a.bin"), b"x").unwrap_err();
        assert!(e.to_string().contains("nope"), "{e}");
        assert!(put(&t.path().join("x/.."), b"x").is_err(), "no file name");
    }

    #[cfg(unix)]
    #[test]
    fn only_links_the_user_cant_have_planted_are_followed() {
        assert!(at::trusted(0, 0, false));
        assert!(!at::trusted(1000, 0, false), "the user's link");
        assert!(!at::trusted(0, 1000, false), "in the user's folder");
        assert!(!at::trusted(0, 0, true), "in a folder others write (/tmp)");
    }

    #[test]
    fn a_file_goes_out_in_pieces_with_its_hash() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("b.bin");
        let data: Vec<u8> = (0..FILE_CHUNK + 7).map(|i| (i * 3) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let (send, got) = sink();
        Files::default().handle(Message::FileGet { id: 9, path: path.to_string_lossy().into_owned() }, &send);
        let done = wait_done(&got);
        let sha256: [u8; 32] = Sha256::digest(&data).into();
        assert_eq!(done, Message::FileDone { id: 9, ok: true, size: data.len() as u64, sha256, why: String::new() });
        let pieces: Vec<u8> = got.lock().unwrap().iter().filter_map(|m| if let Message::FileData { id: 9, data } = m { Some(data.clone()) } else { None }).flatten().collect();
        assert_eq!(pieces, data);
        for bad in [t.path().to_path_buf(), t.path().join("nope")] {
            let (send, got) = sink();
            Files::default().handle(Message::FileGet { id: 1, path: bad.to_string_lossy().into_owned() }, &send);
            assert!(matches!(wait_done(&got), Message::FileDone { id: 1, ok: false, .. }), "{bad:?}");
        }
    }
}
