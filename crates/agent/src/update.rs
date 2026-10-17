//! New guest tools from the host (`UpdateBegin`, then `UpdateData`).
//!
//! Only the privileged side takes them, and only from the host port (the
//! Linux and Windows services, or the agent run as root): the session
//! helper's link never carries them (relay.rs drops them). The files go
//! into a staging folder only root (SYSTEM) can write, each is checked
//! against the size and SHA-256 the manifest gave, and only a complete,
//! checked set is installed:
//! - Linux: a `ready` file next to them starts `vroom-agent-update.service`
//!   (a systemd path unit), which runs the bundle's `install-agent.sh
//!   --update` as root: the service itself is too fenced in to replace
//!   files in /usr or restart itself.
//! - Windows: the new `vroom-agent.exe --apply-update DIR`, started on
//!   its own (winupdate.rs), stops the service, swaps the files, adds
//!   changed drivers and starts it again.
//!
//! The host chooses what runs in the guest here, by design: it already
//! controls the guest entirely. What matters is that nobody else can:
//! not another process in the guest (the port is root's, the staging
//! folder too) and not a half-sent or damaged bundle (the hashes).

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use vroom_proto::{update_files_ok, Message, UpdateFile};

/// Where the staged files go, under the updater's root.
pub const STAGE: &str = "update";
/// Linux: written when the files are in; the path unit watches it.
pub const READY: &str = "ready";

/// Puts the bundle together. `apply` installs it once it is complete.
pub struct Updater {
    root: PathBuf,
    cur: Option<Recv>,
}

struct Recv {
    id: u32,
    version: String,
    files: Vec<UpdateFile>,
    dir: PathBuf,
    /// The file being written, and how much of it is in.
    i: usize,
    got: u64,
    out: Option<File>,
    hash: Sha256,
}

impl Updater {
    pub fn new(root: PathBuf) -> Updater {
        Updater { root, cur: None }
    }

    /// The staging folder.
    pub fn stage(&self) -> PathBuf {
        self.root.join(STAGE)
    }

    /// A frame from the host port. Returns the answer for the host when
    /// the update is complete (installing) or failed; `apply` is called
    /// with the folder and the version once every file checked out.
    pub fn handle(&mut self, m: Message, apply: impl FnOnce(&Path, &str) -> Result<(), String>) -> Option<Message> {
        match m {
            Message::UpdateBegin { id, version, files } => {
                let r = self.begin(id, version, files).and_then(|()| self.advance(&[]));
                self.finish(id, r, apply)
            }
            Message::UpdateData { id, data } => {
                // Frames of an update given up on (a new one started).
                if self.cur.as_ref().is_none_or(|c| c.id != id) {
                    return None;
                }
                let r = self.advance(&data);
                self.finish(id, r, apply)
            }
            _ => None,
        }
    }

    /// Ok(true): all in. Errors drop the update and its files.
    fn finish(&mut self, id: u32, r: Result<bool, String>, apply: impl FnOnce(&Path, &str) -> Result<(), String>) -> Option<Message> {
        let r = match r {
            Ok(false) => return None,
            Ok(true) => {
                let c = self.cur.take()?;
                log::info!("update {id}: {} files checked, installing {}", c.files.len(), c.version);
                apply(&c.dir, &c.version)
            }
            Err(e) => Err(e),
        };
        match r {
            Ok(()) => Some(Message::UpdateDone { id, ok: true, why: String::new() }),
            Err(why) => {
                log::warn!("update {id}: {why}");
                if let Some(c) = self.cur.take() {
                    drop(c.out);
                    let _ = std::fs::remove_dir_all(&c.dir);
                }
                Some(Message::UpdateDone { id, ok: false, why })
            }
        }
    }

    fn begin(&mut self, id: u32, version: String, files: Vec<UpdateFile>) -> Result<(), String> {
        self.cur = None;
        if !update_files_ok(&files) {
            return Err("bad file list".into());
        }
        let dir = self.stage();
        // One that is being installed right now stays.
        if let Ok(m) = std::fs::metadata(dir.join(READY)) {
            let fresh = m.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a.as_secs() < 300);
            if fresh {
                return Err("an update is being installed".into());
            }
        }
        make_private_dir(&self.root)?;
        // Never follows a link: it is only ours anyway.
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?,
            Ok(_) => std::fs::remove_file(&dir).map_err(|e| format!("{}: {e}", dir.display()))?,
            Err(_) => {}
        }
        make_private_dir(&dir)?;
        log::info!("update {id}: {} files, {} bytes, version {version}", files.len(), files.iter().map(|f| f.size).sum::<u64>());
        self.cur = Some(Recv { id, version, files, dir, i: 0, got: 0, out: None, hash: Sha256::new() });
        Ok(())
    }

    /// Take `data` in: into the current file, the next ones... Ok(true)
    /// once the last file is in and checked.
    fn advance(&mut self, mut data: &[u8]) -> Result<bool, String> {
        let c = self.cur.as_mut().ok_or("no update")?;
        loop {
            let Some(f) = c.files.get(c.i) else {
                return if data.is_empty() { Ok(true) } else { Err("more data than the file list says".into()) };
            };
            if c.out.is_none() {
                c.out = Some(create_in(&c.dir, &f.path)?);
                c.got = 0;
                c.hash = Sha256::new();
            }
            let n = (f.size - c.got).min(data.len() as u64) as usize;
            if n > 0 {
                let out = c.out.as_mut().ok_or("no file")?;
                out.write_all(&data[..n]).map_err(|e| format!("{}: {e}", f.path))?;
                c.hash.update(&data[..n]);
                c.got += n as u64;
                data = &data[n..];
            }
            if c.got < f.size {
                return Ok(false);
            }
            let sum: [u8; 32] = std::mem::take(&mut c.hash).finalize().into();
            if sum != f.sha256 {
                return Err(format!("{}: damaged on the way (SHA-256 differs)", f.path));
            }
            if let Some(out) = c.out.take() {
                out.sync_all().map_err(|e| format!("{}: {e}", f.path))?;
            }
            c.i += 1;
        }
    }
}

