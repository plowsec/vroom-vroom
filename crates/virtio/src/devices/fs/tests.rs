use super::*;
use std::path::Path;

struct T {
    s: FuseServer,
    unique: u64,
    _td: tempfile::TempDir,
    root: PathBuf,
}

fn name(n: &str) -> Vec<u8> {
    let mut v = n.as_bytes().to_vec();
    v.push(0);
    v
}

fn le(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// INIT flags: Linux asks for async reads (and more), virtiofs.exe for
/// READDIRPLUS and max_pages only.
const LINUX_INIT: u32 = FUSE_ASYNC_READ | FUSE_BIG_WRITES;
const WINDOWS_INIT: u32 = (1 << 13) | FUSE_MAX_PAGES;

impl T {
    fn new(readonly: bool) -> T {
        T::client(readonly, LINUX_INIT)
    }

    fn windows(readonly: bool) -> T {
        T::client(readonly, WINDOWS_INIT)
    }

    fn client(readonly: bool, flags: u32) -> T {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("share");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/hello.txt"), b"hello world").unwrap();
        std::fs::write(td.path().join("secret"), b"outside").unwrap();
        std::os::unix::fs::symlink(td.path(), root.join("escape")).unwrap();
        let s = FuseServer::new(Share::open(&root, readonly).unwrap());
        let mut t = T { s, unique: 0, _td: td, root };
        let (e, _) = t.call(INIT, 0, &le(&[&7u32.to_le_bytes(), &31u32.to_le_bytes(), &0u32.to_le_bytes(), &flags.to_le_bytes()]));
        assert_eq!(e, 0);
        assert_eq!(t.s.windows, flags == WINDOWS_INIT);
        t
    }

    fn raw(&mut self, op: u32, node: u64, body: &[u8]) -> Option<Vec<u8>> {
        self.unique += 1;
        let len = (IN_HEADER + body.len()) as u32;
        let mut r = Vec::new();
        r.extend_from_slice(&len.to_le_bytes());
        r.extend_from_slice(&op.to_le_bytes());
        r.extend_from_slice(&self.unique.to_le_bytes());
        r.extend_from_slice(&node.to_le_bytes());
        r.extend_from_slice(&1000u32.to_le_bytes());
        r.extend_from_slice(&1000u32.to_le_bytes());
        r.extend_from_slice(&[0u8; 8]);
        r.extend_from_slice(body);
        self.s.handle(&r, 1 << 20)
    }

    /// (negative errno or 0, payload)
    fn call(&mut self, op: u32, node: u64, body: &[u8]) -> (i32, Vec<u8>) {
        let r = self.raw(op, node, body).expect("a reply");
        let len = u32::from_le_bytes(r[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, r.len());
        assert_eq!(u64::from_le_bytes(r[8..16].try_into().unwrap()), self.unique);
        (i32::from_le_bytes(r[4..8].try_into().unwrap()), r[16..].to_vec())
    }

    fn lookup(&mut self, parent: u64, n: &str) -> std::result::Result<u64, i32> {
        let (e, b) = self.call(LOOKUP, parent, &name(n));
        if e != 0 {
            return Err(-e);
        }
        Ok(u64::from_le_bytes(b[0..8].try_into().unwrap()))
    }

    fn open(&mut self, node: u64, flags: u32) -> std::result::Result<u64, i32> {
        let (e, b) = self.call(OPEN, node, &le(&[&flags.to_le_bytes(), &[0; 4]]));
        if e != 0 {
            return Err(-e);
        }
        Ok(u64::from_le_bytes(b[0..8].try_into().unwrap()))
    }

    fn read(&mut self, fh: u64, off: u64, size: u32) -> Vec<u8> {
        let (e, b) = self.call(READ, 0, &le(&[&fh.to_le_bytes(), &off.to_le_bytes(), &size.to_le_bytes(), &[0; 20]]));
        assert_eq!(e, 0);
        b
    }

    fn dir_names(&mut self, node: u64) -> Vec<String> {
        let (e, b) = self.call(OPENDIR, node, &[0; 8]);
        assert_eq!(e, 0);
        let fh = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let (e, b) = self.call(READDIR, 0, &le(&[&fh.to_le_bytes(), &0u64.to_le_bytes(), &4096u32.to_le_bytes(), &[0; 20]]));
        assert_eq!(e, 0);
        let mut names = Vec::new();
        let mut i = 0;
        while i + 24 <= b.len() {
            let n = u32::from_le_bytes(b[i + 16..i + 20].try_into().unwrap()) as usize;
            names.push(String::from_utf8(b[i + 24..i + 24 + n].to_vec()).unwrap());
            i += (24 + n).div_ceil(8) * 8;
        }
        names.sort();
        names
    }
}

#[test]
fn browse_and_read() {
    let mut t = T::new(true);
    assert_eq!(t.dir_names(ROOT), vec![".", "..", "escape", "sub"]);
    let sub = t.lookup(ROOT, "sub").unwrap();
    let f = t.lookup(sub, "hello.txt").unwrap();
    let (e, attr) = t.call(GETATTR, f, &[0; 16]);
    assert_eq!(e, 0);
    assert_eq!(u64::from_le_bytes(attr[24..32].try_into().unwrap()), 11); // size
    let fh = t.open(f, 0).unwrap();
    assert_eq!(t.read(fh, 6, 100), b"world");
    assert_eq!(t.call(RELEASE, 0, &le(&[&fh.to_le_bytes(), &[0; 16]])).0, 0);
    // Same path, same node.
    assert_eq!(t.lookup(sub, "hello.txt").unwrap(), f);
}

#[test]
fn no_way_out() {
    let mut t = T::new(false);
    for bad in ["..", "a/b", ""] {
        assert_eq!(t.lookup(ROOT, bad), Err(EINVAL), "{bad:?}");
    }
    // "." is the node itself, never its parent.
    assert_eq!(t.lookup(ROOT, "."), Ok(ROOT));
    // The symlink is visible, but never followed.
    let esc = t.lookup(ROOT, "escape").unwrap();
    assert_eq!(t.lookup(esc, "."), Ok(esc));
    assert!(t.lookup(esc, "secret").is_err());
    assert!(t.open(esc, 0).is_err());
    assert!(t.call(OPENDIR, esc, &[0; 8]).0 < 0);
    // Can't create links or devices.
    assert_eq!(t.call(6, ROOT, &le(&[&name("l"), &name("/etc/passwd")])).0, -EPERM);
    assert_eq!(t.call(13, ROOT, &le(&[&esc.to_le_bytes(), &name("h")])).0, -EPERM);
    // Unknown node ids are just "no such file".
    assert_eq!(t.call(GETATTR, 999, &[0; 16]).0, -ENOENT);
}

#[test]
fn read_only_means_read_only() {
    let mut t = T::new(true);
    let sub = t.lookup(ROOT, "sub").unwrap();
    let f = t.lookup(sub, "hello.txt").unwrap();
    assert_eq!(t.open(f, 2), Err(EROFS));
    assert_eq!(t.call(MKDIR, ROOT, &le(&[&0o755u32.to_le_bytes(), &[0; 4], &name("d")])).0, -EROFS);
    assert_eq!(t.call(UNLINK, sub, &name("hello.txt")).0, -EROFS);
    assert_eq!(t.call(CREATE, ROOT, &le(&[&[0; 4], &0o644u32.to_le_bytes(), &[0; 8], &name("n")])).0, -EROFS);
    let mut setattr = vec![0u8; 88];
    setattr[0..4].copy_from_slice(&FATTR_SIZE.to_le_bytes());
    assert_eq!(t.call(SETATTR, f, &setattr).0, -EROFS);
    assert!(std::fs::read(t.root.join("sub/hello.txt")).unwrap() == b"hello world");
}

#[test]
fn write_rename_remove() {
    let mut t = T::new(false);
    let (e, b) = t.call(CREATE, ROOT, &le(&[&0o1102u32.to_le_bytes(), &0o644u32.to_le_bytes(), &0o022u32.to_le_bytes(), &[0; 4], &name("new.txt")]));
    assert_eq!(e, 0);
    let node = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let fh = u64::from_le_bytes(b[128..136].try_into().unwrap());
    let data = b"written by the guest";
    let w = le(&[&fh.to_le_bytes(), &0u64.to_le_bytes(), &(data.len() as u32).to_le_bytes(), &[0; 20], data]);
    assert_eq!(t.call(WRITE, 0, &w).0, 0);
    assert_eq!(std::fs::read(t.root.join("new.txt")).unwrap(), data);
    // Rename into sub/: the node follows the file.
    let sub = t.lookup(ROOT, "sub").unwrap();
    let r = le(&[&sub.to_le_bytes(), &name("new.txt"), &name("moved.txt")]);
    assert_eq!(t.call(RENAME, ROOT, &r).0, 0);
    assert_eq!(t.call(GETATTR, node, &[0; 16]).0, 0);
    assert!(t.root.join("sub/moved.txt").exists());
    // Remove: the node goes stale.
    assert_eq!(t.call(UNLINK, sub, &name("moved.txt")).0, 0);
    assert_eq!(t.call(GETATTR, node, &[0; 16]).0, -ENOENT);
    // mkdir / rmdir.
    assert_eq!(t.call(MKDIR, ROOT, &le(&[&0o755u32.to_le_bytes(), &[0; 4], &name("d")])).0, 0);
    assert!(Path::new(&t.root.join("d")).is_dir());
    assert_eq!(t.call(RMDIR, ROOT, &name("d")).0, 0);
}

#[test]
fn garbage_is_refused_not_fatal() {
    let mut t = T::new(false);
    // Short / lying headers.
    assert!(t.s.handle(&[1, 2, 3], 4096).is_none());
    let mut r = vec![0u8; 40];
    r[0..4].copy_from_slice(&1000u32.to_le_bytes()); // len past the end
    r[4..8].copy_from_slice(&LOOKUP.to_le_bytes());
    assert!(t.s.handle(&r, 4096).is_some());
    // Names without NUL, truncated bodies, unknown ops, forgets of nothing.
    assert_eq!(t.call(LOOKUP, ROOT, b"sub").0, -EINVAL);
    assert_eq!(t.call(READ, 0, &[1, 2]).0, -EINVAL);
    assert_eq!(t.call(9999, ROOT, &[]).0, -ENOSYS);
    assert!(t.raw(FORGET, 12345, &7u64.to_le_bytes()).is_none());
    // Reads never exceed the room in the reply buffer.
    let sub = t.lookup(ROOT, "sub").unwrap();
    let f = t.lookup(sub, "hello.txt").unwrap();
    let fh = t.open(f, 0).unwrap();
    t.unique += 1;
    let mut q = Vec::new();
    for v in [&80u32.to_le_bytes()[..], &READ.to_le_bytes(), &t.unique.to_le_bytes(), &0u64.to_le_bytes(), &[0; 16]] {
        q.extend_from_slice(v);
    }
    q.extend_from_slice(&le(&[&fh.to_le_bytes(), &0u64.to_le_bytes(), &u32::MAX.to_le_bytes(), &[0; 20]]));
    let rep = t.s.handle(&q, 4).unwrap();
    assert_eq!(&rep[16..], b"hell");
}

#[test]
fn nothing_before_init() {
    let td = tempfile::tempdir().unwrap();
    let mut s = FuseServer::new(Share::open(td.path(), true).unwrap());
    let mut r = vec![0u8; 40];
    r[0..4].copy_from_slice(&40u32.to_le_bytes());
    r[4..8].copy_from_slice(&GETATTR.to_le_bytes());
    let rep = s.handle(&r, 4096).unwrap();
    assert_eq!(i32::from_le_bytes(rep[4..8].try_into().unwrap()), -EIO);
}

#[test]
fn snapshot_keeps_nodes_and_open_files() {
    let mut t = T::new(true);
    let sub = t.lookup(ROOT, "sub").unwrap();
    let f = t.lookup(sub, "hello.txt").unwrap();
    let fh = t.open(f, 0).unwrap();
    let snap = serde_json::to_vec(&t.s.save()).unwrap();
    let mut s2 = FuseServer::new(Share::open(&t.root, true).unwrap());
    s2.restore(serde_json::from_slice(&snap).unwrap());
    t.s = s2;
    assert_eq!(t.read(fh, 0, 5), b"hello");
    assert_eq!(t.lookup(sub, "hello.txt").unwrap(), f);
    // A doctored snapshot can't smuggle in a path that escapes.
    let mut bad: Snap = serde_json::from_slice(&snap).unwrap();
    bad.nodes.push((77, Node { path: Some(vec![b"..".to_vec(), b"secret".to_vec()]), lookups: 1 }));
    let mut s3 = FuseServer::new(Share::open(&t.root, true).unwrap());
    s3.restore(bad);
    t.s = s3;
    assert_eq!(t.call(GETATTR, 77, &[0; 16]).0, -ENOENT);
}

impl T {
    fn opendir(&mut self, node: u64) -> u64 {
        let (e, b) = self.call(OPENDIR, node, &[0; 8]);
        assert_eq!(e, 0);
        u64::from_le_bytes(b[0..8].try_into().unwrap())
    }

    /// READDIRPLUS: (node id, mode, name) per entry.
    fn readdir_plus(&mut self, fh: u64, off: u64, size: u32) -> Vec<(u64, u32, String)> {
        let (e, b) = self.call(READDIRPLUS, 0, &le(&[&fh.to_le_bytes(), &off.to_le_bytes(), &size.to_le_bytes(), &[0; 20]]));
        assert_eq!(e, 0);
        let mut out = Vec::new();
        let mut i = 0;
        while i + 128 + 24 <= b.len() {
            let id = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
            let mode = u32::from_le_bytes(b[i + 40 + 60..i + 40 + 64].try_into().unwrap());
            let d = i + 128;
            let n = u32::from_le_bytes(b[d + 16..d + 20].try_into().unwrap()) as usize;
            out.push((id, mode, String::from_utf8(b[d + 24..d + 24 + n].to_vec()).unwrap()));
            i = d + (24 + n).div_ceil(8) * 8;
        }
        assert_eq!(i, b.len());
        out
    }

    /// SETATTR with `valid` bits and a mode: (errno, mode after).
    fn setattr(&mut self, node: u64, valid: u32, mode: u32) -> (i32, u32) {
        let mut a = vec![0u8; 88];
        a[0..4].copy_from_slice(&valid.to_le_bytes());
        a[68..72].copy_from_slice(&mode.to_le_bytes());
        let (e, b) = self.call(SETATTR, node, &a);
        let m = if e == 0 { u32::from_le_bytes(b[16 + 60..16 + 64].try_into().unwrap()) } else { 0 };
        (e, m)
    }

    /// CREATE the way virtiofs.exe does: (node, fh).
    fn create(&mut self, parent: u64, n: &str) -> (u64, u64) {
        let (e, b) = self.call(CREATE, parent, &le(&[&(1u32 | O_EXCL).to_le_bytes(), &0o664u32.to_le_bytes(), &[0; 8], &name(n)]));
        assert_eq!(e, 0, "create {n}");
        (u64::from_le_bytes(b[0..8].try_into().unwrap()), u64::from_le_bytes(b[128..136].try_into().unwrap()))
    }

    fn fallocate(&mut self, fh: u64, off: u64, len: u64, mode: u32) -> i32 {
        self.call(FALLOCATE, 0, &le(&[&fh.to_le_bytes(), &off.to_le_bytes(), &len.to_le_bytes(), &mode.to_le_bytes(), &[0; 4]])).0
    }

    fn rename(&mut self, from: u64, old: &str, to: u64, new: &str, flags: u32) -> i32 {
        self.call(RENAME2, from, &le(&[&to.to_le_bytes(), &flags.to_le_bytes(), &[0; 4], &name(old), &name(new)])).0
    }

    /// The node table and its byte count agree.
    fn consistent(&self) {
        let mut bytes = 0;
        for (id, n) in &self.s.nodes {
            if let Some(p) = &n.path {
                bytes += plen(p);
                assert_eq!(self.s.by_path.get(p), Some(id));
            }
        }
        assert_eq!(bytes, self.s.path_bytes);
        assert!(self.s.by_path.values().all(|id| self.s.nodes.contains_key(id)));
    }
}

fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(p).unwrap().permissions().mode()
}

#[test]
fn windows_root_and_listing() {
    let mut t = T::windows(false);
    // The root is looked up as ".".
    assert_eq!(t.lookup(ROOT, "."), Ok(ROOT));
    assert_eq!(t.lookup(ROOT, ".."), Err(EINVAL));
    let fh = t.opendir(ROOT);
    let list = t.readdir_plus(fh, 0, 64 << 10);
    let names: Vec<&str> = list.iter().map(|e| e.2.as_str()).collect();
    assert_eq!(&names[..2], [".", ".."]);
    let mut rest = names[2..].to_vec();
    rest.sort();
    assert_eq!(rest, ["escape", "sub"]);
    for (id, mode, n) in &list {
        match n.as_str() {
            // Symlinks get a node (the client reads them), nothing else does.
            "escape" => assert!(*id > ROOT && mode & S_IFMT == S_IFLNK),
            _ => assert_eq!((*id, mode & S_IFMT), (0, 0o040000), "{n}"),
        }
    }
    // Small buffers: one entry at a time, offsets carry on.
    let one = t.readdir_plus(fh, 1, 128 + 32);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].2, "..");
    // The same handle at offset 0 sees what changed on the host.
    std::fs::write(t.root.join("later.txt"), b"x").unwrap();
    let again = t.readdir_plus(fh, 0, 64 << 10);
    assert!(again.iter().any(|e| e.2 == "later.txt"), "{again:?}");
    // Plain READDIR has the dots too: two entries means empty.
    std::fs::create_dir(t.root.join("empty")).unwrap();
    let d = t.lookup(ROOT, "empty").unwrap();
    assert_eq!(t.dir_names(d), [".", ".."]);
    t.consistent();
}

