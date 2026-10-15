//! Bring a VM over from VMware (Fusion, Workstation).
//!
//! [`plan`] reads the .vmx and says what the vroom VM will look like and
//! what won't carry over; [`run`] copies the disks to qcow2 and writes the
//! VM file. The VMware VM is only read, never changed.
#![forbid(unsafe_code)]

pub mod vmx;

use std::path::{Path, PathBuf};

use vroom_block::vmdk::VmdkImage;
use vroom_block::DiskImage;
use vroom_config::template::toml_str;
use vroom_config::{DiskBus, GuestOs, Identity, VmConfig};

use vmx::{Vmx, MAX_VMX};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Something to fix on the VMware side first (plain words).
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Io(String),
    #[error("disk: {0}")]
    Disk(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn io(p: &Path, e: impl std::fmt::Display) -> Error {
    Error::Io(format!("{}: {e}", p.display()))
}

#[derive(Debug, Clone)]
pub struct PlanDisk {
    /// The VMware device, e.g. "nvme0:0".
    pub slot: String,
    pub source: PathBuf,
    /// An ISO: attached where it is, not copied.
    pub cdrom: bool,
    pub bus: DiskBus,
}

#[derive(Debug, Clone)]
pub struct PlanShare {
    pub path: PathBuf,
    pub tag: String,
    pub readonly: bool,
}

/// What the imported VM will be.
#[derive(Debug, Clone)]
pub struct Plan {
    pub vmx: PathBuf,
    pub name: String,
    pub os: GuestOs,
    pub cpus: u32,
    pub memory_mib: u64,
    pub disks: Vec<PlanDisk>,
    /// One NAT adapter per VMware adapter.
    pub nets: usize,
    pub shares: Vec<PlanShare>,
    pub audio: bool,
    pub tpm: bool,
    /// VMware's BIOS UUID, in the byte order vroom keeps it in.
    pub uuid: Option<[u8; 16]>,
    /// VMware's SMBIOS serial ("VMware-56 4d ...").
    pub vmware_serial: Option<String>,
    /// Suspended in VMware: importing it loses what was in its memory
    /// (like pulling the plug). [`run`] wants `accept_suspended` then.
    pub suspended: bool,
    /// Things that won't carry over, or need doing after the import.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Name for the new VM (default: VMware's display name).
    pub name: Option<String>,
    /// Keep VMware's BIOS UUID and SMBIOS names, so Windows sees the same
    /// machine (activation). The guest can then tell it came from VMware.
    pub keep_hardware_ids: bool,
    /// Import a VM that is suspended in VMware anyway.
    pub accept_suspended: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    /// Disk being copied (0-based) and how many.
    pub disk: usize,
    pub disks: usize,
    pub done: u64,
    pub total: u64,
}

/// The .vmx in a `.vmwarevm` bundle (or the path itself).
pub fn find_vmx(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let rd = std::fs::read_dir(path).map_err(|e| io(path, e))?;
    let mut found: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("vmx"))).collect();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(Error::Refused(format!("no .vmx file in {}", path.display()))),
        _ => Err(Error::Refused(format!("several .vmx files in {}: pick one", path.display()))),
    }
}

/// VM names: `[A-Za-z0-9._-]`, 64 chars, no leading dot.
fn clean_name(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    out.trim_matches(|c| c == '-' || c == '.').chars().take(64).collect()
}

/// "56 4d 8f 3a 2b 1c 9d 0e-1f 2a 3b 4c 5d 6e 7f 80" -> 16 bytes.
fn parse_uuid_bios(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    let mut u = [0u8; 16];
    for (i, b) in u.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(u)
}

/// VMware's uuid.bios is the raw SMBIOS bytes (first three fields little
/// endian); vroom keeps the usual big-endian form and swaps when it
/// builds the SMBIOS table.
fn smbios_to_rfc(mut u: [u8; 16]) -> [u8; 16] {
    u[0..4].reverse();
    u[4..6].reverse();
    u[6..8].reverse();
    u
}

