//! virtio-fs: a host directory shared with the guest (FUSE over a
//! virtqueue). Linux: `mount -t virtiofs <tag> /mnt`.
//!
//! The FUSE server here only parses and answers; every host access goes
//! through `bulwark_fs::Share`, which never leaves the shared directory and
//! never follows a symlink. On top of that, from the guest's side:
//! - read-only shares refuse every change (EROFS);
//! - no symlinks, hard links or device nodes can be created;
//! - nodes, open handles and directory listings are capped;
//! - every length and name in a request is checked before use.
//!
//! Nodes are paths below the root, so they survive snapshots: open files
//! are reopened by path on restore.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bulwark_fs::{linux_errno, Share, Stat};
use serde::{Deserialize, Serialize};

use crate::worker::{QueueCtx, QueueHandler, WorkerSlot};
use crate::{ActivateContext, DeviceState, Error, Result, VirtioDevice};

pub const TYPE_FS: u32 = 26;

const ROOT: u64 = 1;
const MAX_NODES: usize = 1 << 18;
const MAX_HANDLES: usize = 1024;
const MAX_DIR_ENTRIES: usize = 1 << 17;
/// Largest read/write (FUSE max_write, and max_pages * 4K).
pub const MAX_IO: usize = 1 << 20;
const MAX_REQUEST: usize = MAX_IO + 4096;
const TTL_SECS: u64 = 1;

// Opcodes.
const LOOKUP: u32 = 1;
const FORGET: u32 = 2;
const GETATTR: u32 = 3;
const SETATTR: u32 = 4;
const READLINK: u32 = 5;
const MKDIR: u32 = 9;
const UNLINK: u32 = 10;
const RMDIR: u32 = 11;
const RENAME: u32 = 12;
const OPEN: u32 = 14;
const READ: u32 = 15;
const WRITE: u32 = 16;
const STATFS: u32 = 17;
const RELEASE: u32 = 18;
const FSYNC: u32 = 20;
const FLUSH: u32 = 25;
const INIT: u32 = 26;
const OPENDIR: u32 = 27;
const READDIR: u32 = 28;
const RELEASEDIR: u32 = 29;
const FSYNCDIR: u32 = 30;
const ACCESS: u32 = 34;
const CREATE: u32 = 35;
const INTERRUPT: u32 = 36;
const DESTROY: u32 = 38;
const BATCH_FORGET: u32 = 42;
const RENAME2: u32 = 45;

// Linux errno values (what the guest expects).
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EBADF: i32 = 9;
const ENFILE: i32 = 23;
const EMFILE: i32 = 24;
const EINVAL: i32 = 22;
const EROFS: i32 = 30;
const ENOSYS: i32 = 38;

// Linux open flags.
const O_ACCMODE: u32 = 3;
const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;
const O_EXCL: u32 = 0o200;

// SETATTR valid bits.
const FATTR_MODE: u32 = 1;
const FATTR_SIZE: u32 = 8;
const FATTR_ATIME: u32 = 16;
const FATTR_MTIME: u32 = 32;
const FATTR_FH: u32 = 64;
const FATTR_ATIME_NOW: u32 = 128;
const FATTR_MTIME_NOW: u32 = 256;

const FUSE_BIG_WRITES: u32 = 1 << 5;
const FUSE_MAX_PAGES: u32 = 1 << 22;
const RENAME_NOREPLACE: u32 = 1;

const IN_HEADER: usize = 40;

/// Little-endian cursor over a request.
struct In<'a> {
    b: &'a [u8],
}

impl<'a> In<'a> {
    fn take(&mut self, n: usize) -> std::result::Result<&'a [u8], i32> {
        if self.b.len() < n {
            return Err(EINVAL);
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    fn u32(&mut self) -> std::result::Result<u32, i32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| EINVAL)?))
    }
    fn u64(&mut self) -> std::result::Result<u64, i32> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| EINVAL)?))
    }
    /// A NUL-terminated name.
    fn name(&mut self) -> std::result::Result<OsString, i32> {
        let end = self.b.iter().position(|&c| c == 0).ok_or(EINVAL)?;
        let n = OsStr::from_bytes(&self.b[..end]).to_os_string();
        self.b = &self.b[end + 1..];
        if !bulwark_fs::valid_name(&n) {
            return Err(EINVAL);
        }
        Ok(n)
    }
}

