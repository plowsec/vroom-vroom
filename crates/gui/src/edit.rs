//! Settings changes from the UI, by key ("cpus", "net.0.fwd.1.host"), and
//! saving them without wrecking the file: only what changed is rewritten,
//! comments and layout stay.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};
use vroom_config::{
    Boot, Disk, DiskBus, DiskFormat, DiskSync, GuestOs, Gpu, Net, NetKind, PortForward, Serial, Share, Transport, VmConfig,
};

fn bool_of(v: &str) -> Result<bool, String> {
    match v {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(format!("{v:?} is not on or off")),
    }
}

/// One of `n` choices, by index.
fn choice(v: &str, n: usize, what: &str) -> Result<usize, String> {
    let i: usize = num(v, what)?;
    if i < n {
        Ok(i)
    } else {
        Err(format!("no {what} {i}"))
    }
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
        ["memory"] => c.memory_mib = num::<u64>(v, "memory")?.checked_mul(1024).ok_or("memory: too large")?,
        ["nested"] => c.nested = bool_of(v)?,
        ["tpm"] => c.tpm = bool_of(v)?,
        ["audio"] => c.audio.output = bool_of(v)?,
        ["sandbox"] => c.security.sandbox = bool_of(v)?,
        ["os"] => c.os = Some(GuestOs::ALL[choice(v, GuestOs::ALL.len(), "system")?]),
        ["gpu"] => c.display.gpu = [Gpu::None, Gpu::Virtio2d, Gpu::Virtio3d][choice(v, 3, "display")?],
        ["opengl"] => c.display.opengl = bool_of(v)?,
        ["width"] => c.display.width = num(v, "width")?,
        ["height"] => c.display.height = num(v, "height")?,
        ["clip", "in"] => c.clipboard.host_to_guest = bool_of(v)?,
        ["clip", "out"] => c.clipboard.guest_to_host = bool_of(v)?,
        ["clip", "max"] => c.clipboard.max_bytes = num::<u32>(v, "size")?.saturating_mul(1024),
        ["transport"] => c.virtio_transport = [Transport::Auto, Transport::Mmio, Transport::Pci][choice(v, 3, "transport")?],
        ["serial"] => {
            c.serial = match choice(v, 3, "serial")? {
                1 => Serial::File { path: PathBuf::from(format!("{}.state/serial.log", c.name)) },
                2 => Serial::None,
                _ => Serial::Stdio,
            }
        }
        ["boot", "kind"] => {
            let kernel = choice(v, 2, "boot")? == 1;
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
                "bus" => d.bus = [DiskBus::Virtio, DiskBus::Nvme][choice(v, 2, "disk bus")?],
                "sync" => d.sync = [DiskSync::Full, DiskSync::Fsync, DiskSync::None][choice(v, 3, "write safety")?],
                "readonly" => d.readonly = bool_of(v)? || d.cdrom,
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
                "lan" => p.host_addr = if bool_of(v)? { "0.0.0.0" } else { "127.0.0.1" }.into(),
                _ => return Err(format!("unknown setting {key}")),
            }
        }
        ["net", i, f] => {
            let i = idx(&mut c.nets, i, "adapter")?;
            let n = &mut c.nets[i];
            match *f {
                "mode" => {
                    let mode = [NetKind::Nat, NetKind::Bridged, NetKind::HostOnly][choice(v, 3, "network mode")?];
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
                "loopback" => n.allow_host_loopback = bool_of(v)?,
                "block" => n.block_private = bool_of(v)?,
                "mac" => n.mac = Some(v.trim().to_ascii_lowercase()).filter(|m| !m.is_empty()),
                _ => return Err(format!("unknown setting {key}")),
            }
        }
        ["identity", f] => {
            let id = &mut c.identity;
            match *f {
                "vendor" => id.vendor = v.trim().to_string(),
                "product" => id.product = v.trim().to_string(),
                "announce" => id.announce_vm = bool_of(v)?,
                _ => return Err(format!("unknown setting {key}")),
            }
        }
        ["share", i, f] => {
            let i = idx(&mut c.shares, i, "share")?;
            let s = &mut c.shares[i];
            match *f {
                "tag" => s.tag = v.trim().to_string(),
                "writable" => s.readonly = !bool_of(v)?,
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
            let first = if guest == 22 { 2222 } else { 8000 + guest % 1000 };
            let host = (first..=u16::MAX).find(|h| !used.contains(h)).ok_or("no free host port")?;
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
/// the keys that changed. Refuses (and leaves the file alone) when the file
/// no longer holds `before` (edited meanwhile) or when the change can't be
/// made without rewriting what the user wrote.
pub fn save(path: &Path, before: &VmConfig, after: &VmConfig) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if VmConfig::from_toml(&text).ok().as_ref() != Some(before) {
        return Err("the file changed on disk meanwhile. Try again".into());
    }
    let out = merge(&text, before, after)
        .filter(|m| VmConfig::from_toml(m).is_ok_and(|c| &c == after))
        .ok_or("this file is laid out in a way the app can't change without rewriting it. Edit it by hand")?;
    crate::library::write_atomic(path, out.as_bytes())
}

fn merge(text: &str, before: &VmConfig, after: &VmConfig) -> Option<String> {
    let mut doc: DocumentMut = text.parse().ok()?;
    let old: DocumentMut = before.to_toml().ok()?.parse().ok()?;
    let new: DocumentMut = after.to_toml().ok()?.parse().ok()?;
    // New tables go after everything that's there.
    let end = last_position(doc.as_table()).map_or(0, |p| p + 1);
    merge_table(doc.as_table_mut(), old.as_table(), new.as_table(), 0, end);
    Some(doc.to_string())
}

fn render(i: Option<&Item>) -> Option<String> {
    i.map(|i| i.to_string())
}

/// The last place in the file a table (or one under it) sits.
fn last_position(t: &Table) -> Option<usize> {
    let mut m = t.position();
    for (_, i) in t.iter() {
        let sub = match i {
            Item::Table(t) => last_position(t),
            Item::ArrayOfTables(a) => a.iter().filter_map(last_position).max(),
            _ => None,
        };
        m = m.max(sub);
    }
    m
}

/// Put a table (and the ones under it) at `pos` in the file. Tables at the
/// same place keep their order, so this means "right after what's there".
fn place(t: &mut Table, pos: usize) {
    t.set_position(pos);
    for (_, i) in t.iter_mut() {
        match i {
            Item::Table(t) => place(t, pos),
            Item::ArrayOfTables(a) => a.iter_mut().for_each(|t| place(t, pos)),
            _ => {}
        }
    }
}

fn merge_table(dst: &mut Table, old: &Table, new: &Table, depth: usize, end: usize) {
    let empty = Table::new();
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
            // A table the old file didn't write (all defaults) counts as empty.
            (Some(Item::Table(d)), None | Some(Item::Table(_)), Item::Table(nt)) => {
                merge_table(d, o.and_then(Item::as_table).unwrap_or(&empty), nt, depth + 1, end)
            }
            (Some(Item::ArrayOfTables(d)), Some(Item::ArrayOfTables(oa)), Item::ArrayOfTables(na)) if d.len() == oa.len() => {
                merge_entries(d, oa, na, depth + 1, end)
            }
            (Some(Item::Value(dv)), _, n) => {
                // Keep the user's inline style and the comment after it.
                if let Ok(mut nv) = n.clone().into_value() {
                    *nv.decor_mut() = dv.decor().clone();
                    *dv = nv;
                }
            }
            _ => {
                let mut item = n.clone();
                if depth > 0 {
                    // Nested lists (port forwards) read better inline.
                    if item.is_array_of_tables() || item.is_table() {
                        item = item.into_value().map(Item::Value).unwrap_or_else(|i| i);
                    }
                } else if let Some(t) = item.as_table_mut() {
                    place(t, end);
                } else if let Some(a) = item.as_array_of_tables_mut() {
                    a.iter_mut().for_each(|t| place(t, end));
                }
                dst.insert(&k, item);
            }
        }
    }
}

