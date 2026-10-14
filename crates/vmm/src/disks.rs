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

use bulwark_block::qcow2::{OpenPolicy, Qcow2Image};
use bulwark_block::{DiskImage, Format, SwapDisk, SyncMode};
use bulwark_config::{DiskFormat, DiskSync, VmConfig};
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
}

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
        for (i, d) in cfg.disks.iter().enumerate() {
            let policy = OpenPolicy {
                allowed_dirs: vec![dir.clone(), d.path.parent().map(Path::to_path_buf).unwrap_or_default()],
                sync: sync_of(d.sync),
            };
            if d.readonly {
                let img = bulwark_block::open_with(&d.path, fmt_of(d.format), true, &policy)
                    .map_err(|e| Error::Setup(format!("disk {}: {e}", d.path.display())))?;
                disks.push(VmDisk { index: i, image: SwapDisk::new(img), active: None, active_fmt: Format::Raw, sync: policy.sync });
                continue;
            }
            let ptr_path = dir.join(format!("disk{i}.json"));
            let (active, fmt) = if let Some(layer) = restore.get(&i) {
                // Revert: new overlay on the frozen layer.
                let lower = bulwark_block::open_with(layer, None, true, &policy)
                    .map_err(|e| Error::Setup(format!("snapshot layer {}: {e}", layer.display())))?;
                let lfmt = bulwark_block::detect(layer).map_err(|e| Error::Setup(e.to_string()))?;
                let top = dir.join(format!("disk{i}-{}.qcow2", stamp()));
                Qcow2Image::create(&top, lower.size(), Some((layer, lfmt))).map_err(|e| Error::Setup(e.to_string()))?;
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
            let img = bulwark_block::open_with(&active, fmt, false, &policy)
                .map_err(|e| Error::Setup(format!("disk {}: {e}", active.display())))?;
            let active_fmt = fmt.unwrap_or(Format::Raw);
            write_pointer(&ptr_path, &active)?;
            disks.push(VmDisk { index: i, image: SwapDisk::new(img), active: Some(active), active_fmt, sync: policy.sync });
        }
        Ok(DiskSet { dir, disks })
    }

    /// Freeze every writable disk at its current state and continue on a
    /// new overlay. Returns index -> frozen layer for the snapshot manifest.
    /// The VM must be paused.
    pub fn freeze(&mut self) -> Result<BTreeMap<usize, PathBuf>> {
        let mut out = BTreeMap::new();
        for d in &mut self.disks {
            let Some(frozen) = d.active.clone() else { continue };
            d.image.flush().map_err(|e| Error::Setup(e.to_string()))?;
            let top_path = self.dir.join(format!("disk{}-{}.qcow2", d.index, stamp()));
            let lower = d.image.current();
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
        let mut m = BTreeMap::new();
        for d in &self.disks {
            if let Some(a) = &d.active {
                d.image.flush().map_err(|e| Error::Setup(e.to_string()))?;
                m.insert(d.index, (a.clone(), file_stamp(a)?));
            }
        }
        Ok(m)
    }

    pub fn flush_all(&self) {
        for d in &self.disks {
            let _ = d.image.flush();
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
}
