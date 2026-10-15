//! A new VM's definition, written for people to read and edit later.

use std::path::Path;

use crate::{GuestOs, Gpu};

/// What a new VM gets. [`NewVm::defaults`] fills in sensible values.
#[derive(Debug, Clone)]
pub struct NewVm<'a> {
    pub name: &'a str,
    pub os: GuestOs,
    /// The system disk, relative to the config (usually `<name>.qcow2`).
    pub disk_file: &'a str,
    /// Linux: an installer ISO. Windows: an install stick image (see
    /// docs/GUIDE.md).
    pub installer: Option<&'a Path>,
    pub cpus: u32,
    pub memory_mib: u64,
    pub gpu: Gpu,
}

impl<'a> NewVm<'a> {
    pub fn defaults(name: &'a str, os: GuestOs, disk_file: &'a str) -> Self {
        let host = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4);
        let windows = os == GuestOs::Windows;
        NewVm {
            name,
            os,
            disk_file,
            installer: None,
            cpus: host.clamp(1, 4),
            memory_mib: if windows { 8192 } else { 4096 },
            gpu: Gpu::Virtio2d,
        }
    }

    /// Default system disk size in GiB.
    pub fn default_disk_gib(os: GuestOs) -> u64 {
        if os == GuestOs::Windows {
            64
        } else {
            32
        }
    }

    pub fn render(&self) -> String {
        let windows = self.os == GuestOs::Windows;
        let (bus, extra) = if windows { ("nvme", "# Windows setup only has an NVMe driver.\n") } else { ("virtio", "") };
        let gpu = match self.gpu {
            Gpu::None => "none",
            Gpu::Virtio2d => "virtio2d",
            Gpu::Virtio3d => "virtio3d",
        };
        let os = toml_str(&format!("{:?}", self.os).to_ascii_lowercase());
        let mut s = format!(
            "# Reference: README.md, docs/GUIDE.md.\n\
             name = \"{name}\"\n\
             os = {os}\n\
             cpus = {cpus}\n\
             memory_mib = {mem}\n\
             tpm = {windows}\n\
             \n\
             [boot]\n\
             type = \"uefi\"\n\
             vars = \"{name}.state/efivars.fd\"\n\
             \n\
             {extra}[[disk]]\n\
             path = {disk}\n\
             format = \"qcow2\"\n\
             bus = \"{bus}\"\n",
            name = self.name,
            cpus = self.cpus,
            mem = self.memory_mib,
            disk = toml_str(self.disk_file),
        );
        if let Some(iso) = self.installer {
            let p = toml_str(&iso.display().to_string());
            s += &if windows {
                format!(
                    "\n# Install stick (FAT32/MBR image, see docs/GUIDE.md). Remove this block\n\
                     # once Windows is installed: left in, a failed boot can fall back to\n\
                     # it and reinstall over disk 0.\n[[disk]]\npath = {p}\nreadonly = true\nformat = \"raw\"\nbus = \"nvme\"\n"
                )
            } else {
                format!("\n# Installer. Remove this block once installed.\n[[disk]]\npath = {p}\nreadonly = true\ncdrom = true\n")
            };
        }
        s += &format!(
            "\n[[net]]\nmode = \"nat\"\n# port_forwards = [{{ host_port = 2222, guest_port = 22 }}]\n\
             \n[display]\ngpu = \"{gpu}\"\nwidth = 1280\nheight = 800\n\
             \n# Shared folder (Linux guest: mount -t virtiofs host /mnt).\n\
             # [[share]]\n# path = \"/Users/me/shared\"\n# tag = \"host\"\n# readonly = true\n"
        );
        s
    }
}

/// A TOML basic string.
fn toml_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if c.is_control() => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VmConfig;

    #[test]
    fn templates_parse() {
        for os in GuestOs::ALL {
            let iso = Path::new("/tmp/x \"quoted\"\\.iso");
            let mut n = NewVm::defaults("t", os, "t.qcow2");
            n.installer = Some(iso);
            let c = VmConfig::from_toml(&n.render()).unwrap();
            assert_eq!(c.tpm, os == GuestOs::Windows);
            assert_eq!(c.guest_os(), os);
            assert_eq!(c.disks.len(), 2);
            assert_eq!(c.disks[1].path, iso);
        }
    }
}
