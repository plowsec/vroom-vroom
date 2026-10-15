//! `bulwark new`: write a VM definition and create its disk.

use std::path::{Path, PathBuf};

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

fn installer(os: Os, iso: &Path) -> String {
    let p = iso.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    match os {
        Os::Linux => format!(
            "\n# Installer. Remove this block once installed.\n[[disk]]\npath = \"{p}\"\nreadonly = true\ncdrom = true\n"
        ),
        Os::Windows => format!(
            "\n# Install stick (FAT32/MBR image, see docs/GUIDE.md). Remove this block\n\
             # once Windows is installed: left in, a failed boot can fall back to\n\
             # it and reinstall over disk 0.\n[[disk]]\npath = \"{p}\"\nreadonly = true\nformat = \"raw\"\nbus = \"nvme\"\n"
        ),
    }
}

pub fn template(a: &NewArgs, disk_file: &str) -> String {
    let host_cpus = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4);
    let cpus = a.cpus.unwrap_or_else(|| host_cpus.clamp(1, 4));
    let (mem, bus, tpm, extra) = match a.os {
        Os::Linux => (a.memory_mib.unwrap_or(4096), "virtio", false, ""),
        Os::Windows => (a.memory_mib.unwrap_or(8192), "nvme", true, "# Windows setup only has an NVMe driver.\n"),
    };
    let mut s = format!(
        "# Created by `bulwark new`. Reference: README.md, docs/GUIDE.md.\n\
         name = \"{name}\"\n\
         cpus = {cpus}\n\
         memory_mib = {mem}\n\
         tpm = {tpm}\n\
         \n\
         [boot]\n\
         type = \"uefi\"\n\
         vars = \"{name}.state/efivars.fd\"\n\
         \n\
         {extra}[[disk]]\n\
         path = \"{disk_file}\"\n\
         format = \"qcow2\"\n\
         bus = \"{bus}\"\n",
        name = a.name,
    );
    if let Some(iso) = &a.iso {
        s += &installer(a.os, iso);
    }
    s += "\n[[net]]\nmode = \"nat\"\n# port_forwards = [{ host_port = 2222, guest_port = 22 }]\n\
          \n[display]\ngpu = \"virtio2d\"\nwidth = 1280\nheight = 800\n\
          \n# Shared folder (Linux guest: mount -t virtiofs host /mnt).\n\
          # [[share]]\n# path = \"/Users/me/shared\"\n# tag = \"host\"\n# readonly = true\n";
    s
}

pub fn run(a: NewArgs, load: impl Fn(&Path) -> Result<bulwark_config::VmConfig, String>) -> Result<(), String> {
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
    let text = template(&a, &disk_file);
    std::fs::write(&cfg_path, &text).map_err(|e| format!("{}: {e}", cfg_path.display()))?;
    // Check it the way `run` will read it, before creating the disk.
    if let Err(e) = load(&cfg_path) {
        let _ = std::fs::remove_file(&cfg_path);
        return Err(format!("generated config is invalid: {e}"));
    }
    bulwark_block::qcow2::Qcow2Image::create(&disk_path, size, None).map_err(|e| e.to_string())?;
    println!("created {} and {} ({} bytes)", cfg_path.display(), disk_path.display(), size);
    println!("start it: bulwark-gui {}", cfg_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_parse() {
        for os in [Os::Linux, Os::Windows] {
            let a = NewArgs {
                name: "t".into(),
                os,
                disk: "8G".into(),
                iso: Some("/tmp/x \"quoted\".iso".into()),
                cpus: None,
                memory_mib: None,
                dir: ".".into(),
            };
            let c = bulwark_config::VmConfig::from_toml(&template(&a, "t.qcow2")).unwrap();
            c.validate().unwrap();
            assert_eq!(c.tpm, os == Os::Windows);
            assert_eq!(c.disks.len(), 2);
        }
    }
}
