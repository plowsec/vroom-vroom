//! VM definition.
//!
//! One TOML file per VM. Unknown keys are errors (typos must not silently
//! disable a security setting). Everything is validated before use.
#![forbid(unsafe_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const MAX_CPUS: u32 = 64;
pub const MAX_DISKS: usize = 16;
pub const MAX_NICS: usize = 8;
pub const MIN_MEMORY_MIB: u64 = 64;
pub const MAX_MEMORY_MIB: u64 = 512 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parse: {0}")]
    Parse(String),
    #[error("invalid config: {0}")]
    Invalid(String),
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
    #[serde(default = "default_proto")]
    pub proto: String,
    /// Binds to 127.0.0.1 unless set; exposing to the LAN is explicit.
    #[serde(default = "default_bind")]
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
    /// "52:54:00:12:34:56". Generated if absent.
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
}

fn default_w() -> u32 {
    1280
}
fn default_h() -> u32 {
    800
}

impl Default for Display {
    fn default() -> Self {
        Display { gpu: Gpu::None, width: default_w(), height: default_h() }
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
        if let Serial::File { path } = &mut self.serial {
            fix(path);
        }
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| Error::Parse(e.to_string()))
    }

    pub fn validate(&self) -> Result<()> {
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
        if self.nets.len() > MAX_NICS {
            return bad(format!("at most {MAX_NICS} network adapters"));
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
        let mut macs = Vec::new();
        for n in &self.nets {
            if let Some(m) = &n.mac {
                let mac = parse_mac(m)?;
                if mac[0] & 1 != 0 {
                    return bad(format!("mac {m} is multicast"));
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
        if self.clipboard.max_bytes > 64 << 20 {
            return bad("clipboard.max_bytes too large (max 64 MiB)".into());
        }
        Ok(())
    }
}

fn valid_name(s: &str) -> bool {
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

pub fn format_mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
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
    fn parses_sample() {
        let c = VmConfig::from_toml(SAMPLE).unwrap();
        assert_eq!(c.cpus, 4);
        assert_eq!(c.nets.len(), 3);
        assert_eq!(c.nets[1].mode().unwrap(), NetMode::Bridged { interface: "en0".into() });
        let NetMode::Nat { port_forwards } = c.nets[0].mode().unwrap() else { panic!() };
        assert_eq!(port_forwards[0].host_addr, "127.0.0.1");
        assert!(c.clipboard.host_to_guest && !c.clipboard.guest_to_host);
        assert!(c.security.sandbox);
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
