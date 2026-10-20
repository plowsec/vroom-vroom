//! DHCP for a host-only network, answered by netd itself (Linux: nothing
//! else serves the bridge; macOS has vmnet's bootpd).
//!
//! One pool per network. A MAC gets the same address each time: the
//! first free one from a hash of the MAC, so also after netd restarts.
//! No router, no DNS: host-only goes nowhere else. Guest frames are
//! hostile input: every field is checked (packet / dhcp parsers).

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use vroom_net::dhcp::{self, Lease};
use vroom_net::packet::{build_udp, parse_eth, parse_ipv4, parse_udp, ETHERTYPE_IPV4, PROTO_UDP};

use crate::hostonly::Subnet;

pub type Mac = [u8; 6];

/// Lease time: guests renew at half of it, so an address unheard of for
/// this long is free to hand to another MAC when the pool runs out.
const LEASE: Duration = Duration::from_secs(3600);
const REQUEST: u8 = 3;

#[derive(Debug, Clone)]
struct Entry {
    ip: Ipv4Addr,
    name: String,
    seen: Instant,
}

#[derive(Debug)]
pub struct Pool {
    net: Subnet,
    leases: HashMap<Mac, Entry>,
}

fn hash(mac: &Mac) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in mac {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

impl Pool {
    pub fn new(net: Subnet) -> Pool {
        Pool { net, leases: HashMap::new() }
    }

    /// `mac`'s address: the one it has, else the first free one from its
    /// hash, else the one of the longest-silent MAC past its lease.
    fn address(&mut self, mac: Mac, now: Instant) -> Option<Ipv4Addr> {
        if let Some(e) = self.leases.get(&mac) {
            return Some(e.ip);
        }
        let first = u32::from(self.net.host) + 1;
        let n = u32::from(self.net.dhcp_end()) - first + 1;
        let start = hash(&mac) % n;
        let taken = |ip: Ipv4Addr, l: &HashMap<Mac, Entry>| l.values().any(|e| e.ip == ip);
        let free = (0..n).map(|k| Ipv4Addr::from(first + (start + k) % n)).find(|ip| !taken(*ip, &self.leases));
        let ip = match free {
            Some(ip) => ip,
            None => {
                let (&old, e) = self.leases.iter().filter(|(_, e)| now.duration_since(e.seen) > LEASE).min_by_key(|(_, e)| e.seen)?;
                let ip = e.ip;
                self.leases.remove(&old);
                ip
            }
        };
        self.leases.insert(mac, Entry { ip, name: String::new(), seen: now });
        Some(ip)
    }

    /// A guest frame that is a DHCP request (UDP 68 -> 67): the reply
    /// frame for that guest, from `server_mac`. None if it isn't one, or
    /// isn't for us (another server picked, not the sender's own MAC).
    pub fn answer(&mut self, frame: &[u8], server_mac: Mac, now: Instant) -> Option<Vec<u8>> {
        let req = request(frame)?;
        let eth = parse_eth(frame)?;
        let c = dhcp::client(req)?;
        // Anti-spoofing covers the Ethernet source; the hardware address
        // inside must be the same, or one guest could take others' leases.
        if c.mac != eth.src {
            return None;
        }
        let ip = self.address(c.mac, now)?;
        let lease = Lease { client: ip, server: self.net.host, mask: self.net.mask(), router: None, dns: None, seconds: LEASE.as_secs() as u32 };
        let reply = dhcp::handle(req, &lease)?;
        if let Some(e) = self.leases.get_mut(&c.mac) {
            e.seen = now;
            if c.msg_type == REQUEST {
                if let Some(n) = c.host_name {
                    e.name = n;
                }
            }
        }
        Some(build_udp(eth.src, server_mac, (self.net.host, 67), (Ipv4Addr::BROADCAST, 68), &reply))
    }

    /// The address and host name `mac` has here.
    pub fn of(&self, mac: &Mac) -> Option<(Ipv4Addr, String)> {
        self.leases.get(mac).map(|e| (e.ip, e.name.clone()))
    }
}

/// The DHCP payload of a guest frame to a DHCP server (IPv4, UDP 68 ->
/// 67, not a fragment).
pub fn request(frame: &[u8]) -> Option<&[u8]> {
    if parse_eth(frame)?.ethertype != ETHERTYPE_IPV4 {
        return None;
    }
    let ip = parse_ipv4(frame)?;
    if ip.proto != PROTO_UDP || ip.fragmented {
        return None;
    }
    let u = parse_udp(ip.payload)?;
    (u.src_port == 68 && u.dst_port == 67).then_some(u.payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: Mac = [2, 0xbb, 0, 0, 0, 1];

    /// A guest's DHCP frame: `ty` (1 discover, 3 request), wanting `want`.
    fn frame(mac: Mac, chaddr: Mac, ty: u8, want: Option<Ipv4Addr>, name: Option<&str>) -> Vec<u8> {
        let mut b = vec![0u8; 240];
        (b[0], b[1], b[2]) = (1, 1, 6);
        b[4..8].copy_from_slice(&[9, 9, 9, 9]);
        b[28..34].copy_from_slice(&chaddr);
        b[236..240].copy_from_slice(&[99, 130, 83, 99]);
        b.extend_from_slice(&[53, 1, ty]);
        if let Some(w) = want {
            b.extend_from_slice(&[50, 4]);
            b.extend_from_slice(&w.octets());
        }
        if let Some(n) = name {
            b.extend_from_slice(&[12, n.len() as u8]);
            b.extend_from_slice(n.as_bytes());
        }
        b.push(255);
        build_udp([0xff; 6], mac, (Ipv4Addr::UNSPECIFIED, 68), (Ipv4Addr::BROADCAST, 67), &b)
    }

    /// (message type, offered address, router option present) of a reply
    /// frame from the network's host `server`.
    fn reply_from(f: &[u8], server: &str) -> (u8, Ipv4Addr, bool) {
        assert_eq!(parse_eth(f).unwrap().src, SERVER);
        let ip = parse_ipv4(f).unwrap();
        let u = parse_udp(ip.payload).unwrap();
        assert_eq!((ip.src, u.src_port, u.dst_port), (server.parse().unwrap(), 67, 68));
        let p = u.payload;
        let yi = Ipv4Addr::new(p[16], p[17], p[18], p[19]);
        let mut i = 240;
        let (mut ty, mut router) = (0, false);
        while p[i] != 255 {
            match p[i] {
                53 => ty = p[i + 2],
                3 => router = true,
                _ => {}
            }
            i += 2 + p[i + 1] as usize;
        }
        (ty, yi, router)
    }

    fn reply(f: &[u8]) -> (u8, Ipv4Addr, bool) {
        reply_from(f, "192.168.77.1")
    }

    fn pool() -> Pool {
        Pool::new(Subnet::parse("192.168.77.1/24").unwrap())
    }

    #[test]
    fn discover_request_same_address_each_time() {
        let (mut p, t) = (pool(), Instant::now());
        let a: Mac = [2, 0, 0, 0, 0, 0xa];
        let (ty, ip, router) = reply(&p.answer(&frame(a, a, 1, None, None), SERVER, t).unwrap());
        assert_eq!((ty, router), (2, false));
        assert!(ip != Ipv4Addr::new(192, 168, 77, 1) && Subnet::parse("192.168.77.1/24").unwrap().contains(ip));
        assert_eq!(reply(&p.answer(&frame(a, a, 3, Some(ip), Some("deb")), SERVER, t).unwrap()).0, 5);
        assert_eq!(p.of(&a), Some((ip, "deb".into())));
        // Another address asked for: NAK, the client starts over.
        assert_eq!(reply(&p.answer(&frame(a, a, 3, Some("192.168.77.99".parse().unwrap()), None), SERVER, t).unwrap()).0, 6);
        // A netd restart: the same address again (from the MAC).
        let mut q = pool();
        assert_eq!(reply(&q.answer(&frame(a, a, 1, None, None), SERVER, t).unwrap()).1, ip);
        // Another MAC, another address.
        let b: Mac = [2, 0, 0, 0, 0, 0xb];
        assert_ne!(reply(&p.answer(&frame(b, b, 1, None, None), SERVER, t).unwrap()).1, ip);
    }

    #[test]
    fn only_dhcp_and_only_for_yourself() {
        let (mut p, t) = (pool(), Instant::now());
        let a: Mac = [2, 0, 0, 0, 0, 0xa];
        // chaddr of another guest: no answer, no lease.
        assert!(p.answer(&frame(a, [2, 0, 0, 0, 0, 0xc], 1, None, None), SERVER, t).is_none());
        assert!(p.of(&[2, 0, 0, 0, 0, 0xc]).is_none());
        // Not DHCP.
        let dns = build_udp([0xff; 6], a, (Ipv4Addr::UNSPECIFIED, 68), (Ipv4Addr::BROADCAST, 53), &[0; 240]);
        assert!(request(&dns).is_none() && p.answer(&dns, SERVER, t).is_none());
        // Any prefix of a request is safe.
        let f = frame(a, a, 1, None, Some("x"));
        for n in 0..f.len() {
            let _ = p.answer(&f[..n], SERVER, t);
        }
    }

    #[test]
    fn a_full_pool_reuses_silent_addresses() {
        let t = Instant::now();
        let mut p = Pool::new(Subnet::parse("10.9.0.1/29").unwrap());
        let ask = |p: &mut Pool, m: Mac, at: Instant| p.answer(&frame(m, m, 1, None, None), SERVER, at).map(|r| reply_from(&r, "10.9.0.1").1);
        // .2 to .6: five addresses.
        let macs: Vec<Mac> = (0..5).map(|i| [2, 0, 0, 0, 1, i]).collect();
        let ips: Vec<Ipv4Addr> = macs.iter().map(|m| ask(&mut p, *m, t).unwrap()).collect();
        assert_eq!(ips.iter().collect::<std::collections::HashSet<_>>().len(), 5);
        let late: Mac = [2, 0, 0, 0, 2, 0];
        assert!(ask(&mut p, late, t).is_none());
        // Everyone but the first renewed; past its lease, the first's goes.
        let later = t + LEASE + Duration::from_secs(1);
        for m in &macs[1..] {
            ask(&mut p, *m, later);
        }
        assert_eq!(ask(&mut p, late, later), Some(ips[0]));
        assert!(p.of(&macs[0]).is_none());
    }
}
