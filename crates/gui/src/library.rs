//! The list of VM files the app shows, kept in `library.toml`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Library {
    #[serde(default)]
    pub vms: Vec<PathBuf>,
    /// "dark", "light", or absent: follow the system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(skip)]
    file: PathBuf,
}

impl Library {
    pub fn load(file: &Path) -> Library {
        let mut l: Library = std::fs::read_to_string(file).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default();
        l.file = file.to_path_buf();
        l
    }

    pub fn save(&self) -> Result<(), String> {
        let s = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(d) = self.file.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        write_atomic(&self.file, s.as_bytes())
    }

    /// Add a VM file; false if it was already there.
    pub fn add(&mut self, p: &Path) -> bool {
        let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        if self.vms.contains(&p) {
            return false;
        }
        self.vms.push(p);
        true
    }

    pub fn remove(&mut self, p: &Path) {
        self.vms.retain(|v| v != p);
    }
}

/// Write via a temp file and rename: a crash never leaves half a file.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp~");
    std::fs::write(&tmp, data).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("library.toml");
        let vm = t.path().join("a.toml");
        std::fs::write(&vm, "").unwrap();
        let mut l = Library::load(&f);
        assert!(l.add(&vm));
        assert!(!l.add(&vm));
        l.theme = Some("light".into());
        l.save().unwrap();
        let l = Library::load(&f);
        assert_eq!(l.vms.len(), 1);
        assert_eq!(l.theme.as_deref(), Some("light"));
    }
}
