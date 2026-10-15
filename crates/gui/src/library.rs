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
    /// Why the file couldn't be read. Saving then would drop the user's
    /// list, so it's refused until the file is fixed.
    #[serde(skip)]
    pub error: Option<String>,
}

impl Library {
    pub fn load(file: &Path) -> Library {
        let r = match std::fs::read_to_string(file) {
            Ok(s) => toml::from_str(&s).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Library::default()),
            Err(e) => Err(e.to_string()),
        };
        let mut l = r.unwrap_or_else(|e| Library { error: Some(format!("{}: {e}", file.display())), ..Default::default() });
        l.file = file.to_path_buf();
        l
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(e) = &self.error {
            return Err(format!("not overwriting {e}"));
        }
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
/// The file keeps its permissions.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp~");
    std::fs::write(&tmp, data).map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Ok(m) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, m.permissions());
    }
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

    #[test]
    fn broken_file_is_not_overwritten() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("library.toml");
        let text = "vms = [\"/a.toml\", oops";
        std::fs::write(&f, text).unwrap();
        let mut l = Library::load(&f);
        assert!(l.error.is_some());
        l.theme = Some("dark".into());
        assert!(l.save().is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), text);
    }

    #[test]
    fn rewrite_keeps_mode() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("vm.toml");
        std::fs::write(&f, "x").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&f, b"y").unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
