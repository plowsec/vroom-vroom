//! Seatbelt (sandbox_init) for macOS.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;

use crate::{Error, Policy, Result};

extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> i32;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Quote a path for SBPL. Paths with quotes/backslashes are refused.
fn q(p: &Path) -> Result<String> {
    let real = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let s = real.to_str().ok_or_else(|| Error::Apply(format!("non-utf8 path {}", p.display())))?;
    if s.contains('"') || s.contains('\\') {
        return Err(Error::Apply(format!("refusing odd path {s}")));
    }
    Ok(format!("\"{s}\""))
}

/// Build the profile. Public for tests / `vroom sandbox-profile`.
pub fn profile(p: &Policy) -> Result<String> {
    let mut s = String::from(
        r#"(version 1)
(deny default)
(allow signal (target self))
(allow sysctl-read)
(allow process-info-pidinfo (target self))
(allow file-read-metadata)
(allow file-read* (literal "/dev/urandom") (literal "/dev/random") (literal "/dev/null"))
(allow file-write* (literal "/dev/null"))
(allow file-ioctl (literal "/dev/null"))
(allow mach-task-name (target self))
"#,
    );
    for f in &p.rw_files {
        s += &format!("(allow file-read* file-write* (literal {}))\n", q(f)?);
    }
    for f in &p.ro_files {
        s += &format!("(allow file-read* (literal {}))\n", q(f)?);
    }
    for d in &p.write_dirs {
        s += &format!("(allow file-read* file-write* (subpath {}))\n", q(d)?);
    }
    for d in &p.share_ro {
        s += &format!("(allow file-read* (subpath {}))\n", q(d)?);
    }
    for d in &p.share_rw {
        // Setting times (utimensat) isn't file-write-times on current
        // macOS: allow the writes, then take back what the FUSE server
        // never does (set-id bits, owner, ACLs, flags, xattrs).
        s += &format!("(allow file-read* file-write* (subpath {}))\n", q(d)?);
        s += &format!(
            "(deny file-write-setugid file-write-owner file-write-acl file-write-flags file-write-xattr file-write-finderinfo (subpath {}))\n",
            q(d)?
        );
    }
    if p.network_outbound {
        // IP only: no Unix sockets (ssh-agent, docker.sock...). netd is
        // connected before we lock down. Later rules win in SBPL.
        s += "(allow network-outbound (remote ip))\n";
        s += "(allow system-socket)\n";
        if !p.loopback {
            s += "(deny network-outbound (remote ip \"localhost:*\"))\n";
            if p.loopback_dns {
                s += "(allow network-outbound (remote ip \"localhost:53\"))\n";
            }
        }
    }
    for port in &p.listen_ports {
        s += &format!("(allow network-inbound (local tcp \"*:{port}\"))\n");
    }
    if p.clipboard {
        s += "(allow mach-lookup (global-name \"com.apple.pasteboard.1\") (global-name \"com.apple.distributed_notifications@Uv3\"))\n";
    }
    if p.shared_ram {
        s += &format!(
            "(allow ipc-posix-shm-write-create ipc-posix-shm-write-unlink ipc-posix-shm-read-data ipc-posix-shm-write-data\n  (ipc-posix-name-regex #\"^/vroom-ram-{}-[0-9a-f]+-[0-9a-f]+$\"))\n",
            std::process::id()
        );
    }
    if p.gpu {
        s += GPU_RULES;
        // Anonymous shared memory for the render server (Venus, Direct3D)
        // and DXMT: made with shm_open under a name of our own pid and
        // unlinked straight away. Only those names, only this process.
        s += &format!(
            "(allow ipc-posix-shm-write-create ipc-posix-shm-write-unlink ipc-posix-shm-read-data ipc-posix-shm-write-data\n  (ipc-posix-name-regex #\"^/(proxy-ctx|vkr-shmem|vkr-host-mem|npt-shmem|dxn)-{}-[0-9a-f]+-[0-9a-f]+$\"))\n",
            std::process::id()
        );
        if let Some(cache) = user_cache_dir() {
            // Metal's shader cache, shared by the user's apps: read only,
            // so a VMM taken over can't plant GPU code for them (Metal's
            // compiler service fills it). DXMT's cache is per VM, in the
            // state dir (DXMT_SHADER_CACHE_PATH).
            s += &format!("(allow file-read* (subpath {}))\n", q(&cache.join("com.apple.metal"))?);
        }
    }
    Ok(s)
}

