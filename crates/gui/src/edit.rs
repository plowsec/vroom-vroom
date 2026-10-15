//! Settings changes from the UI, by key ("cpus", "net.0.fwd.1.host"), and
//! saving them without wrecking the file: only what changed is rewritten,
//! comments and layout stay.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table};
use vroom_config::{
    Boot, Disk, DiskBus, DiskFormat, DiskSync, GuestOs, Gpu, Net, NetKind, PortForward, Serial, Share, Transport, VmConfig,
};

fn bool_of(v: &str) -> bool {
    v == "true" || v == "1"
}

fn num<T: std::str::FromStr>(v: &str, what: &str) -> Result<T, String> {
    v.trim().parse().map_err(|_| format!("{what}: {v:?} is not a number"))
}

fn idx<T>(list: &mut [T], i: &str, what: &str) -> Result<usize, String> {
    let i: usize = num(i, what)?;
    if i < list.len() {
        Ok(i)
    } else {
        Err(format!("no {what} {i}"))
    }
}

fn opt_path(v: &str) -> Option<PathBuf> {
    let v = v.trim();
    (!v.is_empty()).then(|| PathBuf::from(v))
}

/// Change one setting. The caller validates and saves.
pub fn set(c: &mut VmConfig, key: &str, v: &str, default_iface: Option<&str>) -> Result<(), String> {
    let parts: Vec<&str> = key.split('.').collect();
    match parts.as_slice() {
        ["cpus"] => c.cpus = num(v, "CPUs")?,
        ["memory"] => c.memory_mib = num::<u64>(v, "memory")? * 1024,
        ["nested"] => c.nested = bool_of(v),
        ["tpm"] => c.tpm = bool_of(v),
        ["audio"] => c.audio.output = bool_of(v),
        ["sandbox"] => c.security.sandbox = bool_of(v),
        ["os"] => c.os = GuestOs::ALL.get(num::<usize>(v, "os")?).copied(),
        ["gpu"] => {
            c.display.gpu = match v {
                "0" => Gpu::None,
                "2" => Gpu::Virtio3d,
                _ => Gpu::Virtio2d,
            }
        }
        ["width"] => c.display.width = num(v, "width")?,
        ["height"] => c.display.height = num(v, "height")?,
        ["clip", "in"] => c.clipboard.host_to_guest = bool_of(v),
        ["clip", "out"] => c.clipboard.guest_to_host = bool_of(v),
        ["clip", "max"] => c.clipboard.max_bytes = num::<u32>(v, "size")?.saturating_mul(1024),
        ["transport"] => {
            c.virtio_transport = match v {
                "1" => Transport::Mmio,
                "2" => Transport::Pci,
                _ => Transport::Auto,
            }
        }
        ["serial"] => {
            c.serial = match v {
                "1" => Serial::File { path: PathBuf::from(format!("{}.state/serial.log", c.name)) },
                "2" => Serial::None,
                _ => Serial::Stdio,
            }
        }
        ["boot", "kind"] => {
            let kernel = v == "1";
            match (&c.boot, kernel) {
                (Boot::Uefi { .. }, true) => {
                    c.boot = Boot::Kernel { kernel: PathBuf::new(), initrd: None, cmdline: "console=ttyAMA0 root=/dev/vda1".into() }
                }
                (Boot::Kernel { .. }, false) => {
                    c.boot = Boot::Uefi { firmware: None, vars: PathBuf::from(format!("{}.state/efivars.fd", c.name)) }
                }
                _ => {}
            }
        }
        ["boot", "firmware"] => match &mut c.boot {
            Boot::Uefi { firmware, .. } => *firmware = opt_path(v),
            _ => return Err("not a UEFI VM".into()),
        },
        ["boot", f @ ("kernel" | "initrd" | "cmdline")] => match &mut c.boot {
            Boot::Kernel { kernel, initrd, cmdline } => match *f {
                "kernel" => *kernel = PathBuf::from(v.trim()),
                "initrd" => *initrd = opt_path(v),
                _ => *cmdline = v.to_string(),
            },
            _ => return Err("not a kernel-boot VM".into()),
        },
        ["disk", i, f] => {
            let i = idx(&mut c.disks, i, "disk")?;
            let d = &mut c.disks[i];
            match *f {
                "bus" => d.bus = if v == "1" { DiskBus::Nvme } else { DiskBus::Virtio },
                "sync" => {
                    d.sync = match v {
                        "0" => DiskSync::Full,
                        "2" => DiskSync::None,
                        _ => DiskSync::Fsync,
                    }
                }
                "readonly" => d.readonly = bool_of(v) || d.cdrom,
                _ => return Err(format!("unknown setting {key}")),
            }
        }
        ["net", i, "fwd", j, f] => {
            let i = idx(&mut c.nets, i, "adapter")?;
            let n = &mut c.nets[i];
            let j = idx(&mut n.port_forwards, j, "port forward")?;
            let p = &mut n.port_forwards[j];
            match *f {
                "host" => p.host_port = num(v, "port")?,
                "guest" => p.guest_port = num(v, "port")?,
                "lan" => p.host_addr = if bool_of(v) { "0.0.0.0" } else { "127.0.0.1" }.into(),
                _ => return Err(format!("unknown setting {key}")),
            }
        }
        ["net", i, f] => {
            let i = idx(&mut c.nets, i, "adapter")?;
            let n = &mut c.nets[i];
            match *f {
                "mode" => {
                    let mode = match v {
                        "1" => NetKind::Bridged,
                        "2" => NetKind::HostOnly,
                        _ => NetKind::Nat,
                    };
                    if mode != n.mode {
                        // Keep only what the new mode takes.
                        *n = Net { mode, mac: n.mac.take(), ..empty_net() };
                        match mode {
                            NetKind::Bridged => n.interface = Some(default_iface.unwrap_or("en0").to_string()),
                            NetKind::HostOnly => n.network = Some("lab".into()),
                            NetKind::Nat => {}
                        }
                    }
                }
                "iface" => n.interface = Some(v.trim().to_string()),
                "network" => n.network = Some(v.trim().to_string()),
                "loopback" => n.allow_host_loopback = bool_of(v),
                "block" => n.block_private = bool_of(v),
                "mac" => n.mac = opt_path(v).map(|p| p.display().to_string().to_ascii_lowercase()),
                _ => return Err(format!("unknown setting {key}")),
            }
        }
        ["identity", f] => {
            let id = &mut c.identity;
            match *f {
                "vendor" => id.vendor = v.trim().to_string(),
                "product" => id.product = v.trim().to_string(),
                "announce" => id.announce_vm = bool_of(v),
                _ => return Err(format!("unknown setting {key}")),
            }
        }
        ["share", i, f] => {
            let i = idx(&mut c.shares, i, "share")?;
            let s = &mut c.shares[i];
            match *f {
                "tag" => s.tag = v.trim().to_string(),
                "writable" => s.readonly = !bool_of(v),
                _ => return Err(format!("unknown setting {key}")),
            }
        }
        _ => return Err(format!("unknown setting {key}")),
    }
    Ok(())
}