#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn attr(&mut self, s: &Stat, uid: u32, gid: u32) -> &mut Self {
        self.u64(s.ino).u64(s.size).u64(s.blocks);
        self.u64(s.atime.0 as u64).u64(s.mtime.0 as u64).u64(s.ctime.0 as u64);
        self.u32(s.atime.1).u32(s.mtime.1).u32(s.ctime.1);
        self.u32(s.mode).u32(s.nlink).u32(uid).u32(gid).u32(0).u32(s.blksize).u32(0)
    }
    fn entry(&mut self, node: u64, s: &Stat, uid: u32, gid: u32) -> &mut Self {
        self.u64(node).u64(0).u64(TTL_SECS).u64(TTL_SECS).u32(0).u32(0).attr(s, uid, gid)
    }
}

type Reply = std::result::Result<Vec<u8>, i32>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Node {
    /// Path below the root; None once removed or replaced.
    path: Option<Vec<Vec<u8>>>,
    lookups: u64,
}

enum Handle {
    File { node: u64, file: File, write: bool, append: bool },
    Dir { entries: Vec<bulwark_fs::DirEntry> },
}

/// What survives a snapshot (the guest kernel holds node ids and file
/// handles across it).
#[derive(Serialize, Deserialize, Default)]
struct Snap {
    nodes: Vec<(u64, Node)>,
    files: Vec<(u64, u64, bool, bool)>,
    next_node: u64,
    next_fh: u64,
    inited: bool,
}

pub struct FuseServer {
    share: Share,
    nodes: HashMap<u64, Node>,
    by_path: HashMap<Vec<Vec<u8>>, u64>,
    handles: HashMap<u64, Handle>,
    next_node: u64,
    next_fh: u64,
    inited: bool,
}

impl std::fmt::Debug for FuseServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuseServer").field("nodes", &self.nodes.len()).field("handles", &self.handles.len()).finish()
    }
}

fn os_path(p: &[Vec<u8>]) -> Vec<OsString> {
    p.iter().map(|c| OsString::from_vec(c.clone())).collect()
}

fn now() -> (i64, u32) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() as i64, d.subsec_nanos())
}

impl FuseServer {
    pub fn new(share: Share) -> Self {
        let mut s = FuseServer {
            share,
            nodes: HashMap::new(),
            by_path: HashMap::new(),
            handles: HashMap::new(),
            next_node: 2,
            next_fh: 1,
            inited: false,
        };
        s.reset();
        s
    }

    fn reset(&mut self) {
        self.nodes.clear();
        self.by_path.clear();
        self.handles.clear();
        self.nodes.insert(ROOT, Node { path: Some(Vec::new()), lookups: 1 });
        self.by_path.insert(Vec::new(), ROOT);
        self.inited = false;
    }

    fn path(&self, node: u64) -> std::result::Result<Vec<Vec<u8>>, i32> {
        self.nodes.get(&node).and_then(|n| n.path.clone()).ok_or(ENOENT)
    }

    fn child(&self, parent: u64, name: &OsStr) -> std::result::Result<Vec<Vec<u8>>, i32> {
        let mut p = self.path(parent)?;
        p.push(name.as_bytes().to_vec());
        Ok(p)
    }

    fn stat(&self, p: &[Vec<u8>]) -> std::result::Result<Stat, i32> {
        self.share.stat(&os_path(p)).map_err(|e| linux_errno(&e))
    }

    /// Count a lookup of `p` and return its node.
    fn remember(&mut self, p: Vec<Vec<u8>>) -> std::result::Result<u64, i32> {
        if let Some(&id) = self.by_path.get(&p) {
            if let Some(n) = self.nodes.get_mut(&id) {
                n.lookups += 1;
            }
            return Ok(id);
        }
        if self.nodes.len() >= MAX_NODES {
            return Err(ENFILE);
        }
        let id = self.next_node;
        self.next_node += 1;
        self.by_path.insert(p.clone(), id);
        self.nodes.insert(id, Node { path: Some(p), lookups: 1 });
        Ok(id)
    }

