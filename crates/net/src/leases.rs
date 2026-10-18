//! The DHCP leases macOS hands out on vmnet networks (bootpd, for
//! host-only and shared mode): which address a VM adapter got.
//!
//! `/var/db/dhcpd_leases`, readable by everyone, one block per client:
//!
//! ```text
//! {
//! 	name=debian
//! 	ip_address=192.168.77.2
//! 	hw_address=1,2:a1:0:b:fb:4d
//! 	identifier=1,2:a1:0:b:fb:4d
//! 	lease=0x6531c4b6
//! }
//! ```

use std::net::Ipv4Addr;

pub const PATH: &str = "/var/db/dhcpd_leases";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The host name the guest sent ("" if none).
    pub name: String,
    pub ip: Ipv4Addr,
    pub hw: Option<[u8; 6]>,
    /// Client identifier option, when it is a type-1 MAC (Windows sends
    /// the guest's own MAC there).
    pub id: Option<[u8; 6]>,
    /// Expiry, Unix seconds.
    pub until: u64,
}

/// "1,2:a1:0:b:fb:4d": hardware type 1 (Ethernet), octets without
/// leading zeros.
fn mac(s: &str) -> Option<[u8; 6]> {
    let rest = s.strip_prefix("1,")?;
    let mut out = [0u8; 6];
    let mut parts = rest.split(':');
    for o in &mut out {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 2 {
            return None;
        }
        *o = u8::from_str_radix(p, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// Every well-formed block; anything else is skipped.
pub fn parse(text: &str) -> Vec<Lease> {
    let mut out = Vec::new();
    let mut cur: Option<Vec<(&str, &str)>> = None;
    for line in text.lines().map(str::trim) {
        match line {
            "{" => cur = Some(Vec::new()),
            "}" => {
                let Some(kv) = cur.take() else { continue };
                let get = |k: &str| kv.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);
                let Some(ip) = get("ip_address").and_then(|v| v.parse().ok()) else { continue };
                let until = get("lease").and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()).unwrap_or(0);
                out.push(Lease { name: get("name").unwrap_or("").into(), ip, hw: get("hw_address").and_then(mac), id: get("identifier").and_then(mac), until });
            }
            _ => {
                if let (Some(kv), Some((k, v))) = (cur.as_mut(), line.split_once('=')) {
                    kv.push((k, v));
                }
            }
        }
    }
    out
}

/// The newest lease of the adapter with `mac`.
pub fn of(leases: &[Lease], mac: [u8; 6]) -> Option<&Lease> {
    leases.iter().filter(|l| l.hw == Some(mac) || l.id == Some(mac)).max_by_key(|l| l.until)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "{\n\tname=debian\n\tip_address=192.168.77.2\n\thw_address=1,2:a1:0:b:fb:4d\n\tidentifier=1,2:a1:0:b:fb:4d\n\tlease=0x6531c4b6\n}\n\
{\n\tname=DESKTOP-1\n\tip_address=192.168.77.3\n\thw_address=1,2:a1:0:b:fb:4e\n\tidentifier=1,52:54:0:12:34:56\n\tlease=0x6531c000\n}\n\
{\n\tip_address=192.168.77.9\n\thw_address=1,2:a1:0:b:fb:4d\n\tlease=0x6531c4b7\n}\n\
{\n\tname=broken\n\tip_address=nope\n}\n";

    #[test]
    fn parses_and_finds() {
        let l = parse(FILE);
        assert_eq!(l.len(), 3);
        assert_eq!((l[0].name.as_str(), l[0].ip, l[0].hw, l[0].until), ("debian", Ipv4Addr::new(192, 168, 77, 2), Some([2, 0xa1, 0, 0xb, 0xfb, 0x4d]), 0x6531c4b6));
        // Newest of two for one MAC; the identifier counts too.
        assert_eq!(of(&l, [2, 0xa1, 0, 0xb, 0xfb, 0x4d]).unwrap().ip, Ipv4Addr::new(192, 168, 77, 9));
        assert_eq!(of(&l, [0x52, 0x54, 0, 0x12, 0x34, 0x56]).unwrap().name, "DESKTOP-1");
        assert!(of(&l, [2, 0, 0, 0, 0, 1]).is_none());
    }

    #[test]
    fn odd_input() {
        assert!(parse("").is_empty());
        assert!(parse("}\n{\nip_address=10.0.0.1\n").is_empty());
        for bad in ["1,2:a1:0:b:fb", "1,2:a1:0:b:fb:4d:7", "6,2:a1:0:b:fb:4d", "1,2:a1:0:b:fb:4dd", "1,2:a1::b:fb:4d"] {
            assert_eq!(mac(bad), None, "{bad}");
        }
    }
}