#[test]
fn windows_read_only() {
    let mut t = T::windows(true);
    let sub = t.lookup(ROOT, "sub").unwrap();
    let f = t.lookup(sub, "hello.txt").unwrap();
    // No write bits shown: Windows refuses before asking.
    let (e, attr) = t.call(GETATTR, f, &[0; 16]);
    assert_eq!(e, 0);
    assert_eq!(u32::from_le_bytes(attr[16 + 60..16 + 64].try_into().unwrap()) & 0o222, 0);
    // Asking anyway is "access denied", not "unexpected error".
    assert_eq!(t.open(f, 1), Err(EACCES));
    assert_eq!(t.call(UNLINK, sub, &name("hello.txt")).0, -EACCES);
    assert_eq!(t.setattr(f, FATTR_MODE, 0o664).0, -EACCES);
    assert_eq!(t.rename(sub, "hello.txt", ROOT, "x", 0), -EACCES);
    let fh = t.open(f, 0).unwrap();
    assert_eq!(t.fallocate(fh, 0, 100, 0), -EACCES);
    assert_eq!(std::fs::read(t.root.join("sub/hello.txt")).unwrap(), b"hello world");
    // Linux gets EROFS for the same thing.
    let mut l = T::new(true);
    let sub = l.lookup(ROOT, "sub").unwrap();
    assert_eq!(l.call(UNLINK, sub, &name("hello.txt")).0, -EROFS);
}

