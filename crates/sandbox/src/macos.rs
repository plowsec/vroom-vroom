//! Seatbelt (sandbox_init) for macOS.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::Path;

use crate::{Error, Policy, Result};

extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> i32;
    fn sandbox_free_error(errorbuf: *mut c_char);
    // Sandbox extensions (libsystem_sandbox; what Safari and WebKit hand
    // their sandboxed processes a file with). Not in Apple's public
    // headers, stable for years.
    fn sandbox_extension_issue_file(class: *const c_char, path: *const c_char, flags: u32) -> *mut c_char;
    fn sandbox_extension_consume(token: *const c_char) -> i64;
    fn sandbox_extension_release(handle: i64) -> i32;
}

/// Extension classes for a USB disk's volume shared while the VM runs:
/// the profile lets the VMM into a path only with a token of these.
const EXT_READ: &str = "org.vroom.usb-files.read";
const EXT_WRITE: &str = "org.vroom.usb-files.read-write";

/// A token that lets a sandboxed VMM into `dir`'s subtree (and only
/// with `write`, change it). Made by the app, which may read it.
pub fn issue_extension(dir: &Path, write: bool) -> Result<String> {
    let real = dir.canonicalize().map_err(|e| Error::Apply(format!("{}: {e}", dir.display())))?;
    let path = CString::new(real.as_os_str().as_encoded_bytes()).map_err(|_| Error::Apply("NUL in path".into()))?;
    let class = CString::new(if write { EXT_WRITE } else { EXT_READ }).map_err(|_| Error::Apply("class".into()))?;
    // SAFETY: valid C strings; the result is malloc'ed (or NULL) and ours.
    let t = unsafe { sandbox_extension_issue_file(class.as_ptr(), path.as_ptr(), 0) };
    if t.is_null() {
        return Err(Error::Apply(format!("no sandbox extension for {}: {}", real.display(), std::io::Error::last_os_error())));
    }
    // SAFETY: a NUL-terminated string from the call above, freed once.
    let s = unsafe { CStr::from_ptr(t) }.to_string_lossy().into_owned();
    // SAFETY: allocated with malloc by libsandbox.
    unsafe { libc::free(t.cast()) };
    Ok(s)
}

/// Tokens taken so far, by our own handle. An extension consumed before
/// `sandbox_init` doesn't carry into the sandbox it makes (the VMM
/// handles the viewer's messages, a USB disk of the VM file's among
/// them, while it is still setting up): those wait here and are
/// consumed right after the sandbox is in place.
struct Exts {
    applied: bool,
    next: i64,
    held: Vec<(i64, Ext)>,
}

enum Ext {
    Waiting(CString),
    /// The kernel's handle.
    Live(i64),
}

static EXTS: std::sync::Mutex<Exts> = std::sync::Mutex::new(Exts { applied: false, next: 1, held: Vec::new() });

fn exts() -> std::sync::MutexGuard<'static, Exts> {
    EXTS.lock().unwrap_or_else(|e| e.into_inner())
}

fn consume_now(t: &CStr) -> Result<i64> {
    // SAFETY: a valid C string; the kernel checks the token.
    let h = unsafe { sandbox_extension_consume(t.as_ptr()) };
    if h < 0 {
        return Err(Error::Apply(format!("sandbox extension refused: {}", std::io::Error::last_os_error())));
    }
    Ok(h)
}

/// Take the access a token gives (in the VMM). Returns a handle for
/// `release_extension`. Before the sandbox is applied the token is kept
/// and consumed by `apply` (unsandboxed, nothing is in the way yet).
pub fn consume_extension(token: &str) -> Result<i64> {
    let t = CString::new(token).map_err(|_| Error::Apply("NUL in token".into()))?;
    let mut x = exts();
    let ext = if x.applied { Ext::Live(consume_now(&t)?) } else { Ext::Waiting(t) };
    let id = x.next;
    x.next += 1;
    x.held.push((id, ext));
    Ok(id)
}

/// Give that access back.
pub fn release_extension(handle: i64) {
    let mut x = exts();
    let Some(i) = x.held.iter().position(|(id, _)| *id == handle) else { return };
    if let (_, Ext::Live(h)) = x.held.swap_remove(i) {
        // SAFETY: a handle sandbox_extension_consume returned.
        unsafe { sandbox_extension_release(h) };
    }
}

