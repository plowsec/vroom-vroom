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
        s += "(allow network-outbound (remote ip) (remote unix-socket))\n";
        s += "(allow system-socket)\n";
        // DNS relay only needs the resolver address it already parsed.
    }
    for port in &p.listen_ports {
        s += &format!("(allow network-inbound (local tcp \"*:{port}\"))\n");
    }
    if p.clipboard {
        s += "(allow mach-lookup (global-name \"com.apple.pasteboard.1\") (global-name \"com.apple.distributed_notifications@Uv3\"))\n";
        s += "(allow ipc-posix-shm-read-data ipc-posix-shm-write-data)\n";
    }
    Ok(s)
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