fn empty_net() -> Net {
    Net {
        mode: NetKind::Nat,
        interface: None,
        network: None,
        port_forwards: Vec::new(),
        allow_host_loopback: false,
        block_private: false,
        mac: None,
    }
}

/// Add or remove things. Pickers (share.add, disk.add-*) are done by the
/// caller, which then uses [`add_share`] / [`add_disk`].
pub fn act(c: &mut VmConfig, key: &str) -> Result<(), String> {
    let parts: Vec<&str> = key.split('.').collect();
    match parts.as_slice() {
        ["net", "add"] => c.nets.push(empty_net()),
        ["net", i, "remove"] => {
            let i = idx(&mut c.nets, i, "adapter")?;
            c.nets.remove(i);
        }
        ["net", i, "fwd", "add"] => {
            let used: Vec<u16> = c.nets.iter().flat_map(|n| n.port_forwards.iter().map(|p| p.host_port)).collect();
            let i = idx(&mut c.nets, i, "adapter")?;
            let n = &mut c.nets[i];
            let guest = [22u16, 80, 443, 8080, 3389].into_iter().find(|g| !n.port_forwards.iter().any(|p| p.guest_port == *g)).unwrap_or(8000);
            let mut host = if guest == 22 { 2222 } else { 8000 + guest % 1000 };
            while used.contains(&host) {
                host += 1;
            }
            n.port_forwards.push(PortForward { proto: "tcp".into(), host_addr: "127.0.0.1".into(), host_port: host, guest_port: guest });
        }
        ["net", i, "fwd", j, "remove"] => {
            let i = idx(&mut c.nets, i, "adapter")?;
            let n = &mut c.nets[i];
            let j = idx(&mut n.port_forwards, j, "port forward")?;
            n.port_forwards.remove(j);
        }
        ["net", i, "mac", "random"] => {
            let i = idx(&mut c.nets, i, "adapter")?;
            // identity.mac_prefix, not QEMU's 52:54:00.
            c.nets[i].mac = Some(vroom_config::format_mac(&c.identity.mac(random_bytes())));
        }
        ["identity", "random"] => c.identity.randomize_serials(random_bytes()),
        ["identity", "reset"] => c.identity = vroom_config::Identity::default(),
        ["share", i, "remove"] => {
            let i = idx(&mut c.shares, i, "share")?;
            c.shares.remove(i);
        }
        ["disk", i, "remove"] => {
            let i = idx(&mut c.disks, i, "disk")?;
            c.disks.remove(i);
        }
        _ => return Err(format!("unknown action {key}")),
    }
    Ok(())
}

