//! State directory housekeeping: one VMM per state dir, listing and
//! deleting snapshots, and pruning disk layers nothing needs any more.
//!
//! Layers form a tree (`<state>/disks/*.qcow2`, each backed by its parent,
//! the configured image at the root). A layer is needed if a disk pointer,
//! a snapshot or the suspend image names it, or if a needed layer is built
//! on it. Unneeded layers are deleted; a layer only kept alive by a single
//! child is folded into that child, so chains don't grow without bound.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use vroom_block::qcow2::{fold_backing, OpenPolicy};
use vroom_snapshot::SnapshotDir;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Another vroom process (a running VM) holds the state dir.
    #[error("{0} is in use by another vroom process")]
    Busy(String),
    #[error("{0}")]
    Setup(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Held for as long as a VMM (or a housekeeping command) uses a state dir.
#[derive(Debug)]
pub struct StateLock(File);

impl StateLock {
    pub fn acquire(dir: &Path) -> Result<StateLock> {
        std::fs::create_dir_all(dir)?;
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(true).create(true).truncate(false);
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        // The VMM can write this dir: a link or FIFO planted as `lock` must
        // not make us create files elsewhere or hang.
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let f = o.open(dir.join("lock"))?;
        // A probe (`in_use`) or a forked child that hasn't exec'd yet can
        // hold it for a moment: retry briefly.
        for _ in 0..20 {
            // SAFETY: valid fd we own; flock has no memory effects.
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(StateLock(f));
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(Error::Io(e));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err(Error::Busy(dir.display().to_string()))
    }

    /// Is a VMM running on this state dir? Takes a shared lock for an
    /// instant; never creates anything.
    pub fn in_use(dir: &Path) -> bool {
        let mut o = std::fs::OpenOptions::new();
        o.read(true);
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let Ok(f) = o.open(dir.join("lock")) else { return false };
        // SAFETY: valid fd we own; the lock goes away with `f`.
        let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
        r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
    }

    /// Bytes free for us on the state dir's filesystem (disk layers grow
    /// there). Works under the sandbox: no path lookup.
    pub fn free_bytes(&self) -> std::io::Result<u64> {
        free_bytes(&self.0)
    }
}

/// Bytes free for us on the file system holding `f`.
pub fn free_bytes(f: &File) -> std::io::Result<u64> {
    // SAFETY: zeroed statfs is valid plain data.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd and out pointer.
    if unsafe { libc::fstatfs(f.as_raw_fd(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st.f_bavail as u64 * st.f_bsize as u64)
}

#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub name: String,
    pub path: PathBuf,
    pub created_unix: u64,
    pub description: String,
    /// Bytes on disk for the snapshot itself (not its disk layers).
    pub bytes: u64,
}

fn snapshots_dir(state: &Path) -> PathBuf {
    state.join("snapshots")
}

fn dir_bytes(d: &Path) -> u64 {
    std::fs::read_dir(d)
        .map(|it| it.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum())
        .unwrap_or(0)
}

/// Snapshots, oldest first.
pub fn list(state: &Path) -> Result<Vec<SnapshotInfo>> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(snapshots_dir(state)) else { return Ok(out) };
    for e in rd.flatten() {
        let Ok(s) = SnapshotDir::open(&e.path()) else { continue };
        out.push(SnapshotInfo {
            name: e.file_name().to_string_lossy().into_owned(),
            path: e.path(),
            created_unix: s.manifest.created_unix,
            description: s.manifest.description.clone(),
            bytes: dir_bytes(&e.path()),
        });
    }
    out.sort_by_key(|s| s.created_unix);
    Ok(out)
}

#[derive(Debug, Default)]
pub struct PruneReport {
    pub deleted: Vec<PathBuf>,
    pub folded: Vec<PathBuf>,
}

/// Delete snapshot `name`, then prune layers. The caller holds the lock.
pub fn delete(state: &Path, name: &str, base_dirs: &[PathBuf]) -> Result<PruneReport> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return Err(Error::Setup(format!("bad snapshot name {name:?}")));
    }
    real_dir(&snapshots_dir(state))?;
    let dir = snapshots_dir(state).join(name);
    SnapshotDir::open(&dir).map_err(|e| Error::Setup(format!("snapshot {name}: {e}")))?;
    std::fs::remove_dir_all(&dir)?;
    prune(state, base_dirs)
}

