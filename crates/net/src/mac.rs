//! A guest whose MAC isn't the one its port passes.
//!
//! The guest reads its MAC from the virtio-net device once, when its
//! driver starts, and keeps it. A port vroom-netd hands out on macOS only
//! passes frames from and to the MAC vmnet chose for it. When an adapter
//! goes from NAT to bridged while the guest runs (or resumes on another
//! mode), the two differ. [`MacXlate`] sits between them and rewrites the
//! MAC wherever the LAN needs to see the port's one: the Ethernet header,
//! ARP, the DHCP client hardware address, and IPv6 neighbor discovery's
//! link-layer address options (checksums fixed up). Everything else passes
//! as it is. Frames are guest input: every offset is checked.

use std::io;
use std::time::Duration;

use mio::Registry;

use crate::nat::NatPolicy;
use crate::NetBackend;

pub type Mac = [u8; 6];

const ETH_HLEN: usize = 14;

#[derive(Debug)]
pub struct MacXlate {
    inner: Box<dyn NetBackend>,
    /// What the guest uses.
    guest: Mac,
    /// What the port passes.
    port: Mac,
}

impl MacXlate {
    pub fn new(inner: Box<dyn NetBackend>, guest: Mac, port: Mac) -> Self {
        MacXlate { inner, guest, port }
    }

    /// `inner`, with a translation only if the two differ.
    pub fn wrap(inner: Box<dyn NetBackend>, guest: Mac, port: Mac) -> Box<dyn NetBackend> {
        if guest == port {
            inner
        } else {
            Box::new(MacXlate::new(inner, guest, port))
        }
    }
}

impl NetBackend for MacXlate {
    fn send_frame(&mut self, frame: &[u8]) {
        let mut f = frame.to_vec();
        rewrite(&mut f, self.guest, self.port, Dir::Out);
        self.inner.send_frame(&f);
    }

    fn send_frames(&mut self, frames: &[Vec<u8>]) {
        let mut v = frames.to_vec();
        for f in &mut v {
            rewrite(f, self.guest, self.port, Dir::Out);
        }
        self.inner.send_frames(&v);
    }

    fn recv_frame(&mut self) -> Option<Vec<u8>> {
        let mut f = self.inner.recv_frame()?;
        rewrite(&mut f, self.port, self.guest, Dir::In);
        Some(f)
    }

    fn register(&mut self, reg: &Registry) -> io::Result<()> {
        self.inner.register(reg)
    }

    fn deregister(&mut self, reg: &Registry) {
        self.inner.deregister(reg)
    }

    fn ready(&mut self, token: usize) {
        self.inner.ready(token)
    }

    fn poll_delay(&mut self) -> Option<Duration> {
        self.inner.poll_delay()
    }

    fn tick(&mut self) {
        self.inner.tick()
    }

    fn set_nat_policy(&mut self, p: &NatPolicy) -> bool {
        self.inner.set_nat_policy(p)
    }
}

/// A frame the guest sends, as the port must see it (fuzzing, tests).
pub fn outgoing(f: &mut [u8], guest: Mac, port: Mac) {
    rewrite(f, guest, port, Dir::Out);
}

/// A frame from the port, as the guest must see it.
pub fn incoming(f: &mut [u8], port: Mac, guest: Mac) {
    rewrite(f, port, guest, Dir::In);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    /// Guest to port.
    Out,
    /// Port to guest.
    In,
}

/// Replace `from` by `to` at `at` (6 bytes), if it is there. True if it
/// was.
fn swap_at(f: &mut [u8], at: usize, from: Mac, to: Mac) -> bool {
    match f.get_mut(at..at + 6) {
        Some(m) if m == from => {
            m.copy_from_slice(&to);
            true
        }
        _ => false,
    }
}

/// RFC 1624: the checksum at `sum` after a 16-bit aligned field changed
/// from `old` to `new` (both 6 bytes here).
fn fix_sum(f: &mut [u8], sum: usize, old: Mac, new: Mac, udp: bool) {
    let Some(b) = f.get(sum..sum + 2) else { return };
    let hc = u16::from_be_bytes([b[0], b[1]]);
    if udp && hc == 0 {
        // No checksum (IPv4 UDP may leave it out).
        return;
    }
    let mut s = (!hc) as u32;
    for i in (0..6).step_by(2) {
        s += (!u16::from_be_bytes([old[i], old[i + 1]])) as u32;
        s += u16::from_be_bytes([new[i], new[i + 1]]) as u32;
    }
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    let mut c = !(s as u16);
    if udp && c == 0 {
        c = 0xffff;
    }
    f[sum..sum + 2].copy_from_slice(&c.to_be_bytes());
}

