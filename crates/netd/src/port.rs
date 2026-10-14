//! Host-side network ports: vmnet (macOS) and tap (Linux).

use std::io;
use std::time::Duration;

/// One VM NIC's attachment to a real network.
pub trait Port: Send + Sync {
    /// MAC the guest must use (vmnet assigns one; tap uses the requested).
    fn mac(&self) -> [u8; 6];
    fn mtu(&self) -> u16;
    fn send(&self, frame: &[u8]) -> io::Result<()>;
    /// Ok(0) on timeout.
    fn recv(&self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;
}

/// Stable 16-byte id from a string (vmnet interface / network ids).
pub fn uuid_of(s: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (i, chunk) in out.chunks_mut(8).enumerate() {
        for b in s.bytes().chain([i as u8]) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        chunk.copy_from_slice(&h.to_le_bytes());
    }
    // RFC 4122 version 4 / variant bits, so vmnet accepts it as a UUID.
    out[6] = (out[6] & 0x0f) | 0x40;
    out[8] = (out[8] & 0x3f) | 0x80;
    out
}

#[cfg(target_os = "macos")]
pub mod vmnet {
    use super::*;
    use std::ffi::{c_char, c_int, c_long, c_void, CString};

    extern "C" {
        fn bw_vmnet_start(mode: c_int, bridge_if: *const c_char, uuid: *const u8, netid: *const u8, err: *mut c_char, errlen: usize) -> *mut c_void;
        fn bw_vmnet_info(v: *mut c_void, mac: *mut u8, max_packet: *mut u64);
        fn bw_vmnet_read(v: *mut c_void, buf: *mut u8, len: usize, timeout_ms: c_int) -> c_long;
        fn bw_vmnet_write(v: *mut c_void, buf: *const u8, len: usize) -> c_int;
        fn bw_vmnet_stop(v: *mut c_void);
    }

    pub struct Vmnet {
        h: *mut c_void,
        mac: [u8; 6],
        max_packet: u64,
    }

    // SAFETY: vmnet interface refs are usable from any thread; reads and
    // writes go through vmnet's own locking.
    unsafe impl Send for Vmnet {}
    // SAFETY: see above.
    unsafe impl Sync for Vmnet {}

    impl Vmnet {
        /// `bridge`: Some(ifname) for bridged, None for host-only.
        pub fn start(bridge: Option<&str>, iface_id: [u8; 16], network: Option<[u8; 16]>) -> io::Result<Vmnet> {
            let ifname = bridge.map(CString::new).transpose().map_err(|_| io::Error::other("bad interface name"))?;
            let mut err = [0 as c_char; 256];
            let net_ptr = network.as_ref().map(|n| n.as_ptr()).unwrap_or(std::ptr::null());
            // SAFETY: valid pointers for the call's duration.
            let h = unsafe {
                bw_vmnet_start(
                    bridge.is_some() as c_int,
                    ifname.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null()),
                    iface_id.as_ptr(),
                    net_ptr,
                    err.as_mut_ptr(),
                    err.len(),
                )
            };
            if h.is_null() {
                // SAFETY: shim wrote a NUL-terminated message.
                let msg = unsafe { std::ffi::CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned();
                return Err(io::Error::other(msg));
            }
            let mut mac = [0u8; 6];
            let mut max_packet = 0u64;
            // SAFETY: h is live; out pointers valid.
            unsafe { bw_vmnet_info(h, mac.as_mut_ptr(), &mut max_packet) };
            Ok(Vmnet { h, mac, max_packet })
        }
    }

    impl Port for Vmnet {
        fn mac(&self) -> [u8; 6] {
            self.mac
        }
        fn mtu(&self) -> u16 {
            self.max_packet.saturating_sub(14).min(9000) as u16
        }
        fn send(&self, f: &[u8]) -> io::Result<()> {
            // SAFETY: live handle, valid slice.
            match unsafe { bw_vmnet_write(self.h, f.as_ptr(), f.len()) } {
                0 => Ok(()),
                _ => Err(io::Error::other("vmnet write failed")),
            }
        }
        fn recv(&self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
            // SAFETY: live handle, valid buffer.
            let n = unsafe { bw_vmnet_read(self.h, buf.as_mut_ptr(), buf.len(), timeout.as_millis() as c_int) };
            if n < 0 {
                Err(io::Error::other("vmnet read failed"))
            } else {
                Ok(n as usize)
            }
        }
    }

    impl Drop for Vmnet {
        fn drop(&mut self) {
            // SAFETY: handle from bw_vmnet_start, stopped exactly once.
            unsafe { bw_vmnet_stop(self.h) }
        }
    }
}

#[cfg(target_os = "linux")]
pub mod tap {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileExt;
    use std::process::Command;

    const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
    const IFF_TAP: libc::c_short = 0x0002;
    const IFF_NO_PI: libc::c_short = 0x1000;

    #[repr(C)]
    struct IfReq {
        name: [u8; 16],
        flags: libc::c_short,
        _pad: [u8; 22],
    }

    pub struct Tap {
        file: File,
        name: String,
        mac: [u8; 6],
    }

    fn ip(args: &[&str]) -> io::Result<()> {
        let st = Command::new("ip").args(args).status()?;
        if st.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("ip {}: failed", args.join(" "))))
        }
    }

    /// Ensure a host-only bridge exists with the gateway address.
    pub fn ensure_bridge(bridge: &str, gateway_cidr: &str) -> io::Result<()> {
        if !std::path::Path::new(&format!("/sys/class/net/{bridge}")).exists() {
            ip(&["link", "add", "name", bridge, "type", "bridge"])?;
            ip(&["addr", "add", gateway_cidr, "dev", bridge])?;
        }
        ip(&["link", "set", "dev", bridge, "up"])
    }

    impl Tap {
        /// New tap enslaved to `bridge`.
        pub fn open(bridge: &str, mac: [u8; 6]) -> io::Result<Tap> {
            let file = OpenOptions::new().read(true).write(true).open("/dev/net/tun")?;
            let mut req = IfReq { name: [0; 16], flags: IFF_TAP | IFF_NO_PI, _pad: [0; 22] };
            req.name[..7].copy_from_slice(b"bwtap%d");
            // SAFETY: valid fd and ifreq-sized struct.
            if unsafe { libc::ioctl(file.as_raw_fd(), TUNSETIFF as _, &mut req) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let end = req.name.iter().position(|&b| b == 0).unwrap_or(16);
            let name = String::from_utf8_lossy(&req.name[..end]).into_owned();
            ip(&["link", "set", "dev", &name, "master", bridge, "up"])?;
            Ok(Tap { file, name, mac })
        }
    }

    impl Port for Tap {
        fn mac(&self) -> [u8; 6] {
            self.mac
        }
        fn mtu(&self) -> u16 {
            1500
        }
        fn send(&self, f: &[u8]) -> io::Result<()> {
            self.file.write_at(f, 0).map(|_| ())
        }
        fn recv(&self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
            let mut pfd = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            let r = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
            if r <= 0 {
                return Ok(0);
            }
            self.file.read_at(buf, 0)
        }
    }

    impl Drop for Tap {
        fn drop(&mut self) {
            let _ = ip(&["link", "del", &self.name]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_stable_and_distinct() {
        assert_eq!(uuid_of("vm/0"), uuid_of("vm/0"));
        assert_ne!(uuid_of("vm/0"), uuid_of("vm/1"));
        assert_eq!(uuid_of("x")[6] >> 4, 4);
    }
}