#[test]
fn windows_mode_is_the_read_only_bit() {
    use std::os::unix::fs::PermissionsExt;
    let mut t = T::windows(false);
    let sub = t.lookup(ROOT, "sub").unwrap();
    let before = mode_of(&t.root.join("sub"));
    // virtiofs.exe sends 0664 for a directory when it closes it: the
    // directory must stay a usable directory.
    assert_eq!(t.setattr(sub, FATTR_MODE, 0o664).0, 0);
    assert_eq!(mode_of(&t.root.join("sub")), before);
    // Read-only means nothing on a Windows directory (robocopy sets it
    // when it copies one from a read-only share).
    assert_eq!(t.setattr(sub, FATTR_MODE, 0o40555).0, 0);
    assert_eq!(mode_of(&t.root.join("sub")), before);
    let f = t.lookup(sub, "hello.txt").unwrap();
    std::fs::set_permissions(t.root.join("sub/hello.txt"), std::fs::Permissions::from_mode(0o755)).unwrap();
    // Read-only attribute: write bits go, x stays.
    assert_eq!(t.setattr(f, FATTR_MODE, 0o444), (0, 0o100555));
    // Cleared again: the owner can write.
    assert_eq!(t.setattr(f, FATTR_MODE, 0o664), (0, 0o100755));
    // Linux chmod is a chmod.
    let mut l = T::new(false);
    let sub = l.lookup(ROOT, "sub").unwrap();
    let f = l.lookup(sub, "hello.txt").unwrap();
    assert_eq!(l.setattr(f, FATTR_MODE, 0o600), (0, 0o100600));
}

