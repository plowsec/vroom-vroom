//! Ready-made VMs: a VM folder with everything it needs to come up ready
//! to use, with the guest tools in and, if asked, SSH with a key.
//! - Ubuntu 24.04: the cloud image (downloaded, or a file given) as the
//!   base of a new disk, and a cloud-init seed (seed.rs): the user, the
//!   guest tools, optionally the desktop.
//! - Windows 11 ARM64: an install stick made from the Windows ISO the
//!   user downloaded (winstick.rs): unattended, with the virtio drivers
//!   in before the first logon (so NAT works at once), the guest tools,
//!   optionally OpenSSH.
//!
//! `vroom new --ready` and the app's New VM sheet both use this.

use std::path::{Path, PathBuf};

use vroom_config::template::toml_str;
use vroom_config::Identity;

/// Who uses the VM.
#[derive(Debug, Clone)]
pub struct Account {
    pub user: String,
    pub password: String,
    /// An OpenSSH public key line: SSH is set up for it.
    pub ssh_key: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Kind {
    /// `image`: the Ubuntu cloud image (qcow2), copied (cloned on APFS)
    /// into the VM's folder as its disk's base.
    Ubuntu { image: PathBuf, desktop: bool },
    /// `iso`: a Windows 11 ARM64 ISO.
    Windows { iso: PathBuf },
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub name: String,
    /// The VM's folder (made; must not exist).
    pub dir: PathBuf,
    pub cpus: u32,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub gpu3d: bool,
    pub account: Account,
    /// Host ports other VMs forward already: SSH gets another.
    pub taken_ports: Vec<u16>,
}

/// Check what a ready-made VM gets before making anything.
pub fn check(spec: &Spec, kind: &Kind) -> Result<(), String> {
    if !vroom_config::valid_name(&spec.name) {
        return Err("Use only letters, digits, '.', '_' and '-' in the name.".into());
    }
    let a = &spec.account;
    match kind {
        Kind::Ubuntu { image, .. } => {
            if !crate::seed::user_ok(&a.user) {
                return Err("User name: lowercase letters, digits, '-' or '_', starting with a letter.".into());
            }
            // Empty: downloaded later (ubuntu_image).
            if !image.as_os_str().is_empty() && !image.is_file() {
                return Err(format!("{}: no such file", image.display()));
            }
        }
        Kind::Windows { iso } => {
            if !crate::winstick::user_ok(&a.user) {
                return Err("That user name isn't one Windows takes.".into());
            }
            if !iso.is_file() {
                return Err(format!("{}: no such file", iso.display()));
            }
        }
    }
    if a.password.is_empty() {
        return Err("Set a password.".into());
    }
    if let Some(k) = &a.ssh_key {
        if !crate::seed::ssh_key_ok(k) {
            return Err("That isn't an SSH public key (one line, like ssh-ed25519 AAAA...).".into());
        }
    }
    if spec.dir.exists() {
        return Err(format!("{} exists already", spec.dir.display()));
    }
    Ok(())
}

/// A free host port for SSH: not in `taken`, and nothing listens on it.
pub fn ssh_port(taken: &[u16]) -> u16 {
    (2222..2400).find(|p| !taken.contains(p) && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok()).unwrap_or(2222)
}

/// Make the VM. Returns its config file. `progress` gets short lines.
/// On failure the folder is removed.
pub fn create(spec: &Spec, kind: &Kind, progress: &mut dyn FnMut(&str)) -> Result<PathBuf, String> {
    check(spec, kind)?;
    let os = match kind {
        Kind::Ubuntu { .. } => crate::Os::Linux,
        Kind::Windows { .. } => crate::Os::Windows,
    };
    let tools = crate::os_dir(os).ok_or("this vroom has no guest tools (scripts/make-guest-tools.sh builds them)")?;
    std::fs::create_dir_all(&spec.dir).map_err(|e| format!("{}: {e}", spec.dir.display()))?;
    let r = match kind {
        Kind::Ubuntu { image, desktop } => ubuntu(spec, image, *desktop, &tools, progress),
        Kind::Windows { iso } => windows(spec, iso, &tools, progress),
    };
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&spec.dir);
    }
    r
}

fn identity() -> String {
    let mut id = Identity::default();
    id.randomize_serials(vroom_config::template::random8());
    format!(
        "\n# What the guest can read about its hardware. Every key: docs/CONFIG.md.\n[identity]\nserial = {}\ndisk_serial = {}\n",
        toml_str(id.serial.as_deref().unwrap_or_default()),
        toml_str(&id.disk_serial)
    )
}

fn net(ssh: Option<u16>) -> String {
    match ssh {
        Some(p) => format!("\n[[net]]\nmode = \"nat\"\n# SSH: ssh -p {p} <user>@localhost\nport_forwards = [{{ host_port = {p}, guest_port = 22 }}]\n"),
        None => "\n[[net]]\nmode = \"nat\"\n".into(),
    }
}