    fn entry(&mut self, p: Vec<Vec<u8>>, uid: u32, gid: u32) -> Reply {
        let st = self.stat(&p)?;
        let id = self.remember(p)?;
        let mut o = Out::default();
        o.entry(id, &st, uid, gid);
        Ok(o.0)
    }

    fn forget(&mut self, node: u64, n: u64) {
        if node == ROOT {
            return;
        }
        let gone = match self.nodes.get_mut(&node) {
            Some(x) => {
                x.lookups = x.lookups.saturating_sub(n);
                x.lookups == 0
            }
            None => false,
        };
        if gone {
            if let Some(Node { path: Some(p), .. }) = self.nodes.remove(&node) {
                if self.by_path.get(&p) == Some(&node) {
                    self.by_path.remove(&p);
                }
            }
        }
    }

    /// `p` (and everything under it) no longer exists by that path.
    fn drop_path(&mut self, p: &[Vec<u8>]) {
        let ids: Vec<u64> = self.by_path.iter().filter(|(k, _)| k.starts_with(p)).map(|(_, &v)| v).collect();
        for id in ids {
            if let Some(n) = self.nodes.get_mut(&id) {
                if let Some(old) = n.path.take() {
                    self.by_path.remove(&old);
                }
            }
        }
    }

    /// Everything under `from` is now under `to`.
    fn moved(&mut self, from: &[Vec<u8>], to: &[Vec<u8>]) {
        self.drop_path(to);
        let ids: Vec<(Vec<Vec<u8>>, u64)> =
            self.by_path.iter().filter(|(k, _)| k.starts_with(from)).map(|(k, &v)| (k.clone(), v)).collect();
        for (old, id) in ids {
            self.by_path.remove(&old);
            let mut new = to.to_vec();
            new.extend_from_slice(&old[from.len()..]);
            self.by_path.insert(new.clone(), id);
            if let Some(n) = self.nodes.get_mut(&id) {
                n.path = Some(new);
            }
        }
    }

    fn add_handle(&mut self, h: Handle) -> std::result::Result<u64, i32> {
        if self.handles.len() >= MAX_HANDLES {
            return Err(EMFILE);
        }
        let fh = self.next_fh;
        self.next_fh += 1;
        self.handles.insert(fh, h);
        Ok(fh)
    }

    fn writable(&self) -> std::result::Result<(), i32> {
        if self.share.readonly() {
            Err(EROFS)
        } else {
            Ok(())
        }
    }

    /// Handle one request. `room`: bytes the reply may take (without
    /// header). None: no reply (FORGET and friends).
    pub fn handle(&mut self, req: &[u8], room: usize) -> Option<Vec<u8>> {
        let mut r = In { b: req };
        let hdr = r.take(IN_HEADER).ok()?;
        let mut h = In { b: hdr };
        let (len, op, unique, node) = (h.u32().ok()?, h.u32().ok()?, h.u64().ok()?, h.u64().ok()?);
        let (uid, gid) = (h.u32().ok()?, h.u32().ok()?);
        if (len as usize) < IN_HEADER || len as usize > req.len() {
            return Some(reply(unique, Err(EINVAL)));
        }
        let mut body = In { b: &req[IN_HEADER..len as usize] };
        match op {
            FORGET => {
                if let Ok(n) = body.u64() {
                    self.forget(node, n);
                }
                return None;
            }
            BATCH_FORGET => {
                let count = body.u32().unwrap_or(0);
                let _ = body.u32();
                for _ in 0..count.min(1 << 16) {
                    let (Ok(id), Ok(n)) = (body.u64(), body.u64()) else { break };
                    self.forget(id, n);
                }
                return None;
            }
            INTERRUPT => return None,
            _ => {}
        }
        let res = if op != INIT && !self.inited { Err(EIO) } else { self.op(op, node, &mut body, uid, gid, room) };
        Some(reply(unique, res))
    }

