//! `vroom new`: write a VM definition and create its disk; with `--ready`,
//! a VM that comes up installed, with the guest tools and optionally SSH
//! (vroom_tools::ready).

use std::path::{Path, PathBuf};

use vroom_config::template::NewVm;
use vroom_config::GuestOs;
use clap::{Args, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Os {
    /// Ubuntu or another Linux distribution (UEFI, virtio devices).
    Linux,
    /// Ubuntu (with --ready: 24.04 from its cloud image).
    Ubuntu,
    /// Windows 11 ARM64 (UEFI, NVMe, TPM 2.0).
    Windows,
}

#[derive(Debug, Args)]
pub struct NewArgs {
    /// VM name (also the file names: <name>.toml, <name>.qcow2).
    pub name: String,
    #[arg(long, value_enum, default_value = "linux")]
    pub os: Os,
    /// Disk size, e.g. 64G.
    #[arg(long, default_value = "32G")]
    pub disk: String,
    /// Installer: an ISO (Linux) or an install stick image (Windows, see
    /// docs/GUIDE.md). With --ready and --os windows: the Windows 11
    /// ARM64 ISO.
    #[arg(long)]
    pub iso: Option<PathBuf>,
    #[arg(long)]
    pub cpus: Option<u32>,
    #[arg(long)]
    pub memory_mib: Option<u64>,
    /// Where to put the files (default: current directory; with --ready,
    /// a folder named after the VM in it).
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// Make it ready to use: installed, guest tools in (ubuntu, windows).
    #[arg(long)]
    pub ready: bool,
    /// --ready: the user to make (default: yours).
    #[arg(long)]
    pub user: Option<String>,
    /// --ready: their password (default: $VROOM_NEW_PASSWORD).
    #[arg(long)]
    pub password: Option<String>,
    /// --ready: set up SSH for this public key (e.g. ~/.ssh/id_ed25519.pub).
    #[arg(long)]
    pub ssh_key: Option<PathBuf>,
    /// --ready, ubuntu: install the desktop (GNOME).
    #[arg(long)]
    pub desktop: bool,
    /// --ready, ubuntu: the cloud image to start from (default: download it).
    #[arg(long)]
    pub image: Option<PathBuf>,
    /// 3D graphics (Linux guests).
    #[arg(long)]
    pub gpu3d: bool,
}

impl Os {
    fn guest(self) -> GuestOs {
        match self {
            Os::Linux => GuestOs::Linux,
            Os::Ubuntu => GuestOs::Ubuntu,
            Os::Windows => GuestOs::Windows,
        }
    }
}

pub fn run(a: NewArgs, load: impl Fn(&Path) -> Result<vroom_config::VmConfig, String>) -> Result<(), String> {
    if a.ready {
        return ready(a);
    }
    if !a.name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) || a.name.is_empty() {
        return Err(format!("name {:?}: use letters, digits, '.', '_' or '-'", a.name));
    }
    std::fs::create_dir_all(&a.dir).map_err(|e| format!("{}: {e}", a.dir.display()))?;
    let cfg_path = a.dir.join(format!("{}.toml", a.name));
    let disk_file = format!("{}.qcow2", a.name);
    let disk_path = a.dir.join(&disk_file);
    for p in [&cfg_path, &disk_path] {
        if p.exists() {
            return Err(format!("{} already exists", p.display()));
        }
    }
    if let Some(iso) = &a.iso {
        if !iso.is_file() {
            return Err(format!("{}: no such file", iso.display()));
        }
    }
    let size = crate::disk::parse_size(&a.disk)?;
    let mut n = NewVm::defaults(&a.name, a.os.guest(), &disk_file);
    n.installer = a.iso.as_deref();
    n.cpus = a.cpus.unwrap_or(n.cpus);
    n.memory_mib = a.memory_mib.unwrap_or(n.memory_mib);
    let text = n.render();
    std::fs::write(&cfg_path, &text).map_err(|e| format!("{}: {e}", cfg_path.display()))?;
    // Check it the way `run` will read it, before creating the disk.
    if let Err(e) = load(&cfg_path) {
        let _ = std::fs::remove_file(&cfg_path);
        return Err(format!("generated config is invalid: {e}"));
    }
    vroom_block::qcow2::Qcow2Image::create(&disk_path, size, None).map_err(|e| e.to_string())?;
    println!("created {} and {} ({} bytes)", cfg_path.display(), disk_path.display(), size);
    println!("start it: vroom-gui {}", cfg_path.display());
    Ok(())
}

fn ready(a: NewArgs) -> Result<(), String> {
    use vroom_tools::ready::{Account, Kind, Spec};
    let user = a.user.or_else(|| std::env::var("USER").ok()).unwrap_or_else(|| "vroom".into());
    let password = a.password.or_else(|| std::env::var("VROOM_NEW_PASSWORD").ok()).ok_or("give a password: --password, or VROOM_NEW_PASSWORD")?;
    let ssh_key = match &a.ssh_key {
        Some(p) => Some(std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?.trim().to_string()),
        None => None,
    };
    let windows = a.os == Os::Windows;
    let defaults = NewVm::defaults(&a.name, a.os.guest(), "");
    let disk = if a.disk == "32G" && windows { "64G".to_string() } else { a.disk.clone() };
    let spec = Spec {
        name: a.name.clone(),
        dir: a.dir.join(&a.name),
        cpus: a.cpus.unwrap_or(defaults.cpus),
        memory_mib: a.memory_mib.unwrap_or(defaults.memory_mib),
        disk_gib: (crate::disk::parse_size(&disk)? >> 30).max(8),
        gpu3d: a.gpu3d,
        account: Account { user, password, ssh_key },
        taken_ports: Vec::new(),
    };
    let mut say = |s: &str| println!("{s}");
    let kind = match a.os {
        Os::Windows => Kind::Windows { iso: a.iso.ok_or("--os windows --ready needs --iso: the Windows 11 ARM64 ISO")? },
        Os::Ubuntu => {
            let image = match a.image {
                Some(p) => p,
                None => {
                    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
                    let cache = if cfg!(target_os = "macos") { home.join("Library/Caches/vroom") } else { home.join(".cache/vroom") };
                    vroom_tools::ready::ubuntu_image(&cache, &mut say)?
                }
            };
            Kind::Ubuntu { image, desktop: a.desktop }
        }
        Os::Linux => return Err("--ready makes ubuntu or windows VMs".into()),
    };
    let cfg = vroom_tools::ready::create(&spec, &kind, &mut say)?;
    println!("created {}", cfg.display());
    println!("start it: vroom-gui {}", cfg.display());
    Ok(())
}