fn guest_os(vmx_os: &str) -> GuestOs {
    let s = vmx_os.to_ascii_lowercase();
    if s.contains("windows") {
        GuestOs::Windows
    } else if s.contains("ubuntu") {
        GuestOs::Ubuntu
    } else if s.contains("debian") {
        GuestOs::Debian
    } else if s.contains("fedora") || s.contains("rhel") {
        GuestOs::Fedora
    } else if s.contains("linux") || s.contains("other") {
        GuestOs::Linux
    } else {
        GuestOs::Other
    }
}

/// "sata0:1" -> (1, 0, 1); nvme first, then sata, scsi, ide.
fn slot_order(slot: &str) -> (u8, u32, u32) {
    let bus = ["nvme", "sata", "scsi", "ide"].iter().position(|b| slot.starts_with(b)).unwrap_or(9) as u8;
    let digits = slot.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let (c, u) = digits.split_once(':').unwrap_or(("0", "0"));
    (bus, c.parse().unwrap_or(99), u.parse().unwrap_or(99))
}

/// Read a .vmx (or a .vmwarevm bundle) and decide what to import.
pub fn plan(path: &Path) -> Result<Plan> {
    let vmx_path = find_vmx(path)?;
    let dir = vmx_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let len = std::fs::metadata(&vmx_path).map_err(|e| io(&vmx_path, e))?.len();
    if len > MAX_VMX {
        return Err(Error::Refused(format!("{}: too big for a .vmx", vmx_path.display())));
    }
    let text = std::fs::read_to_string(&vmx_path).map_err(|e| io(&vmx_path, e))?;
    let v = Vmx::parse(&text);
    let mut notes = Vec::new();

    // Things to sort out in VMware first.
    let lck = PathBuf::from(format!("{}.lck", vmx_path.display()));
    if lck.exists() {
        return Err(Error::Refused(format!(
            "VMware has this VM open (it's running or its window is open). Shut it down and quit VMware first. If VMware isn't running, the lock is left over from a crash: delete {}.",
            lck.display()
        )));
    }
    // Fusion encrypts a VM with a TPM partly (just what the TPM needs);
    // the disks stay plain. Fully encrypted disks are refused when read.
    let encrypted = v.keys_with("encryption.").next().is_some();
    let suspended = v.get("checkpoint.vmState").is_some_and(|s| !s.is_empty());
    if suspended {
        notes.push("This VM is suspended in VMware. Importing it now is like pulling the plug: what's in its memory is lost. Better: resume it and shut it down in VMware first.".into());
    }
    let vmx_os = v.get("guestOS").unwrap_or("");
    if !vmx_os.is_empty() && !vmx_os.to_ascii_lowercase().starts_with("arm-") {
        return Err(Error::Refused(format!(
            "This is an x86 VM (guestOS = \"{vmx_os}\"). vroom runs ARM64 guests only, so it can't run this one."
        )));
    }
    if vmx_os.is_empty() {
        notes.push("The .vmx doesn't say which OS it runs; assuming an ARM64 Linux.".into());
    }
    let os = if vmx_os.is_empty() { GuestOs::Linux } else { guest_os(vmx_os) };
    let windows = os == GuestOs::Windows;

    let stem = vmx_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut name = clean_name(v.get("displayName").unwrap_or(&stem));
    if !vroom_config::valid_name(&name) {
        name = clean_name(&stem);
    }
    if !vroom_config::valid_name(&name) {
        name = "imported".into();
    }

    // Disks and CDs, boot disk first.
    let mut slots: Vec<String> = v
        .keys_with("")
        .filter_map(|k| k.strip_suffix(".present"))
        .filter(|s| ["nvme", "sata", "scsi", "ide"].iter().any(|b| s.starts_with(b)) && s.contains(':'))
        .map(str::to_string)
        .collect();
    slots.sort_by_key(|s| slot_order(s));
    let mut disks = Vec::new();
    for slot in slots {
        if !v.flag(&format!("{slot}.present"), false) {
            continue;
        }
        let Some(file) = v.get(&format!("{slot}.fileName")).filter(|f| !f.is_empty()) else { continue };
        let kind = v.get(&format!("{slot}.deviceType")).unwrap_or("").to_ascii_lowercase();
        let source = if Path::new(file).is_absolute() { PathBuf::from(file) } else { dir.join(file) };
        let cd = kind.starts_with("cdrom") || kind.starts_with("atapi");
        if cd {
            if kind == "cdrom-image" && source.is_file() {
                disks.push(PlanDisk { slot, source, cdrom: true, bus: DiskBus::Virtio });
            } else if kind == "cdrom-image" {
                notes.push(format!("{slot}: the ISO {} isn't there any more; left out.", source.display()));
            }
            continue;
        }
        if !file.to_ascii_lowercase().ends_with(".vmdk") {
            notes.push(format!("{slot}: {file} isn't a VMDK disk; left out."));
            continue;
        }
        let from_nvme = slot.starts_with("nvme");
        let bus = if windows || from_nvme { DiskBus::Nvme } else { DiskBus::Virtio };
        if windows && !from_nvme && !disks.iter().any(|d: &PlanDisk| !d.cdrom) {
            notes.push(format!(
                "Windows was installed on a {} disk and will see an NVMe one. It may fail to boot (INACCESSIBLE_BOOT_DEVICE).",
                slot.trim_end_matches(|c: char| c.is_ascii_digit() || c == ':')
            ));
        }
        disks.push(PlanDisk { slot, source, cdrom: false, bus });
    }
    if !disks.iter().any(|d| !d.cdrom) {
        return Err(Error::Refused(if encrypted && v.get("displayName").is_none() {
            "This VM is encrypted by VMware. Remove the encryption in VMware first (VM settings, Encryption).".into()
        } else {
            "This VM has no disk to import.".into()
        }));
    }
    if disks.iter().any(|d| !d.cdrom && d.source.file_stem().is_some_and(|s| is_delta_name(&s.to_string_lossy()))) {
        notes.push("The VM has VMware snapshots. Its current state is imported; the snapshots stay in VMware.".into());
    }

    // Network: NAT for every adapter. Bridged and host-only need the root
    // helper here, so they'd fail to start; switch them in the settings.
    let mut nets = 0;
    for i in 0..10 {
        if !v.flag(&format!("ethernet{i}.present"), false) {
            continue;
        }
        nets += 1;
        let kind = v.get(&format!("ethernet{i}.connectionType")).unwrap_or("nat").to_ascii_lowercase();
        if kind != "nat" {
            notes.push(format!(
                "Network adapter {} was {kind} in VMware; it's NAT here. Switch it in Network settings (bridged and host-only need vroom-netd).",
                nets
            ));
        }
    }
    if windows && nets > 0 {
        notes.push("Windows needs the virtio-win drivers and the vroom tools for network, clipboard and resizing: attach the tools disk and run vroom-setup.cmd (see the guide).".into());
    }

    // Shared folders.
    let mut shares: Vec<PlanShare> = Vec::new();
    if v.flag("isolation.tools.hgfs.disable", false) {
        // Sharing off in VMware: keep it off.
    } else {
        for i in 0..32 {
            let k = |f: &str| format!("sharedFolder{i}.{f}");
            if !v.flag(&k("present"), false) || !v.flag(&k("enabled"), true) {
                continue;
            }
            let Some(host) = v.get(&k("hostPath")).filter(|p| Path::new(p).is_absolute()) else { continue };
            let base = clean_name(v.get(&k("guestName")).unwrap_or("share")).chars().take(30).collect::<String>();
            let base = if base.is_empty() { "share".to_string() } else { base };
            let mut tag = base.clone();
            let mut n = 2;
            while shares.iter().any(|s| s.tag == tag) {
                tag = format!("{base}{n}");
                n += 1;
            }
            shares.push(PlanShare { path: PathBuf::from(host), tag, readonly: !v.flag(&k("writeAccess"), false) });
        }
    }
    if !shares.is_empty() && windows {
        notes.push("Shared folders only work in Linux guests for now; they're in the file but Windows won't see them.".into());
    }

    let tpm = v.flag("vtpm.present", false);
    if tpm || encrypted {
        notes.push("The TPM's contents can't come along (VMware encrypts them). If BitLocker is on in the guest, have the recovery key ready.".into());
    }
    let audio = v.flag("sound.present", false) && !windows;

    let raw_uuid = v.get("uuid.bios").and_then(parse_uuid_bios);
    let vmware_serial = v.get("uuid.bios").map(|u| format!("VMware-{}", u.trim()));

    let cpus = v.num("numvcpus").unwrap_or(1).clamp(1, vroom_config::MAX_CPUS as u64) as u32;
    let memory_mib = v.num("memsize").unwrap_or(2048).clamp(vroom_config::MIN_MEMORY_MIB, vroom_config::MAX_MEMORY_MIB);
    Ok(Plan {
        vmx: vmx_path,
        name,
        os,
        cpus,
        memory_mib,
        disks,
        nets,
        shares,
        audio,
        tpm,
        uuid: raw_uuid.map(smbios_to_rfc),
        vmware_serial,
        suspended,
        notes,
    })
}

