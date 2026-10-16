//! VM definition.
//!
//! One TOML file per VM. Unknown keys are errors (typos must not silently
//! disable a security setting). Everything is validated before use.
#![forbid(unsafe_code)]

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

pub mod template;

use serde::{Deserialize, Serialize};

pub const MAX_CPUS: u32 = 64;
pub const MAX_DISKS: usize = 16;
pub const MAX_NICS: usize = 8;
pub const MAX_USB: usize = 8;
pub const MIN_MEMORY_MIB: u64 = 64;
pub const MAX_MEMORY_MIB: u64 = 512 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parse: {0}")]
    Parse(String),
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("{0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmConfig {
    pub name: String,
    #[serde(default = "default_cpus")]
    pub cpus: u32,
    #[serde(default = "default_memory")]
    pub memory_mib: u64,
    pub boot: Boot,
    #[serde(default, rename = "disk")]
    pub disks: Vec<Disk>,
    #[serde(default, rename = "net")]
    pub nets: Vec<Net>,
    #[serde(default)]
    pub display: Display,
    #[serde(default)]
    pub clipboard: Clipboard,
    #[serde(default)]
    pub serial: Serial,
    #[serde(default)]
    pub security: Security,
    /// How virtio devices attach. Auto: MMIO for direct kernel boot, PCI
    /// for UEFI (Windows drivers only speak PCI).
    #[serde(default)]
    pub virtio_transport: Transport,
    /// Let the guest run its own VMs (KVM inside the guest). ARM: needs an
    /// M3 or newer Mac on macOS 15+. Snapshots are not supported then.
    #[serde(default)]
    pub nested: bool,
    #[serde(default)]
    pub audio: Audio,
    /// The Mac's camera, as a USB webcam in the guest (Windows and Linux
    /// drivers built in). On only while the guest uses it and the VM's
    /// window is open; macOS asks for permission the first time.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub camera: bool,
    /// Host folders the guest can mount (virtio-fs).
    #[serde(default, rename = "share")]
    pub shares: Vec<Share>,
    /// A TPM 2.0 (swtpm in its own process; state in the state dir).
    /// Windows 11 wants one; Linux uses it for measured boot.
    #[serde(default)]
    pub tpm: bool,
    /// What the guest runs. Only picks icons and sensible defaults; the
    /// VM is the same either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<GuestOs>,
    /// Host USB devices handed to the guest (passthrough).
    #[serde(default, rename = "usb", skip_serializing_if = "Vec::is_empty")]
    pub usb: Vec<Usb>,
    /// What the guest can read about its "hardware".
    #[serde(default, skip_serializing_if = "Identity::is_default")]
    pub identity: Identity,
}

/// A host USB device the guest gets to use. The VM takes the first
/// device that matches `id` (and `serial`, if set) while it runs; a
/// device that is unplugged and plugged back in comes back to the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usb {
    /// "vendor:product", 4 hex digits each, as `vroom usb list` and
    /// `lsusb` print them.
    pub id: String,
    /// Only the device with this serial number (two of the same kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// A label for the app; the guest never sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Usb {
    /// (vendor, product) from `id`.
    pub fn ids(&self) -> Option<(u16, u16)> {
        usb_id(&self.id)
    }
}

/// "vendor:product" in hex, e.g. "1050:0407". Vendor 0 isn't a device.
pub fn usb_id(s: &str) -> Option<(u16, u16)> {
    let hex = |x: &str| (x.len() == 4 && x.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u16::from_str_radix(x, 16).ok()).flatten();
    let (v, p) = s.split_once(':')?;
    let (v, p) = (hex(v)?, hex(p)?);
    (v != 0).then_some((v, p))
}

/// Every name and number the guest can read about the machine it runs on
/// (firmware tables, PCI IDs, disk and USB descriptors, MACs and NAT
/// addresses, the agent's port). Defaults say "vroom" or match QEMU and set
/// the "this is a VM" flags; change them to look like
/// something else, e.g. for malware analysis. The guest can still tell it
/// is virtualized from other things (virtio devices, CPU registers): see
/// docs/SECURITY.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Identity {
    /// SMBIOS: BIOS vendor and the system, chassis, CPU and memory maker.
    pub vendor: String,
    /// SMBIOS system product name.
    pub product: String,
    /// SMBIOS system family.
    pub family: String,
    /// SMBIOS system SKU. Default: the VM's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sku: Option<String>,
    /// SMBIOS BIOS, system and chassis version.
    pub version: String,
    /// SMBIOS BIOS date, MM/DD/YYYY.
    pub bios_date: String,
    /// SMBIOS system and chassis serial. Default: from the VM's UUID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// SMBIOS chassis type (1 other, 3 desktop, 10 notebook, ...).
    pub chassis_type: u8,
    /// SMBIOS processor version: the CPU name Windows and `dmidecode` show.
    pub cpu_name: String,
    /// Set the SMBIOS "virtual machine" BIOS flag and the ACPI hypervisor
    /// vendor field.
    pub announce_vm: bool,
    /// ACPI OEM ID (up to 6 chars), table ID (8), creator ID (4).
    pub acpi_oem_id: String,
    pub acpi_table_id: String,
    pub acpi_creator_id: String,
    /// ACPI FADT hypervisor vendor (up to 8 chars), when `announce_vm`.
    pub hypervisor_id: String,
    /// Device tree model and compatible (direct kernel boot only).
    pub dt_model: String,
    /// NVMe controller model (up to 40 chars).
    pub disk_model: String,
    /// Disk serial prefix (up to 16 chars): NVMe and virtio disks append
    /// a number.
    pub disk_serial: String,
    /// NVMe firmware revision (up to 8 chars).
    pub disk_firmware: String,
    /// PCI IDs, "vendor:device" or "vendor:device:subvendor:subdevice" in
    /// hex. Without the subsystem part it is vendor:0000.
    pub pci_bridge_id: String,
    pub nvme_pci_id: String,
    pub usb_pci_id: String,
    /// Sound card (HD Audio controller), when `[audio]` uses one.
    pub hda_pci_id: String,
    /// HD Audio codec: vendor:device, then its subsystem ID (same format).
    pub hda_codec_id: String,
    /// USB keyboard and tablet (and virtio input): vendor ID and
    /// manufacturer string.
    pub usb_vendor_id: u16,
    pub usb_manufacturer: String,
    /// First 1-3 bytes of generated NIC MACs ("02" or "00:1b:21"); the rest
    /// comes from the VM name. `mac` in `[[net]]` still wins.
    pub mac_prefix: String,
    /// NAT network of the first adapter: guest, gateway and DNS addresses,
    /// one private /24. Adapter N adds N to the third number. DNS may be
    /// the gateway.
    pub nat_guest: Ipv4Addr,
    pub nat_gateway: Ipv4Addr,
    pub nat_dns: Ipv4Addr,
    /// MAC of the NAT gateway (the guest's ARP table).
    pub nat_gateway_mac: String,
    /// The guest agent's virtio-console port. The agent must be told if
    /// this changes (`vroom-agent --port`).
    pub agent_port: String,
    /// OpenGL renderer name (virtio3d): the guest shows "virgl (<this>)".
    /// Default: the host's, which names its GPU and macOS version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gl_renderer: Option<String>,
}

