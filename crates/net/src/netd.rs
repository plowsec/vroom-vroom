//! Protocol between the VMM and the privileged `vroom-netd` helper.
//!
//! The VMM connects to the netd socket, sends one JSON line (a
//! [`Request`]), and gets back one JSON line (a [`Response`]) followed by
//! a datagram socket fd (SCM_RIGHTS) that carries raw Ethernet frames.
//! netd validates everything; the VMM never gets interface privileges.

use std::io::{BufRead, BufReader, Read, Write};
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

/// Ask netd for a frame socket. Returns it and the MAC to use.
pub fn attach(socket: &Path, req: &Request) -> Result<(UnixDatagram, [u8; 6]), NetdError> {
    let mut s = UnixStream::connect(socket).map_err(|e| NetdError::Connect(socket.display().to_string(), e))?;
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| NetdError::Proto(e.to_string()))?;
    let mut line = serde_json::to_string(req).map_err(|e| NetdError::Proto(e.to_string()))?;
    line.push('\n');
    s.write_all(line.as_bytes()).map_err(|e| NetdError::Proto(e.to_string()))?;
    let mut r = BufReader::new(s.try_clone().map_err(|e| NetdError::Proto(e.to_string()))?);
    let resp: Response = serde_json::from_str(&read_line(&mut r).map_err(|e| NetdError::Proto(e.to_string()))?)
        .map_err(|e| NetdError::Proto(e.to_string()))?;
    if !resp.ok {
        return Err(NetdError::Refused(resp.error));
    }
    if resp.mac[0] & 1 != 0 {
        return Err(NetdError::Proto("netd returned a multicast MAC".into()));
    }
    let fd = vroom_ipc::recv_fd(&s).map_err(|e| NetdError::Proto(e.to_string()))?;
    Ok((UnixDatagram::from(fd), resp.mac))
}

#[cfg(test)]
mod tests {
    use super::*;

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