    fn op(&mut self, op: u32, node: u64, b: &mut In<'_>, uid: u32, gid: u32, room: usize) -> Reply {
        match op {
            INIT => {
                let (major, minor, readahead, flags) = (b.u32()?, b.u32()?, b.u32()?, b.u32()?);
                if major < 7 {
                    return Err(EINVAL);
                }
                self.reset();
                self.inited = true;
                let mut o = Out::default();
                o.u32(7).u32(minor.min(31)).u32(readahead).u32(flags & (FUSE_BIG_WRITES | FUSE_MAX_PAGES));
                o.u16(64).u16(48).u32(MAX_IO as u32).u32(1).u16((MAX_IO / 4096) as u16).u16(0);
                o.u32(0).u32(0);
                o.0.resize(64, 0);
                Ok(o.0)
            }
            DESTROY => {
                self.reset();
                Ok(Vec::new())
            }
            LOOKUP => {
                let name = b.name()?;
                let p = self.child(node, &name)?;
                self.entry(p, uid, gid)
            }
            GETATTR => {
                let st = self.stat(&self.path(node)?)?;
                let mut o = Out::default();
                o.u64(TTL_SECS).u32(0).u32(0).attr(&st, uid, gid);
                Ok(o.0)
            }
            SETATTR => self.setattr(node, b, uid, gid),
            READLINK => {
                let t = self.share.read_link(&os_path(&self.path(node)?)).map_err(|e| linux_errno(&e))?;
                Ok(t.into_vec())
            }
            MKDIR => {
                self.writable()?;
                let (mode, umask) = (b.u32()?, b.u32()?);
                let name = b.name()?;
                let parent = os_path(&self.path(node)?);
                self.share.mkdir(&parent, &name, mode & !umask).map_err(|e| linux_errno(&e))?;
                let p = self.child(node, &name)?;
                self.entry(p, uid, gid)
            }
            UNLINK | RMDIR => {
                self.writable()?;
                let name = b.name()?;
                let parent = os_path(&self.path(node)?);
                self.share.remove(&parent, &name, op == RMDIR).map_err(|e| linux_errno(&e))?;
                let p = self.child(node, &name)?;
                self.drop_path(&p);
                Ok(Vec::new())
            }
            RENAME | RENAME2 => {
                self.writable()?;
                let newdir = b.u64()?;
                let flags = if op == RENAME2 {
                    let f = b.u32()?;
                    b.u32()?;
                    f
                } else {
                    0
                };
                if flags & !RENAME_NOREPLACE != 0 {
                    return Err(EINVAL);
                }
                let (old, new) = (b.name()?, b.name()?);
                let (from, to) = (self.child(node, &old)?, self.child(newdir, &new)?);
                let (fp, tp) = (os_path(&self.path(node)?), os_path(&self.path(newdir)?));
                self.share.rename(&fp, &old, &tp, &new, flags & RENAME_NOREPLACE != 0).map_err(|e| linux_errno(&e))?;
                self.moved(&from, &to);
                Ok(Vec::new())
            }
            OPEN => {
                let flags = b.u32()?;
                let write = flags & O_ACCMODE != 0;
                if write {
                    self.writable()?;
                }
                let p = os_path(&self.path(node)?);
                let file = self.share.open_file(&p, write).map_err(|e| linux_errno(&e))?;
                if write && flags & O_TRUNC != 0 {
                    file.set_len(0).map_err(|e| linux_errno(&e))?;
                }
                let fh = self.add_handle(Handle::File { node, file, write, append: flags & O_APPEND != 0 })?;
                let mut o = Out::default();
                o.u64(fh).u32(0).u32(0);
                Ok(o.0)
            }
            CREATE => {
                self.writable()?;
                let (flags, mode, umask, _) = (b.u32()?, b.u32()?, b.u32()?, b.u32()?);
                let name = b.name()?;
                let parent = os_path(&self.path(node)?);
                let file = self.share.create(&parent, &name, mode & !umask, flags & O_EXCL != 0).map_err(|e| linux_errno(&e))?;
                if flags & O_TRUNC != 0 {
                    file.set_len(0).map_err(|e| linux_errno(&e))?;
                }
                let p = self.child(node, &name)?;
                let mut o = Out(self.entry(p.clone(), uid, gid)?);
                let id = self.by_path.get(&p).copied().ok_or(EIO)?;
                let fh = self.add_handle(Handle::File { node: id, file, write: true, append: flags & O_APPEND != 0 })?;
                o.u64(fh).u32(0).u32(0);
                Ok(o.0)
            }
            READ => {
                let (fh, off, size) = (b.u64()?, b.u64()?, b.u32()?);
                let Some(Handle::File { file, .. }) = self.handles.get(&fh) else { return Err(EBADF) };
                let n = (size as usize).min(MAX_IO).min(room);
                let mut buf = vec![0u8; n];
                let mut done = 0;
                while done < n {
                    match file.read_at(&mut buf[done..], off.saturating_add(done as u64)) {
                        Ok(0) => break,
                        Ok(k) => done += k,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(linux_errno(&e)),
                    }
                }
                buf.truncate(done);
                Ok(buf)
            }
            WRITE => {
                let (fh, off, size, _wflags, _lock, _flags, _pad) = (b.u64()?, b.u64()?, b.u32()?, b.u32()?, b.u64()?, b.u32()?, b.u32()?);
                let data = b.take(size as usize)?;
                let Some(Handle::File { file, write, append, .. }) = self.handles.get(&fh) else { return Err(EBADF) };
                if !*write {
                    return Err(EBADF);
                }
                let off = if *append { file.metadata().map(|m| m.len()).unwrap_or(off) } else { off };
                file.write_all_at(data, off).map_err(|e| linux_errno(&e))?;
                let mut o = Out::default();
                o.u32(size).u32(0);
                Ok(o.0)
            }
            RELEASE | RELEASEDIR => {
                let fh = b.u64()?;
                self.handles.remove(&fh);
                Ok(Vec::new())
            }
            FLUSH | FSYNCDIR => Ok(Vec::new()),
            FSYNC => {
                let fh = b.u64()?;
                if let Some(Handle::File { file, write: true, .. }) = self.handles.get(&fh) {
                    file.sync_data().map_err(|e| linux_errno(&e))?;
                }
                Ok(Vec::new())
            }
            OPENDIR => {
                let p = os_path(&self.path(node)?);
                let entries = self.share.read_dir(&p, MAX_DIR_ENTRIES).map_err(|e| linux_errno(&e))?;
                let fh = self.add_handle(Handle::Dir { entries })?;
                let mut o = Out::default();
                o.u64(fh).u32(0).u32(0);
                Ok(o.0)
            }
            READDIR => {
                let (fh, off, size) = (b.u64()?, b.u64()?, b.u32()?);
                let Some(Handle::Dir { entries }) = self.handles.get(&fh) else { return Err(EBADF) };
                let cap = (size as usize).min(room);
                let mut o = Out::default();
                for (i, e) in entries.iter().enumerate().skip(off.min(usize::MAX as u64) as usize) {
                    let name = e.name.as_bytes();
                    let rec = (24 + name.len()).div_ceil(8) * 8;
                    if o.0.len() + rec > cap {
                        break;
                    }
                    o.u64(e.ino).u64(i as u64 + 1).u32(name.len() as u32).u32(e.kind);
                    o.0.extend_from_slice(name);
                    o.0.resize(o.0.len().div_ceil(8) * 8, 0);
                }
                Ok(o.0)
            }
            STATFS => {
                let s = self.share.statfs().map_err(|e| linux_errno(&e))?;
                let mut o = Out::default();
                o.u64(s.blocks).u64(s.bfree).u64(s.bavail).u64(s.files).u64(s.ffree);
                o.u32(s.bsize).u32(bulwark_fs::NAME_MAX as u32).u32(s.bsize).u32(0);
                o.0.resize(80, 0);
                Ok(o.0)
            }
            ACCESS => {
                let mask = b.u32()?;
                self.stat(&self.path(node)?)?;
                if mask & 2 != 0 {
                    self.writable()?;
                }
                Ok(Vec::new())
            }
            // No symlinks, hard links or device nodes from the guest.
            6 | 8 | 13 => Err(EPERM),
            _ => Err(ENOSYS),
        }
    }