impl Default for Identity {
    fn default() -> Self {
        Identity {
            vendor: "vroom".into(),
            product: "vroom VM".into(),
            family: "Virtual Machine".into(),
            sku: None,
            version: "1.0".into(),
            bios_date: "10/14/2026".into(),
            serial: None,
            chassis_type: 1,
            cpu_name: "CPU".into(),
            announce_vm: true,
            acpi_oem_id: "VROOM".into(),
            acpi_table_id: "VROOMVM".into(),
            acpi_creator_id: "VRM".into(),
            hypervisor_id: "VROOM".into(),
            dt_model: "linux,dummy-virt".into(),
            disk_model: "vroom NVMe".into(),
            disk_serial: "VRM".into(),
            disk_firmware: "1.0".into(),
            pci_bridge_id: "1b36:0008".into(),
            nvme_pci_id: "1b36:0010".into(),
            usb_pci_id: "1b36:000d".into(),
            hda_pci_id: "8086:2668:1af4:1100".into(),
            hda_codec_id: "1af4:0022:1af4:0022".into(),
            usb_vendor_id: 0x1209,
            usb_manufacturer: "vroom".into(),
            mac_prefix: "02".into(),
            nat_guest: Ipv4Addr::new(10, 0, 2, 15),
            nat_gateway: Ipv4Addr::new(10, 0, 2, 2),
            nat_dns: Ipv4Addr::new(10, 0, 2, 3),
            nat_gateway_mac: "02:42:57:4b:00:01".into(),
            agent_port: "org.vroom.agent.0".into(),
            gl_renderer: None,
        }
    }
}

impl Identity {
    pub fn is_default(&self) -> bool {
        *self == Identity::default()
    }

    fn validate(&self) -> Result<()> {
        let text = |what: &str, v: &str, max: usize, empty_ok: bool| -> Result<()> {
            if (!empty_ok && v.is_empty()) || v.len() > max || !v.bytes().all(|b| (0x20..0x7f).contains(&b)) {
                return Err(Error::Invalid(format!("identity.{what}: {} printable ASCII chars", if empty_ok { format!("up to {max}") } else { format!("1-{max}") })));
            }
            Ok(())
        };
        text("vendor", &self.vendor, 64, false)?;
        text("product", &self.product, 64, false)?;
        text("family", &self.family, 64, false)?;
        text("version", &self.version, 64, false)?;
        text("bios_date", &self.bios_date, 10, false)?;
        if let Some(s) = &self.serial {
            text("serial", s, 64, false)?;
        }
        text("acpi_oem_id", &self.acpi_oem_id, 6, false)?;
        text("acpi_table_id", &self.acpi_table_id, 8, false)?;
        text("acpi_creator_id", &self.acpi_creator_id, 4, false)?;
        text("hypervisor_id", &self.hypervisor_id, 8, true)?;
        text("disk_model", &self.disk_model, 40, false)?;
        text("disk_serial", &self.disk_serial, 16, false)?;
        text("usb_manufacturer", &self.usb_manufacturer, 64, false)?;
        if let Some(s) = &self.sku {
            text("sku", s, 64, false)?;
        }
        text("cpu_name", &self.cpu_name, 64, false)?;
        text("dt_model", &self.dt_model, 64, false)?;
        text("disk_firmware", &self.disk_firmware, 8, false)?;
        if let Some(s) = &self.gl_renderer {
            // 63 + NUL fill virgl's 64-byte field.
            text("gl_renderer", s, 63, false)?;
        }
        if !(1..=0x24).contains(&self.chassis_type) {
            return Err(Error::Invalid("identity.chassis_type: 1-36 (SMBIOS chassis type)".into()));
        }
        for (what, v) in [
            ("pci_bridge_id", &self.pci_bridge_id),
            ("nvme_pci_id", &self.nvme_pci_id),
            ("usb_pci_id", &self.usb_pci_id),
            ("hda_pci_id", &self.hda_pci_id),
            ("hda_codec_id", &self.hda_codec_id),
        ] {
            // Vendor 0xffff reads as "no device here".
            if !pci_ids(v).is_some_and(|[vendor, ..]| vendor != 0 && vendor != 0xffff) {
                return Err(Error::Invalid(format!(
                    "identity.{what}: \"vendor:device\" or \"vendor:device:subvendor:subdevice\", 4 hex digits each"
                )));
            }
        }
        if mac_prefix(&self.mac_prefix).is_none() {
            return Err(Error::Invalid("identity.mac_prefix: 1-3 bytes like \"02\" or \"00:1b:21\", not multicast".into()));
        }
        if !parse_mac(&self.nat_gateway_mac).is_ok_and(|m| m[0] & 1 == 0 && m != [0; 6]) {
            return Err(Error::Invalid("identity.nat_gateway_mac: a unicast MAC like \"02:42:57:4b:00:01\"".into()));
        }
        let (g, gw, dns) = (self.nat_guest.octets(), self.nat_gateway.octets(), self.nat_dns.octets());
        let host_ok = |o: [u8; 4]| o[3] != 0 && o[3] != 255;
        if !self.nat_gateway.is_private()
            || g[..3] != gw[..3]
            || dns[..3] != gw[..3]
            || ![g, gw, dns].into_iter().all(host_ok)
            || g == gw
            || g == dns
        {
            return Err(Error::Invalid(
                "identity.nat_guest/nat_gateway/nat_dns: one private /24 (10/8, 172.16/12, 192.168/16), host parts 1-254, guest apart from the others".into(),
            ));
        }
        if !valid_name(&self.agent_port) {
            return Err(Error::Invalid("identity.agent_port: 1-64 chars of [A-Za-z0-9._-]".into()));
        }
        Ok(())
    }