/// Layer paths something points at directly.
fn roots(state: &Path) -> Result<(BTreeSet<PathBuf>, BTreeSet<PathBuf>)> {
    let mut roots = BTreeSet::new();
    // Layers whose bytes a suspend image pins (it checks size + mtime).
    let mut pinned = BTreeSet::new();
    let disks = state.join("disks");
    for e in std::fs::read_dir(&disks)?.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json") {
            let v: serde_json::Value = serde_json::from_slice(&read_small(&p)?).map_err(|e| Error::Setup(e.to_string()))?;
            if let Some(a) = v.get("active").and_then(|a| a.as_str()) {
                roots.insert(canon(Path::new(a)));
            }
        }
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(snapshots_dir(state)).map(|it| it.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    dirs.push(state.join("suspend"));
    for d in dirs {
        let Ok(s) = SnapshotDir::open(&d) else { continue };
        for (k, v) in &s.manifest.meta {
            if k.starts_with("disk.") {
                roots.insert(canon(Path::new(v)));
            } else if k.starts_with("active.") {
                let p = canon(Path::new(v.split_once('|').map(|(p, _)| p).unwrap_or(v)));
                roots.insert(p.clone());
                pinned.insert(p);
            }
        }
    }
    Ok((roots, pinned))
}

/// Disk pointers are a line of JSON. Refuses links, FIFOs and big files:
/// a VMM writes these.
fn read_small(p: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    const MAX: u64 = 1 << 20;
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let f = o.open(p)?;
    if !f.metadata()?.is_file() {
        return Err(Error::Setup(format!("{}: not a file", p.display())));
    }
    let mut v = Vec::new();
    f.take(MAX + 1).read_to_end(&mut v)?;
    if v.len() as u64 > MAX {
        return Err(Error::Setup(format!("{}: too large", p.display())));
    }
    Ok(v)
}

/// A directory in the state dir, not a link to one somewhere else (we
/// delete files in there).
fn real_dir(p: &Path) -> Result<()> {
    if std::fs::symlink_metadata(p)?.is_dir() {
        Ok(())
    } else {
        Err(Error::Setup(format!("{} is not a directory", p.display())))
    }
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Backing file of a layer. A layer we can't open stops everything: guessing
/// would make its parent look unused and get it deleted.
fn parent_of(layer: &Path, policy: &OpenPolicy) -> Result<Option<PathBuf>> {
    let q = vroom_block::qcow2::Qcow2Image::open_with(layer, true, policy, 0)
        .map_err(|e| Error::Setup(format!("layer {}: {e}; not pruning", layer.display())))?;
    Ok(q.backing_path().map(canon))
}

/// Delete unneeded layers and fold single-child intermediates.
pub fn prune(state: &Path, base_dirs: &[PathBuf]) -> Result<PruneReport> {
    real_dir(&state.join("disks"))?;
    let disks = canon(&state.join("disks"));
    let mut allowed = vec![disks.clone()];
    allowed.extend(base_dirs.iter().cloned());
    let policy = OpenPolicy { allowed_dirs: allowed, ..Default::default() };
    let mut report = PruneReport::default();
    loop {
        let (roots, pinned) = roots(state)?;
        // Regular files only: following a link the VMM planted would make
        // its target (anywhere) a layer we may delete.
        let layers: Vec<PathBuf> = std::fs::read_dir(&disks)?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "qcow2"))
            .collect();
        let parent: BTreeMap<PathBuf, Option<PathBuf>> =
            layers.iter().map(|l| Ok((l.clone(), parent_of(l, &policy)?))).collect::<Result<_>>()?;
        // Needed: roots and everything under them.
        let mut needed = BTreeSet::new();
        for r in &roots {
            let mut cur = Some(r.clone());
            while let Some(c) = cur {
                if !needed.insert(c.clone()) {
                    break;
                }
                cur = parent.get(&c).cloned().flatten();
            }
        }
        let mut changed = false;
        for l in &layers {
            if !needed.contains(l) {
                std::fs::remove_file(l)?;
                report.deleted.push(l.clone());
                changed = true;
            }
        }
        if changed {
            continue;
        }
        // A needed layer no one names directly, with one needed child: fold.
        let mut children: BTreeMap<&PathBuf, Vec<&PathBuf>> = BTreeMap::new();
        for (c, p) in &parent {
            if let Some(p) = p {
                if needed.contains(c) {
                    children.entry(p).or_default().push(c);
                }
            }
        }
        let fold = layers.iter().find_map(|l| match children.get(l).map(Vec::as_slice) {
            Some([only]) if !roots.contains(l) && !pinned.contains(*only) && l.starts_with(&disks) => Some((*only).clone()),
            _ => None,
        });
        let Some(child) = fold else { break };
        let gone = fold_backing(&child, &policy).map_err(|e| Error::Setup(format!("fold into {}: {e}", child.display())))?;
        std::fs::remove_file(&gone)?;
        report.folded.push(gone);
    }
    Ok(report)
}

