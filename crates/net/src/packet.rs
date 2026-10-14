//! Minimal, bounds-checked Ethernet/IPv4/UDP/TCP parsing and building.
//! Everything here reads guest-controlled bytes: no indexing without a
//! length check, no panics.

use std::net::Ipv4Addr;

pub const ETH_HLEN: usize = 14;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

pub const TCP_FIN: u8 = 0x01;
pub const TCP_SYN: u8 = 0x02;
pub const TCP_RST: u8 = 0x04;
pub const TCP_ACK: u8 = 0x10;

pub type Mac = [u8; 6];

fn be16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?]))
}

fn be32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?]))
}

fn ip_at(b: &[u8], o: usize) -> Option<Ipv4Addr> {
    Some(Ipv4Addr::new(*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eth {
    pub dst: Mac,
    pub src: Mac,
    pub ethertype: u16,
}

pub fn parse_eth(f: &[u8]) -> Option<Eth> {
    if f.len() < ETH_HLEN {
        return None;
    }
    let mut dst = [0u8; 6];
    let mut src = [0u8; 6];
    dst.copy_from_slice(&f[0..6]);
    src.copy_from_slice(&f[6..12]);
    Some(Eth { dst, src, ethertype: be16(f, 12)? })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arp {
    pub op: u16,
    pub sender_mac: Mac,
    pub sender_ip: Ipv4Addr,
    pub target_ip: Ipv4Addr,
}

pub fn parse_arp(f: &[u8]) -> Option<Arp> {
    let a = f.get(ETH_HLEN..)?;
    // Ethernet + IPv4 only.
    if a.len() < 28 || be16(a, 0)? != 1 || be16(a, 2)? != 0x0800 || a[4] != 6 || a[5] != 4 {
        return None;
    }
    let mut sender_mac = [0u8; 6];
    sender_mac.copy_from_slice(&a[8..14]);
    Some(Arp { op: be16(a, 6)?, sender_mac, sender_ip: ip_at(a, 14)?, target_ip: ip_at(a, 24)? })
}

/// IPv4 header view. `payload` is bounded by total_length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4<'a> {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub proto: u8,
    pub ttl: u8,
    pub fragmented: bool,
    pub payload: &'a [u8],
}

pub fn parse_ipv4(f: &[u8]) -> Option<Ipv4<'_>> {
    let p = f.get(ETH_HLEN..)?;
    if p.len() < 20 || p[0] >> 4 != 4 {
        return None;
    }
    let ihl = (p[0] & 0xf) as usize * 4;
    let total = be16(p, 2)? as usize;
    if ihl < 20 || total < ihl || total > p.len() || checksum(0, &p[..ihl]) != 0 {
        return None;
    }
    let frag = be16(p, 6)?;
    Some(Ipv4 {
        src: ip_at(p, 12)?,
        dst: ip_at(p, 16)?,
        proto: p[9],
        ttl: p[8],
        // MF set or non-zero offset.
        fragmented: frag & 0x2000 != 0 || frag & 0x1fff != 0,
        payload: &p[ihl..total],
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Udp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
}

pub fn parse_udp(p: &[u8]) -> Option<Udp<'_>> {
    let len = be16(p, 4)? as usize;
    if len < 8 || len > p.len() {
        return None;
    }
    Some(Udp { src_port: be16(p, 0)?, dst_port: be16(p, 2)?, payload: &p[8..len] })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tcp {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub payload_len: usize,
}

pub fn parse_tcp(p: &[u8]) -> Option<Tcp> {
    if p.len() < 20 {
        return None;
    }
    let off = (p[12] >> 4) as usize * 4;
    if off < 20 || off > p.len() {
        return None;
    }
    Some(Tcp {
        src_port: be16(p, 0)?,
        dst_port: be16(p, 2)?,
        seq: be32(p, 4)?,
        ack: be32(p, 8)?,
        flags: p[13],
        payload_len: p.len() - off,
    })
}

/// RFC 1071 checksum over `data`, starting from `init`.
pub fn checksum(init: u32, data: &[u8]) -> u16 {
    let mut sum = init;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [b] = chunks.remainder() {
        sum += (*b as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn pseudo_sum(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, len: usize) -> u32 {
    let s = src.octets();
    let d = dst.octets();
    u16::from_be_bytes([s[0], s[1]]) as u32
        + u16::from_be_bytes([s[2], s[3]]) as u32
        + u16::from_be_bytes([d[0], d[1]]) as u32
        + u16::from_be_bytes([d[2], d[3]]) as u32
        + proto as u32
        + len as u32
}

fn eth_ipv4_header(out: &mut Vec<u8>, dst: Mac, src: Mac, sip: Ipv4Addr, dip: Ipv4Addr, proto: u8, l4_len: usize) {
    out.extend_from_slice(&dst);
    out.extend_from_slice(&src);
    out.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    let total = (20 + l4_len) as u16;
    let mut ip = [0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&total.to_be_bytes());
    ip[6] = 0x40; // DF
    ip[8] = 64;
    ip[9] = proto;
    ip[12..16].copy_from_slice(&sip.octets());
    ip[16..20].copy_from_slice(&dip.octets());
    let c = checksum(0, &ip);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    out.extend_from_slice(&ip);
}

/// Build an ICMP packet, calculating its checksum (including odd payloads).
pub fn build_icmp(dst_mac: Mac, src_mac: Mac, src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
    if payload.len() < 8 || payload.len() > 65515 {
        return Vec::new();
    }
    let mut f = Vec::with_capacity(ETH_HLEN + 20 + payload.len());
    eth_ipv4_header(&mut f, dst_mac, src_mac, src, dst, PROTO_ICMP, payload.len());
    let mut p = payload.to_vec();
    p[2..4].fill(0);
    let c = checksum(0, &p);
    p[2..4].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&p);
    f
}

/// Quote the original IPv4 header and eight payload bytes in an ICMP error.
/// Never answer errors, noninitial fragments, or broadcast/multicast traffic.
pub fn icmp_error(frame: &[u8], gateway_mac: Mac, gateway: Ipv4Addr, kind: u8, code: u8) -> Option<Vec<u8>> {
    let eth = parse_eth(frame)?;
    let ip = parse_ipv4(frame)?;
    let p = &frame[ETH_HLEN..];
    if eth.dst[0] & 1 != 0
        || ip.src.is_unspecified()
        || ip.src.is_multicast()
        || ip.src.is_broadcast()
        || ip.dst.is_multicast()
        || ip.dst.is_broadcast()
        || be16(p, 6)? & 0x1fff != 0
        || (ip.proto == PROTO_ICMP && !matches!(ip.payload.first(), Some(8)))
    {
        return None;
    }
    let ihl = (p[0] & 15) as usize * 4;
    let mut payload = vec![kind, code, 0, 0, 0, 0, 0, 0];
    payload.extend_from_slice(&p[..ihl + ip.payload.len().min(8)]);
    Some(build_icmp(eth.src, gateway_mac, gateway, ip.src, &payload))
}

/// Fragment a locally generated IPv4 packet into Ethernet frames at `mtu`.
/// Local builders emit no IP options; guest packets are never passed here.
pub fn fragment_ipv4(frame: Vec<u8>, mtu: usize, id: u16) -> Vec<Vec<u8>> {
    if frame.len() <= ETH_HLEN + mtu {
        return vec![frame];
    }
    let Some(ip) = parse_ipv4(&frame) else { return Vec::new() };
    if frame[ETH_HLEN] != 0x45 || mtu < 28 || ip.fragmented {
        return Vec::new();
    }
    let chunk = (mtu - 20) / 8 * 8;
    let mut out = Vec::new();
    for (i, part) in ip.payload.chunks(chunk).enumerate() {
        let mut f = frame[..ETH_HLEN + 20].to_vec();
        let h = &mut f[ETH_HLEN..];
        h[2..4].copy_from_slice(&((20 + part.len()) as u16).to_be_bytes());
        h[4..6].copy_from_slice(&id.to_be_bytes());
        let flags = (i * chunk / 8) as u16 | if (i + 1) * chunk < ip.payload.len() { 0x2000 } else { 0 };
        h[6..8].copy_from_slice(&flags.to_be_bytes());
        h[10..12].fill(0);
        let c = checksum(0, h);
        h[10..12].copy_from_slice(&c.to_be_bytes());
        f.extend_from_slice(part);
        out.push(f);
    }
    out
}

/// Build Ethernet + IPv4 + UDP around `payload`.
pub fn build_udp(dst_mac: Mac, src_mac: Mac, src: (Ipv4Addr, u16), dst: (Ipv4Addr, u16), payload: &[u8]) -> Vec<u8> {
    let ulen = 8 + payload.len();
    let mut f = Vec::with_capacity(ETH_HLEN + 20 + ulen);
    eth_ipv4_header(&mut f, dst_mac, src_mac, src.0, dst.0, PROTO_UDP, ulen);
    let mut u = Vec::with_capacity(ulen);
    u.extend_from_slice(&src.1.to_be_bytes());
    u.extend_from_slice(&dst.1.to_be_bytes());
    u.extend_from_slice(&(ulen as u16).to_be_bytes());
    u.extend_from_slice(&[0, 0]);
    u.extend_from_slice(payload);
    let mut c = checksum(pseudo_sum(src.0, dst.0, PROTO_UDP, ulen), &u);
    if c == 0 {
        c = 0xffff;
    }
    u[6..8].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&u);
    f
}

/// Build a bare TCP RST|ACK (refuse a connection).
pub fn build_tcp_rst(dst_mac: Mac, src_mac: Mac, src: (Ipv4Addr, u16), dst: (Ipv4Addr, u16), ack: u32) -> Vec<u8> {
    let mut f = Vec::with_capacity(ETH_HLEN + 40);
    eth_ipv4_header(&mut f, dst_mac, src_mac, src.0, dst.0, PROTO_TCP, 20);
    let mut t = [0u8; 20];
    t[0..2].copy_from_slice(&src.1.to_be_bytes());
    t[2..4].copy_from_slice(&dst.1.to_be_bytes());
    t[8..12].copy_from_slice(&ack.to_be_bytes());
    t[12] = 5 << 4;
    t[13] = TCP_RST | TCP_ACK;
    let c = checksum(pseudo_sum(src.0, dst.0, PROTO_TCP, 20), &t);
    t[16..18].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&t);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Mac = [2, 0, 0, 0, 0, 1];
    const B: Mac = [2, 0, 0, 0, 0, 2];

    #[test]
    fn udp_roundtrip() {
        let f = build_udp(A, B, (Ipv4Addr::new(10, 0, 2, 2), 53), (Ipv4Addr::new(10, 0, 2, 15), 4000), b"hi");
        let e = parse_eth(&f).unwrap();
        assert_eq!((e.dst, e.src, e.ethertype), (A, B, ETHERTYPE_IPV4));
        let ip = parse_ipv4(&f).unwrap();
        assert_eq!(ip.proto, PROTO_UDP);
        assert!(!ip.fragmented);
        // IP header checksum verifies to 0.
        assert_eq!(checksum(0, &f[14..34]), 0);
        let u = parse_udp(ip.payload).unwrap();
        assert_eq!((u.src_port, u.dst_port, u.payload), (53, 4000, &b"hi"[..]));
        // UDP checksum over pseudo header verifies to 0.
        assert_eq!(checksum(pseudo_sum(ip.src, ip.dst, PROTO_UDP, ip.payload.len()), ip.payload), 0);
    }

    #[test]
    fn rst_parses() {
        let f = build_tcp_rst(A, B, (Ipv4Addr::new(1, 2, 3, 4), 80), (Ipv4Addr::new(10, 0, 2, 15), 5555), 1001);
        let ip = parse_ipv4(&f).unwrap();
        let t = parse_tcp(ip.payload).unwrap();
        assert_eq!((t.src_port, t.dst_port, t.ack, t.flags), (80, 5555, 1001, TCP_RST | TCP_ACK));
    }

    #[test]
    fn truncated_and_lying_headers() {
        let f = build_udp(A, B, (Ipv4Addr::LOCALHOST, 1), (Ipv4Addr::LOCALHOST, 2), b"xyz");
        for n in 0..f.len() {
            // Must never panic on any truncation.
            let _ = parse_ipv4(&f[..n]).and_then(|ip| parse_udp(ip.payload));
            let _ = parse_arp(&f[..n]);
            let _ = parse_tcp(&f[..n]);
        }
        let mut bad = f.clone();
        bad[14 + 2] = 0xff; // total length > frame
        assert!(parse_ipv4(&bad).is_none());
        let mut bad = f.clone();
        bad[14] = 0x41; // ihl = 4 bytes
        assert!(parse_ipv4(&bad).is_none());
    }
}
