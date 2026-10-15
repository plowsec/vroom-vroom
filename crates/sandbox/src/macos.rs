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

/// Build the profile. Public for tests / `bulwark sandbox-profile`.
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
        s += "(allow ipc-posix-shm-read-data ipc-posix-shm-write-data)\n";
    }
    if p.gpu {
        s += GPU_RULES;
        if let Some(cache) = user_cache_dir() {
            // Metal's shader cache.
            s += &format!("(allow file-read* file-write* (subpath {}))\n", q(&cache.join("com.apple.metal"))?);
        }
    }
    Ok(s)
}

/// Profile for a helper we exec (swtpm): may read its own install and
/// system libraries, read/write `rw_dir`, nothing else (no network, no
/// home directory, no further exec).
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
(allow ipc-posix-shm-read-data ipc-posix-shm-write-data)
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
    Some(std::path::PathBuf::from(s))
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