    /// The SMBIOS SKU: `sku`, or the VM's name.
    pub fn sku_or<'a>(&'a self, vm_name: &'a str) -> &'a str {
        self.sku.as_deref().unwrap_or(vm_name)
    }

    /// A NIC MAC: `mac_prefix`, then bytes from `fill`.
    pub fn mac(&self, fill: [u8; 6]) -> [u8; 6] {
        let prefix = mac_prefix(&self.mac_prefix).unwrap_or_else(|| vec![0x02]);
        let mut m = [0u8; 6];
        m[..prefix.len()].copy_from_slice(&prefix);
        m[prefix.len()..].copy_from_slice(&fill[..6 - prefix.len()]);
        m
    }

    /// NAT addresses of adapter `nic`: (guest, gateway, DNS).
    pub fn nat_addrs(&self, nic: usize) -> (Ipv4Addr, Ipv4Addr, Ipv4Addr) {
        let shift = |a: Ipv4Addr| {
            let mut o = a.octets();
            o[2] = o[2].wrapping_add(nic as u8);
            Ipv4Addr::from(o)
        };
        (shift(self.nat_guest), shift(self.nat_gateway), shift(self.nat_dns))
    }

    /// New random serials (system and disks), so two VMs never match.
    pub fn randomize_serials(&mut self, random: [u8; 8]) {
        let hex: String = random.iter().map(|b| format!("{b:02X}")).collect();
        self.serial = Some(hex[..12].to_string());
        self.disk_serial = hex[12..].to_string();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GuestOs {
    Windows,
    Ubuntu,
    Debian,
    Fedora,
    Alpine,
    Linux,
    Other,
}

impl GuestOs {
    pub const ALL: [GuestOs; 7] =
        [GuestOs::Ubuntu, GuestOs::Windows, GuestOs::Debian, GuestOs::Fedora, GuestOs::Alpine, GuestOs::Linux, GuestOs::Other];

    pub fn label(self) -> &'static str {
        match self {
            GuestOs::Windows => "Windows",
            GuestOs::Ubuntu => "Ubuntu",
            GuestOs::Debian => "Debian",
            GuestOs::Fedora => "Fedora",
            GuestOs::Alpine => "Alpine",
            GuestOs::Linux => "Linux",
            GuestOs::Other => "Other",
        }
    }

    pub fn is_linux(self) -> bool {
        !matches!(self, GuestOs::Windows | GuestOs::Other)
    }

    /// Best guess from a file name ("ubuntu-24.04-arm64.iso", "Win11_ARM64.iso").
    pub fn guess(name: &str) -> Option<GuestOs> {
        let n = name.to_ascii_lowercase();
        let has = |k: &str| n.contains(k);
        let windows = has("windows") || n.starts_with("win") || ["win1", "-win", "_win", " win"].iter().any(|k| has(k));
        Some(if windows {
            GuestOs::Windows
        } else if ["ubuntu", "noble", "jammy", "focal", "oracular", "plucky", "questing"].iter().any(|k| has(k)) {
            GuestOs::Ubuntu
        } else if has("debian") {
            GuestOs::Debian
        } else if has("fedora") {
            GuestOs::Fedora
        } else if has("alpine") {
            GuestOs::Alpine
        } else if has("linux") || has("arch") || has("mint") || has("kali") || has("suse") || has("rocky") || has("alma") {
            GuestOs::Linux
        } else {
            return None;
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    #[default]
    Auto,
    Mmio,
    Pci,
}

impl VmConfig {
    /// The sound card to build, if any (`auto` resolved).
    pub fn sound_device(&self) -> Option<SoundDevice> {
        if !self.audio.output && !self.audio.input {
            return None;
        }
        Some(match self.audio.device {
            SoundDevice::Auto if self.guest_os() == GuestOs::Windows => SoundDevice::Hda,
            SoundDevice::Auto => SoundDevice::Virtio,
            d => d,
        })
    }

    /// Resolved transport.
    pub fn use_pci(&self) -> bool {
        match self.virtio_transport {
            Transport::Pci => true,
            Transport::Mmio => false,
            Transport::Auto => matches!(self.boot, Boot::Uefi { .. }),
        }
    }
}

fn default_cpus() -> u32 {
    2
}
fn default_memory() -> u64 {
    2048
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Boot {
    /// Direct kernel boot (fast, no firmware).
    Kernel {
        kernel: PathBuf,
        #[serde(default)]
        initrd: Option<PathBuf>,
        #[serde(default)]
        cmdline: String,
    },
    /// UEFI firmware (needed for Windows and installer ISOs).
    Uefi {
        #[serde(default)]
        firmware: Option<PathBuf>,
        /// Per-VM NVRAM; created on first boot.
        vars: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskFormat {
    #[default]
    Auto,
    Raw,
    Qcow2,
}

/// Sound. The microphone is a separate opt-in (`input`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Audio {
    /// Guest playback through the host's speakers.
    #[serde(default)]
    pub output: bool,
    /// The host microphone, for the guest to record from. Flows only while
    /// the VM's window is open, the VM runs and the guest records.
    #[serde(default)]
    pub input: bool,
    /// Which sound card the guest gets.
    #[serde(default)]
    pub device: SoundDevice,
}

/// The emulated sound card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SoundDevice {
    /// HD Audio for Windows guests, virtio-snd for the others.
    #[default]
    Auto,
    /// Intel HD Audio: Windows and Linux have a driver built in.
    Hda,
    /// virtio-snd: Linux (`virtio_snd`); Windows has no driver.
    Virtio,
}

/// A host folder shared with the guest. Linux: `mount -t virtiofs <tag> /mnt`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Share {
    pub path: PathBuf,
    /// Name the guest mounts it by.
    pub tag: String,
    /// The guest can't change anything (default). Set false to allow it.
    #[serde(default = "yes")]
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disk {
    pub path: PathBuf,
    #[serde(default)]
    pub readonly: bool,
    #[serde(default)]
    pub format: DiskFormat,
    /// Present as a CD-ROM style read-only device (installer ISOs).
    #[serde(default)]
    pub cdrom: bool,
    /// virtio-blk (fast, needs a driver) or NVMe (every OS has a driver,
    /// including Windows setup).
    #[serde(default)]
    pub bus: DiskBus,
    /// What a guest flush costs: "full" (power-loss safe, slow on macOS),
    /// "fsync" (default, survives a host crash) or "none" (scratch disks).
    #[serde(default)]
    pub sync: DiskSync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskSync {
    Full,
    #[default]
    Fsync,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskBus {
    #[default]
    Virtio,
    Nvme,
}

/// Typed network mode, derived from a validated [`Net`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetMode {
    /// Userspace NAT. Guest reaches the outside; nothing reaches the guest
    /// except explicit port forwards.
    Nat { port_forwards: Vec<PortForward> },
    /// Guest appears on the host's physical LAN.
    Bridged { interface: String },
    /// Private network shared by VMs and the host only. No outside access.
    HostOnly { network: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetKind {
    Nat,
    Bridged,
    HostOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortForward {
    #[serde(default = "default_proto", skip_serializing_if = "is_default_proto")]
    pub proto: String,
    /// Binds to 127.0.0.1 unless set; exposing to the LAN is explicit.
    #[serde(default = "default_bind", skip_serializing_if = "is_default_bind")]
    pub host_addr: String,
    pub host_port: u16,
    pub guest_port: u16,
}

fn default_proto() -> String {
    "tcp".into()
}
fn default_bind() -> String {
    "127.0.0.1".into()
}
fn is_default_proto(p: &str) -> bool {
    p == "tcp"
}
fn is_default_bind(a: &str) -> bool {
    a == "127.0.0.1"
}

/// One network adapter, as written in TOML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Net {
    pub mode: NetKind,
    /// Bridged: host interface (e.g. "en0").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// Host-only: network name shared between VMs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// NAT: forwarded ports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub port_forwards: Vec<PortForward>,
    /// NAT: let the guest reach host loopback services via the gateway IP.
    /// Off by default: localhost services often have no auth.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_host_loopback: bool,
    /// NAT: block private (LAN) destinations; internet only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub block_private: bool,
    /// "02:12:34:56:78:9a". Generated if absent (`identity.mac_prefix`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

impl Net {
    /// Typed view. Fails if fields don't match the mode.
    pub fn mode(&self) -> Result<NetMode> {
        let bad = |m: &str| Err(Error::Invalid(format!("net ({:?}): {m}", self.mode)));
        match self.mode {
            NetKind::Nat => {
                if self.interface.is_some() || self.network.is_some() {
                    return bad("nat takes no interface/network");
                }
                if self.port_forwards.iter().any(|p| p.proto != "tcp") {
                    return bad("only tcp port forwards are supported for now");
                }
                Ok(NetMode::Nat { port_forwards: self.port_forwards.clone() })
            }
            NetKind::Bridged | NetKind::HostOnly if self.allow_host_loopback || self.block_private => {
                bad("allow_host_loopback / block_private only apply to nat")
            }
            NetKind::Bridged => {
                if self.network.is_some() || !self.port_forwards.is_empty() {
                    return bad("bridged takes only interface");
                }
                match &self.interface {
                    Some(i) => Ok(NetMode::Bridged { interface: i.clone() }),
                    None => bad("bridged needs interface"),
                }
            }
            NetKind::HostOnly => {
                if self.interface.is_some() || !self.port_forwards.is_empty() {
                    return bad("host-only takes only network");
                }
                match &self.network {
                    Some(n) => Ok(NetMode::HostOnly { network: n.clone() }),
                    None => bad("host-only needs network"),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Gpu {
    /// No GPU; serial console only.
    #[default]
    None,
    /// virtio-gpu 2D scanout.
    Virtio2d,
    /// virtio-gpu with 3D (Vulkan via Venus, GL via virgl), host-rendered.
    Virtio3d,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Display {
    #[serde(default)]
    pub gpu: Gpu,
    #[serde(default = "default_w")]
    pub width: u32,
    #[serde(default = "default_h")]
    pub height: u32,
    /// Full resolution on HiDPI (Retina) screens: the window gives the
    /// guest its size in physical pixels and has the guest agent scale
    /// the guest's UI to match. Without an agent that can, or off: the
    /// guest gets logical pixels and the window scales the picture up.
    #[serde(default = "yes")]
    pub hidpi: bool,
    /// With `virtio3d`: OpenGL (virgl) next to Vulkan.
    /// Off keeps Venus only: less host code parsing guest commands.
    #[serde(default = "yes")]
    pub opengl: bool,
    /// What runs the guest's OpenGL on the host (see [`OpenglHost`]).
    #[serde(default)]
    pub opengl_host: OpenglHost,
    /// Guest monitors, 1 to [`MAX_MONITORS`]: the app shows each in a
    /// window of its own. Linux guests get one GPU with that many
    /// outputs; Windows guests one GPU per monitor (its drivers drive one
    /// output each). UEFI's firmware screen stays on the first.
    #[serde(default = "one")]
    pub monitors: u32,
}

/// Most guest monitors a VM can have.
pub const MAX_MONITORS: u32 = 4;

fn one() -> u32 {
    1
}

/// What runs guest OpenGL on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpenglHost {
    /// macOS's own OpenGL 4.1: the guest gets OpenGL 4.1 (GL 3.3 apps
    /// run), with fewer checks between the guest and Apple's driver.
    #[default]
    Apple,
    /// ANGLE on Metal: OpenGL ES 3.0, so the guest gets OpenGL 2.1. ANGLE
    /// checks every call before the driver sees it.
    Angle,
}

fn default_w() -> u32 {
    1280
}
fn default_h() -> u32 {
    800
}

impl Default for Display {
    fn default() -> Self {
        Display { gpu: Gpu::None, width: default_w(), height: default_h(), hidpi: true, opengl: true, opengl_host: OpenglHost::Apple, monitors: 1 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clipboard {
    #[serde(default = "yes")]
    pub host_to_guest: bool,
    #[serde(default = "yes")]
    pub guest_to_host: bool,
    /// Per-transfer cap. Text only; no files, no rich formats.
    #[serde(default = "default_clip_max")]
    pub max_bytes: u32,
}

fn yes() -> bool {
    true
}
fn default_clip_max() -> u32 {
    1 << 20
}

impl Default for Clipboard {
    fn default() -> Self {
        Clipboard { host_to_guest: true, guest_to_host: true, max_bytes: default_clip_max() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Serial {
    #[default]
    Stdio,
    File {
        path: PathBuf,
    },
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Security {
    /// Lock the VMM process down (seatbelt / seccomp+landlock) after setup.
    #[serde(default = "yes")]
    pub sandbox: bool,
}

impl Default for Security {
    fn default() -> Self {
        Security { sandbox: true }
    }
}

impl VmConfig {
    /// The guest OS, said or guessed (old files don't say).
    pub fn guest_os(&self) -> GuestOs {
        if let Some(o) = self.os {
            return o;
        }
        if let Some(o) = GuestOs::guess(&self.name) {
            return o;
        }
        // Installer and image names usually say ("noble.img", "Win11.iso").
        for d in &self.disks {
            let file = d.path.file_name();
            let dir = d.path.parent().and_then(Path::file_name);
            if let Some(o) = [file, dir].into_iter().flatten().find_map(|n| GuestOs::guess(&n.to_string_lossy())) {
                return o;
            }
        }
        if matches!(self.boot, Boot::Kernel { .. }) {
            return GuestOs::Linux;
        }
        // Windows setups here use NVMe + TPM; Linux ones virtio.
        if self.tpm && self.disks.iter().any(|d| d.bus == DiskBus::Nvme) {
            GuestOs::Windows
        } else {
            GuestOs::Linux
        }
    }

    /// Read and check a VM file, then make its relative paths absolute
    /// against the file's directory, so nothing derived from them (qcow2
    /// backing names in snapshot overlays) depends on the current dir.
    pub fn load(p: &Path) -> Result<Self> {
        let mut c = Self::load_raw(p)?;
        c.rebase_paths(&config_dir(p)?);
        Ok(c)
    }

    /// Read and check a VM file, paths as written (for editing it).
    pub fn load_raw(p: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(p).map_err(|e| Error::Io(format!("{}: {e}", p.display())))?;
        Self::from_toml(&s)
    }

    /// `<config dir>/<name>.state`: snapshots, suspend image, NVRAM, overlays.
    pub fn default_state_dir(&self, config: &Path) -> PathBuf {
        let dir = config.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        dir.join(format!("{}.state", self.name))
    }

    pub fn from_toml(s: &str) -> Result<Self> {
        let c: VmConfig = toml::from_str(s).map_err(|e| Error::Parse(e.to_string()))?;
        c.validate()?;
        Ok(c)
    }

    /// Make relative paths absolute against `base` (the config's directory).
    pub fn rebase_paths(&mut self, base: &std::path::Path) {
        let fix = |p: &mut PathBuf| {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        };
        match &mut self.boot {
            Boot::Kernel { kernel, initrd, .. } => {
                fix(kernel);
                if let Some(i) = initrd {
                    fix(i);
                }
            }
            Boot::Uefi { firmware, vars } => {
                if let Some(f) = firmware {
                    fix(f);
                }
                fix(vars);
            }
        }
        for d in &mut self.disks {
            fix(&mut d.path);
        }
        for s in &mut self.shares {
            fix(&mut s.path);
        }
        if let Serial::File { path } = &mut self.serial {
            fix(path);
        }
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| Error::Parse(e.to_string()))
    }

    /// Valid but risky choices, in plain words (shown by `check` and at
    /// start). Not errors: each can be what the user wants.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if !self.security.sandbox {
            w.push("security.sandbox = false: a guest that breaks the VMM gets your user's full access".into());
        }
        for n in &self.nets {
            if n.allow_host_loopback {
                w.push("net.allow_host_loopback: the guest can reach services listening on this Mac's 127.0.0.1".into());
            }
            for f in &n.port_forwards {
                if f.host_addr != "127.0.0.1" && f.host_addr != "::1" {
                    w.push(format!(
                        "port forward {}:{} -> guest {}: reachable from other machines, not only this one",
                        f.host_addr, f.host_port, f.guest_port
                    ));
                }
            }
            if n.mode == NetKind::Bridged {
                w.push("bridged network: the guest sits on your LAN like a physical machine".into());
            }
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        for s in &self.shares {
            let p = std::fs::canonicalize(&s.path).unwrap_or_else(|_| s.path.clone());
            let broad = p.parent().is_none() || home.as_deref().is_some_and(|h| h.starts_with(&p));
            if broad {
                w.push(format!("share {:?} ({}) exposes your whole home folder or more", s.tag, p.display()));
            }
            if !s.readonly {
                w.push(format!("share {:?} is writable: the guest can change or delete files in {}", s.tag, p.display()));
            }
        }
        if self.audio.input {
            w.push("audio.input: the guest hears your microphone while its window is open and it records".into());
        }
        if self.clipboard.guest_to_host && self.clipboard.host_to_guest {
            // The default, and gated on window focus: worth a mention only
            // together with a disabled sandbox.
            if !self.security.sandbox {
                w.push("clipboard both ways with no sandbox".into());
            }
        }
        w
    }

    pub fn validate(&self) -> Result<()> {
        self.identity.validate()?;
        let bad = |m: String| Err(Error::Invalid(m));
        if !valid_name(&self.name) {
            return bad(format!("name {:?}: use 1-64 chars of [A-Za-z0-9._-]", self.name));
        }
        if self.cpus == 0 || self.cpus > MAX_CPUS {
            return bad(format!("cpus must be 1..={MAX_CPUS}"));
        }
        if !(MIN_MEMORY_MIB..=MAX_MEMORY_MIB).contains(&self.memory_mib) {
            return bad(format!("memory_mib must be {MIN_MEMORY_MIB}..={MAX_MEMORY_MIB}"));
        }
        if self.disks.len() > MAX_DISKS {
            return bad(format!("at most {MAX_DISKS} disks"));
        }
        for (i, s) in self.shares.iter().enumerate() {
            if s.tag.is_empty() || s.tag.len() > 36 || !s.tag.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
                return bad(format!("share tag {:?}: 1-36 chars of [A-Za-z0-9._-]", s.tag));
            }
            if self.shares[..i].iter().any(|o| o.tag == s.tag) {
                return bad(format!("share tag {:?} used twice", s.tag));
            }
        }
        if self.shares.len() > 8 {
            return bad("at most 8 shares".into());
        }
        if self.nets.len() > MAX_NICS {
            return bad(format!("at most {MAX_NICS} network adapters"));
        }
        if self.usb.len() > MAX_USB {
            return bad(format!("at most {MAX_USB} USB devices"));
        }
        let printable = |v: &str, max: usize| !v.is_empty() && v.len() <= max && v.bytes().all(|b| (0x20..0x7f).contains(&b));
        for (i, u) in self.usb.iter().enumerate() {
            if u.ids().is_none() {
                return bad(format!("usb id {:?}: \"vendor:product\", 4 hex digits each (vroom usb list shows them)", u.id));
            }
            if u.serial.as_deref().is_some_and(|s| !printable(s, 126)) {
                return bad(format!("usb {}: serial must be 1-126 printable ASCII chars", u.id));
            }
            if u.name.as_deref().is_some_and(|s| !printable(s, 64)) {
                return bad(format!("usb {}: name must be 1-64 printable ASCII chars", u.id));
            }
            if self.usb[..i].iter().any(|o| o.ids() == u.ids() && o.serial == u.serial) {
                return bad(format!("usb {} listed twice", u.id));
            }
        }
        if let Boot::Kernel { cmdline, .. } = &self.boot {
            if cmdline.len() > 2048 {
                return bad("cmdline too long".into());
            }
        }
        for d in &self.disks {
            if d.cdrom && !d.readonly {
                return bad(format!("{}: cdrom must be readonly", d.path.display()));
            }
        }
        // Adapter N's NAT network is the first one's plus N.
        if usize::from(self.identity.nat_gateway.octets()[2]) + self.nets.len().saturating_sub(1) > 255 {
            return bad("identity.nat_gateway: no room for the other adapters' networks (third number too high)".into());
        }
        let gateway_mac = parse_mac(&self.identity.nat_gateway_mac)?;
        let mut macs = Vec::new();
        for n in &self.nets {
            if let Some(m) = &n.mac {
                let mac = parse_mac(m)?;
                if mac[0] & 1 != 0 {
                    return bad(format!("mac {m} is multicast"));
                }
                if mac == gateway_mac {
                    return bad(format!("mac {m} is the NAT gateway's (identity.nat_gateway_mac)"));
                }
                if macs.contains(&mac) {
                    return bad(format!("duplicate mac {m}"));
                }
                macs.push(mac);
            }
            match &n.mode()? {
                NetMode::Bridged { interface } if !valid_ifname(interface) => {
                    return bad(format!("bad interface name {interface:?}"))
                }
                NetMode::HostOnly { network } if !valid_name(network) => {
                    return bad(format!("bad host-only network name {network:?}"))
                }
                NetMode::Nat { port_forwards } => {
                    for p in port_forwards {
                        if p.proto != "tcp" && p.proto != "udp" {
                            return bad(format!("port forward proto {:?}", p.proto));
                        }
                        if p.host_addr.parse::<std::net::IpAddr>().is_err() {
                            return bad(format!("port forward host_addr {:?}", p.host_addr));
                        }
                    }
                }
                _ => {}
            }
        }
        let d = &self.display;
        if d.width < 320 || d.height < 200 || d.width > 7680 || d.height > 4320 {
            return bad("display size out of range".into());
        }
        if !(1..=MAX_MONITORS).contains(&d.monitors) {
            return bad(format!("display.monitors: 1 to {MAX_MONITORS}"));
        }
        if d.monitors > 1 && d.gpu == Gpu::None {
            return bad("display.monitors needs a GPU (display.gpu)".into());
        }
        if self.clipboard.max_bytes > 64 << 20 {
            return bad("clipboard.max_bytes too large (max 64 MiB)".into());
        }
        Ok(())
    }
}

/// The directory a config's relative paths are relative to, canonical.
pub fn config_dir(config: &Path) -> Result<PathBuf> {
    let base = config.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::canonicalize(base).map_err(|e| Error::Io(format!("{}: {e}", base.display())))
}

/// VM, network and share names: 1-64 of `[A-Za-z0-9._-]`, no leading dot.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('.')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn valid_ifname(s: &str) -> bool {
    !s.is_empty() && s.len() <= 15 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let mut out = [0u8; 6];
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return Err(Error::Invalid(format!("bad mac {s:?}")));
    }
    for (o, p) in out.iter_mut().zip(parts) {
        if p.len() != 2 {
            return Err(Error::Invalid(format!("bad mac {s:?}")));
        }
        *o = u8::from_str_radix(p, 16).map_err(|_| Error::Invalid(format!("bad mac {s:?}")))?;
    }
    Ok(out)
}

/// A `*_pci_id` value: [vendor, device, subsystem vendor, subsystem].
/// "vendor:device" means subsystem vendor:0000.
pub fn pci_ids(s: &str) -> Option<[u16; 4]> {
    let hex = |x: &str| (x.len() == 4 && x.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u16::from_str_radix(x, 16).ok()).flatten();
    let p = s.split(':').map(hex).collect::<Option<Vec<u16>>>()?;
    match p[..] {
        [v, d] => Some([v, d, v, 0]),
        [v, d, sv, sd] => Some([v, d, sv, sd]),
        _ => None,
    }
}

/// `mac_prefix`: 1-3 bytes, the first one unicast.
fn mac_prefix(s: &str) -> Option<Vec<u8>> {
    let hex = |x: &str| (x.len() == 2 && x.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u8::from_str_radix(x, 16).ok()).flatten();
    let p = s.split(':').map(hex).collect::<Option<Vec<u8>>>()?;
    ((1..=3).contains(&p.len()) && p[0] & 1 == 0).then_some(p)
}

pub fn format_mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    #[test]
    fn usb_entries() {
        let base = "name = \"u\"\n[boot]\ntype = \"kernel\"\nkernel = \"/k\"\n";
        let ok = super::VmConfig::from_toml(&format!("{base}[[usb]]\nid = \"1050:0407\"\nserial = \"123\"\n[[usb]]\nid = \"1050:0407\"\n")).unwrap();
        assert_eq!(ok.usb[0].ids(), Some((0x1050, 0x0407)));
        for bad in [
            "[[usb]]\nid = \"1050\"\n",
            "[[usb]]\nid = \"0000:0001\"\n",
            "[[usb]]\nid = \"1050:04079\"\n",
            "[[usb]]\nid = \"1050:0407\"\nserial = \"\"\n",
            "[[usb]]\nid = \"1050:0407\"\nbus = 1\n",
            "[[usb]]\nid = \"1050:0407\"\n[[usb]]\nid = \"1050:0407\"\n",
        ] {
            assert!(super::VmConfig::from_toml(&format!("{base}{bad}")).is_err(), "{bad}");
        }
    }

    #[test]
    fn risky_choices_are_named() {
        let c = super::VmConfig::from_toml(
            r#"
name = "w"
[boot]
type = "uefi"
vars = "w.state/efivars.fd"
[[net]]
mode = "nat"
allow_host_loopback = true
port_forwards = [{ host_port = 2222, guest_port = 22, host_addr = "0.0.0.0" }]
[[share]]
path = "/"
tag = "root"
readonly = false
[security]
sandbox = false
"#,
        )
        .unwrap();
        let w = c.warnings().join("\n");
        for needle in ["sandbox", "127.0.0.1", "0.0.0.0:2222", "whole home folder", "writable"] {
            assert!(w.contains(needle), "{needle} missing in {w}");
        }
        let quiet = super::VmConfig::from_toml("name = \"q\"\n[boot]\ntype = \"uefi\"\nvars = \"v.fd\"\n").unwrap();
        assert!(quiet.warnings().is_empty());
    }

    use super::*;

    const SAMPLE: &str = r#"
name = "ubuntu-dev"
cpus = 4
memory_mib = 4096

[boot]
type = "kernel"
kernel = "/vm/Image"
initrd = "/vm/initrd"
cmdline = "console=ttyAMA0 root=/dev/vda1"

[[disk]]
path = "/vm/disk.qcow2"

[[disk]]
path = "/vm/install.iso"
readonly = true
cdrom = true

[[net]]
mode = "nat"
port_forwards = [{ host_port = 2222, guest_port = 22 }]

[[net]]
mode = "bridged"
interface = "en0"
mac = "52:54:00:aa:bb:cc"

[[net]]
mode = "host-only"
network = "lab"

[display]
gpu = "virtio3d"

[clipboard]
guest_to_host = false
"#;

    #[test]
    fn identity_defaults_and_checks() {
        let c = VmConfig::from_toml(SAMPLE).unwrap();
        assert!(c.identity.is_default());
        assert!(!c.to_toml().unwrap().contains("[identity]"), "defaults stay out of the file");
        let custom = format!("{SAMPLE}\n[identity]\nvendor = \"Dell Inc.\"\nannounce_vm = false\nacpi_oem_id = \"DELL\"\n");
        let c = VmConfig::from_toml(&custom).unwrap();
        assert_eq!(c.identity.vendor, "Dell Inc.");
        assert_eq!(c.identity.acpi_table_id, "VROOMVM", "unset keys keep defaults");
        assert!(VmConfig::from_toml(&custom.replace("DELL", "TOOLONGID")).is_err());
        assert!(VmConfig::from_toml(&custom.replace("Dell Inc.", "Dell\\u0007")).is_err());
        let more = format!(
            "{SAMPLE}\n[identity]\nsku = \"30FA\"\nchassis_type = 3\ncpu_name = \"Snapdragon X Elite\"\n\
             nvme_pci_id = \"144d:a808:144d:a801\"\nusb_pci_id = \"8086:a0ed\"\nmac_prefix = \"00:1b:21\"\n\
             nat_guest = \"192.168.1.100\"\nnat_gateway = \"192.168.1.1\"\nnat_dns = \"192.168.1.1\"\n"
        );
        let c = VmConfig::from_toml(&more).unwrap();
        let i = &c.identity;
        assert_eq!(i.sku_or(&c.name), "30FA");
        assert_eq!(Identity::default().sku_or("dev"), "dev");
        assert_eq!(pci_ids(&i.nvme_pci_id), Some([0x144d, 0xa808, 0x144d, 0xa801]));
        assert_eq!(pci_ids(&i.usb_pci_id), Some([0x8086, 0xa0ed, 0x8086, 0]));
        assert_eq!(i.mac([9, 8, 7, 6, 5, 4]), [0x00, 0x1b, 0x21, 9, 8, 7]);
        assert_eq!(Identity::default().mac([9, 8, 7, 6, 5, 4]), [0x02, 9, 8, 7, 6, 5]);
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert_eq!(i.nat_addrs(2), (ip("192.168.3.100"), ip("192.168.3.1"), ip("192.168.3.1")));
        assert_eq!(Identity::default().nat_addrs(0), (ip("10.0.2.15"), ip("10.0.2.2"), ip("10.0.2.3")));
        assert_eq!(VmConfig::from_toml(&c.to_toml().unwrap()).unwrap(), c, "round trip");
        for (from, to) in [
            ("usb_pci_id = \"8086:a0ed\"", "usb_pci_id = \"ffff:a0ed\""),
            ("usb_pci_id = \"8086:a0ed\"", "usb_pci_id = \"8086:a0e\""),
            ("usb_pci_id = \"8086:a0ed\"", "usb_pci_id = \"8086:a0ed:1\""),
            ("mac_prefix = \"00:1b:21\"", "mac_prefix = \"01\""),
            ("mac_prefix = \"00:1b:21\"", "mac_prefix = \"00:1b:21:00\""),
            ("chassis_type = 3", "chassis_type = 0"),
            ("nat_dns = \"192.168.1.1\"", "nat_dns = \"192.168.2.1\""),
            ("nat_guest = \"192.168.1.100\"", "nat_guest = \"192.168.1.1\""),
            ("nat_guest = \"192.168.1.100\"", "nat_guest = \"192.168.1.255\""),
            ("nat_gateway = \"192.168.1.1\"\nnat_dns = \"192.168.1.1\"", "nat_gateway = \"8.8.8.1\"\nnat_dns = \"8.8.8.8\""),
        ] {
            assert!(VmConfig::from_toml(&more.replace(from, to)).is_err(), "{to}");
        }
        // Three adapters need three /24s above the first one.
        let high = more.replace("192.168.1.", "192.168.254.");
        assert!(VmConfig::from_toml(&high).is_err());
        assert!(VmConfig::from_toml(&high.replace("192.168.254.", "192.168.253.")).is_ok());
        // A [[net]] MAC can't be the gateway's.
        let clash = format!("{SAMPLE}\n[identity]\nnat_gateway_mac = \"52:54:00:aa:bb:cc\"\n");
        assert!(VmConfig::from_toml(&clash).is_err());
        // The GL renderer name fills a 64-byte C string.
        let gl = format!("{SAMPLE}\n[identity]\ngl_renderer = \"Mali-G78\"\n");
        assert_eq!(VmConfig::from_toml(&gl).unwrap().identity.gl_renderer.as_deref(), Some("Mali-G78"));
        assert!(VmConfig::from_toml(&gl.replace("Mali-G78", &"x".repeat(64))).is_err());
        assert!(VmConfig::from_toml(&gl.replace("Mali-G78", "")).is_err());

        let mut i = Identity::default();
        i.randomize_serials([1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(i.serial.as_deref(), Some("010203040506"));
        assert_eq!(i.disk_serial, "0708");
    }

    #[test]
    fn opengl_host() {
        let base = "name = \"x\"\n[boot]\ntype = \"uefi\"\nvars = \"v.fd\"\n[display]\ngpu = \"virtio3d\"\n";
        let c = |extra: &str| VmConfig::from_toml(&format!("{base}{extra}")).map(|c| c.display.opengl_host);
        assert_eq!(c("").unwrap(), OpenglHost::Apple);
        assert_eq!(c("opengl_host = \"angle\"\n").unwrap(), OpenglHost::Angle);
        assert_eq!(c("opengl_host = \"apple\"\n").unwrap(), OpenglHost::Apple);
        assert!(c("opengl_host = \"metal\"\n").is_err());
    }

    #[test]
    fn sound_device_follows_the_guest() {
        let base = "name = \"x\"\n[boot]\ntype = \"uefi\"\nvars = \"v.fd\"\n";
        let c = |extra: &str| VmConfig::from_toml(&format!("{base}{extra}")).unwrap();
        assert_eq!(c("").sound_device(), None);
        assert_eq!(c("[audio]\noutput = true\n").sound_device(), Some(SoundDevice::Virtio));
        let win = VmConfig::from_toml(&format!("os = \"windows\"\n{base}[audio]\ninput = true\n")).unwrap();
        assert_eq!(win.sound_device(), Some(SoundDevice::Hda));
        assert_eq!(c("[audio]\noutput = true\ndevice = \"hda\"\n").sound_device(), Some(SoundDevice::Hda));
        assert!(c("[audio]\ninput = true\n").warnings().iter().any(|w| w.contains("microphone")));
        assert!(VmConfig::from_toml(&format!("{base}[audio]\ndevice = \"ac97\"\n")).is_err());
        let ids = c("[identity]\nhda_pci_id = \"8086:a0c8:17aa:22d8\"\nhda_codec_id = \"10ec:0287:17aa:22d8\"\n");
        assert_eq!(pci_ids(&ids.identity.hda_codec_id), Some([0x10ec, 0x0287, 0x17aa, 0x22d8]));
        assert!(VmConfig::from_toml(&format!("{base}[identity]\nhda_codec_id = \"0000:0287\"\n")).is_err());
    }

    #[test]
    fn parses_sample() {
        let c = VmConfig::from_toml(SAMPLE).unwrap();
        assert_eq!(c.cpus, 4);
        assert_eq!(c.nets.len(), 3);
        assert_eq!(c.nets[1].mode().unwrap(), NetMode::Bridged { interface: "en0".into() });
        let NetMode::Nat { port_forwards } = c.nets[0].mode().unwrap() else { panic!() };
        assert_eq!(port_forwards[0].host_addr, "127.0.0.1");
        assert!(c.clipboard.host_to_guest && !c.clipboard.guest_to_host);
        assert!(c.security.sandbox);
        assert!(c.display.opengl, "GL on by default with 3D");
        // Round trip.
        let again = VmConfig::from_toml(&c.to_toml().unwrap()).unwrap();
        assert_eq!(again, c);
    }

    #[test]
    fn rejects_typos_and_bad_values() {
        let typo = SAMPLE.replace("memory_mib", "memroy_mib");
        assert!(VmConfig::from_toml(&typo).is_err());
        let zero = SAMPLE.replace("cpus = 4", "cpus = 0");
        assert!(VmConfig::from_toml(&zero).is_err());
        let mc = SAMPLE.replace("52:54:00:aa:bb:cc", "01:00:5e:00:00:01");
        assert!(VmConfig::from_toml(&mc).is_err());
        let name = SAMPLE.replace("ubuntu-dev", "../etc");
        assert!(VmConfig::from_toml(&name).is_err());
        let mixed = SAMPLE.replace("network = \"lab\"", "network = \"lab\"\ninterface = \"en0\"");
        assert!(VmConfig::from_toml(&mixed).is_err());
        let cd = SAMPLE.replace("readonly = true\ncdrom", "cdrom");
        assert!(VmConfig::from_toml(&cd).is_err());
    }
}
