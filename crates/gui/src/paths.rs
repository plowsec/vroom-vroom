//! Where the app keeps its own files (not the VMs' state: that lives next
//! to each VM file).

use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! {
    /// Unit tests: a data dir per test (they run on their own threads).
    pub static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// `VROOM_HOME` if set (tests), else the platform's app data dir.
pub fn data_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(h) = TEST_HOME.with(|h| h.borrow().clone()) {
        return h;
    }
    if let Some(h) = std::env::var_os("VROOM_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/vroom")
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share")).join("vroom")
    }
}

pub fn library_file() -> PathBuf {
    data_dir().join("library.toml")
}

/// New VMs go here, one folder each.
pub fn vms_dir() -> PathBuf {
    data_dir().join("VMs")
}

/// Screen pictures we took ourselves (never files the VMM wrote: it runs
/// sandboxed with write access to the state dir, so those are untrusted).
pub fn thumbs_dir(key: &str) -> PathBuf {
    data_dir().join("thumbs").join(key)
}

pub fn log_file(key: &str) -> PathBuf {
    data_dir().join("logs").join(format!("{key}.log"))
}

/// Stable short id for a VM file: FNV-1a of its canonical path.
pub fn key(config: &Path) -> String {
    let p = std::fs::canonicalize(config).unwrap_or_else(|_| config.to_path_buf());
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in p.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// `~/x/y` for display.
pub fn tilde(p: &Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}
