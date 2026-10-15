//! Process sandbox for the VMM.
//!
//! Applied once setup is done (disks open, sockets bound, hypervisor up).
//! From then on a compromised device model can't read the user's files,
//! spawn programs or open listening sockets.
//!
//! - macOS: Seatbelt profile via `sandbox_init` (deny by default).
//! - Linux: Landlock filesystem rules + seccomp-bpf allowlist + no_new_privs.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sandbox: {0}")]
    Apply(String),
    #[error("sandbox not supported on this OS")]
    Unsupported,
}

pub type Result<T> = std::result::Result<T, Error>;

/// What the VMM still needs after setup.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Files opened read-write later (e.g. disk images reopened on reset).
    pub rw_files: Vec<PathBuf>,
    pub ro_files: Vec<PathBuf>,
    /// Directories where new files may be created (snapshots, overlays).
    pub write_dirs: Vec<PathBuf>,
    /// Shared folders the guest may read.
    pub share_ro: Vec<PathBuf>,
    /// Shared folders the guest may change (files and directories only).
    pub share_rw: Vec<PathBuf>,
    /// Outbound TCP/UDP (userspace NAT).
    pub network_outbound: bool,
    /// ... including to the host's loopback (NAT `allow_host_loopback`).
    /// Otherwise the sandbox refuses it too, not only the NAT code.
    pub loopback: bool,
    /// Host resolver is on loopback: allow port 53 there for the DNS relay.
    pub loopback_dns: bool,
    /// Accept on these already-bound local TCP ports (port forwards).
    pub listen_ports: Vec<u16>,
    /// Talk to the system pasteboard (macOS) in-process.
    pub clipboard: bool,
    /// Use the host GPU (virtio-gpu 3D through virglrenderer/Vulkan).
    pub gpu: bool,
    /// Our helper processes: the only ones we may signal (Linux; macOS's
    /// profile allows no signals but to ourselves, and we cope).
    pub helper_pids: Vec<u32>,
}

/// The Seatbelt profile `apply` would install (macOS), for debugging.
#[cfg(target_os = "macos")]
pub fn profile_text(p: &Policy) -> Result<String> {
    macos::profile(p)
}

/// Seatbelt profile for the audio helper (macOS). The microphone only
/// with `input`.
#[cfg(target_os = "macos")]
pub fn audio_profile(exe: &std::path::Path, wav_dir: Option<&std::path::Path>, input: bool) -> Result<String> {
    macos::audio_profile(exe, wav_dir, input)
}

/// Seatbelt profile for a helper process (macOS); run it with
/// `sandbox-exec -p`.
#[cfg(target_os = "macos")]
pub fn helper_profile(exe: &std::path::Path, rw_dir: &std::path::Path) -> Result<String> {
    macos::helper_profile(exe, rw_dir)
}

/// Apply the sandbox to the whole process. Irreversible.
pub fn apply(p: &Policy) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::apply(p)
    }
    #[cfg(target_os = "linux")]
    {
        linux::apply(p)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = p;
        Err(Error::Unsupported)
    }
}