/// A folder only we (root, SYSTEM) may write; made if missing.
fn make_private_dir(p: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => {
                // Made by someone else, or open to others: not ours to use.
                // SAFETY: geteuid has no preconditions.
                #[allow(unsafe_code)]
                let me = unsafe { libc::geteuid() };
                if m.uid() != me {
                    return Err(format!("{} belongs to someone else", p.display()));
                }
                std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("{}: {e}", p.display()))
            }
            Ok(_) => Err(format!("{} is not a folder", p.display())),
            Err(_) => std::fs::DirBuilder::new().mode(0o700).create(p).map_err(|e| format!("{}: {e}", p.display())),
        }
    }
    #[cfg(not(unix))]
    {
        // Under Program Files: users may read, not write.
        std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))
    }
}

/// A new file at `rel` (checked by the manifest check) in `dir`, its
/// folders made on the way. Never one that exists.
fn create_in(dir: &Path, rel: &str) -> Result<File, String> {
    let mut p = dir.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for d in &parts[..parts.len() - 1] {
        p.push(d);
        if !p.is_dir() {
            make_private_dir(&p)?;
        }
    }
    p.push(parts[parts.len() - 1]);
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o700).custom_flags(libc::O_NOFOLLOW);
    }
    o.open(&p).map_err(|e| format!("{rel}: {e}"))
}

/// Linux: the update unit `install-agent.sh` sets up is there, so an
/// update can be installed (it is what runs the bundle's installer).
#[cfg(target_os = "linux")]
pub fn can_apply() -> bool {
    // SAFETY: geteuid has no preconditions.
    #[allow(unsafe_code)]
    let root = unsafe { libc::geteuid() } == 0;
    root && Path::new("/etc/systemd/system/vroom-agent-update.path").exists()
}

/// Linux: where updates are staged (the service's StateDirectory).
#[cfg(target_os = "linux")]
pub fn root() -> PathBuf {
    PathBuf::from("/var/lib/vroom-agent")
}

/// Linux: hand the checked files to the update unit.
#[cfg(target_os = "linux")]
pub fn apply(dir: &Path, version: &str) -> Result<(), String> {
    if !can_apply() {
        return Err("this guest's agent wasn't installed with install-agent.sh: install the new tools by hand".into());
    }
    for need in ["install-agent.sh", "vroom-agent"] {
        if !dir.join(need).is_file() {
            return Err(format!("the update has no {need}"));
        }
    }
    let ready = dir.join(READY);
    std::fs::write(&ready, version).map_err(|e| format!("{}: {e}", ready.display()))?;
    Ok(())
}

/// Windows: where updates are staged (Program Files: users may not write).
#[cfg(windows)]
pub fn root() -> PathBuf {
    let pf = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    pf.join("Vroom")
}