#[test]
fn fallocate() {
    let mut t = T::windows(false);
    let (node, fh) = t.create(ROOT, "big.bin");
    let size = |t: &T| std::fs::metadata(t.root.join("big.bin")).unwrap().len();
    // Windows: the allocation size of a new file. Size unchanged.
    assert_eq!(t.fallocate(fh, 0, 10 << 20, FALLOC_FL_KEEP_SIZE), 0);
    assert_eq!(size(&t), 0);
    // Plain: grows, never shrinks.
    assert_eq!(t.fallocate(fh, 4096, 4096, 0), 0);
    assert_eq!(size(&t), 8192);
    assert_eq!(t.fallocate(fh, 0, 10, 0), 0);
    assert_eq!(size(&t), 8192);
    // Hole punching and friends: not supported. Silly ranges: refused.
    assert_eq!(t.fallocate(fh, 0, 4096, 3), -EOPNOTSUPP);
    assert_eq!(t.fallocate(fh, u64::MAX, 2, 0), -EFBIG);
    assert_eq!(t.fallocate(fh, 0, 0, 0), -EINVAL);
    assert_eq!(t.fallocate(999, 0, 1, 0), -EBADF);
    // Not through a read-only handle.
    let rfh = t.open(node, 0).unwrap();
    assert_eq!(t.fallocate(rfh, 0, 1 << 20, 0), -EBADF);
    assert_eq!(size(&t), 8192);
}