fn write_config(dir: &Path, name: &str, text: &str) -> Result<PathBuf, String> {
    vroom_config::VmConfig::from_toml(text).map_err(|e| format!("made a bad VM file: {e}"))?;
    let p = dir.join(format!("{name}.toml"));
    std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p)
}

fn ubuntu(spec: &Spec, image: &Path, desktop: bool, tools: &Path, progress: &mut dyn FnMut(&str)) -> Result<PathBuf, String> {
    let dir = &spec.dir;
    progress("Copying the Ubuntu image");
    // A clone on APFS: instant, no space used until it changes.
    let base = dir.join("base.img");
    std::fs::copy(image, &base).map_err(|e| format!("{}: {e}", image.display()))?;
    mark_readonly(&base);
    let disk = dir.join("disk.qcow2");
    vroom_block::qcow2::Qcow2Image::create(&disk, spec.disk_gib << 30, Some((&base, vroom_block::Format::Qcow2))).map_err(|e| format!("disk: {e}"))?;
    progress("Writing the first-boot settings");
    let host = crate::seed::hostname(&spec.name);
    let seed = crate::seed::Seed {
        hostname: &host,
        user: &spec.account.user,
        password: &spec.account.password,
        ssh_key: spec.account.ssh_key.as_deref(),
        desktop,
        instance: vroom_config::template::random8(),
    };
    seed.write(&dir.join("seed.iso"), tools)?;
    let ssh = spec.account.ssh_key.as_ref().map(|_| ssh_port(&spec.taken_ports));
    let gpu = if spec.gpu3d { "virtio3d" } else { "virtio2d" };
    let mut t = format!(
        "# Ubuntu 24.04, made ready by vroom: the cloud image below a disk of\n\
         # its own, set up at first boot by cloud-init (seed.iso: the user and\n\
         # the guest tools{}). Reference: docs/GUIDE.md, docs/CONFIG.md.\n\
         name = {}\nos = \"ubuntu\"\ncpus = {}\nmemory_mib = {}\n\
         \n[boot]\ntype = \"uefi\"\nvars = {}\n\
         \n# base.img (the cloud image) is never written: changes go here.\n\
         [[disk]]\npath = \"disk.qcow2\"\nformat = \"qcow2\"\n\
         \n# Read at first boot only; can go after that.\n\
         [[disk]]\npath = \"seed.iso\"\nreadonly = true\nformat = \"raw\"\n",
        if desktop { ", the desktop" } else { "" },
        toml_str(&spec.name),
        spec.cpus,
        spec.memory_mib,
        toml_str(&format!("{}.state/efivars.fd", spec.name)),
    );
    t += &identity();
    t += &net(ssh);
    t += &format!("\n[display]\ngpu = \"{gpu}\"\nwidth = 1280\nheight = 800\nhidpi = true\n");
    write_config(dir, &spec.name, &t)
}

fn windows(spec: &Spec, iso: &Path, tools: &Path, progress: &mut dyn FnMut(&str)) -> Result<PathBuf, String> {
    let dir = &spec.dir;
    let stick = dir.join("install.img");
    crate::winstick::build(iso, tools, &spec.account, &crate::winstick::computer_name(&spec.name), &stick, progress)?;
    // Windows writes to the stick (FAT access times, setup's files) and
    // stalls for good when a read-only disk refuses (it ignores NVMe's
    // write-protect flag): it gets a layer of its own to write to, and
    // install.img stays as it was made.
    let size = std::fs::metadata(&stick).map_err(|e| e.to_string())?.len();
    vroom_block::qcow2::Qcow2Image::create(&dir.join("install.qcow2"), size, Some((&stick, vroom_block::Format::Raw))).map_err(|e| format!("install disk: {e}"))?;
    mark_readonly(&stick);
    let disk = format!("{}.qcow2", spec.name);
    vroom_block::qcow2::Qcow2Image::create(&dir.join(&disk), spec.disk_gib << 30, None).map_err(|e| format!("disk: {e}"))?;
    let ssh = spec.account.ssh_key.as_ref().map(|_| ssh_port(&spec.taken_ports));
    let mut t = format!(
        "# Windows 11 ARM64, installed unattended by vroom from install.img (the\n\
         # Windows ISO, the virtio drivers, the guest tools{}).\n\
         # Reference: docs/GUIDE.md, docs/CONFIG.md.\n\
         name = {}\nos = \"windows\"\ncpus = {}\nmemory_mib = {}\ntpm = true\n\
         \n[boot]\ntype = \"uefi\"\nvars = {}\n\
         \n# Windows setup only has an NVMe driver.\n\
         [[disk]]\npath = {}\nformat = \"qcow2\"\nbus = \"nvme\"\n\
         \n# The install stick (install.img below a layer Windows may write to).\n\
         # Setup stops rather than reinstall over a disk that has Windows;\n\
         # still, remove this block once Windows runs.\n\
         [[disk]]\npath = \"install.qcow2\"\nformat = \"qcow2\"\nbus = \"nvme\"\n",
        if ssh.is_some() { ", OpenSSH" } else { "" },
        toml_str(&spec.name),
        spec.cpus,
        spec.memory_mib,
        toml_str(&format!("{}.state/efivars.fd", spec.name)),
        toml_str(&disk),
    );
    t += &identity();
    t += &net(ssh);
    t += "\n[display]\ngpu = \"virtio2d\"\nwidth = 1280\nheight = 800\nhidpi = true\n";
    write_config(dir, &spec.name, &t)
}

