//! Renaming a VM, and moving it to another folder. Its file, its state
//! folder (`<name>.state`) and, when it has one of its own, its folder go
//! along; the paths in the file follow. Only for a VM that isn't running:
//! a suspended one is fine (its saves name files relative to its folder).

use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use vroom_config::VmConfig;

use crate::paths::tilde;

/// The folder only this VM lives in: named after it (or its file) and
/// holding no other VM file. A VM file loose in ~/Desktop has none.
pub fn own_folder(file: &Path) -> Option<PathBuf> {
    let dir = vroom_config::config_dir(file).ok()?;
    let c = VmConfig::load_raw(file).ok()?;
    let n = dir.file_name()?.to_str()?;
    if n != c.name && Some(n) != file.file_stem().and_then(|s| s.to_str()) {
        return None;
    }
    let me = std::fs::canonicalize(file).ok()?;
    let others = std::fs::read_dir(&dir).ok()?.flatten().any(|e| {
        let p = e.path();
        p.extension().is_some_and(|x| x == "toml") && std::fs::canonicalize(&p).ok().as_ref() != Some(&me) && VmConfig::load_raw(&p).is_ok()
    });
    (!others).then_some(dir)
}

/// The same file or folder (`Dev` and `dev` on a Mac disk).
fn same(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

fn taken(new: &Path, old: &Path) -> Result<(), String> {
    if new != old && new.exists() && !same(new, old) {
        return Err(format!("{} already exists.", tilde(new)));
    }
    Ok(())
}

fn mv(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| format!("{}: {e}", tilde(from)))
}

/// Name the VM `new`: in its file, its state folder, and the file's and
/// the folder's names where they are the VM's (disk files keep theirs:
/// snapshot layers name them). Network adapters keep their MACs, which are
/// made from the name otherwise, so the guest sees the same cards.
/// Returns where the file is now.
pub fn rename(file: &Path, new: &str) -> Result<PathBuf, String> {
    if !vroom_config::valid_name(new) {
        return Err("Use only letters, digits, '.', '_' and '-' in the name.".into());
    }
    let before = VmConfig::load_raw(file).map_err(|e| e.to_string())?;
    let old = before.name.clone();
    if old == new {
        return Ok(file.to_path_buf());
    }
    let dir = vroom_config::config_dir(file).map_err(|e| e.to_string())?;
    let file = dir.join(file.file_name().ok_or("no file name")?);
    let (old_state, new_state) = (dir.join(format!("{old}.state")), dir.join(format!("{new}.state")));
    let new_file = if file.file_stem().is_some_and(|s| s == old.as_str()) { dir.join(format!("{new}.toml")) } else { file.clone() };
    let folder = own_folder(&file).filter(|d| d.file_name().is_some_and(|n| n == old.as_str()));
    let new_folder = folder.as_ref().map(|d| d.with_file_name(new));
    taken(&new_state, &old_state)?;
    taken(&new_file, &file)?;
    if let (Some(a), Some(b)) = (&folder, &new_folder) {
        taken(b, a)?;
    }

    let mut after = before.clone();
    after.name = new.to_string();
    for (i, n) in after.nets.iter_mut().enumerate() {
        if n.mac.is_none() {
            n.mac = Some(vroom_config::format_mac(&n.mac_of(&before.identity, &old, i).map_err(|e| e.to_string())?));
        }
    }
    let (rel_old, rel_new) = (PathBuf::from(format!("{old}.state")), PathBuf::from(format!("{new}.state")));
    after.each_path(|p| {
        let q = if let Ok(r) = p.strip_prefix(&rel_old) {
            rel_new.join(r)
        } else if let Ok(r) = p.strip_prefix(&old_state) {
            new_state.join(r)
        } else {
            return;
        };
        *p = q;
    });
    if let (Some(a), Some(b)) = (&folder, &new_folder) {
        after.each_path(|p| {
            if let Ok(r) = p.strip_prefix(a) {
                *p = b.join(r);
            }
        });
    }

    // State first: a file naming a state folder that isn't there would
    // boot the VM afresh (its UEFI variables gone). Each step undone if a
    // later one fails.
    let state = old_state.exists();
    if state {
        mv(&old_state, &new_state)?;
    }
    let undo_state = || {
        if state {
            let _ = std::fs::rename(&new_state, &old_state);
        }
    };
    if let Err(e) = crate::edit::save(&file, &before, &after) {
        undo_state();
        return Err(e);
    }
    let undo_file = || {
        let _ = crate::edit::save(&file, &after, &before);
        undo_state();
    };
    if new_file != file {
        if let Err(e) = mv(&file, &new_file) {
            undo_file();
            return Err(e);
        }
    }
    if let (Some(a), Some(b)) = (&folder, &new_folder) {
        if let Err(e) = mv(a, b) {
            let _ = std::fs::rename(&new_file, &file);
            undo_file();
            return Err(e);
        }
        return Ok(b.join(new_file.file_name().ok_or("no file name")?));
    }
    Ok(new_file)
}