pub fn add_share(c: &mut VmConfig, path: &Path) {
    let base: String = path
        .file_name()
        .map(|n| n.to_string_lossy().chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).take(30).collect())
        .unwrap_or_default();
    let base = if base.is_empty() || base.starts_with('.') { "host".to_string() } else { base.to_ascii_lowercase() };
    let mut tag = base.clone();
    let mut n = 2;
    while c.shares.iter().any(|s| s.tag == tag) {
        tag = format!("{base}{n}");
        n += 1;
    }
    c.shares.push(Share { path: path.to_path_buf(), tag, readonly: true });
}

pub enum NewDisk {
    Image,
    Iso,
}

pub fn add_disk(c: &mut VmConfig, path: &Path, kind: NewDisk) {
    let nvme = c.guest_os() == GuestOs::Windows;
    c.disks.push(match kind {
        NewDisk::Iso => Disk {
            path: path.to_path_buf(),
            readonly: true,
            format: DiskFormat::Raw,
            cdrom: true,
            bus: DiskBus::Virtio,
            sync: DiskSync::Fsync,
        },
        NewDisk::Image => Disk {
            path: path.to_path_buf(),
            readonly: false,
            format: DiskFormat::Auto,
            cdrom: false,
            bus: if nvme { DiskBus::Nvme } else { DiskBus::Virtio },
            sync: DiskSync::Fsync,
        },
    });
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use std::io::Read;
    let mut b = [0u8; N];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        let t = crate::fmt::now() ^ std::process::id() as u64;
        for (i, x) in b.iter_mut().enumerate() {
            *x = (t >> (i * 8)) as u8;
        }
    }
    b
}

/// Write `after` to `path`, which currently holds `before`. Rewrites only
/// the keys that changed; falls back to a clean rewrite if the result
/// wouldn't read back as `after`.
pub fn save(path: &Path, before: &VmConfig, after: &VmConfig) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let out = match merge(&text, before, after) {
        Some(m) if VmConfig::from_toml(&m).is_ok_and(|c| &c == after) => m,
        _ => after.to_toml().map_err(|e| e.to_string())?,
    };
    crate::library::write_atomic(path, out.as_bytes())
}

fn merge(text: &str, before: &VmConfig, after: &VmConfig) -> Option<String> {
    let mut doc: DocumentMut = text.parse().ok()?;
    let old: DocumentMut = before.to_toml().ok()?.parse().ok()?;
    let new: DocumentMut = after.to_toml().ok()?.parse().ok()?;
    merge_table(doc.as_table_mut(), old.as_table(), new.as_table(), 0);
    Some(doc.to_string())
}

fn render(i: Option<&Item>) -> Option<String> {
    i.map(|i| i.to_string())
}