/// Windows: start the new agent to install itself; it stops this one.
#[cfg(windows)]
pub fn apply(dir: &Path, _version: &str) -> Result<(), String> {
    crate::winupdate::launch(dir)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn root() -> PathBuf {
    std::env::temp_dir().join("vroom-agent")
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn apply(_dir: &Path, _version: &str) -> Result<(), String> {
    Err("updates aren't installed on this system".into())
}

/// The `DriverVer` of an INF file (UTF-16 or 8-bit text), as written
/// ("10/02/2024,100.95.104.26600"), spaces taken out. Windows only; tested
/// everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn driver_ver(inf: &[u8]) -> Option<String> {
    let text = if inf.starts_with(&[0xff, 0xfe]) {
        let w: Vec<u16> = (2..inf.len().saturating_sub(1)).step_by(2).map(|i| u16::from_le_bytes([inf[i], inf[i + 1]])).collect();
        String::from_utf16_lossy(&w)
    } else {
        String::from_utf8_lossy(inf).into_owned()
    };
    let mut in_version = false;
    for line in text.lines() {
        let line = line.split(';').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_version = line.eq_ignore_ascii_case("[version]");
            continue;
        }
        if !in_version {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case("driverver") {
                let v: String = v.chars().filter(|c| !c.is_whitespace()).collect();
                return (!v.is_empty()).then_some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, data: &[u8]) -> UpdateFile {
        UpdateFile { path: path.into(), size: data.len() as u64, sha256: Sha256::digest(data).into() }
    }

    fn begin(files: Vec<UpdateFile>) -> Message {
        Message::UpdateBegin { id: 1, version: "0.1.0+2.ab".into(), files }
    }

    #[test]
    fn a_bundle_comes_in_checked() {
        let t = tempfile::tempdir().unwrap();
        let mut u = Updater::new(t.path().join("root"));
        let (a, b) = (b"agent bytes".to_vec(), b"#!/bin/sh\n".to_vec());
        let none = |_: &Path, _: &str| -> Result<(), String> { panic!("not yet") };
        assert_eq!(u.handle(begin(vec![file("vroom-agent", &a), file("sub/dir/install.sh", &b), file("empty", b"")]), none), None);
        // Data across the files' edges, in odd pieces.
        let all: Vec<u8> = [a.clone(), b.clone()].concat();
        assert_eq!(u.handle(Message::UpdateData { id: 1, data: all[..5].to_vec() }, none), None);
        // Another update's frames don't count.
        assert_eq!(u.handle(Message::UpdateData { id: 9, data: vec![0; 3] }, none), None);
        let mut got = None;
        let r = u.handle(Message::UpdateData { id: 1, data: all[5..].to_vec() }, |d, v| {
            got = Some((d.to_path_buf(), v.to_string()));
            Ok(())
        });
        assert_eq!(r, Some(Message::UpdateDone { id: 1, ok: true, why: String::new() }));
        let (dir, v) = got.unwrap();
        assert_eq!(v, "0.1.0+2.ab");
        assert_eq!(std::fs::read(dir.join("vroom-agent")).unwrap(), a);
        assert_eq!(std::fs::read(dir.join("sub/dir/install.sh")).unwrap(), b);
        assert_eq!(std::fs::read(dir.join("empty")).unwrap(), b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn damaged_or_long_bundles_are_dropped() {
        let t = tempfile::tempdir().unwrap();
        let mut u = Updater::new(t.path().to_path_buf());
        let ok = |_: &Path, _: &str| -> Result<(), String> { panic!("must not install") };
        u.handle(begin(vec![file("a", b"right")]), ok);
        let r = u.handle(Message::UpdateData { id: 1, data: b"wrong".to_vec() }, ok);
        assert!(matches!(r, Some(Message::UpdateDone { ok: false, .. })), "{r:?}");
        assert!(!u.stage().exists(), "its files are gone");
        u.handle(begin(vec![file("a", b"right")]), ok);
        let r = u.handle(Message::UpdateData { id: 1, data: b"right and more".to_vec() }, ok);
        assert!(matches!(r, Some(Message::UpdateDone { ok: false, .. })), "{r:?}");
        // A bad list never starts.
        let r = u.handle(begin(vec![file("../x", b"x")]), ok);
        assert!(matches!(r, Some(Message::UpdateDone { ok: false, .. })), "{r:?}");
    }

    #[test]
    fn a_failed_install_says_why() {
        let t = tempfile::tempdir().unwrap();
        let mut u = Updater::new(t.path().to_path_buf());
        let r = u.handle(begin(vec![file("a", b"")]), |_, _| Err("no installer".into()));
        assert_eq!(r, Some(Message::UpdateDone { id: 1, ok: false, why: "no installer".into() }));
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_link_is_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let victim = t.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("keep"), b"x").unwrap();
        let root = t.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&victim, root.join(STAGE)).unwrap();
        let mut u = Updater::new(root);
        let r = u.handle(begin(vec![file("a", b"")]), |_, _| Ok(()));
        assert!(matches!(r, Some(Message::UpdateDone { ok: true, .. })), "{r:?}");
        assert!(victim.join("keep").exists(), "the link's target is untouched");
    }

    #[test]
    fn driver_versions() {
        let inf = "; netkvm\r\n[Version]\r\nSignature = \"$WINDOWS NT$\"\r\nDriverVer = 10/02/2024, 100.95.104.26600 ; date\r\n[Strings]\r\nDriverVer=1\r\n";
        assert_eq!(driver_ver(inf.as_bytes()).as_deref(), Some("10/02/2024,100.95.104.26600"));
        let mut w = vec![0xff, 0xfe];
        w.extend(inf.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(driver_ver(&w).as_deref(), Some("10/02/2024,100.95.104.26600"));
        assert_eq!(driver_ver(b"[Strings]\nDriverVer=1\n"), None);
    }
}