/// A suspend image is waiting: the next start resumes it.
pub fn is_suspended(state: &Path) -> bool {
    state.join("suspend").join("manifest.json").is_file()
}

/// Throw the suspend image away: the next start boots fresh. The caller
/// holds the lock. Its disk layers go with the next prune.
pub fn discard_suspend(state: &Path, base_dirs: &[PathBuf]) -> Result<PruneReport> {
    let d = state.join("suspend");
    if d.exists() {
        std::fs::remove_dir_all(&d)?;
    }
    prune(state, base_dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_and_probe() {
        let t = tempfile::tempdir().unwrap();
        assert!(!StateLock::in_use(t.path()));
        let l = StateLock::acquire(t.path()).unwrap();
        assert!(StateLock::in_use(t.path()));
        assert!(matches!(StateLock::acquire(t.path()), Err(Error::Busy(_))));
        drop(l);
        assert!(!StateLock::in_use(t.path()));
        // The probe itself doesn't block a VMM from starting.
        let _l = StateLock::acquire(t.path()).unwrap();
    }

    fn mkfifo(p: &Path) {
        let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn planted_lock_files() {
        let t = tempfile::tempdir().unwrap();
        let (state, victim) = (t.path().join("vm.state"), t.path().join("victim"));
        std::fs::create_dir(&state).unwrap();
        std::os::unix::fs::symlink(&victim, state.join("lock")).unwrap();
        assert!(StateLock::acquire(&state).is_err());
        assert!(!victim.exists(), "followed the link");
        std::fs::remove_file(state.join("lock")).unwrap();
        mkfifo(&state.join("lock"));
        assert!(!StateLock::in_use(&state));
    }

    #[test]
    fn prune_stays_in_the_state_dir() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("vm.state");
        let disks = state.join("disks");
        std::fs::create_dir_all(&disks).unwrap();
        let outside = t.path().join("other");
        std::fs::create_dir(&outside).unwrap();
        let victim = outside.join("victim.qcow2");
        vroom_block::qcow2::Qcow2Image::create(&victim, 1 << 20, None).unwrap();
        vroom_block::qcow2::Qcow2Image::create(&disks.join("old.qcow2"), 1 << 20, None).unwrap();
        std::os::unix::fs::symlink(&victim, disks.join("evil.qcow2")).unwrap();
        let r = prune(&state, &[]).unwrap();
        assert_eq!(r.deleted, vec![disks.canonicalize().unwrap().join("old.qcow2")]);
        assert!(victim.exists(), "deleted a file outside the state dir");

        // The whole disks dir swapped for a link.
        std::fs::remove_dir_all(&disks).unwrap();
        std::os::unix::fs::symlink(&outside, &disks).unwrap();
        assert!(prune(&state, &[]).is_err());
        assert!(victim.exists());

        // A pointer file that never ends.
        std::fs::remove_file(&disks).unwrap();
        std::fs::create_dir(&disks).unwrap();
        mkfifo(&disks.join("vda.json"));
        assert!(prune(&state, &[]).is_err());
    }
}
