//! Folders opened one at a time, never through a link someone else
//! planted, and files made, renamed and removed relative to the folder's
//! handle (openat and friends; Windows: `NtCreateFile` with a root
//! directory). For the privileged side writing where a guest user may
//! write: pushed files (files.rs), drops when run elevated (drop.rs).
//!
//! Each step names one entry in a folder we hold: nothing is looked up
//! by path again, so a folder swapped for a link meanwhile changes
//! nothing.
//!
//! Unix: a link on the way is followed (by us, at most `MAX_LINKS`) only
//! if nobody but root could have made it: the link and the folder holding
//! it are root's, and that folder isn't group- or world-writable: system
//! links (`/var/run`, `/tmp` or `/home` on some distros, `/var` on macOS),
//! never a user's. With `own_links`, ours too (a user's helper follows the
//! user's own: `~/.cache` on another disk).
//!
//! Windows: links on the way (symlinks, junctions: name surrogates) are
//! refused, all of them, system ones too: telling one users can't have
//! made would need the folder's ACL judged for every user. Other reparse
//! points (OneDrive, dedup) are the folder itself: walked. `..` is taken
//! off by name, as Windows does.

#[cfg(all(unix, test))]
pub use unix::trusted;
#[cfg(unix)]
pub use unix::Dir;
#[cfg(windows)]
pub use win::Dir;