fn merge_table(dst: &mut Table, old: &Table, new: &Table, depth: usize) {
    let keys: BTreeSet<String> = old.iter().chain(new.iter()).map(|(k, _)| k.to_string()).collect();
    for k in keys {
        let (o, n) = (old.get(&k), new.get(&k));
        if render(o) == render(n) {
            continue;
        }
        let Some(n) = n else {
            dst.remove(&k);
            continue;
        };
        match (dst.get_mut(&k), o, n) {
            (Some(Item::Table(d)), Some(Item::Table(ot)), Item::Table(nt)) => merge_table(d, ot, nt, depth + 1),
            (Some(Item::ArrayOfTables(d)), Some(Item::ArrayOfTables(oa)), Item::ArrayOfTables(na))
                if d.len() == na.len() && oa.len() == na.len() =>
            {
                for i in 0..na.len() {
                    if let (Some(d), Some(o), Some(n)) = (d.get_mut(i), oa.get(i), na.get(i)) {
                        merge_table(d, o, n, depth + 1);
                    }
                }
            }
            (Some(Item::Value(dv)), _, n) => {
                // Keep the user's inline style and the comment after it.
                if let Ok(mut nv) = n.clone().into_value() {
                    *nv.decor_mut() = dv.decor().clone();
                    *dv = nv;
                }
            }
            _ => {
                // Nested lists (port forwards) read better inline.
                let item = if depth > 0 && n.is_array_of_tables() { n.clone().into_value().map(Item::Value).unwrap_or_else(|i| i) } else { n.clone() };
                dst.insert(&k, item);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"# my dev box
name = "dev"
cpus = 2  # keep it small
memory_mib = 4096

[boot]
type = "uefi"
vars = "dev.state/efivars.fd"

# system disk
[[disk]]
path = "dev.qcow2"

[[net]]
mode = "nat"
port_forwards = [{ host_port = 2222, guest_port = 22 }]
"#;

    fn edit(f: impl FnOnce(&mut VmConfig)) -> (String, VmConfig) {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("dev.toml");
        std::fs::write(&p, FILE).unwrap();
        let before = VmConfig::from_toml(FILE).unwrap();
        let mut after = before.clone();
        f(&mut after);
        after.validate().unwrap();
        save(&p, &before, &after).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(VmConfig::from_toml(&text).unwrap(), after);
        (text, after)
    }

    #[test]
    fn keeps_comments() {
        let (t, _) = edit(|c| set(c, "cpus", "4", None).unwrap());
        assert!(t.contains("# my dev box") && t.contains("# system disk"), "{t}");
        assert!(t.contains("cpus = 4  # keep it small"), "{t}");
        assert!(!t.contains("[clipboard]"), "untouched defaults stay out: {t}");
    }

    #[test]
    fn nested_lists_stay_inline() {
        let (t, c) = edit(|c| {
            act(c, "net.0.fwd.add").unwrap();
            set(c, "net.0.fwd.1.lan", "true", None).unwrap();
        });
        assert_eq!(c.nets[0].port_forwards[1].host_addr, "0.0.0.0");
        assert!(!t.contains("[[net.port_forwards]]"), "{t}");
        assert!(t.contains("# system disk"), "{t}");
    }

    #[test]
    fn adding_tables_and_modes() {
        let (t, c) = edit(|c| {
            act(c, "net.add").unwrap();
            set(c, "net.1.mode", "1", Some("en7")).unwrap();
            set(c, "gpu", "2", None).unwrap();
            add_share(c, Path::new("/Users/me/My Stuff"));
            set(c, "memory", "8", None).unwrap();
        });
        assert_eq!(c.nets[1].interface.as_deref(), Some("en7"));
        assert_eq!(c.shares[0].tag, "mystuff");
        assert_eq!(c.memory_mib, 8192);
        assert!(t.contains("# my dev box"), "{t}");
    }

    #[test]
    fn mode_switch_drops_foreign_fields() {
        let mut c = VmConfig::from_toml(FILE).unwrap();
        set(&mut c, "net.0.mode", "2", None).unwrap();
        assert!(c.nets[0].port_forwards.is_empty());
        assert_eq!(c.nets[0].network.as_deref(), Some("lab"));
        c.validate().unwrap();
        assert!(set(&mut c, "net.5.mode", "0", None).is_err());
        assert!(set(&mut c, "cpus", "lots", None).is_err());
    }
}
