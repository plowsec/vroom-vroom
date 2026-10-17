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
    /// Guest RAM is shared memory: a reset makes new objects.
    pub shared_ram: bool,
    /// Our helper processes: the only ones we may signal (Linux; macOS's
    /// profile allows no signals but to ourselves, and we cope).
    pub helper_pids: Vec<u32>,
    /// USB disks handed over while the VM runs (macOS): ioctls on raw
    /// disk fds we are given, and volumes we get a token for.
    pub usb_storage: bool,
}

/// A token for the VMM: read (`write`: and change) `dir`'s subtree from
/// its sandbox (macOS). Issued by the app.
pub fn issue_extension(dir: &std::path::Path, write: bool) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        macos::issue_extension(dir, write)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (dir, write);
        Err(Error::Unsupported)
    }
}

/// Use a token from `issue_extension` (the VMM, sandboxed). The handle
/// gives it back (`release_extension`).
pub fn consume_extension(token: &str) -> Result<i64> {
    #[cfg(target_os = "macos")]
    {
        macos::consume_extension(token)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = token;
        Err(Error::Unsupported)
    }
}

pub fn release_extension(handle: i64) {
    #[cfg(target_os = "macos")]
    macos::release_extension(handle);
    #[cfg(not(target_os = "macos"))]
    let _ = handle;
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

/// Seatbelt profile for the camera helper (macOS).
#[cfg(target_os = "macos")]
pub fn camera_profile(exe: &std::path::Path) -> Result<String> {
    macos::camera_profile(exe)
}

/// Seatbelt profile for the video decoding helper (macOS).
#[cfg(target_os = "macos")]
pub fn video_profile(exe: &std::path::Path) -> Result<String> {
    macos::video_profile(exe)
}

/// Seatbelt profile for the USB passthrough helper (macOS).
#[cfg(target_os = "macos")]
pub fn usb_profile(exe: &std::path::Path) -> Result<String> {
    macos::usb_profile(exe)
}

/// The same for the helper vroom-netd starts as root: libusb only from
/// `lib`, a root-owned copy.
#[cfg(target_os = "macos")]
pub fn usb_profile_root(exe: &std::path::Path, lib: &std::path::Path) -> Result<String> {
    macos::usb_profile_root(exe, lib)
}

/// Lock down the USB passthrough helper (Linux), once libusb is loaded:
/// no new privileges, and Landlock leaves it the USB device nodes and
/// sysfs (read-only) only. Its seccomp filter is still to come.
#[cfg(target_os = "linux")]
pub fn apply_usb_helper() -> Result<()> {
    linux::apply_usb_helper()
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