    fn setattr(&mut self, node: u64, b: &mut In<'_>, uid: u32, gid: u32) -> Reply {
        let (valid, _pad, fh, size) = (b.u32()?, b.u32()?, b.u64()?, b.u64()?);
        let (_lock, atime, mtime, _ctime) = (b.u64()?, b.u64()?, b.u64()?, b.u64()?);
        let (atimens, mtimens, _ctimens, mode) = (b.u32()?, b.u32()?, b.u32()?, b.u32()?);
        let p = os_path(&self.path(node)?);
        let changes = FATTR_MODE | FATTR_SIZE | FATTR_ATIME | FATTR_MTIME | FATTR_ATIME_NOW | FATTR_MTIME_NOW;
        if valid & changes != 0 {
            self.writable()?;
        }
        if valid & FATTR_SIZE != 0 {
            match (valid & FATTR_FH != 0, self.handles.get(&fh)) {
                (true, Some(Handle::File { file, write: true, .. })) => file.set_len(size).map_err(|e| linux_errno(&e))?,
                _ => self.share.set_len(&p, size).map_err(|e| linux_errno(&e))?,
            }
        }
        if valid & FATTR_MODE != 0 {
            self.share.set_mode(&p, mode).map_err(|e| linux_errno(&e))?;
        }
        let t = |set: u32, now_bit: u32, s: u64, ns: u32| {
            if valid & now_bit != 0 {
                Some(now())
            } else if valid & set != 0 {
                Some((s as i64, ns))
            } else {
                None
            }
        };
        let (a, m) = (t(FATTR_ATIME, FATTR_ATIME_NOW, atime, atimens), t(FATTR_MTIME, FATTR_MTIME_NOW, mtime, mtimens));
        if a.is_some() || m.is_some() {
            self.share.set_times(&p, a, m).map_err(|e| linux_errno(&e))?;
        }
        // uid/gid changes: accepted and ignored (files are the host user's).
        let st = self.share.stat(&p).map_err(|e| linux_errno(&e))?;
        let mut o = Out::default();
        o.u64(TTL_SECS).u32(0).u32(0).attr(&st, uid, gid);
        Ok(o.0)
    }

