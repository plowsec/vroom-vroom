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

impl T {
    fn new(readonly: bool) -> T {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("share");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/hello.txt"), b"hello world").unwrap();
        std::fs::write(td.path().join("secret"), b"outside").unwrap();
        std::os::unix::fs::symlink(td.path(), root.join("escape")).unwrap();
        let s = FuseServer::new(Share::open(&root, readonly).unwrap());
        let mut t = T { s, unique: 0, _td: td, root };
        let (e, _) = t.call(INIT, 0, &le(&[&7u32.to_le_bytes(), &31u32.to_le_bytes(), &[0; 8]]));
        assert_eq!(e, 0);
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
    assert_eq!(t.dir_names(ROOT), vec!["escape", "sub"]);
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
    for bad in ["..", ".", "a/b", ""] {
        assert_eq!(t.lookup(ROOT, bad), Err(EINVAL), "{bad:?}");
    }
    // The symlink is visible, but never followed.
    let esc = t.lookup(ROOT, "escape").unwrap();
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
