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

/// The source of netd's DHCP replies when the bridge's own MAC can't be
/// read (locally administered).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const FALLBACK_SERVER_MAC: [u8; 6] = [0x02, 0x76, 0x72, 0x6f, 0x6f, 0x6d];

/// Stable 16-byte id from a string (vmnet interface ids).
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
        fn bw_vmnet_start(
            mode: c_int,
            bridge_if: *const c_char,
            uuid: *const u8,
            start: *const c_char,
            end: *const c_char,
            mask: *const c_char,
            err: *mut c_char,
            errlen: usize,
        ) -> *mut c_void;
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
        /// Bridged onto `ifname`.
        pub fn bridged(ifname: &str, iface_id: [u8; 16]) -> io::Result<Vmnet> {
            let ifname = CString::new(ifname).map_err(|_| io::Error::other("bad interface name"))?;
            Self::start(Some(&ifname), iface_id, None)
        }

        /// On host-only subnet `net`: the host has its address there and
        /// vmnet serves DHCP. Every port on the same subnet shares one host
        /// bridge, so the VMs on it see each other.
        pub fn host_only(net: &crate::hostonly::Subnet, iface_id: [u8; 16]) -> io::Result<Vmnet> {
            let s = |a: std::net::Ipv4Addr| CString::new(a.to_string()).expect("no NUL in an address");
            Self::start(None, iface_id, Some([s(net.host), s(net.dhcp_end()), s(net.mask())]))
        }

        fn start(bridge: Option<&CString>, iface_id: [u8; 16], range: Option<[CString; 3]>) -> io::Result<Vmnet> {
            let mut err = [0 as c_char; 256];
            let addr = |i: usize| range.as_ref().map_or(std::ptr::null(), |r| r[i].as_ptr());
            // SAFETY: valid pointers for the call's duration.
            let h = unsafe {
                bw_vmnet_start(
                    bridge.is_some() as c_int,
                    bridge.map_or(std::ptr::null(), |c| c.as_ptr()),
                    iface_id.as_ptr(),
                    addr(0),
                    addr(1),
                    addr(2),
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

    /// An interface's MAC (`/sys/class/net/<name>/address`).
    pub fn mac_of(ifname: &str) -> Option<[u8; 6]> {
        let s = std::fs::read_to_string(format!("/sys/class/net/{ifname}/address")).ok()?;
        let mut out = [0u8; 6];
        let mut parts = s.trim().split(':');
        for o in &mut out {
            *o = u8::from_str_radix(parts.next()?, 16).ok()?;
        }
        parts.next().is_none().then_some(out)
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

#[cfg(all(test, target_os = "macos"))]
mod vmnet_tests {
    /// Needs root: `sudo cargo test -p vroom-netd -- --ignored host_only_port`.
    /// The host gets the subnet's first address on a bridge.
    #[test]
    #[ignore]
    fn host_only_port() {
        let net = crate::hostonly::Subnet::parse("192.168.79.1/24").unwrap();
        let p = super::vmnet::Vmnet::host_only(&net, super::uuid_of("test/0")).unwrap();
        let out = std::process::Command::new("/sbin/ifconfig").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("inet 192.168.79.1 "), "no host address on a bridge");
        drop(p);
    }
}
