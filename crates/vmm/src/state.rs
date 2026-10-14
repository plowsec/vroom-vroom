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

use bulwark_block::qcow2::{fold_backing, OpenPolicy};
use bulwark_snapshot::SnapshotDir;

use crate::{Error, Result};

/// Held for as long as a VMM (or a housekeeping command) uses a state dir.
#[derive(Debug)]
pub struct StateLock(File);

impl StateLock {
    pub fn acquire(dir: &Path) -> Result<StateLock> {
        std::fs::create_dir_all(dir)?;
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(true).create(true).truncate(false);
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let f = o.open(dir.join("lock"))?;
        // SAFETY: valid fd we own; flock has no memory effects.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                Error::Setup(format!("{} is in use by another bulwark process", dir.display()))
            } else {
                Error::Io(e)
            });
        }
        Ok(StateLock(f))
    }

    /// Bytes free for us on the state dir's filesystem (disk layers grow
    /// there). Works under the sandbox: no path lookup.
    pub fn free_bytes(&self) -> std::io::Result<u64> {
        // SAFETY: zeroed statfs is valid; fd is ours.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd and out pointer.
        if unsafe { libc::fstatfs(self.0.as_raw_fd(), &mut st) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(st.f_bavail as u64 * st.f_bsize as u64)
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub name: String,
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
            let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p)?).map_err(|e| Error::Setup(e.to_string()))?;
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

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Backing file of a layer. A layer we can't open stops everything: guessing
/// would make its parent look unused and get it deleted.
fn parent_of(layer: &Path, policy: &OpenPolicy) -> Result<Option<PathBuf>> {
    let q = bulwark_block::qcow2::Qcow2Image::open_with(layer, true, policy, 0)
        .map_err(|e| Error::Setup(format!("layer {}: {e}; not pruning", layer.display())))?;
    Ok(q.backing_path().map(canon))
}

/// Delete unneeded layers and fold single-child intermediates.
pub fn prune(state: &Path, base_dirs: &[PathBuf]) -> Result<PruneReport> {
    let disks = canon(&state.join("disks"));
    let mut allowed = vec![disks.clone()];
    allowed.extend(base_dirs.iter().cloned());
    let policy = OpenPolicy { allowed_dirs: allowed, ..Default::default() };
    let mut report = PruneReport::default();
    loop {
        let (roots, pinned) = roots(state)?;
        let layers: Vec<PathBuf> = std::fs::read_dir(&disks)?
            .flatten()
            .map(|e| canon(&e.path()))
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