/// Rewrite `from` to `to` in one frame: the Ethernet source (out) or
/// destination (in), and the MACs carried inside.
fn rewrite(f: &mut [u8], from: Mac, to: Mac, dir: Dir) {
    if f.len() < ETH_HLEN {
        return;
    }
    let eth = if dir == Dir::Out { 6 } else { 0 };
    swap_at(f, eth, from, to);
    let ty = u16::from_be_bytes([f[12], f[13]]);
    match ty {
        0x0806 => arp(f, from, to, dir),
        0x0800 => dhcp(f, from, to, dir),
        0x86dd => ndp(f, from, to),
        _ => {}
    }
}

/// ARP over Ethernet/IPv4: the sender's address going out, the target's
/// coming in.
fn arp(f: &mut [u8], from: Mac, to: Mac, dir: Dir) {
    let a = ETH_HLEN;
    // htype 1, ptype IPv4, hlen 6, plen 4.
    if f.get(a..a + 6) != Some(&[0, 1, 8, 0, 6, 4][..]) {
        return;
    }
    let at = if dir == Dir::Out { a + 8 } else { a + 18 };
    swap_at(f, at, from, to);
}

/// DHCP's client hardware address (BOOTP `chaddr`), in IPv4 UDP between
/// ports 68 and 67.
fn dhcp(f: &mut [u8], from: Mac, to: Mac, dir: Dir) {
    let ip = ETH_HLEN;
    let Some(&vihl) = f.get(ip) else { return };
    let ihl = (vihl & 0x0f) as usize * 4;
    if vihl >> 4 != 4 || ihl < 20 || f.get(ip + 9) != Some(&17) {
        return;
    }
    // Not a fragment past the first (no UDP header there).
    let Some(fo) = f.get(ip + 6..ip + 8).map(|b| u16::from_be_bytes([b[0], b[1]])) else { return };
    if fo & 0x1fff != 0 {
        return;
    }
    let udp = ip + ihl;
    let Some(p) = f.get(udp..udp + 4).map(|b| (u16::from_be_bytes([b[0], b[1]]), u16::from_be_bytes([b[2], b[3]]))) else { return };
    let want = if dir == Dir::Out { (68, 67) } else { (67, 68) };
    if p != want {
        return;
    }
    // op htype hlen hops xid secs flags ciaddr yiaddr siaddr giaddr: 28.
    let bootp = udp + 8;
    if f.get(bootp + 1..bootp + 3) != Some(&[1, 6][..]) {
        return;
    }
    if swap_at(f, bootp + 28, from, to) {
        fix_sum(f, udp + 6, from, to, true);
    }
}