    fn save(&self) -> Snap {
        let files = self
            .handles
            .iter()
            .filter_map(|(&fh, h)| match h {
                Handle::File { node, write, append, .. } => Some((fh, *node, *write, *append)),
                Handle::Dir { .. } => None,
            })
            .collect();
        Snap {
            nodes: self.nodes.iter().map(|(&k, v)| (k, v.clone())).collect(),
            files,
            next_node: self.next_node,
            next_fh: self.next_fh,
            inited: self.inited,
        }
    }

    fn restore(&mut self, s: Snap) {
        self.reset();
        for (id, mut n) in s.nodes.into_iter().take(MAX_NODES) {
            // Snapshot data is input like any other: names must be names.
            if n.path.as_ref().is_some_and(|p| !p.iter().all(|c| bulwark_fs::valid_name(OsStr::from_bytes(c)))) {
                n.path = None;
            }
            if id == ROOT {
                continue;
            }
            if let Some(p) = &n.path {
                self.by_path.insert(p.clone(), id);
            }
            self.nodes.insert(id, n);
        }
        for (fh, node, write, append) in s.files.into_iter().take(MAX_HANDLES) {
            let Ok(p) = self.path(node) else { continue };
            // Gone or changed on the host meanwhile: the guest gets EBADF.
            if let Ok(file) = self.share.open_file(&os_path(&p), write) {
                self.handles.insert(fh, Handle::File { node, file, write, append });
            }
        }
        // Directory handles are dropped: the guest reopens on EBADF.
        self.next_node = s.next_node.max(2);
        self.next_fh = s.next_fh.max(1);
        self.inited = s.inited;
    }
}

fn reply(unique: u64, res: Reply) -> Vec<u8> {
    let (err, body) = match res {
        Ok(b) => (0, b),
        Err(e) => (-e, Vec::new()),
    };
    let mut o = Out::default();
    o.u32((16 + body.len()) as u32);
    o.0.extend_from_slice(&err.to_le_bytes());
    o.u64(unique);
    o.0.extend_from_slice(&body);
    o.0
}

