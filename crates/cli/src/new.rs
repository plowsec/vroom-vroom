//! `vroom new`: write a VM definition and create its disk.

use std::path::{Path, PathBuf};

use vroom_config::template::NewVm;
use vroom_config::GuestOs;
use clap::{Args, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Os {
    /// Ubuntu or another Linux distribution (UEFI, virtio devices).
    Linux,
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
    /// docs/GUIDE.md).
    #[arg(long)]
    pub iso: Option<PathBuf>,
    #[arg(long)]
    pub cpus: Option<u32>,
    #[arg(long)]
    pub memory_mib: Option<u64>,
    /// Where to put the files (default: current directory).
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
}

impl Os {
    fn guest(self) -> GuestOs {
        match self {
            Os::Linux => GuestOs::Linux,
            Os::Windows => GuestOs::Windows,
        }
    }
}

pub fn run(a: NewArgs, load: impl Fn(&Path) -> Result<vroom_config::VmConfig, String>) -> Result<(), String> {
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