/// Moving a VM's folder into another one.
#[derive(Debug)]
pub struct Move {
    file: PathBuf,
    from: PathBuf,
    pub dest: PathBuf,
}

/// Check a move of the VM in `file` into folder `to` before doing it.
pub fn plan_move(file: &Path, to: &Path) -> Result<Move, String> {
    let from = own_folder(file).ok_or("Its folder has other things in it, so only a VM in a folder of its own can move. Move it in Finder, then open it from there.")?;
    let to = std::fs::canonicalize(to).map_err(|e| format!("{}: {e}", tilde(to)))?;
    if from.parent() == Some(to.as_path()) {
        return Err("It's in that folder already.".into());
    }
    if to.starts_with(&from) {
        return Err("That folder is inside the VM's own.".into());
    }
    let dest = to.join(from.file_name().ok_or("no folder name")?);
    taken(&dest, &from)?;
    let file = from.join(file.file_name().ok_or("no file name")?);
    Ok(Move { file, from, dest })
}

impl Move {
    /// On the same disk: a rename, done at once. Elsewhere it copies.
    pub fn same_disk(&self) -> bool {
        let dev = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).ok();
        dev(&self.from).is_some() && dev(&self.from) == self.dest.parent().and_then(dev)
    }

    /// Move it; returns where the VM file is now. To another disk it is
    /// copied next to where it goes, renamed into place, then the original
    /// deleted: a copy that fails leaves the VM where it was.
    pub fn run(&self) -> Result<PathBuf, String> {
        match std::fs::rename(&self.from, &self.dest) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => self.copy()?,
            Err(e) => return Err(format!("{}: {e}", tilde(&self.dest))),
        }
        let file = self.dest.join(self.file.file_name().ok_or("no file name")?);
        self.fix_paths(&file)?;
        Ok(file)
    }

    fn copy(&self) -> Result<(), String> {
        let name = self.dest.file_name().ok_or("no folder name")?.to_string_lossy();
        let tmp = self.dest.with_file_name(format!(".{name}.moving"));
        let _ = std::fs::remove_dir_all(&tmp);
        let out = std::process::Command::new("cp").arg("-Rp").arg(&self.from).arg(&tmp).output().map_err(|e| format!("cp: {e}"))?;
        if !out.status.success() {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("Couldn't copy it: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        if let Err(e) = mv(&tmp, &self.dest) {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
        std::fs::remove_dir_all(&self.from).map_err(|e| format!("Moved, but the old copy is still in {}: {e}", tilde(&self.from)))
    }

    /// Paths into the old folder by absolute path point into the new one;
    /// relative ones out of it (an ISO next to it) become absolute.
    fn fix_paths(&self, file: &Path) -> Result<(), String> {
        let before = VmConfig::load_raw(file).map_err(|e| e.to_string())?;
        let mut after = before.clone();
        after.each_path(|p| {
            if let Ok(r) = p.strip_prefix(&self.from) {
                *p = self.dest.join(r);
            } else if p.is_relative() {
                let abs = lexical(&self.from.join(&*p));
                if !abs.starts_with(&self.from) {
                    *p = abs;
                }
            }
        });
        if after == before {
            return Ok(());
        }
        crate::edit::save(file, &before, &after).map_err(|e| format!("Moved, but its file still names the old place: {e}"))
    }
}

/// `a/b/../c` as `a/c`, without asking the disk.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const VM: &str = "name = \"dev\" # mine\n[boot]\ntype = \"uefi\"\nvars = \"dev.state/efivars.fd\"\n\n[[disk]]\npath = \"dev.qcow2\"\n\n[[disk]]\npath = \"../isos/x.iso\"\nreadonly = true\n\n[[net]]\nmode = \"nat\"\n";

    /// `<t>/VMs/dev/dev.toml` with its state, a disk and an ISO next door.
    fn vm(t: &Path) -> PathBuf {
        let dir = t.join("VMs/dev");
        std::fs::create_dir_all(dir.join("dev.state")).unwrap();
        std::fs::create_dir_all(t.join("VMs/isos")).unwrap();
        std::fs::write(dir.join("dev.state/efivars.fd"), "vars").unwrap();
        std::fs::write(dir.join("dev.qcow2"), "disk").unwrap();
        std::fs::write(t.join("VMs/isos/x.iso"), "iso").unwrap();
        std::fs::write(dir.join("dev.toml"), VM).unwrap();
        dir.join("dev.toml")
    }

    #[test]
    fn rename_takes_the_state_and_folder_along() {
        let t = tempfile::tempdir().unwrap();
        let file = vm(t.path());
        let mac = {
            let c = VmConfig::load_raw(&file).unwrap();
            vroom_config::format_mac(&c.nets[0].mac_of(&c.identity, "dev", 0).unwrap())
        };
        let new = rename(&file, "web").unwrap();
        let dir = std::fs::canonicalize(t.path()).unwrap().join("VMs/web");
        assert_eq!(new, dir.join("web.toml"));
        assert!(!t.path().join("VMs/dev").exists());
        let text = std::fs::read_to_string(&new).unwrap();
        assert!(text.contains("name = \"web\" # mine"), "comments kept: {text}");
        let c = VmConfig::load(&new).unwrap();
        let vroom_config::Boot::Uefi { vars, .. } = &c.boot else { panic!() };
        assert_eq!(vars, &dir.join("web.state/efivars.fd"));
        assert_eq!(std::fs::read_to_string(dir.join("web.state/efivars.fd")).unwrap(), "vars");
        assert_eq!(c.disks[0].path, dir.join("dev.qcow2"));
        assert_eq!(c.nets[0].mac.as_deref(), Some(mac.as_str()), "same MAC as before");
    }

    #[test]
    fn rename_in_a_shared_folder_leaves_the_folder() {
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("dev.toml");
        std::fs::write(&file, "name = \"dev\"\n[boot]\ntype = \"uefi\"\nvars = \"dev.state/efivars.fd\"\n").unwrap();
        std::fs::write(t.path().join("other.toml"), "name = \"other\"\n[boot]\ntype = \"uefi\"\nvars = \"other.state/efivars.fd\"\n").unwrap();
        assert_eq!(own_folder(&file), None);
        let new = rename(&file, "web").unwrap();
        assert_eq!(new.file_name().unwrap(), "web.toml");
        assert!(new.parent().unwrap().ends_with(t.path().file_name().unwrap()));
        // Taken: nothing changes.
        std::fs::create_dir_all(t.path().join("other2.state")).unwrap();
        assert!(rename(&new, "other2").is_err());
        assert!(std::fs::read_to_string(&new).unwrap().contains("name = \"web\""));
    }

    #[test]
    fn move_into_another_folder() {
        let t = tempfile::tempdir().unwrap();
        let file = vm(t.path());
        let to = t.path().join("Elsewhere");
        std::fs::create_dir_all(&to).unwrap();
        assert!(plan_move(&file, &t.path().join("VMs")).is_err(), "already there");
        assert!(plan_move(&file, &t.path().join("VMs/dev/dev.state")).is_err(), "inside itself");
        let m = plan_move(&file, &to).unwrap();
        assert!(m.same_disk());
        let new = m.run().unwrap();
        let root = std::fs::canonicalize(t.path()).unwrap();
        assert_eq!(new, root.join("Elsewhere/dev/dev.toml"));
        let c = VmConfig::load(&new).unwrap();
        assert_eq!(c.disks[0].path, root.join("Elsewhere/dev/dev.qcow2"));
        // The ISO it reached by "../isos" is named where it is.
        assert_eq!(c.disks[1].path, root.join("VMs/isos/x.iso"));
        assert!(std::fs::read_to_string(&new).unwrap().contains(&format!("path = \"{}\"", root.join("VMs/isos/x.iso").display())));
    }

    #[test]
    fn copy_when_rename_cant() {
        let t = tempfile::tempdir().unwrap();
        let file = vm(t.path());
        let to = t.path().join("Elsewhere");
        std::fs::create_dir_all(&to).unwrap();
        let m = plan_move(&file, &to).unwrap();
        m.copy().unwrap();
        assert!(!t.path().join("VMs/dev").exists());
        assert_eq!(std::fs::read_to_string(to.join("dev/dev.state/efivars.fd")).unwrap(), "vars");
        assert!(!to.join(".dev.moving").exists());
    }
}