/// Profile for a helper we exec (swtpm): may read its own install and
/// system libraries, read/write `rw_dir`, nothing else (no network, no
/// home directory, no further exec).
/// Profile for the audio helper: CoreAudio's client side and nothing
/// else (no files beyond the system, no network, no exec). The
/// microphone only if `input` (`[audio] input`). `wav_dir`: a test mode
/// writing (and reading) WAV files instead of using the devices.
pub fn audio_profile(exe: &Path, wav_dir: Option<&Path>, input: bool) -> Result<String> {
    let exe_real = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let mut s = format!(
        r#"(version 1)
(deny default)
(allow process-exec (literal {exe}))
(allow file-read* (literal {exe}) (subpath "/usr/lib") (subpath "/System") (subpath "/Library/Apple")
  (subpath "/Library/Audio/Plug-Ins") (literal "/dev/urandom") (literal "/dev/null")
  (literal "/private/etc/localtime") (subpath "/private/var/db/timezone"))
(allow file-read-metadata)
(allow file-read-data (literal "/"))
(allow sysctl-read)
(allow signal (target self))
(allow mach-task-name (target self))
(allow mach-lookup (global-name "com.apple.audio.audiohald") (global-name "com.apple.audio.AudioComponentRegistrar")
  (global-name "com.apple.audio.AudioSession") (global-name "com.apple.coreaudio.components.server"))
(allow ipc-posix-shm-read-data ipc-posix-shm-write-data ipc-posix-shm-read-metadata)
(allow iokit-open (iokit-user-client-class "IOAudioEngineUserClient"))
"#,
        exe = q(&exe_real)?,
    );
    if input {
        s += "(allow device-microphone)\n";
    }
    if let Some(d) = wav_dir {
        s += &format!("(allow file-read* file-write* (subpath {}))\n", q(d)?);
    }
    Ok(s)
}

/// Profile for the camera helper: AVFoundation's client side, the camera
/// services and frame buffers (IOSurface), the camera itself. No files
/// beyond the system, no network, no exec, no writes.
pub fn camera_profile(exe: &Path) -> Result<String> {
    let exe_real = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    Ok(format!(
        r#"(version 1)
(deny default)
(allow process-exec (literal {exe}))
(allow file-read* (literal {exe}) (subpath "/usr/lib") (subpath "/System") (subpath "/Library/Apple")
  (subpath "/Library/CoreMediaIO") (literal "/dev/urandom") (literal "/dev/null")
  (literal "/private/etc/localtime") (subpath "/private/var/db/timezone"))
(allow file-read-metadata)
(allow file-read-data (literal "/"))
(allow sysctl-read)
(allow signal (target self))
(allow mach-task-name (target self))
(allow device-camera)
(allow mach-lookup (global-name-regex #"^com\.apple\.cmio\.") (global-name "com.apple.tccd") (global-name "com.apple.tccd.system")
  (global-name "com.apple.coremedia.capturesession") (global-name "com.apple.coremedia.capturesource")
  (global-name "com.apple.CoreServices.coreservicesd") (global-name "com.apple.lsd.mapdb"))
(allow iokit-get-properties)
(allow iokit-open (iokit-user-client-class "IOSurfaceRootUserClient" "IOUserClient"))
(allow ipc-posix-shm-read-data ipc-posix-shm-read-metadata)
"#,
        exe = q(&exe_real)?,
    ))
}

/// Profile for the video helper: VideoToolbox's client side. The
/// decoding itself runs in Apple's own decoder service (an XPC service,
/// sandboxed by macOS); we may reach that service and the frame buffers
/// (IOSurface) it hands back, nothing else: no files beyond the system,
/// no network, no exec, no GPU, no writes.
pub fn video_profile(exe: &Path) -> Result<String> {
    let exe_real = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    Ok(format!(
        r#"(version 1)
(deny default)
(allow process-exec (literal {exe}))
(allow file-read* (literal {exe}) (subpath "/usr/lib") (subpath "/System") (subpath "/Library/Apple")
  (literal "/dev/urandom") (literal "/dev/null") (literal "/private/etc/localtime") (subpath "/private/var/db/timezone"))
(allow file-read-metadata)
(allow file-read-data (literal "/"))
(allow sysctl-read)
(allow signal (target self))
(allow mach-task-name (target self))
(allow mach-lookup (xpc-service-name "com.apple.coremedia.videodecoder"))
(allow iokit-open (iokit-user-client-class "IOSurfaceRootUserClient"))
"#,
        exe = q(&exe_real)?,
    ))
}

/// Profile for the USB helper: libusb (from Homebrew or /usr/local), the
/// IOKit registry, and the USB device and interface user clients. No
/// files beyond the system and libusb, no network, no exec, no other
/// IOKit drivers. Which device it opens is up to the helper (the one its
/// command line names); the sandbox can't tell USB devices apart.
pub fn usb_profile(exe: &Path) -> Result<String> {
    usb_profile_with(
        exe,
        r#"(subpath "/opt/homebrew/lib") (subpath "/opt/homebrew/Cellar/libusb") (subpath "/opt/homebrew/opt/libusb")
  (subpath "/usr/local/lib") (subpath "/usr/local/Cellar/libusb") (subpath "/usr/local/opt/libusb")"#,
    )
}

/// The USB helper vroom-netd starts as root: libusb from `lib` (its own
/// root-owned copy) and nowhere else, as nothing a user can write may be
/// loaded into a root process.
pub fn usb_profile_root(exe: &Path, lib: &Path) -> Result<String> {
    usb_profile_with(exe, &format!("(literal {})", q(lib)?))
}

fn usb_profile_with(exe: &Path, libs: &str) -> Result<String> {
    let exe_real = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    Ok(format!(
        r#"(version 1)
(deny default)
(allow process-exec (literal {exe}))
(allow file-read* (literal {exe}) (subpath "/usr/lib") (subpath "/System") (subpath "/Library/Apple")
  {libs}
  (literal "/dev/urandom") (literal "/dev/null") (literal "/private/etc/localtime") (subpath "/private/var/db/timezone"))
(allow file-write* (literal "/dev/null"))
(allow file-read-metadata)
(allow file-read-data (literal "/"))
(allow sysctl-read)
(allow signal (target self))
(allow mach-task-name (target self))
(allow iokit-get-properties)
(allow iokit-open (iokit-user-client-class "AppleUSBHostDeviceUserClient" "AppleUSBHostInterfaceUserClient"
  "AppleUSBLegacyDeviceUserClient" "AppleUSBLegacyInterfaceUserClient" "IOUSBDeviceUserClientV2" "IOUSBInterfaceUserClientV3"))
"#,
        exe = q(&exe_real)?,
    ))
}

pub fn helper_profile(exe: &Path, rw_dir: &Path) -> Result<String> {
    let exe_real = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let prefix = exe_real.ancestors().nth(3).unwrap_or(Path::new("/opt/homebrew"));
    Ok(format!(
        r#"(version 1)
(deny default)
(allow process-exec (literal {exe}))
(allow file-read* (subpath {prefix}) (subpath "/opt/homebrew") (subpath "/usr/lib") (subpath "/System")
  (subpath "/Library/Apple") (literal "/dev/urandom") (literal "/dev/random") (literal "/dev/null")
  (literal "/private/etc/localtime") (subpath "/private/var/db/timezone"))
(allow file-write* (literal "/dev/null"))
(allow file-read-metadata)
(allow file-read-data (literal "/"))
(allow file-read* file-write* (subpath {dir}))
(allow sysctl-read)
(allow signal (target self))
(allow mach-task-name (target self))
"#,
        exe = q(&exe_real)?,
        prefix = q(prefix)?,
        dir = q(rw_dir)?,
    ))
}

/// Metal through MoltenVK: driver bundles, the GPU's IOKit user clients
/// (only those), and the shader compiler service. This is the attack
/// surface 3D adds; without `gpu` none of it is reachable.
const GPU_RULES: &str = r#"(allow file-read* (subpath "/System/Library/Extensions") (subpath "/System/Library/Frameworks")
  (subpath "/System/Library/PrivateFrameworks") (subpath "/usr/lib") (subpath "/Library/Apple/System/Library"))
