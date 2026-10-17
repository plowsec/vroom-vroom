//! Protocol between vroom and the privileged `vroom-netd` helper.
//!
//! A client connects to the netd socket and sends one JSON line:
//!
//! - a [`Request`] (the VMM, for a network adapter): back comes one JSON
//!   line (a [`Response`]) and a datagram socket fd (SCM_RIGHTS) that
//!   carries raw Ethernet frames;
//! - a [`UsbRequest`] (the app, for a USB device macOS holds): a
//!   [`Response`], then the stream socket of a USB helper netd started
//!   as root for that one device;
//! - a [`StatusRequest`]: a [`Status`] line, what this daemon offers.
//!
//! netd validates everything; the VMM never gets interface privileges and
//! the app never gets root.

use std::io::{BufRead, Read, Write};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const DEFAULT_SOCKET: &str = "/var/run/vroom/netd.sock";
pub const VERSION: u32 = 1;
const MAX_LINE: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Attach {
    Bridged { interface: String },
    HostOnly { network: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    /// VM name + NIC index: used to derive a stable interface identity.
    pub vm: String,
    pub nic: u32,
    pub attach: Attach,
    /// Requested MAC. netd may override (vmnet assigns its own).
    pub mac: [u8; 6],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub error: String,
    /// MAC the guest must use (others are dropped by netd).
    #[serde(default)]
    pub mac: [u8; 6],
    #[serde(default)]
    pub mtu: u16,
}

/// Ask for a USB device that a macOS driver holds (taking it from the
/// driver needs root). By id, never by path or location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbRequest {
    pub version: u32,
    /// The VM it is for (for the log).
    pub vm: String,
    pub usb: UsbWanted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsbWanted {
    /// "vendor:product", 4 hex digits each.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

/// "What do you do for me?"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    pub version: u32,
    pub status: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Status {
    pub ok: bool,
    #[serde(default)]
    pub version: u32,
    /// Interfaces VMs may bridge onto (`en*` patterns as configured).
    #[serde(default)]
    pub bridge_interfaces: Vec<String>,
    /// Host-only network names.
    #[serde(default)]
    pub host_only: Vec<String>,
    /// USB devices macOS holds: served to this user.
    #[serde(default)]
    pub usb: bool,
    /// Why not, if not.
    #[serde(default)]
    pub usb_why: String,
}

/// Any first line netd takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Hello {
    Net(Request),
    Usb(UsbRequest),
    Status(StatusRequest),
}

/// A USB id as `[[usb]]` writes it: "vendor:product", 4 hex digits each.
pub fn usb_id(s: &str) -> Option<(u16, u16)> {
    let (v, p) = s.split_once(':')?;
    let hex = |x: &str| (x.len() == 4 && x.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u16::from_str_radix(x, 16).ok()).flatten();
    Some((hex(v)?, hex(p)?))
}

/// A serial number netd accepts: printable ASCII, as long as USB allows.
pub fn usb_serial_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 126 && s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

#[derive(Debug, thiserror::Error)]
pub enum NetdError {
    #[error("vroom-netd not reachable at {0} ({1}); is it installed and running?")]
    Connect(String, std::io::Error),
    #[error("netd: {0}")]
    Refused(String),
    #[error("netd protocol: {0}")]
    Proto(String),
}

/// Read one bounded line (hostile-peer safe).
pub fn read_line(r: &mut impl BufRead) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let n = r.by_ref().take(MAX_LINE as u64).read_until(b'\n', &mut buf)?;
    if n == 0 || buf.last() != Some(&b'\n') {
        return Err(std::io::Error::other("line too long or truncated"));
    }
    buf.pop();
    String::from_utf8(buf).map_err(|_| std::io::Error::other("not utf-8"))
}

fn connect(socket: &Path, timeout: Duration) -> Result<UnixStream, NetdError> {
    let s = UnixStream::connect(socket).map_err(|e| NetdError::Connect(socket.display().to_string(), e))?;
    s.set_read_timeout(Some(timeout)).map_err(|e| NetdError::Proto(e.to_string()))?;
    Ok(s)
}

/// Send one line, read one back.
fn ask<T: serde::de::DeserializeOwned>(mut s: &UnixStream, req: &impl Serialize) -> Result<T, NetdError> {
    let mut line = serde_json::to_string(req).map_err(|e| NetdError::Proto(e.to_string()))?;
    line.push('\n');
    s.write_all(line.as_bytes()).map_err(|e| NetdError::Proto(e.to_string()))?;
    // Byte by byte: an fd follows the line, in a message of its own; a
    // buffered read could take its byte along.
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match s.read(&mut b) {
            Ok(1) if b[0] == b'\n' => break,
            Ok(1) if buf.len() < MAX_LINE => buf.push(b[0]),
            Ok(1) => return Err(NetdError::Proto("line too long".into())),
            Ok(_) => return Err(NetdError::Proto("netd closed the connection".into())),
            Err(e) => return Err(NetdError::Proto(e.to_string())),
        }
    }
    serde_json::from_slice(&buf).map_err(|e| NetdError::Proto(e.to_string()))
}

