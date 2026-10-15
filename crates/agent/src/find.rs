//! Which virtio-serial port is ours, when `--port` isn't given.
//!
//! The VM config may rename the port (`[identity].agent_port`). Use the
//! default name if it's there, else the only port there is. Several
//! ports and none is the default: don't guess, say which ones exist and
//! wait for `--port`.

use std::io;
use std::sync::Mutex;

use vroom_proto::AGENT_PORT;

/// The port to use among `names`: `default` if present, else the only
/// one. None: no port, or several and we can't tell.
pub fn pick<'a>(names: &'a [String], default: &str) -> Option<&'a str> {
    if let Some(n) = names.iter().find(|n| *n == default) {
        return Some(n);
    }
    match names {
        [one] => Some(one),
        _ => None,
    }
}

/// Windows: does the DOS device `name` -> `target` look like a vioserial
/// port? Listing DOS devices gives drive letters, COM/NUL/PhysicalDrive,
/// volume and device-interface links too, hundreds of them, so filter:
/// - vioserial doesn't name its port devices, so Windows gives them
///   `\Device\` + 8 hex digits. Drives, COM, NUL etc. point at named
///   devices (`\Device\HarddiskVolume3`, `\Device\Serial0`).
/// - Device-interface links point at such devices too, but their names
///   have `#`, `{` and `}`. A port name from the config can't
///   (`[A-Za-z0-9._-]`, see `valid_name` in the config crate).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn is_vioserial(name: &str, target: &str) -> bool {
    let name_ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    let auto = target
        .strip_prefix(r"\Device\")
        .is_some_and(|id| id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit()));
    name_ok && auto
}

/// Path of the agent port, found as above.
pub fn port_path() -> io::Result<String> {
    if os::exists(AGENT_PORT) {
        return Ok(os::path(AGENT_PORT));
    }
    let mut names = os::list()?;
    names.sort();
    if let Some(n) = pick(&names, AGENT_PORT) {
        log::info!("no port {AGENT_PORT}, using {n}");
        return Ok(os::path(n));
    }
    if names.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no virtio-serial port"));
    }
    // Say it once per change, not on every retry.
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let list = names.join(", ");
    if let Ok(mut seen) = SEEN.lock() {
        if *seen != names {
            log::warn!("no port {AGENT_PORT} and several others ({list}): start the agent with --port");
            *seen = names.clone();
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, format!("several ports ({list}), none is {AGENT_PORT}")))
}

#[cfg(not(windows))]
mod os {
    use std::io;

    const DIR: &str = "/dev/virtio-ports";

    pub fn path(name: &str) -> String {
        format!("{DIR}/{name}")
    }

    pub fn exists(name: &str) -> bool {
        std::path::Path::new(&path(name)).exists()
    }

    /// Named ports: udev links them in here.
    pub fn list() -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for e in std::fs::read_dir(DIR)? {
            if let Ok(n) = e?.file_name().into_string() {
                names.push(n);
            }
        }
        Ok(names)
    }
}

#[cfg(windows)]
mod os {
    use std::io;

    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;

    /// vioserial links each named port as `\DosDevices\<name>`, in the
    /// global namespace.
    pub fn path(name: &str) -> String {
        format!(r"\\.\Global\{name}")
    }

    pub fn exists(name: &str) -> bool {
        query(Some(name)).is_ok()
    }

    /// DOS device names that look like vioserial ports.
    pub fn list() -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for n in query(None)? {
            let Ok(targets) = query(Some(&n)) else { continue };
            if targets.first().is_some_and(|t| super::is_vioserial(&n, t)) {
                names.push(n);
            }
        }
        Ok(names)
    }

    /// QueryDosDeviceW: targets of `name`, or with None, every DOS device
    /// name. The answer is a NUL-separated list.
    fn query(name: Option<&str>) -> io::Result<Vec<String>> {
        let wide: Option<Vec<u16>> = name.map(|n| n.encode_utf16().chain([0]).collect());
        let p = wide.as_ref().map_or(std::ptr::null(), |w| w.as_ptr());
        // The full list is a few thousand chars; one name's targets are short.
        let mut buf = vec![0u16; if name.is_some() { 1 << 10 } else { 32 << 10 }];
        loop {
            // SAFETY: p is null or a NUL-terminated name; buf is writable
            // for its length (at most 16M chars, fits u32).
            let n = unsafe { QueryDosDeviceW(p, buf.as_mut_ptr(), buf.len() as u32) };
            if n != 0 {
                let out = &buf[..(n as usize).min(buf.len())];
                return Ok(out.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect());
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) || buf.len() >= 16 << 20 {
                return Err(e);
            }
            buf.resize(buf.len() * 2, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_wins() {
        let n = names(&["com.example.0", "org.vroom.agent.0"]);
        assert_eq!(pick(&n, "org.vroom.agent.0"), Some("org.vroom.agent.0"));
    }

    #[test]
    fn only_port_is_ours() {
        let n = names(&["com.example.0"]);
        assert_eq!(pick(&n, "org.vroom.agent.0"), Some("com.example.0"));
    }

    #[test]
    fn no_guessing() {
        assert_eq!(pick(&[], "org.vroom.agent.0"), None);
        let n = names(&["a", "b"]);
        assert_eq!(pick(&n, "org.vroom.agent.0"), None);
    }

    #[test]
    fn vioserial_looks() {
        assert!(is_vioserial("org.vroom.agent.0", r"\Device\0000002d"));
        assert!(is_vioserial("serial-x", r"\Device\0000ABCD"));
        // Standard devices point at named ones.
        assert!(!is_vioserial("C:", r"\Device\HarddiskVolume3"));
        assert!(!is_vioserial("COM1", r"\Device\Serial0"));
        assert!(!is_vioserial("NUL", r"\Device\Null"));
        assert!(!is_vioserial("PhysicalDrive0", r"\Device\Harddisk0\DR0"));
        // Interface links point at auto-named devices but have #{}.
        assert!(!is_vioserial(r"USB#VID_1209&PID_0001#5&1#{a5dcbf10-6530-11d2-901f-00c04fb951ed}", r"\Device\00000031"));
        assert!(!is_vioserial("x", r"\Device\000000"));
        assert!(!is_vioserial("", r"\Device\0000002d"));
    }
}