#[test]
fn names_windows_cares_about_stay_names() {
    let mut t = T::windows(false);
    // Stream syntax and reserved device names are plain names on the
    // host: they make files inside the share and nothing else.
    for n in ["a:b", "x::$DATA", "CON", "nul.txt", "back\\slash", "trailing. "] {
        t.create(ROOT, n);
        assert!(t.root.join(n).is_file(), "{n}");
    }
    // macOS resource forks ("file/..namedfork/rsrc") are no way around:
    // a file is never walked through.
    let sub = t.lookup(ROOT, "sub").unwrap();
    let f = t.lookup(sub, "hello.txt").unwrap();
    assert!(t.lookup(f, "..namedfork").is_err());
    // No dot names besides the root's ".".
    assert_eq!(t.call(MKDIR, ROOT, &le(&[&0o755u32.to_le_bytes(), &[0; 4], &name("..")])).0, -EINVAL);
    assert_eq!(t.call(MKDIR, ROOT, &le(&[&0o755u32.to_le_bytes(), &[0; 4], &name(".")])).0, -EINVAL);
    t.consistent();
}

#[test]
fn long_paths_are_capped() {
    let mut t = T::new(false);
    let comp = "d".repeat(200);
    let mkdir = |t: &mut T, parent: u64, n: &str| t.call(MKDIR, parent, &le(&[&0o755u32.to_le_bytes(), &[0; 4], &name(n)])).0;
    assert_eq!(mkdir(&mut t, ROOT, "s"), 0);
    let mut nodes = vec![t.lookup(ROOT, "s").unwrap()];
    // "s/" + 20 * 201 bytes: 4022, under 4096.
    for _ in 0..20 {
        let parent = *nodes.last().unwrap();
        assert_eq!(mkdir(&mut t, parent, &comp), 0);
        nodes.push(t.lookup(parent, &comp).unwrap());
    }
    // One more level is too long.
    assert_eq!(mkdir(&mut t, nodes[20], &comp), -ENAMETOOLONG);
    assert_eq!(t.lookup(nodes[20], &comp), Err(ENAMETOOLONG));
    // Moving the tree under a longer name: the nodes that would be too
    // long lose their path, the others follow.
    let long = "l".repeat(255);
    assert_eq!(t.rename(ROOT, "s", ROOT, &long, 0), 0);
    assert_eq!(t.call(GETATTR, nodes[20], &[0; 16]).0, -ENOENT);
    assert_eq!(t.call(GETATTR, nodes[19], &[0; 16]).0, 0);
    t.consistent();
}