/// Ask netd for a frame socket. Returns it and the MAC to use.
pub fn attach(socket: &Path, req: &Request) -> Result<(UnixDatagram, [u8; 6]), NetdError> {
    // The same adapter's last port (a VM restarted or resumed at once, an
    // adapter switched back) may still be closing: vmnet refuses a second
    // interface with its id until then.
    let mut tries = 0;
    loop {
        match attach_once(socket, req) {
            Err(NetdError::Refused(m)) if m.contains("start failed") && tries < 6 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(800));
            }
            r => return r,
        }
    }
}

fn attach_once(socket: &Path, req: &Request) -> Result<(UnixDatagram, [u8; 6]), NetdError> {
    let s = connect(socket, Duration::from_secs(10))?;
    let resp: Response = ask(&s, req)?;
    if !resp.ok {
        return Err(NetdError::Refused(resp.error));
    }
    if resp.mac[0] & 1 != 0 {
        return Err(NetdError::Proto("netd returned a multicast MAC".into()));
    }
    let fd = vroom_ipc::recv_fd(&s).map_err(|e| NetdError::Proto(e.to_string()))?;
    Ok((UnixDatagram::from(fd), resp.mac))
}

/// What the netd at `socket` offers (and whether it is there at all).
pub fn status(socket: &Path) -> Result<Status, NetdError> {
    let s = connect(socket, Duration::from_secs(3))?;
    status_on(&s)
}

/// `status` on a connection made already.
pub fn status_on(s: &UnixStream) -> Result<Status, NetdError> {
    let st: Status = ask(s, &StatusRequest { version: VERSION, status: true })?;
    if !st.ok {
        return Err(NetdError::Refused("status".into()));
    }
    Ok(st)
}

/// Have netd start a USB helper for a device macOS holds. Returns the
/// helper's socket, to hand to the VMM.
pub fn usb(socket: &Path, req: &UsbRequest) -> Result<UnixStream, NetdError> {
    let s = connect(socket, Duration::from_secs(15))?;
    usb_on(&s, req)
}

/// `usb` on a connection made already.
pub fn usb_on(s: &UnixStream, req: &UsbRequest) -> Result<UnixStream, NetdError> {
    let resp: Response = ask(s, req)?;
    if !resp.ok {
        return Err(NetdError::Refused(resp.error));
    }
    let fd = vroom_ipc::recv_fd(s).map_err(|e| NetdError::Proto(e.to_string()))?;
    Ok(UnixStream::from(fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn first_lines_tell_apart() {
        let net = r#"{"version":1,"vm":"a","nic":0,"attach":{"mode":"host-only","network":"lab"},"mac":[2,0,0,0,0,1]}"#;
        assert!(matches!(serde_json::from_str::<Hello>(net).unwrap(), Hello::Net(_)));
        let usb = r#"{"version":1,"vm":"a","usb":{"id":"1209:000a","serial":"X"}}"#;
        assert!(matches!(serde_json::from_str::<Hello>(usb).unwrap(), Hello::Usb(_)));
        assert!(matches!(serde_json::from_str::<Hello>(r#"{"version":1,"status":true}"#).unwrap(), Hello::Status(_)));
        // Mixed or extra fields match nothing.
        assert!(serde_json::from_str::<Hello>(r#"{"version":1,"status":true,"usb":{"id":"1209:000a"}}"#).is_err());
        assert!(serde_json::from_str::<Hello>(r#"{"version":1,"vm":"a","usb":{"id":"1209:000a","bus":"1-2"}}"#).is_err());
    }

    #[test]
    fn usb_ids_and_serials() {
        assert_eq!(usb_id("1209:000a"), Some((0x1209, 0x000a)));
        for bad in ["1209:0a", "1209:000g", "+209:000a", "1209000a", "1209:000a:1", ""] {
            assert_eq!(usb_id(bad), None, "{bad}");
        }
        assert!(usb_serial_ok("A1 B2-c"));
        assert!(!usb_serial_ok(""));
        assert!(!usb_serial_ok("a\nb"));
        assert!(!usb_serial_ok(&"a".repeat(127)));
        assert!(!usb_serial_ok("é"));
    }

    #[test]
    fn request_roundtrip_and_strictness() {
        let r = Request {
            version: VERSION,
            vm: "win".into(),
            nic: 0,
            attach: Attach::Bridged { interface: "en0".into() },
            mac: [2, 0, 0, 0, 0, 1],
        };
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains("\"mode\":\"bridged\""));
        assert_eq!(serde_json::from_str::<Request>(&j).unwrap(), r);
        let extra = j.replace("\"nic\":0", "\"nic\":0,\"evil\":1");
        assert!(serde_json::from_str::<Request>(&extra).is_err());
    }

    #[test]
    fn bounded_lines() {
        let mut ok = BufReader::new(&b"hello\n"[..]);
        assert_eq!(read_line(&mut ok).unwrap(), "hello");
        let long = vec![b'a'; MAX_LINE + 10];
        assert!(read_line(&mut BufReader::new(&long[..])).is_err());
        assert!(read_line(&mut BufReader::new(&b"no newline"[..])).is_err());
    }
}