#[derive(Debug)]
pub struct FsHandler {
    /// Always Some while the worker runs; taken back on stop.
    server: Option<FuseServer>,
}

impl QueueHandler for FsHandler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        loop {
            let chain = match ctx.queues[q].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("fs: {e}");
                    break;
                }
            };
            let head = chain.head();
            let mut len = 0;
            if let Ok((mut r, mut w)) = chain.into_rw() {
                let n = r.available().min(MAX_REQUEST);
                let mut req = vec![0u8; n];
                if r.read_exact(&mut req).is_ok() {
                    let room = w.available().saturating_sub(16);
                    if let Some(rep) = self.server.as_mut().and_then(|s| s.handle(&req, room)) {
                        if w.write_all(&rep).is_ok() {
                            len = rep.len() as u32;
                        }
                    }
                }
            }
            ctx.add_used(q, head, len);
        }
    }
}

/// The device. One request queue (plus the high-priority one).
pub struct Fs {
    tag: String,
    root: PathBuf,
    readonly: bool,
    /// Kept between activations: guest reboots re-INIT it.
    server: Option<FuseServer>,
    worker: WorkerSlot<FsHandler>,
}

impl std::fmt::Debug for Fs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fs").field("tag", &self.tag).field("root", &self.root).finish()
    }
}

impl Fs {
    /// `tag` is what the guest mounts (36 bytes max).
    pub fn new(tag: &str, root: PathBuf, readonly: bool) -> Result<Self> {
        if tag.is_empty() || tag.len() > 36 {
            return Err(Error::Device(format!("share tag {tag:?}: 1 to 36 bytes")));
        }
        let share = Share::open(&root, readonly).map_err(|e| Error::Device(format!("share {}: {e}", root.display())))?;
        Ok(Fs { tag: tag.into(), root, readonly, server: Some(FuseServer::new(share)), worker: WorkerSlot::default() })
    }

    fn take_server(&mut self) -> Result<FuseServer> {
        if let Some(Some(s)) = self.worker.stop_with(|h| h.server.take()) {
            return Ok(s);
        }
        match self.server.take() {
            Some(s) => Ok(s),
            None => {
                let share = Share::open(&self.root, self.readonly).map_err(|e| Error::Device(e.to_string()))?;
                Ok(FuseServer::new(share))
            }
        }
    }
}

impl VirtioDevice for Fs {
    fn device_type(&self) -> u32 {
        TYPE_FS
    }
    fn name(&self) -> String {
        format!("fs-{}", self.tag)
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![128, 128]
    }
    fn device_features(&self) -> u64 {
        0
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut c = [0u8; 40];
        c[..self.tag.len()].copy_from_slice(self.tag.as_bytes());
        c[36..40].copy_from_slice(&1u32.to_le_bytes());
        data.fill(0);
        let o = offset as usize;
        if o < c.len() {
            let n = data.len().min(c.len() - o);
            data[..n].copy_from_slice(&c[o..o + n]);
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        let server = self.take_server()?;
        self.worker.start(&self.name(), ctx, FsHandler { server: Some(server) }, false)
    }
    fn queue_notify(&mut self, q: u16) {
        self.worker.notify(q);
    }
    fn reset(&mut self) {
        if let Ok(mut s) = self.take_server() {
            s.reset();
            self.server = Some(s);
        }
    }
    fn pause(&mut self) {
        self.worker.pause();
    }
    fn resume(&mut self) {
        self.worker.resume();
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        let snap = self.worker.with(|_, h| h.server.as_ref().map(|s| s.save())).flatten().unwrap_or_default();
        let data = serde_json::to_vec(&snap).map_err(|e| Error::Device(e.to_string()))?;
        Ok(DeviceState { queues: self.worker.queues(), data })
    }
    fn restore_active(&mut self, ctx: ActivateContext, data: &[u8]) -> Result<()> {
        let mut server = self.take_server()?;
        let snap: Snap = serde_json::from_slice(data).map_err(|e| Error::Device(e.to_string()))?;
        server.restore(snap);
        self.worker.start(&self.name(), ctx, FsHandler { server: Some(server) }, true)
    }
}

#[cfg(test)]
mod tests;