fn mark_readonly(p: &Path) {
    if let Ok(m) = std::fs::metadata(p) {
        let mut perm = m.permissions();
        perm.set_readonly(true);
        let _ = std::fs::set_permissions(p, perm);
    }
}

/// Ubuntu 24.04's cloud image for ARM64.
pub const UBUNTU_URL: &str = "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img";
const UBUNTU_SUMS: &str = "https://cloud-images.ubuntu.com/noble/current/SHA256SUMS";
const UBUNTU_FILE: &str = "noble-server-cloudimg-arm64.img";

/// The Ubuntu cloud image in `cache`, downloaded (curl, over HTTPS) if it
/// isn't there or isn't the current one, and checked against Ubuntu's
/// SHA256SUMS.
pub fn ubuntu_image(cache: &Path, progress: &mut dyn FnMut(&str)) -> Result<PathBuf, String> {
    use sha2::{Digest, Sha256};
    std::fs::create_dir_all(cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    let curl = |args: &[&str]| -> Result<Vec<u8>, String> {
        let o = std::process::Command::new("curl").args(["-fsSL", "--retry", "3"]).args(args).output().map_err(|e| format!("curl: {e}"))?;
        if o.status.success() {
            Ok(o.stdout)
        } else {
            Err(format!("download failed: {}", String::from_utf8_lossy(&o.stderr).trim()))
        }
    };
    let sums = String::from_utf8_lossy(&curl(&[UBUNTU_SUMS])?).into_owned();
    let want = sums
        .lines()
        .find_map(|l| l.split_once(' ').filter(|(_, n)| n.trim_start_matches([' ', '*']) == UBUNTU_FILE).map(|(h, _)| h.to_ascii_lowercase()))
        .ok_or("Ubuntu's SHA256SUMS doesn't list the ARM64 cloud image")?;
    let path = cache.join(UBUNTU_FILE);
    let sum = |p: &Path| -> Option<String> {
        let mut f = std::fs::File::open(p).ok()?;
        let mut h = Sha256::new();
        std::io::copy(&mut f, &mut h).ok()?;
        Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    };
    if path.is_file() && sum(&path).as_deref() == Some(want.as_str()) {
        return Ok(path);
    }
    progress("Downloading Ubuntu 24.04 (about 600 MB)");
    let part = cache.join(format!("{UBUNTU_FILE}.part"));
    curl(&["-o", &part.to_string_lossy(), UBUNTU_URL])?;
    progress("Checking the download");
    if sum(&part).as_deref() != Some(want.as_str()) {
        let _ = std::fs::remove_file(&part);
        return Err("the download doesn't match Ubuntu's SHA256SUMS".into());
    }
    std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(dir: &Path) -> Spec {
        Spec {
            name: "t".into(),
            dir: dir.join("t"),
            cpus: 2,
            memory_mib: 2048,
            disk_gib: 20,
            gpu3d: false,
            account: Account { user: "me".into(), password: "pw".into(), ssh_key: None },
            taken_ports: vec![],
        }
    }

    #[test]
    fn bad_requests_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let img = t.path().join("noble.img");
        std::fs::write(&img, b"x").unwrap();
        let ok = Kind::Ubuntu { image: img.clone(), desktop: false };
        assert_eq!(check(&spec(t.path()), &ok), Ok(()));
        let mut s = spec(t.path());
        s.account.password.clear();
        assert!(check(&s, &ok).is_err());
        let mut s = spec(t.path());
        s.account.user = "Root Me".into();
        assert!(check(&s, &ok).is_err());
        let mut s = spec(t.path());
        s.account.ssh_key = Some("hello".into());
        assert!(check(&s, &ok).is_err());
        let mut s = spec(t.path());
        s.name = "a/b".into();
        assert!(check(&s, &ok).is_err());
        assert!(check(&spec(t.path()), &Kind::Windows { iso: t.path().join("none.iso") }).is_err());
    }

    #[test]
    fn configs_parse() {
        let mut t = format!("name = \"x\"\nos = \"ubuntu\"\n[boot]\ntype = \"uefi\"\nvars = \"x.state/efivars.fd\"\n{}", identity());
        t += &net(Some(2299));
        let c = vroom_config::VmConfig::from_toml(&t).unwrap();
        assert_eq!(c.nets.len(), 1);
    }

    #[test]
    fn ssh_ports_skip_taken_ones() {
        let p = ssh_port(&[2222, 2223]);
        assert!(p >= 2224);
    }
}
