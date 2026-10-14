//! Bounded IPv4 reassembly. Overlapping fragments discard the whole datagram.
use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::packet::{checksum, parse_ipv4, ETH_HLEN};

const MAX_DATAGRAMS: usize = 64;
const MAX_FRAGMENTS: usize = 128;
const LIFETIME: Duration = Duration::from_secs(30);
type Key = (Ipv4Addr, Ipv4Addr, u8, u16);

#[derive(Default)]
pub(super) struct Reassembly {
    entries: HashMap<Key, Datagram>,
}
struct Datagram {
    created: Instant,
    header: Option<Vec<u8>>,
    parts: BTreeMap<usize, Vec<u8>>,
    end: Option<usize>,
    bytes: usize,
}
impl Reassembly {
    pub fn expire(&mut self) {
        self.entries.retain(|_, d| d.created.elapsed() < LIFETIME);
    }

    pub fn push(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        let ip = parse_ipv4(frame)?;
        let h = &frame[ETH_HLEN..];
        let flags = u16::from_be_bytes([h[6], h[7]]);
        let offset = (flags & 0x1fff) as usize * 8;
        let more = flags & 0x2000 != 0;
        let key = (ip.src, ip.dst, ip.proto, u16::from_be_bytes([h[4], h[5]]));
        let end = offset + ip.payload.len();
        self.expire();
        if ip.payload.is_empty() || end > 65515 || (more && ip.payload.len() % 8 != 0) || flags & 0xc000 != 0 {
            self.entries.remove(&key);
            return None;
        }
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_DATAGRAMS {
            return None;
        }
        let d = self.entries.entry(key).or_insert_with(|| Datagram {
            created: Instant::now(),
            header: None,
            parts: BTreeMap::new(),
            end: None,
            bytes: 0,
        });
        let overlap = d.parts.iter().any(|(&start, p)| start < end && offset < start + p.len());
        let invalid_end = d.end.is_some_and(|n| end > n || (!more && end != n) || (more && end >= n));
        if overlap
            || invalid_end
            || d.parts.len() >= MAX_FRAGMENTS
            || (!more && d.parts.iter().any(|(&start, p)| start + p.len() > end))
        {
            self.entries.remove(&key);
            return None;
        }
        if offset == 0 {
            let ihl = (h[0] & 15) as usize * 4;
            d.header = Some(frame[..ETH_HLEN + ihl].to_vec());
        }
        if !more {
            d.end = Some(end);
        }
        d.bytes += ip.payload.len();
        d.parts.insert(offset, ip.payload.to_vec());
        if d.header.is_none() || d.end != Some(d.bytes) {
            return None;
        }
        let d = self.entries.remove(&key)?;
        let mut f = d.header?;
        let total = f.len() - ETH_HLEN + d.bytes;
        if total > 65535 {
            return None;
        }
        let h = &mut f[ETH_HLEN..];
        h[2..4].copy_from_slice(&(total as u16).to_be_bytes());
        h[6..8].fill(0);
        h[10..12].fill(0);
        let c = checksum(0, h);
        h[10..12].copy_from_slice(&c.to_be_bytes());
        for part in d.parts.into_values() {
            f.extend_from_slice(&part);
        }
        Some(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{build_udp, fragment_ipv4, parse_udp};

    fn fragments() -> Vec<Vec<u8>> {
        fragment_ipv4(
            build_udp(
                [2; 6],
                [4; 6],
                (Ipv4Addr::new(10, 0, 2, 15), 1234),
                (Ipv4Addr::new(1, 1, 1, 1), 53),
                &vec![42; 8000],
            ),
            1500,
            7,
        )
    }
    #[test]
    fn out_of_order_round_trip() {
        let mut r = Reassembly::default();
        let mut fs = fragments();
        assert!(fs.iter().all(|f| f.len() <= 1514));
        fs.reverse();
        let mut result = None;
        for f in fs {
            result = r.push(&f);
        }
        let f = result.unwrap();
        let ip = parse_ipv4(&f).unwrap();
        assert!(!ip.fragmented);
        assert_eq!(parse_udp(ip.payload).unwrap().payload, vec![42; 8000]);
        assert!(r.entries.is_empty());
    }
    #[test]
    fn overlap_and_timeout_discard() {
        let fs = fragments();
        let mut r = Reassembly::default();
        assert!(r.push(&fs[0]).is_none());
        assert!(r.push(&fs[0]).is_none());
        assert!(r.entries.is_empty());
        r.push(&fs[0]);
        for d in r.entries.values_mut() {
            d.created = Instant::now() - LIFETIME;
        }
        r.expire();
        assert!(r.entries.is_empty());
        for f in fs.iter().skip(1) {
            assert!(r.push(f).is_none());
        }
    }
    #[test]
    fn bounds_and_bad_checksum() {
        let mut r = Reassembly::default();
        for id in 0..100u16 {
            let mut f = fragments().remove(0);
            f[18..20].copy_from_slice(&id.to_be_bytes());
            f[24..26].fill(0);
            let c = checksum(0, &f[14..34]);
            f[24..26].copy_from_slice(&c.to_be_bytes());
            assert!(r.push(&f).is_none());
        }
        assert_eq!(r.entries.len(), MAX_DATAGRAMS);
        let mut f = fragments().remove(0);
        f[24] ^= 1;
        assert!(parse_ipv4(&f).is_none());
    }
}