#[cfg(unix)]
#[allow(unsafe_code)]
mod unix {
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Path};

    const MAX_LINKS: usize = 8;

    /// Whether to follow a link owned by `link_uid` in a folder owned by
    /// `dir_uid`, writable by group or others (`shared`) or not; we are
    /// `me`.
    pub fn trusted(link_uid: u32, dir_uid: u32, shared: bool, me: u32) -> bool {
        let ours = |u| u == 0 || u == me;
        ours(link_uid) && ours(dir_uid) && !shared
    }

    /// A folder, and whose links we follow besides root's (0: root's only).
    #[derive(Debug)]
    pub struct Dir(OwnedFd, u32);

    fn me(own_links: bool) -> u32 {
        // SAFETY: plain call.
        if own_links { unsafe { libc::geteuid() } } else { 0 }
    }

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

    /// From `dir`, through the pieces in `todo`.
    fn walk(mut dir: OwnedFd, mut todo: Vec<OsString>, me: u32) -> io::Result<OwnedFd> {
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
            let Some(st) = lstat_at(&dir, &cn).filter(|st| st.st_mode & libc::S_IFMT == libc::S_IFLNK) else { return Err(e) };
            let d = stat(&dir)?;
            if !trusted(st.st_uid, d.st_uid, d.st_mode & 0o022 != 0, me) {
                return Err(io::Error::other(format!("{}: a link, not followed", c.to_string_lossy())));
            }
            links += 1;
            if links > MAX_LINKS {
                return Err(io::Error::other(format!("{}: too many links", c.to_string_lossy())));
            }
            pieces(Path::new(&read_link(&dir, &cn, &c)?), &mut todo);
        }
        Ok(dir)
    }

    impl Dir {
        /// The folder at `path`.
        pub fn open(path: &Path, own_links: bool) -> io::Result<Dir> {
            let mut todo = Vec::new();
            pieces(path, &mut todo);
            let me = me(own_links);
            Ok(Dir(walk(open_dir(libc::AT_FDCWD, c".", OsStr::new("."))?, todo, me)?, me))
        }

        /// The folder `path` is in, and the file's name in it.
        pub fn parent_of(path: &Path, own_links: bool) -> io::Result<(Dir, OsString)> {
            let mut todo = Vec::new();
            pieces(path, &mut todo);
            let name = match todo.first() {
                Some(n) if n != "/" && n != ".." => n.clone(),
                _ => return Err(io::Error::other("no file name")),
            };
            todo.remove(0);
            let me = me(own_links);
            Ok((Dir(walk(open_dir(libc::AT_FDCWD, c".", OsStr::new("."))?, todo, me)?, me), name))
        }

        /// Folder `name` in this one, there already.
        pub fn sub(&self, name: &OsStr) -> io::Result<Dir> {
            Ok(Dir(walk(self.0.try_clone()?, vec![name.to_os_string()], self.1)?, self.1))
        }

        /// Folder `name` in this one: made, or there already.
        pub fn dir(&self, name: &OsStr) -> io::Result<Dir> {
            match self.mkdir(name) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => self.sub(name),
                r => r,
            }
        }

        /// Folder `name` in this one, new.
        pub fn mkdir(&self, name: &OsStr) -> io::Result<Dir> {
            let n = cstr(name)?;
            // SAFETY: valid fd and name.
            if unsafe { libc::mkdirat(self.0.as_raw_fd(), n.as_ptr(), 0o777) } != 0 {
                return Err(last(name));
            }
            Ok(Dir(open_dir(self.0.as_raw_fd(), &n, name)?, self.1))
        }

        /// File `name`, new (`mode`, less the umask); whatever was there
        /// (not a folder) removed, never written through.
        pub fn create(&self, name: &OsStr, mode: u32) -> io::Result<std::fs::File> {
            let p = cstr(name)?;
            if let Some(st) = lstat_at(&self.0, &p) {
                if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
                    return Err(io::Error::other(format!("{} is a folder", name.to_string_lossy())));
                }
                // SAFETY: valid fd and name.
                if unsafe { libc::unlinkat(self.0.as_raw_fd(), p.as_ptr(), 0) } != 0 {
                    return Err(last(name));
                }
            }
            let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            // SAFETY: valid fd and name; mode for O_CREAT.
            let f = fd(unsafe { libc::openat(self.0.as_raw_fd(), p.as_ptr(), flags, mode as libc::c_uint) }, name)?;
            Ok(f.into())
        }

        /// `f`, file `from` here, renamed `to` here (replacing one there).
        pub fn rename(&self, _f: &std::fs::File, from: &OsStr, to: &OsStr) -> io::Result<()> {
            self.move_to(from, self, to)
        }

        /// Entry `name` here moved to `to_name` in `to`.
        pub fn move_to(&self, name: &OsStr, to: &Dir, to_name: &OsStr) -> io::Result<()> {
            let (p, n) = (cstr(name)?, cstr(to_name)?);
            // SAFETY: valid fds and names.
            if unsafe { libc::renameat(self.0.as_raw_fd(), p.as_ptr(), to.0.as_raw_fd(), n.as_ptr()) } != 0 {
                return Err(last(to_name));
            }
            Ok(())
        }

        /// `f`, file `name` here, closed and removed.
        pub fn discard(&self, f: std::fs::File, name: &OsStr) {
            drop(f);
            if let Ok(p) = cstr(name) {
                // SAFETY: valid fd and name.
                unsafe { libc::unlinkat(self.0.as_raw_fd(), p.as_ptr(), 0) };
            }
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win {
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::{Component, Path};

    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FileRenameInformation, NtCreateFile, NtSetInformationFile, FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
    };
    use windows_sys::Win32::Foundation::{RtlNtStatusToDosError, HANDLE, UNICODE_STRING};
    use windows_sys::Win32::Storage::FileSystem::{
        FileAttributeTagInfo, FileDispositionInfo, GetFileInformationByHandleEx, SetFileInformationByHandle, DELETE,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    const OBJ_CASE_INSENSITIVE: u32 = 0x40;
    /// Symlinks, junctions: reparse points that name another place.
    const NAME_SURROGATE: u32 = 0x2000_0000;
    const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    const DIR_ACCESS: u32 = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES;

    #[derive(Debug)]
    pub struct Dir(OwnedHandle);

    /// Entry `name` of folder `root`, never through a reparse point.
    fn nt_open(root: HANDLE, name: &OsStr, access: u32, share: u32, disposition: u32, options: u32) -> io::Result<OwnedHandle> {
        let mut w: Vec<u16> = name.encode_wide().collect();
        // "file:stream" would open a hidden stream of another file.
        if w.contains(&(b':' as u16)) {
            return Err(io::Error::other(format!("{}: a name with a colon", name.to_string_lossy())));
        }
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

    /// A folder we just opened, refused if it is a link.
    fn no_link(h: OwnedHandle, name: &OsStr) -> io::Result<Dir> {
        let t = tag(&h)?;
        if t.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && t.ReparseTag & NAME_SURROGATE != 0 {
            return Err(io::Error::other(format!("{}: a link, not followed", name.to_string_lossy())));
        }
        Ok(Dir(h))
    }

    fn delete(h: HANDLE) -> io::Result<()> {
        let d = FILE_DISPOSITION_INFO { DeleteFile: 1 };
        // SAFETY: valid handle opened with DELETE; in buffer of the size given.
        if unsafe { SetFileInformationByHandle(h, FileDispositionInfo, (&d as *const FILE_DISPOSITION_INFO).cast(), std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Rename what `h` (opened with DELETE) is to `name` in folder `root`.
    /// The NT call: SetFileInformationByHandle reads the name as a Win32
    /// path (a bare one lands in the service's current folder, System32;
    /// with a root handle it answers "The parameter is incorrect").
    fn rename_handle(h: HANDLE, root: HANDLE, name: &OsStr, replace: bool) -> io::Result<()> {
        let w: Vec<u16> = name.encode_wide().collect();
        let off = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
        let size = off + w.len() * 2;
        let bytes = u32::try_from(w.len() * 2).map_err(|_| io::Error::other("name too long"))?;
        // u64s: aligned for FILE_RENAME_INFORMATION, and never smaller.
        let mut buf = vec![0u64; size.max(std::mem::size_of::<FILE_RENAME_INFORMATION>()).div_ceil(8)];
        let p = buf.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        // SAFETY: buf is aligned and big enough for the struct and the name.
        unsafe {
            (*p).Anonymous.ReplaceIfExists = u8::from(replace);
            (*p).RootDirectory = root;
            (*p).FileNameLength = bytes;
            std::ptr::copy_nonoverlapping(w.as_ptr(), p.cast::<u8>().add(off).cast::<u16>(), w.len());
        }
        // SAFETY: zeroed IO_STATUS_BLOCK is valid plain data.
        let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        // SAFETY: valid handle; buffer of the size given.
        let st = unsafe { NtSetInformationFile(h, &mut iosb, p.cast(), (buf.len() * 8) as u32, FileRenameInformation) };
        if st < 0 {
            // SAFETY: plain call.
            let e = io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(st) } as i32);
            return Err(io::Error::new(e.kind(), format!("{}: {e}", name.to_string_lossy())));
        }
        Ok(())
    }

    impl Dir {
        /// The folder at `path`. (`_own_links`: Unix only.)
        pub fn open(path: &Path, _own_links: bool) -> io::Result<Dir> {
            let comps: Vec<Component> = path.components().collect();
            Self::walk(&comps)
        }

        /// The folder `path` is in, and the file's name in it.
        pub fn parent_of(path: &Path, _own_links: bool) -> io::Result<(Dir, OsString)> {
            let mut comps: Vec<Component> = path.components().collect();
            let name = match comps.pop() {
                Some(Component::Normal(n)) => n.to_os_string(),
                _ => return Err(io::Error::other("no file name")),
            };
            Ok((Self::walk(&comps)?, name))
        }

        fn walk(comps: &[Component]) -> io::Result<Dir> {
            // Where to start: the drive or share's root ("C:" "\"), or ours.
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
                .access_mode(DIR_ACCESS | SYNCHRONIZE)
                .share_mode(SHARE_ALL)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(&start)
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", start.to_string_lossy())))?;
            let mut stack = vec![Dir(OwnedHandle::from(root))];
            for c in rest {
                match c {
                    Component::Normal(n) => {
                        let Some(top) = stack.last() else { return Err(io::Error::other("no folder")) };
                        let d = top.sub(n)?;
                        stack.push(d);
                    }
                    Component::ParentDir if stack.len() > 1 => {
                        stack.pop();
                    }
                    _ => {}
                }
            }
            stack.pop().ok_or_else(|| io::Error::other("no folder"))
        }

        /// Folder `name` in this one, there already.
        pub fn sub(&self, name: &OsStr) -> io::Result<Dir> {
            no_link(nt_open(self.0.as_raw_handle(), name, DIR_ACCESS, SHARE_ALL, FILE_OPEN, FILE_DIRECTORY_FILE)?, name)
        }

        /// Folder `name` in this one: made, or there already.
        pub fn dir(&self, name: &OsStr) -> io::Result<Dir> {
            match self.mkdir(name) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => self.sub(name),
                r => r,
            }
        }

        /// Folder `name` in this one, new.
        pub fn mkdir(&self, name: &OsStr) -> io::Result<Dir> {
            Ok(Dir(nt_open(self.0.as_raw_handle(), name, DIR_ACCESS, SHARE_ALL, FILE_CREATE, FILE_DIRECTORY_FILE)?))
        }

        /// File `name`, new; whatever was there (not a folder) removed,
        /// never written through. Not shared: nobody opens, renames or
        /// removes it under us. (`_mode`: Unix only.)
        pub fn create(&self, name: &OsStr, _mode: u32) -> io::Result<std::fs::File> {
            let d = self.0.as_raw_handle();
            match nt_open(d, name, DELETE | FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, 0) {
                Ok(h) => {
                    let t = tag(&h)?;
                    if t.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 && t.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                        return Err(io::Error::other(format!("{} is a folder", name.to_string_lossy())));
                    }
                    delete(h.as_raw_handle())?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let h = nt_open(d, name, FILE_GENERIC_WRITE | DELETE | FILE_READ_ATTRIBUTES, 0, FILE_CREATE, FILE_NON_DIRECTORY_FILE)?;
            Ok(h.into())
        }

        /// `f` (opened by `create`) renamed `to` here, replacing one there.
        pub fn rename(&self, f: &std::fs::File, _from: &OsStr, to: &OsStr) -> io::Result<()> {
            rename_handle(f.as_raw_handle(), self.0.as_raw_handle(), to, true)
        }

        /// Entry `name` here moved to `to_name` in `to`, not replacing.
        pub fn move_to(&self, name: &OsStr, to: &Dir, to_name: &OsStr) -> io::Result<()> {
            let h = nt_open(self.0.as_raw_handle(), name, DELETE | FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, 0)?;
            rename_handle(h.as_raw_handle(), to.0.as_raw_handle(), to_name, false)
        }

        /// `f` (opened by `create`) removed.
        pub fn discard(&self, f: std::fs::File, _name: &OsStr) {
            let _ = delete(f.as_raw_handle());
        }
    }
}