/// IPv6 neighbor discovery: router and neighbor solicitations and
/// advertisements, redirects. Their link-layer address options (source,
/// target) name the MAC.
fn ndp(f: &mut [u8], from: Mac, to: Mac) {
    let ip = ETH_HLEN;
    // Next header ICMPv6, no extension headers (NDP has none).
    if f.get(ip).is_none_or(|v| v >> 4 != 6) || f.get(ip + 6) != Some(&58) {
        return;
    }
    let icmp = ip + 40;
    let Some(&ty) = f.get(icmp) else { return };
    let opts = match ty {
        133 => 8,
        134 => 16,
        135 | 136 => 24,
        137 => 40,
        _ => return,
    };
    let mut o = icmp + opts;
    // A handful of options at most; each says its length in 8 bytes.
    for _ in 0..16 {
        let (Some(&t), Some(&len)) = (f.get(o), f.get(o + 1)) else { break };
        if len == 0 {
            break;
        }
        if (t == 1 || t == 2) && len == 1 && swap_at(f, o + 2, from, to) {
            fix_sum(f, icmp + 2, from, to, false);
        }
        o += len as usize * 8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{build_udp, checksum};
    use std::net::Ipv4Addr;

    const G: Mac = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
    const P: Mac = [0x3e, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
    const OTHER: Mac = [0x00, 0x1b, 0x21, 1, 2, 3];

    fn udp_ok(f: &[u8]) -> bool {
        let ip = &f[14..34];
        let udp = &f[34..];
        let mut sum = 17u32 + udp.len() as u32;
        for c in ip[12..20].chunks(2) {
            sum += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
        checksum(sum, udp) == 0
    }

    fn dhcp_frame(dst: Mac, src: Mac, chaddr: Mac, ports: (u16, u16)) -> Vec<u8> {
        let mut d = vec![0u8; 240];
        d[0] = 1;
        d[1] = 1;
        d[2] = 6;
        d[28..34].copy_from_slice(&chaddr);
        d[236..240].copy_from_slice(&[99, 130, 83, 99]);
        build_udp(dst, src, (Ipv4Addr::UNSPECIFIED, ports.0), (Ipv4Addr::BROADCAST, ports.1), &d)
    }

    #[test]
    fn dhcp_goes_out_as_the_port_and_comes_back_as_the_guest() {
        let mut f = dhcp_frame([0xff; 6], G, G, (68, 67));
        assert!(udp_ok(&f));
        rewrite(&mut f, G, P, Dir::Out);
        assert_eq!(&f[6..12], &P);
        assert_eq!(&f[14 + 20 + 8 + 28..][..6], &P);
        assert!(udp_ok(&f), "checksum fixed");
        let mut r = dhcp_frame(P, OTHER, P, (67, 68));
        rewrite(&mut r, P, G, Dir::In);
        assert_eq!(&r[0..6], &G);
        assert_eq!(&r[14 + 20 + 8 + 28..][..6], &G);
        assert!(udp_ok(&r));
        // Other UDP: the payload stays as it is.
        let mut o = dhcp_frame([0xff; 6], G, G, (5353, 5353));
        rewrite(&mut o, G, P, Dir::Out);
        assert_eq!(&o[14 + 20 + 8 + 28..][..6], &G);
    }

    #[test]
    fn arp_sender_out_target_in() {
        let arp = |src: Mac, sha: Mac, tha: Mac| {
            let mut f = vec![0xff; 6];
            f.extend_from_slice(&src);
            f.extend_from_slice(&[8, 6, 0, 1, 8, 0, 6, 4, 0, 1]);
            f.extend_from_slice(&sha);
            f.extend_from_slice(&[10, 0, 0, 2]);
            f.extend_from_slice(&tha);
            f.extend_from_slice(&[10, 0, 0, 1]);
            f
        };
        let mut f = arp(G, G, [0; 6]);
        rewrite(&mut f, G, P, Dir::Out);
        assert_eq!((&f[6..12], &f[22..28]), (&P[..], &P[..]));
        let mut r = arp(OTHER, OTHER, P);
        r[0..6].copy_from_slice(&P);
        rewrite(&mut r, P, G, Dir::In);
        assert_eq!((&r[0..6], &r[22..28], &r[32..38]), (&G[..], &OTHER[..], &G[..]));
    }

    #[test]
    fn neighbor_discovery_options() {
        // Neighbor solicitation with a source link-layer address option.
        let mut f = vec![0x33, 0x33, 0xff, 0, 0, 1];
        f.extend_from_slice(&G);
        f.extend_from_slice(&[0x86, 0xdd]);
        let icmp_len = 24 + 8;
        let mut ip = vec![0x60, 0, 0, 0, 0, icmp_len as u8, 58, 255];
        ip.extend_from_slice(&[0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        ip.extend_from_slice(&[0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xff, 0, 0, 1]);
        let mut icmp = vec![135, 0, 0, 0, 0, 0, 0, 0];
        icmp.extend_from_slice(&[0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        icmp.extend_from_slice(&[1, 1]);
        icmp.extend_from_slice(&G);
        let sum = |ip: &[u8], icmp: &[u8]| {
            let mut s = 58u32 + icmp.len() as u32;
            for c in ip[8..40].chunks(2) {
                s += u16::from_be_bytes([c[0], c[1]]) as u32;
            }
            checksum(s, icmp)
        };
        let c = sum(&ip, &icmp);
        icmp[2..4].copy_from_slice(&c.to_be_bytes());
        f.extend_from_slice(&ip);
        f.extend_from_slice(&icmp);
        rewrite(&mut f, G, P, Dir::Out);
        assert_eq!(&f[6..12], &P);
        assert_eq!(&f[14 + 40 + 26..][..6], &P);
        assert_eq!(sum(&f[14..54], &f[54..]), 0, "checksum fixed");
    }

    #[test]
    fn short_and_odd_frames_pass() {
        for n in 0..80 {
            let mut f = dhcp_frame([0xff; 6], G, G, (68, 67));
            f.truncate(n);
            let before = f.clone();
            rewrite(&mut f, G, P, Dir::Out);
            if n >= 14 {
                assert_eq!(&f[6..12], &P);
            }
            assert_eq!(f.len(), before.len());
        }
        // Options of length 0 end the walk.
        let mut f = vec![0u8; 14 + 40 + 24 + 2];
        f[12..14].copy_from_slice(&[0x86, 0xdd]);
        f[14] = 0x60;
        f[14 + 6] = 58;
        f[54] = 135;
        rewrite(&mut f, G, P, Dir::Out);
    }
}