/// `[[disk]]`, `[[net]]`...: match old entries to new ones so adding or
/// removing one leaves the others (and their comments) alone. The app adds
/// at the end and removes one at a time.
fn merge_entries(dst: &mut ArrayOfTables, old: &ArrayOfTables, new: &ArrayOfTables, depth: usize, end: usize) {
    let o: Vec<String> = old.iter().map(Table::to_string).collect();
    let n: Vec<String> = new.iter().map(Table::to_string).collect();
    // Old entries that are gone: skip the ones that don't match the next
    // new one, as long as there are more old ones left than new ones.
    let mut gone = Vec::new();
    let mut j = 0;
    for (i, ot) in o.iter().enumerate() {
        if o.len() - gone.len() > n.len() && n.get(j) != Some(ot) {
            gone.push(i);
        } else {
            j += 1;
        }
    }
    for &i in gone.iter().rev() {
        dst.remove(i);
    }
    let kept: Vec<usize> = (0..o.len()).filter(|i| !gone.contains(i)).collect();
    for (di, &oi) in kept.iter().enumerate() {
        if let (Some(d), Some(ot), Some(nt)) = (dst.get_mut(di), old.get(oi), new.get(di)) {
            merge_table(d, ot, nt, depth, end);
        }
    }
    // Added ones go right after the last entry.
    let at = dst.iter().filter_map(last_position).max().unwrap_or(end);
    for t in new.iter().skip(kept.len()) {
        let mut t = t.clone();
        place(&mut t, at);
        dst.push(t);
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
        edit_file(FILE, f)
    }

    fn edit_file(file: &str, f: impl FnOnce(&mut VmConfig)) -> (String, VmConfig) {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("dev.toml");
        std::fs::write(&p, file).unwrap();
        let before = VmConfig::from_toml(file).unwrap();
        let mut after = before.clone();
        f(&mut after);
        after.validate().unwrap();
        save(&p, &before, &after).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(VmConfig::from_toml(&text).unwrap(), after, "{text}");
        (text, after)
    }

    fn has_all(text: &str, want: &[&str]) {
        for w in want {
            assert!(text.contains(w), "lost {w:?}:\n{text}");
        }
    }

    const COMMENTS: &str = r#"# top
# more top

name = "dev" # the name
# before cpus
cpus = 2
memory_mib = 4096 # mem

# boot it
[boot] # header comment
# inside boot
type = "uefi"
vars = "dev.state/efivars.fd" # nvram

# first disk
[[disk]]
path = "a.qcow2" # a
# second disk
[[disk]]
path = "b.qcow2" # b
readonly = true
# third disk
[[disk]]
path = "c.iso" # c
cdrom = true
readonly = true

# the network
[[net]]
mode = "nat" # nat
# forwards
[[net.port_forwards]]
host_port = 2222 # ssh
guest_port = 22

# second net
[[net]]
mode = "host-only"
network = "lab" # lab

# trailing
"#;

    const COMMENTS_KEPT: &[&str] = &[
        "# top",
        "# more top",
        "# the name",
        "# before cpus",
        "# mem",
        "# boot it",
        "# header comment",
        "# inside boot",
        "# nvram",
        "# the network",
        "# nat",
        "# forwards",
        "# ssh",
        "# second net",
        "# lab",
        "# trailing",
    ];

    #[test]
    fn comments_everywhere() {
        let (t, _) = edit_file(COMMENTS, |c| {
            set(c, "cpus", "4", None).unwrap();
            set(c, "disk.1.readonly", "false", None).unwrap();
            set(c, "net.0.fwd.0.host", "2200", None).unwrap();
            set(c, "clip.out", "false", None).unwrap();
        });
        has_all(&t, COMMENTS_KEPT);
        has_all(&t, &["# first disk", "# a", "# second disk", "# b", "# third disk", "# c"]);
        assert!(t.contains("host_port = 2200 # ssh"), "{t}");
        // A new table goes at the end, not between [[disk]] entries.
        assert!(t.find("[clipboard]") > t.find("network = \"lab\""), "{t}");
    }

    #[test]
    fn removing_and_adding_entries() {
        // The middle disk goes: the others keep their comments.
        let (t, c) = edit_file(COMMENTS, |c| act(c, "disk.1.remove").unwrap());
        assert_eq!(c.disks.len(), 2);
        has_all(&t, COMMENTS_KEPT);
        has_all(&t, &["# first disk", "# a", "# third disk", "# c"]);
        assert!(!t.contains("b.qcow2"), "{t}");
        // The first one.
        let (t, _) = edit_file(COMMENTS, |c| act(c, "disk.0.remove").unwrap());
        has_all(&t, &["# second disk", "# b", "# third disk", "# c"]);
        // A disk added: after the last one, everything else kept.
        let (t, c) = edit_file(COMMENTS, |c| add_disk(c, Path::new("d.qcow2"), NewDisk::Image));
        assert_eq!(c.disks[3].path, Path::new("d.qcow2"));
        has_all(&t, COMMENTS_KEPT);
        has_all(&t, &["# first disk", "# a", "# second disk", "# b", "# third disk", "# c"]);
        // A net added after one with port forward subtables: those must stay
        // with their own net.
        let (t, c) = edit_file(COMMENTS, |c| act(c, "net.add").unwrap());
        assert_eq!(c.nets.len(), 3);
        assert_eq!(c.nets[0].port_forwards.len(), 1);
        has_all(&t, COMMENTS_KEPT);
        // The first net goes.
        let (t, c) = edit_file(COMMENTS, |c| act(c, "net.0.remove").unwrap());
        assert_eq!(c.nets[0].network.as_deref(), Some("lab"));
        has_all(&t, &["# second net", "# lab", "# first disk", "# trailing"]);
        // A port forward added to a net whose forwards are subtables.
        let (t, c) = edit_file(COMMENTS, |c| act(c, "net.0.fwd.add").unwrap());
        assert_eq!(c.nets[0].port_forwards.len(), 2);
        has_all(&t, COMMENTS_KEPT);
    }

    #[test]
    fn inline_tables() {
        let file = r#"name = "dev"
boot = { type = "uefi", vars = "dev.fd" } # inline boot
display = { gpu = "virtio2d", width = 1024 } # screen
disk = [{ path = "a.qcow2" }, { path = "b.iso", cdrom = true, readonly = true }] # disks
net = [{ mode = "nat", port_forwards = [{ host_port = 2222, guest_port = 22 }] }]
"#;
        let (t, c) = edit_file(file, |c| {
            set(c, "width", "1920", None).unwrap();
            set(c, "boot.firmware", "/fw.fd", None).unwrap();
            act(c, "disk.0.remove").unwrap();
            act(c, "net.0.fwd.add").unwrap();
        });
        assert_eq!(c.display.width, 1920);
        has_all(&t, &["# inline boot", "# screen", "# disks"]);
        // Still inline: no headers appeared.
        assert!(!t.contains("[display]") && !t.contains("[[disk]]") && !t.contains("[boot]"), "{t}");
        let (t, _) = edit_file(file, |c| add_disk(c, Path::new("c.qcow2"), NewDisk::Image));
        has_all(&t, &["# disks"]);
    }

    #[test]
    fn dotted_keys() {
        let file = r#"name = "dev"
boot.type = "uefi" # dotted
boot.vars = "dev.fd"
clipboard.host_to_guest = false # no paste
security.sandbox = true

[display] # hdr
gpu = "virtio2d"
"#;
        let (t, c) = edit_file(file, |c| {
            set(c, "clip.out", "false", None).unwrap();
            set(c, "clip.in", "true", None).unwrap();
            set(c, "sandbox", "false", None).unwrap();
            set(c, "height", "900", None).unwrap();
        });
        assert!(!c.clipboard.guest_to_host && c.clipboard.host_to_guest && !c.security.sandbox);
        has_all(&t, &["# dotted", "# no paste", "# hdr", "clipboard.host_to_guest = true"]);
    }

    #[test]
    fn identity_table_of_defaults() {
        // An [identity] the user wrote that only holds defaults: to_toml
        // skips it, the merge must not wipe the user's comments in it.
        let file = "name = \"dev\"\n[boot]\ntype = \"uefi\"\nvars = \"x.fd\"\n# looks\n[identity] # who\nvendor = \"vroom\" # maker\n";
        let (t, c) = edit_file(file, |c| set(c, "identity.product", "Box", None).unwrap());
        assert_eq!(c.identity.product, "Box");
        has_all(&t, &["# looks", "# who", "# maker"]);
    }

    #[test]
    fn file_changed_meanwhile() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("dev.toml");
        let before = VmConfig::from_toml(FILE).unwrap();
        // Someone set cpus to 3 by hand after we read the file.
        let theirs = FILE.replace("cpus = 2", "cpus = 3");
        std::fs::write(&p, &theirs).unwrap();
        let mut after = before.clone();
        set(&mut after, "memory", "8", None).unwrap();
        assert!(save(&p, &before, &after).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), theirs);
    }

    /// Every settings key the UI sends, with a value it can send.
    const UI_SETS: &[(&str, &str)] = &[
        ("cpus", "3"),
        ("memory", "4"),
        ("nested", "true"),
        ("os", "1"),
        ("gpu", "2"),
        ("opengl", "false"),
        ("width", "1600"),
        ("height", "900"),
        ("disk.0.bus", "1"),
        ("disk.0.readonly", "true"),
        ("disk.0.sync", "0"),
        ("net.0.mode", "0"),
        ("net.0.fwd.0.host", "2201"),
        ("net.0.fwd.0.guest", "2022"),
        ("net.0.fwd.0.lan", "true"),
        ("net.0.loopback", "true"),
        ("net.0.block", "true"),
        ("net.0.mac", "52:54:00:aa:bb:cc"),
        ("share.0.tag", "work"),
        ("share.0.writable", "true"),
        ("audio", "true"),
        ("clip.in", "false"),
        ("clip.out", "false"),
        ("clip.max", "64"),
        ("sandbox", "false"),
        ("tpm", "true"),
        ("transport", "2"),
        ("identity.vendor", "Acme"),
        ("identity.product", "Box"),
        ("identity.announce", "false"),
        ("serial", "1"),
        ("boot.firmware", "/fw.fd"),
        ("boot.kind", "1"),
        ("boot.kernel", "/k"),
        ("boot.initrd", "/i"),
        ("boot.cmdline", "console=hvc0"),
        // Last: only valid for bridged/host-only adapters.
        ("net.0.mode", "1"),
        ("net.0.iface", "en1"),
        ("net.0.mode", "2"),
        ("net.0.network", "lab2"),
    ];

    const UI_ACTS: &[&str] =
        &["net.0.fwd.add", "net.0.fwd.0.remove", "net.0.mac.random", "identity.random", "share.0.remove", "disk.0.remove", "net.add", "net.0.remove"];

    #[test]
    fn every_ui_key_works() {
        let mut c = VmConfig::from_toml(FILE).unwrap();
        add_share(&mut c, Path::new("/tmp/x"));
        for (k, v) in UI_SETS {
            set(&mut c, k, v, None).unwrap_or_else(|e| panic!("{k}={v}: {e}"));
            c.validate().unwrap_or_else(|e| panic!("{k}={v}: {e}"));
            // Garbage is refused, not taken as some default.
            let text = ["boot.firmware", "boot.kernel", "boot.initrd", "boot.cmdline", "share.0.tag", "identity.vendor", "identity.product", "net.0.iface", "net.0.network"];
            if !text.contains(k) {
                for bad in ["nonsense", "99999999999999999999", "-1"] {
                    let mut d = c.clone();
                    assert!(set(&mut d, k, bad, None).is_err() || d.validate().is_err(), "{k} took {bad}");
                }
            }
        }
        set(&mut c, "net.0.mode", "0", None).unwrap();
        for a in UI_ACTS {
            act(&mut c, a).unwrap_or_else(|e| panic!("{a}: {e}"));
            c.validate().unwrap_or_else(|e| panic!("{a}: {e}"));
        }
        assert!(set(&mut c, "memory", "18014398509481984", None).is_err(), "overflow");
        assert!(act(&mut c, "net.1.remove").is_err(), "no such adapter");
    }

    /// The literal keys in the settings UI are all in the lists above (keys
    /// built from an index are checked there by hand).
    #[test]
    fn ui_sends_known_keys() {
        let ui = include_str!("../ui/settings.slint");
        for (call, rest) in ui.split("Bus.").skip(1).filter_map(|s| s.split_once("(\"")) {
            let key = rest.split('"').next().unwrap();
            if key.ends_with('.') {
                continue; // "disk." + i + ...: in the lists with an index
            }
            match call {
                "set" => assert!(UI_SETS.iter().any(|(k, _)| k == &key), "Bus.set({key:?}) not tested"),
                "act" => assert!(
                    UI_ACTS.contains(&key) || ["disk.add-new", "disk.add-existing", "disk.add-iso", "share.add", "edit-file"].contains(&key),
                    "Bus.act({key:?}) not tested"
                ),
                _ => {}
            }
        }
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
