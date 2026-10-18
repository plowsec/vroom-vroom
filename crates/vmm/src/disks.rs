//! VM disks and snapshot layering (VMware-style delta disks).
//!
//! The configured image is the bottom of a chain. Taking a snapshot
//! freezes the current top layer and continues on a new qcow2 overlay in
//! `<state>/disks/`. Reverting starts a fresh overlay on the snapshot's
//! frozen layer. All new files live in the state dir, so the sandboxed
//! VMM only ever needs write access there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use vroom_block::qcow2::{OpenPolicy, Qcow2Image};
#[cfg(feature = "research")]
use vroom_block::rollback::RollbackDisk;
use vroom_block::{DiskImage, Format, SwapDisk, SyncMode};
use vroom_config::{DiskFormat, DiskSync, VmConfig};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Where disk `i` stands now (persisted in `<state>/disks/disk<i>.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pointer {
    active: PathBuf,
}

#[derive(Debug)]
pub(crate) struct VmDisk {
    pub index: usize,
    pub image: Arc<SwapDisk>,
    /// Current top layer on disk (None for read-only disks).
    active: Option<PathBuf>,
    active_fmt: Format,
    sync: SyncMode,
}

#[derive(Debug)]
pub(crate) struct DiskSet {
    dir: PathBuf,
    pub disks: Vec<VmDisk>,
    /// Directories of writable images outside the state dir (opened now,
    /// before the sandbox): their file systems fill up too.
    watch: Vec<std::fs::File>,
    /// In-memory top layers while research resets are armed.
    #[cfg(feature = "research")]
    rollback: Vec<Arc<RollbackDisk>>,
    /// Overlays `open` started on snapshot layers: (overlay, pointer file,
    /// what the pointer file held before).
    #[cfg(feature = "research")]
    fresh: Vec<(PathBuf, PathBuf, Option<Vec<u8>>)>,
    /// Delete `fresh` on close and put the pointers back.
    #[cfg(feature = "research")]
    discard_fresh: bool,
}

/// RAM a disk's research rollback layer may hold; more spills to a
/// scratch file in the disks dir.
#[cfg(feature = "research")]
const ROLLBACK_MEMORY: u64 = 64 << 20;

fn sync_of(s: DiskSync) -> SyncMode {
    match s {
        DiskSync::Full => SyncMode::Full,
        DiskSync::Fsync => SyncMode::Fsync,
        DiskSync::None => SyncMode::None,
    }
}

fn fmt_of(f: DiskFormat) -> Option<Format> {
    match f {
        DiskFormat::Auto => None,
        DiskFormat::Raw => Some(Format::Raw),
        DiskFormat::Qcow2 => Some(Format::Qcow2),
    }
}

fn stamp() -> String {
    let d = std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{}-{:09}", d.as_secs(), d.subsec_nanos())
}

/// Identity of a file for "did it change?" checks.
pub(crate) fn file_stamp(p: &Path) -> Result<String> {
    let m = std::fs::metadata(p)?;
    let mt = m.modified()?.duration_since(UNIX_EPOCH).unwrap_or_default();
    Ok(format!("{}:{}.{}", m.len(), mt.as_secs(), mt.subsec_nanos()))
}

impl DiskSet {
    /// Open all disks. `restore`: disk index -> frozen layer to start a
    /// fresh overlay on (reverting to a snapshot).
    pub fn open(cfg: &VmConfig, state_dir: &Path, restore: &BTreeMap<usize, PathBuf>) -> Result<DiskSet> {
        // Absolute now, while we may still look up the cwd: overlays made
        // later (under the sandbox) record these paths as backing names.
        let dir = std::path::absolute(state_dir.join("disks"))?;
        std::fs::create_dir_all(&dir)?;
        let mut disks = Vec::new();
        #[cfg(feature = "research")]
        let mut fresh = Vec::new();
        for (i, d) in cfg.disks.iter().enumerate() {
            let policy = OpenPolicy {
                allowed_dirs: vec![dir.clone(), d.path.parent().map(Path::to_path_buf).unwrap_or_default()],
                sync: sync_of(d.sync),
            };
            if d.readonly {
                let img = vroom_block::open_with(&d.path, fmt_of(d.format), true, &policy)
                    .map_err(|e| Error::Setup(format!("disk {}: {e}", d.path.display())))?;
                disks.push(VmDisk { index: i, image: SwapDisk::new(img), active: None, active_fmt: Format::Raw, sync: policy.sync });
                continue;
            }
            let ptr_path = dir.join(format!("disk{i}.json"));
            let (active, fmt) = if let Some(layer) = restore.get(&i) {
                // Revert: new overlay on the frozen layer.
                let lower = vroom_block::open_with(layer, None, true, &policy)
                    .map_err(|e| Error::Setup(format!("snapshot layer {}: {e}", layer.display())))?;
                let lfmt = vroom_block::detect(layer).map_err(|e| Error::Setup(e.to_string()))?;
                let top = dir.join(format!("disk{i}-{}.qcow2", stamp()));
                Qcow2Image::create(&top, lower.size(), Some((layer, lfmt))).map_err(|e| Error::Setup(e.to_string()))?;
                #[cfg(feature = "research")]
                fresh.push((top.clone(), ptr_path.clone(), std::fs::read(&ptr_path).ok()));
                (top, Format::Qcow2)
            } else {
                match std::fs::read(&ptr_path).ok().and_then(|b| serde_json::from_slice::<Pointer>(&b).ok()) {
                    // Only trust pointers into our own disks dir: take the
                    // file name and look for it there (older pointers were
                    // written with paths relative to wherever we ran from).
                    Some(p) => match p.active.file_name().map(|n| dir.join(n)) {
                        Some(f) if f.is_file() => (f, Format::Qcow2),
                        _ if p.active.file_name() == d.path.file_name() => {
                            (std::path::absolute(&d.path)?, fmt_of(d.format).unwrap_or(Format::Raw))
                        }
                        _ => {
                            return Err(Error::Setup(format!(
                                "disk {}: state points at {} which is missing; refusing to fall back to the base image",
                                d.path.display(),
                                p.active.display()
                            )))
                        }
                    },
                    None => (std::path::absolute(&d.path)?, fmt_of(d.format).unwrap_or(Format::Raw)),
                }
            };
            let fmt = if active == std::path::absolute(&d.path)? { fmt_of(d.format) } else { Some(fmt) };
            let img = vroom_block::open_with(&active, fmt, false, &policy)
                .map_err(|e| Error::Setup(format!("disk {}: {e}", active.display())))?;
            let active_fmt = fmt.unwrap_or(Format::Raw);
            write_pointer(&ptr_path, &active)?;
            disks.push(VmDisk { index: i, image: SwapDisk::new(img), active: Some(active), active_fmt, sync: policy.sync });
        }
        let watch = disks
            .iter()
            .filter_map(|d| d.active.as_ref())
            .filter(|a| !a.starts_with(&dir))
            .filter_map(|a| a.parent().and_then(|p| std::fs::File::open(p).ok()))
            .collect();
        Ok(DiskSet {
            dir,
            disks,
            watch,
            #[cfg(feature = "research")]
            rollback: Vec::new(),
            #[cfg(feature = "research")]
            fresh,
            #[cfg(feature = "research")]
            discard_fresh: false,
        })
    }

    /// The overlays `open` started on snapshot layers are scratch (a
    /// fuzzer restoring the same checkpoint again and again): delete them
    /// when the disks close and put the state's disk pointers back, instead
    /// of leaving one more overlay behind per restore.
    #[cfg(feature = "research")]
    pub fn discard_fresh_layers(&mut self) {
        self.discard_fresh = true;
    }

    /// Keep the guest's writes from now on in memory, over every writable
    /// disk, until [`DiskSet::rollback`] drops them. Research resets put
    /// RAM back in place; without this the disk keeps what each testcase
    /// wrote, and the next one's page cache no longer matches it.
    /// The VM must be paused.
    #[cfg(feature = "research")]
    pub fn arm_rollback(&mut self) -> Result<()> {
        if !self.rollback.is_empty() {
            return Err(Error::Setup("disk rollback is already armed".into()));
        }
        for d in &self.disks {
            if d.active.is_none() {
                continue;
            }
            d.image.flush().map_err(|e| Error::Setup(e.to_string()))?;
            let layer = Arc::new(RollbackDisk::new(d.image.current(), self.dir.clone(), ROLLBACK_MEMORY));
            d.image.swap(layer.clone()).map_err(|e| Error::Setup(e.to_string()))?;
            self.rollback.push(layer);
        }
        Ok(())
    }

    /// Forget what the guest wrote since [`DiskSet::arm_rollback`]. The VM
    /// must be paused (no request running). Returns the bytes dropped.
    #[cfg(feature = "research")]
    pub fn rollback(&self) -> Result<u64> {
        let mut dropped = 0;
        for l in &self.rollback {
            dropped += l.held_bytes();
            l.rollback().map_err(|e| Error::Setup(format!("disk rollback: {e}")))?;
        }
        Ok(dropped)
    }

    /// Snapshots and suspends record files, which don't hold what the
    /// rollback layers do.
    fn check_not_rolling_back(&self) -> Result<()> {
        #[cfg(feature = "research")]
        if !self.rollback.is_empty() {
            return Err(Error::Setup("disks keep writes in memory for research resets; can't save them".into()));
        }
        Ok(())
    }

    /// Freeze every writable disk at its current state and continue on a
    /// new overlay. Returns index -> frozen layer for the snapshot manifest.
    /// The VM must be paused.
    pub fn freeze(&mut self) -> Result<BTreeMap<usize, PathBuf>> {
        self.check_not_rolling_back()?;
        // A snapshot refers to the overlays now: keep them.
        #[cfg(feature = "research")]
        {
            self.discard_fresh = false;
        }
        let mut out = BTreeMap::new();
        for d in &mut self.disks {
            let Some(frozen) = d.active.clone() else { continue };
            d.image.flush().map_err(|e| Error::Setup(e.to_string()))?;
            let top_path = self.dir.join(format!("disk{}-{}.qcow2", d.index, stamp()));
            let lower = d.image.current();
            lower.freeze();
            let top = Qcow2Image::create_overlay(&top_path, &frozen, d.active_fmt, lower)
                .map_err(|e| Error::Setup(format!("overlay: {e}")))?
                .with_sync(d.sync);
            d.image.swap(Arc::new(top)).map_err(|e| Error::Setup(e.to_string()))?;
            write_pointer(&self.dir.join(format!("disk{}.json", d.index)), &top_path)?;
            d.active = Some(top_path);
            d.active_fmt = Format::Qcow2;
            out.insert(d.index, frozen);
        }
        Ok(out)
    }

    /// Current top layers with change stamps (suspend images).
    pub fn active_stamps(&self) -> Result<BTreeMap<usize, (PathBuf, String)>> {
        self.check_not_rolling_back()?;
        let mut m = BTreeMap::new();
        for d in &self.disks {
            if let Some(a) = &d.active {
                d.image.flush().map_err(|e| Error::Setup(e.to_string()))?;
                m.insert(d.index, (a.clone(), file_stamp(a)?));
            }
        }
        Ok(m)
    }

    /// Least free space among the file systems writable images live on.
    pub fn min_free(&self) -> Option<u64> {
        self.watch.iter().filter_map(|f| crate::state::free_bytes(f).ok()).min()
    }

    pub fn flush_all(&self) {
        for d in &self.disks {
            let _ = d.image.flush();
        }
    }
}

#[cfg(feature = "research")]
impl Drop for DiskSet {
    fn drop(&mut self) {
        if !self.discard_fresh {
            return;
        }
        for (layer, pointer, before) in &self.fresh {
            let restored = match before {
                Some(bytes) => std::fs::write(pointer, bytes),
                None => std::fs::remove_file(pointer),
            };
            if let Err(e) = restored.and_then(|()| std::fs::remove_file(layer)) {
                log::warn!("scratch disk layer {}: {e}", layer.display());
            }
        }
    }
}

fn write_pointer(path: &Path, active: &Path) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(&Pointer { active: active.to_path_buf() }).map_err(|e| Error::Setup(e.to_string()))?;
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(disk: &Path) -> VmConfig {
        let mut c = VmConfig::from_toml(
            "name = \"t\"\n[boot]\ntype = \"kernel\"\nkernel = \"/k\"\n[[disk]]\npath = \"/x\"\nformat = \"qcow2\"\n",
        )
        .unwrap();
        c.disks[0].path = disk.to_path_buf();
        c
    }

    #[test]
    fn snapshot_then_revert() {
        let td = tempfile::tempdir().unwrap();
        let disk = td.path().join("disk.qcow2");
        Qcow2Image::create(&disk, 1 << 20, None).unwrap();
        let state = td.path().join("vm.state");
        let c = cfg(&disk);

        let mut ds = DiskSet::open(&c, &state, &BTreeMap::new()).unwrap();
        ds.disks[0].image.write_at(b"v1", 0).unwrap();
        let layers = ds.freeze().unwrap();
        ds.disks[0].image.write_at(b"v2", 0).unwrap();
        drop(ds);

        // Normal restart continues where we were (v2).
        let ds = DiskSet::open(&c, &state, &BTreeMap::new()).unwrap();
        let mut b = [0u8; 2];
        ds.disks[0].image.read_at(&mut b, 0).unwrap();
        assert_eq!(&b, b"v2");
        drop(ds);

        // Revert to the snapshot: v1, and the original file is intact.
        let ds = DiskSet::open(&c, &state, &layers).unwrap();
        ds.disks[0].image.read_at(&mut b, 0).unwrap();
        assert_eq!(&b, b"v1");
        ds.disks[0].image.write_at(b"v3", 0).unwrap();
        let orig = Qcow2Image::open(&disk, true).unwrap();
        orig.read_at(&mut b, 0).unwrap();
        assert_eq!(&b, b"v1");
    }

    #[cfg(feature = "research")]
    #[test]
    fn research_rollback_keeps_files_untouched() {
        let td = tempfile::tempdir().unwrap();
        let disk = td.path().join("disk.qcow2");
        Qcow2Image::create(&disk, 2 * ROLLBACK_MEMORY, None).unwrap();
        let state = td.path().join("vm.state");
        let c = cfg(&disk);
        let mut ds = DiskSet::open(&c, &state, &BTreeMap::new()).unwrap();
        // Revert to a frozen layer, as the fuzzer's checkpoint restore does.
        ds.disks[0].image.write_at(b"base", 0).unwrap();
        let layers = ds.freeze().unwrap();
        drop(ds);
        let mut ds = DiskSet::open(&c, &state, &layers).unwrap();
        let top = ds.disks[0].active.clone().unwrap();
        let top_len = std::fs::metadata(&top).unwrap().len();
        let read = |ds: &DiskSet| {
            let mut b = [0u8; 4];
            ds.disks[0].image.read_at(&mut b, 0).unwrap();
            b
        };

        // Without the layer a testcase's write stays for the next one.
        ds.disks[0].image.write_at(b"tc-0", 0).unwrap();
        assert_eq!(&read(&ds), b"tc-0");
        ds.disks[0].image.write_at(b"base", 0).unwrap();
        ds.disks[0].image.flush().unwrap();
        let top_len = std::fs::metadata(&top).unwrap().len().max(top_len);

        ds.arm_rollback().unwrap();
        assert!(ds.arm_rollback().is_err());
        for tc in [b"tc-1", b"tc-2"] {
            ds.disks[0].image.write_at(tc, 0).unwrap();
            ds.disks[0].image.write_at(&[7; 8192], 512 << 10).unwrap();
            ds.disks[0].image.flush().unwrap();
            assert_eq!(&read(&ds), tc);
            assert_eq!(ds.rollback().unwrap(), 3 * 4096);
            assert_eq!(&read(&ds), b"base");
        }
        assert_eq!(std::fs::metadata(&top).unwrap().len(), top_len, "the overlay file grew");
        // RAM past the limit goes to an unlinked file in the disks dir.
        let files = || std::fs::read_dir(state.join("disks")).unwrap().count();
        let before = files();
        ds.disks[0].image.write_at(&vec![9; (ROLLBACK_MEMORY + (64 << 10)) as usize], 0).unwrap();
        assert_eq!(ds.rollback.iter().map(|l| l.memory_bytes()).sum::<u64>(), ROLLBACK_MEMORY);
        assert_eq!(files(), before);
        assert_eq!(ds.rollback().unwrap(), ROLLBACK_MEMORY + (64 << 10));
        assert_eq!(&read(&ds), b"base");
        assert!(ds.freeze().is_err());
        assert!(ds.active_stamps().is_err());
    }

    #[cfg(feature = "research")]
    #[test]
    fn scratch_overlays_go_away_on_close() {
        let td = tempfile::tempdir().unwrap();
        let disk = td.path().join("disk.qcow2");
        Qcow2Image::create(&disk, 1 << 20, None).unwrap();
        let state = td.path().join("vm.state");
        let c = cfg(&disk);
        let mut ds = DiskSet::open(&c, &state, &BTreeMap::new()).unwrap();
        let layers = ds.freeze().unwrap();
        drop(ds);
        let names = || {
            let mut v: Vec<_> = std::fs::read_dir(state.join("disks")).unwrap().map(|e| e.unwrap().file_name()).collect();
            v.sort();
            v
        };
        let pointer = state.join("disks/disk0.json");
        let (files, before) = (names(), std::fs::read(&pointer).unwrap());
        for _ in 0..3 {
            let mut ds = DiskSet::open(&c, &state, &layers).unwrap();
            ds.discard_fresh_layers();
            ds.disks[0].image.write_at(b"scratch", 0).unwrap();
            assert_eq!(names().len(), files.len() + 1);
            drop(ds);
            assert_eq!(names(), files, "a restore left its overlay behind");
            assert_eq!(std::fs::read(&pointer).unwrap(), before);
        }
        // Without it (or once a snapshot refers to it) the overlay stays.
        let mut ds = DiskSet::open(&c, &state, &layers).unwrap();
        ds.discard_fresh_layers();
        ds.freeze().unwrap();
        drop(ds);
        assert_eq!(names().len(), files.len() + 2);
    }
}