#[test]
fn rename_case_only() {
    let mut t = T::windows(false);
    let (node, _) = t.create(ROOT, "readme.txt");
    // Explorer renames with RENAME_NOREPLACE; on a case-insensitive host
    // the target "exists": it's the file itself.
    assert_eq!(t.rename(ROOT, "readme.txt", ROOT, "README.txt", RENAME_NOREPLACE), 0);
    let names: Vec<String> = std::fs::read_dir(&t.root).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert!(names.contains(&"README.txt".to_string()) && !names.contains(&"readme.txt".to_string()));
    assert_eq!(t.call(GETATTR, node, &[0; 16]).0, 0);
    // A real other file is not replaced.
    t.create(ROOT, "other.txt");
    assert_eq!(t.rename(ROOT, "README.txt", ROOT, "other.txt", RENAME_NOREPLACE), -17);
    t.consistent();
}

#[test]
fn windows_nodes_are_evicted_not_leaked() {
    let mut t = T::windows(false);
    let sub = t.lookup(ROOT, "sub").unwrap();
    let f = t.lookup(sub, "hello.txt").unwrap();
    let esc = t.lookup(ROOT, "escape").unwrap();
    let fh = t.open(f, 0).unwrap();
    t.s.evict();
    t.consistent();
    // Open handles keep their node; the rest is looked up again.
    assert_eq!(t.call(GETATTR, f, &[0; 16]).0, 0);
    assert_eq!(t.read(fh, 0, 5), b"hello");
    assert_eq!(t.call(GETATTR, esc, &[0; 16]).0, -ENOENT);
    assert_eq!(t.call(GETATTR, sub, &[0; 16]).0, -ENOENT);
    assert!(t.lookup(ROOT, "sub").unwrap() > esc);
    assert_eq!(t.call(GETATTR, ROOT, &[0; 16]).0, 0);
    // Snapshots keep the client kind.
    let snap: Snap = serde_json::from_slice(&serde_json::to_vec(&t.s.save()).unwrap()).unwrap();
    let mut s2 = FuseServer::new(Share::open(&t.root, false).unwrap());
    s2.restore(snap);
    assert!(s2.windows);
}

#[test]
fn bookkeeping_survives_renames_and_removes() {
    let mut t = T::new(false);
    for d in ["a", "a/b", "a/b/c"] {
        std::fs::create_dir(t.root.join(d)).unwrap();
    }
    let a = t.lookup(ROOT, "a").unwrap();
    let b = t.lookup(a, "b").unwrap();
    let c = t.lookup(b, "c").unwrap();
    t.consistent();
    assert_eq!(t.rename(ROOT, "a", ROOT, "zz", 0), 0);
    t.consistent();
    assert_eq!(t.s.path(c).unwrap(), vec![b"zz".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    assert_eq!(t.call(RMDIR, b, &name("c")).0, 0);
    t.consistent();
    assert!(t.raw(FORGET, c, &1u64.to_le_bytes()).is_none());
    t.consistent();
    assert!(!t.s.nodes.contains_key(&c));
}