(allow file-read* (literal "/System/Library/CoreServices/SystemVersion.plist"))
(allow iokit-open (iokit-user-client-class "AGXDeviceUserClient" "AGXSharedUserClient" "IOSurfaceRootUserClient"))
(allow iokit-get-properties)
(allow mach-lookup (global-name "com.apple.MTLCompilerService") (xpc-service-name "com.apple.MTLCompilerService")
  (global-name "com.apple.gpumemd.source") (global-name "com.apple.PowerManagement.control"))
"#;

/// confstr(_CS_DARWIN_USER_CACHE_DIR), e.g. /var/folders/xx/yyy/C/.
fn user_cache_dir() -> Option<std::path::PathBuf> {
    const CS_DARWIN_USER_CACHE_DIR: i32 = 65538;
    let mut buf = vec![0u8; 1024];
    // SAFETY: buf is valid for its length.
    let n = unsafe { libc::confstr(CS_DARWIN_USER_CACHE_DIR, buf.as_mut_ptr() as *mut c_char, buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    let s = CStr::from_bytes_until_nul(&buf).ok()?.to_str().ok()?;
    // Rules match real paths: this is under /var, a link to /private/var.
    let p = std::path::PathBuf::from(s);
    Some(p.canonicalize().unwrap_or(p))
}

pub fn apply(p: &Policy) -> Result<()> {
    let prof = profile(p)?;
    log::debug!("seatbelt profile:\n{prof}");
    let c = CString::new(prof).map_err(|_| Error::Apply("NUL in profile".into()))?;
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated profile; err is an out pointer.
    let rc = unsafe { sandbox_init(c.as_ptr(), 0, &mut err) };
    if rc != 0 {
        let msg = if err.is_null() {
            "unknown error".to_string()
        } else {
            // SAFETY: sandbox_init returns a C string we must free.
            let m = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
            // SAFETY: freeing the buffer sandbox_init allocated.
            unsafe { sandbox_free_error(err) };
            m
        };
        return Err(Error::Apply(msg));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_quotes_and_rejects() {
        let p = Policy { rw_files: vec!["/tmp/a b.qcow2".into()], network_outbound: true, ..Default::default() };
        let s = profile(&p).unwrap();
        assert!(s.contains("(deny default)"));
        assert!(s.contains("a b.qcow2\"))"));
        assert!(s.contains("network-outbound"));
        let bad = Policy { rw_files: vec!["/tmp/x\")) (allow default) ((\"".into()], ..Default::default() };
        assert!(profile(&bad).is_err());
    }
}