/// Right after `sandbox_init`: the tokens taken before it.
fn consume_waiting(x: &mut Exts) {
    x.applied = true;
    for (_, e) in &mut x.held {
        if let Ext::Waiting(t) = e {
            match consume_now(t) {
                Ok(h) => *e = Ext::Live(h),
                // The share then reads as denied, as without a token.
                Err(err) => log::warn!("usb files: {err}"),
            }
        }
    }
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
    if p.usb_storage {
        // USB disks the app hands over while the VM runs. A raw disk
        // comes as an fd: only its size and cache ioctls are needed, and
        // nothing here can open a disk by name. A volume's files come
        // with a token the app issued for that one directory.
        s += "(allow file-ioctl (regex #\"^/dev/rdisk[0-9]+$\"))\n";
        s += &format!("(allow file-read* (extension \"{EXT_READ}\"))\n");
        s += &format!("(allow file-read* file-write* (extension \"{EXT_WRITE}\"))\n");
        s += &format!(
            "(deny file-write-setugid file-write-owner file-write-acl file-write-flags file-write-xattr file-write-finderinfo (extension \"{EXT_WRITE}\"))\n"
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
    // Held across sandbox_init: a token taken meanwhile waits for it.
    let mut x = exts();
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
    consume_waiting(&mut x);
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

    #[test]
    fn usb_storage_rules_and_tokens() {
        assert!(!profile(&Policy::default()).unwrap().contains("rdisk"));
        let s = profile(&Policy { usb_storage: true, ..Default::default() }).unwrap();
        assert!(s.contains("(allow file-ioctl (regex #\"^/dev/rdisk[0-9]+$\"))"));
        assert!(s.contains(&format!("(allow file-read* (extension \"{EXT_READ}\"))")));
        let d = tempfile::tempdir().unwrap();
        let t = issue_extension(d.path(), false).unwrap();
        assert!(t.len() > 16, "{t}");
        assert_ne!(t, issue_extension(d.path(), true).unwrap());
        assert!(issue_extension(&d.path().join("nothing"), false).is_err());
    }

    /// A USB disk's files handed over while the VMM is still setting up
    /// (a VM file's disk): the token must work once the sandbox is on.
    /// Run in a child, as a sandbox is for the whole process.
    #[test]
    fn a_token_taken_before_the_sandbox_works_in_it() {
        const ME: &str = "macos::tests::a_token_taken_before_the_sandbox_works_in_it";
        let Some(dir) = std::env::var_os("VROOM_SB_EXT_CHILD") else {
            let d = tempfile::tempdir().unwrap();
            let st = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", ME, "--nocapture", "--test-threads=1"])
                .env("VROOM_SB_EXT_CHILD", d.path())
                .status()
                .unwrap();
            assert!(st.success());
            assert_eq!(std::fs::read_to_string(d.path().join("rw/in.txt")).unwrap(), "x");
            assert!(!d.path().join("ro/out.txt").exists());
            return;
        };
        let (rw, ro) = (Path::new(&dir).join("rw"), Path::new(&dir).join("ro"));
        std::fs::create_dir(&rw).unwrap();
        std::fs::create_dir(&ro).unwrap();
        std::fs::write(ro.join("f"), "ro").unwrap();
        let early = consume_extension(&issue_extension(&rw, true).unwrap()).unwrap();
        let late = issue_extension(&ro, false).unwrap();
        apply(&Policy { usb_storage: true, ..Default::default() }).unwrap();
        std::fs::write(rw.join("in.txt"), "x").expect("write with a token taken before the sandbox");
        assert!(std::fs::read(ro.join("f")).is_err(), "read without a token");
        let late = consume_extension(&late).unwrap();
        assert_eq!(std::fs::read(ro.join("f")).unwrap(), b"ro");
        assert!(std::fs::write(ro.join("out.txt"), "y").is_err(), "write with a read token");
        release_extension(early);
        release_extension(late);
        assert!(std::fs::read(ro.join("f")).is_err(), "read after the release");
    }
}