/// "disk-000001": a VMware snapshot delta.
fn is_delta_name(stem: &str) -> bool {
    stem.rsplit_once('-').is_some_and(|(_, n)| n.len() == 6 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The new VM's file, as text people can read.
pub fn render(p: &Plan, name: &str, disk_files: &[String], opts: &Options, serials: [u8; 8]) -> String {
    let mut s = format!(
        "# Imported from VMware: {}\nname = {}\nos = {}\ncpus = {}\nmemory_mib = {}\ntpm = {}\n\n[boot]\ntype = \"uefi\"\nvars = {}\n",
        p.vmx.display(),
        toml_str(name),
        toml_str(&format!("{:?}", p.os).to_ascii_lowercase()),
        p.cpus,
        p.memory_mib,
        p.tpm,
        toml_str(&format!("{name}.state/efivars.fd")),
    );
    let mut files = disk_files.iter();
    for d in &p.disks {
        if d.cdrom {
            s += &format!("\n# From VMware {}.\n[[disk]]\npath = {}\nreadonly = true\ncdrom = true\n", d.slot, toml_str(&d.source.display().to_string()));
        } else if let Some(f) = files.next() {
            let bus = if d.bus == DiskBus::Nvme { "nvme" } else { "virtio" };
            s += &format!("\n# From VMware {} ({}).\n[[disk]]\npath = {}\nformat = \"qcow2\"\nbus = \"{bus}\"\n", d.slot, file_name(&d.source), toml_str(f));
        }
    }
    for _ in 0..p.nets {
        s += "\n[[net]]\nmode = \"nat\"\n";
    }
    s += "\n[display]\ngpu = \"virtio2d\"\nwidth = 1280\nheight = 800\n";
    for sh in &p.shares {
        s += &format!("\n[[share]]\npath = {}\ntag = {}\nreadonly = {}\n", toml_str(&sh.path.display().to_string()), toml_str(&sh.tag), sh.readonly);
    }
    if p.audio {
        s += "\n[audio]\noutput = true\n";
    }
    let mut id = Identity::default();
    id.randomize_serials(serials);
    s += "\n# What the guest can read about its hardware. Every key: docs/CONFIG.md.\n[identity]\n";
    if opts.keep_hardware_ids {
        // Same names as under VMware, so Windows sees the same machine.
        s += "vendor = \"VMware, Inc.\"\nproduct = \"VMware20,1\"\nfamily = \"VMware\"\n";
        if let Some(sn) = &p.vmware_serial {
            s += &format!("serial = {}\n", toml_str(sn));
        }
    } else if let Some(sn) = &id.serial {
        s += &format!("serial = {}\n", toml_str(sn));
    }
    s += &format!("disk_serial = {}\n", toml_str(&id.disk_serial));
    s
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Copy a disk to a new qcow2, skipping what's all zeros.
pub fn copy_disk(src: &dyn DiskImage, dst: &Path, mut progress: impl FnMut(u64, u64)) -> Result<()> {
    const CHUNK: usize = 1 << 20;
    let size = src.size();
    vroom_block::qcow2::Qcow2Image::create(dst, size, None).map_err(|e| Error::Disk(format!("{}: {e}", dst.display())))?;
    let out = vroom_block::open(dst, Some(vroom_block::Format::Qcow2), false).map_err(|e| Error::Disk(format!("{}: {e}", dst.display())))?;
    let mut buf = vec![0u8; CHUNK];
    let mut off = 0u64;
    while off < size {
        let n = (size - off).min(CHUNK as u64) as usize;
        src.read_at(&mut buf[..n], off).map_err(|e| Error::Disk(format!("read at {off}: {e}")))?;
        if buf[..n].iter().any(|&b| b != 0) {
            out.write_at(&buf[..n], off).map_err(|e| Error::Disk(format!("{}: {e}", dst.display())))?;
        }
        off += n as u64;
        progress(off, size);
    }
    out.flush().map_err(|e| Error::Disk(format!("{}: {e}", dst.display())))
}

/// Import into `<parent>/<name>/`. Built in a hidden dir and renamed at
/// the end, so a failed import leaves nothing half done.
pub fn run(p: &Plan, parent: &Path, opts: &Options, mut progress: impl FnMut(Progress)) -> Result<PathBuf> {
    let name = opts.name.clone().unwrap_or_else(|| p.name.clone());
    if !vroom_config::valid_name(&name) {
        return Err(Error::Refused(format!("name {name:?}: use letters, digits, '.', '_' or '-'")));
    }
    if p.suspended && !opts.accept_suspended {
        return Err(Error::Refused(
            "This VM is suspended in VMware. Resume it and shut it down there first, or import anyway (what's in its memory is lost).".into(),
        ));
    }
    let dest = parent.join(&name);
    if dest.exists() {
        return Err(Error::Refused(format!("{} already exists", dest.display())));
    }
    let tmp = parent.join(format!(".{name}.importing"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| io(&tmp, e))?;
    let r = (|| -> Result<()> {
        let sources: Vec<&PlanDisk> = p.disks.iter().filter(|d| !d.cdrom).collect();
        let mut files = Vec::new();
        for (i, d) in sources.iter().enumerate() {
            let img = VmdkImage::open(&d.source).map_err(|e| Error::Disk(format!("{}: {e}", d.source.display())))?;
            let f = if i == 0 { format!("{name}.qcow2") } else { format!("{name}-disk{}.qcow2", i + 1) };
            copy_disk(&img, &tmp.join(&f), |done, total| progress(Progress { disk: i, disks: sources.len(), done, total }))?;
            files.push(f);
        }
        let text = render(p, &name, &files, opts, vroom_config::template::random8());
        VmConfig::from_toml(&text).map_err(|e| Error::Io(format!("generated VM file is wrong: {e}")))?;
        let cfg = tmp.join(format!("{name}.toml"));
        std::fs::write(&cfg, text).map_err(|e| io(&cfg, e))?;
        if let (true, Some(u)) = (opts.keep_hardware_ids, p.uuid) {
            let state = tmp.join(format!("{name}.state"));
            std::fs::create_dir_all(&state).map_err(|e| io(&state, e))?;
            let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
            let text = format!("{}-{}-{}-{}-{}\n", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]);
            std::fs::write(state.join("uuid"), text).map_err(|e| io(&state, e))?;
        }
        std::fs::rename(&tmp, &dest).map_err(|e| io(&dest, e))
    })();
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    r.map(|_| dest.join(format!("{name}.toml")))
}

#[cfg(test)]
mod tests;
