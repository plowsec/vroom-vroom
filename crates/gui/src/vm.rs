//! One VM as the app sees it: its file, what's in it, and whether it runs.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use vroom_config::VmConfig;
use vroom_state::StateLock;

/// Where a VM is, from the files alone (the app's own running VMs are
/// tracked by their console instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskState {
    Off,
    Suspended,
    /// Some other process (`vroom run` in a terminal) has it.
    RunningElsewhere,
}

#[derive(Debug)]
pub struct Vm {
    pub path: PathBuf,
    pub key: String,
    /// As written: paths relative to the file. None if unreadable (or
    /// broken since: the app then shows the error, not a stale copy).
    pub raw: Option<VmConfig>,
    pub error: Option<String>,
    /// From the last good read: a VM running on it keeps it while the file
    /// is broken.
    state: PathBuf,
    pub mtime: Option<SystemTime>,
    pub disk_state: DiskState,
    pub thumb: Option<slint::Image>,
    pub thumb_mtime: Option<SystemTime>,
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl Vm {
    pub fn open(path: &Path) -> Vm {
        let mut v = Vm {
            path: path.to_path_buf(),
            key: crate::paths::key(path),
            raw: None,
            error: None,
            state: path.with_extension("state"),
            mtime: None,
            disk_state: DiskState::Off,
            thumb: None,
            thumb_mtime: None,
        };
        v.reload();
        v.refresh_state();
        v.refresh_thumb();
        v
    }

    pub fn reload(&mut self) {
        self.mtime = mtime(&self.path);
        match VmConfig::load_raw(&self.path) {
            Ok(c) => {
                self.state = c.default_state_dir(&self.path);
                self.raw = Some(c);
                self.error = None;
            }
            Err(e) => {
                self.raw = None;
                self.error = Some(e.to_string());
            }
        }
    }

    /// The file changed on disk (edited by hand)?
    pub fn changed_on_disk(&self) -> bool {
        mtime(&self.path) != self.mtime
    }

    pub fn name(&self) -> String {
        match &self.raw {
            Some(c) => c.name.clone(),
            None => self.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        }
    }

    /// Paths made absolute, as the VMM will see them.
    pub fn resolved(&self) -> Option<VmConfig> {
        let mut c = self.raw.clone()?;
        let base = vroom_config::config_dir(&self.path).ok()?;
        c.rebase_paths(&base);
        Some(c)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.state.clone()
    }

    pub fn refresh_state(&mut self) {
        let st = self.state_dir();
        self.disk_state = if StateLock::in_use(&st) {
            DiskState::RunningElsewhere
        } else if vroom_state::is_suspended(&st) {
            DiskState::Suspended
        } else {
            DiskState::Off
        };
    }

    pub fn thumb_path(&self) -> PathBuf {
        crate::paths::thumbs_dir(&self.key).join("screen.png")
    }

    pub fn refresh_thumb(&mut self) {
        let p = self.thumb_path();
        let m = mtime(&p);
        if m != self.thumb_mtime {
            self.thumb_mtime = m;
            self.thumb = m.and_then(|_| slint::Image::load_from_path(&p).ok());
        }
    }

    /// Dirs the VM's disk layer chains may end in (for pruning).
    pub fn base_dirs(&self) -> Vec<PathBuf> {
        self.resolved()
            .map(|c| c.disks.iter().filter_map(|d| d.path.parent().map(Path::to_path_buf)).collect())
            .unwrap_or_default()
    }
}

/// Virtual size of an image without opening it for real (no lock, so it
/// works while a VM runs): qcow2 header, else the file length.
pub fn virtual_size(p: &Path) -> Option<u64> {
    use std::io::Read;
    let mut f = std::fs::File::open(p).ok()?;
    let mut h = [0u8; 32];
    let len = f.metadata().ok()?.len();
    if f.read_exact(&mut h).is_ok() && &h[..4] == b"QFI\xfb" {
        return Some(u64::from_be_bytes(h[24..32].try_into().ok()?));
    }
    Some(len)
}

/// A state dir, its suspend manifest's time, and the VM file in it.
type SuspendedRead = (PathBuf, Option<SystemTime>, Option<VmConfig>);

thread_local! {
    /// The last suspend image's VM file read, by its manifest's time.
    static SUSPENDED: std::cell::RefCell<Option<SuspendedRead>> = const { std::cell::RefCell::new(None) };
}

/// The VM file a suspended VM was saved with (what it resumes with:
/// its network adapters, its MACs). Read again only when it changes.
pub fn suspended_config(state: &Path) -> Option<VmConfig> {
    let at = mtime(&state.join("suspend").join("manifest.json"));
    SUSPENDED.with(|s| {
        let mut s = s.borrow_mut();
        if let Some((p, t, c)) = s.as_ref() {
            if p == state && *t == at {
                return c.clone();
            }
        }
        // The VMM wrote it: a VmConfig parse checks what we use.
        let c = vroom_state::suspended_config(state).and_then(|v| serde_json::from_value::<VmConfig>(v).ok());
        *s = Some((state.to_path_buf(), at, c.clone()));
        c
    })
}

/// Bytes the file really takes on the host (sparse files take less).
pub fn used_bytes(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.blocks() * 512)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_broken_file_shows_as_broken() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("dev.toml");
        std::fs::write(&p, "name = \"dev\"\n[boot]\ntype = \"uefi\"\nvars = \"x.fd\"\n").unwrap();
        let mut v = Vm::open(&p);
        assert!(v.raw.is_some());
        let state = v.state_dir();
        assert!(state.ends_with("dev.state"));
        std::fs::write(&p, "name = \"dev\"\ncpus = \"many\"\n").unwrap();
        v.reload();
        assert!(v.raw.is_none() && v.error.is_some(), "kept showing the old settings");
        assert_eq!(v.state_dir(), state);
    }
}
