//! Host-only networks: which ones netd offers and their subnets.
//!
//! Each is a private IPv4 subnet with the host on it (`gateway`, e.g.
//! 192.168.77.1/24) and DHCP for the rest. macOS: vmnet's host mode makes
//! the host's bridge and serves DHCP. Linux: a bridge with the address.

use std::net::Ipv4Addr;

use serde::Deserialize;

/// The network a VM gets when its file names none.
pub const DEFAULT_NAME: &str = "lab";
pub const DEFAULT_GATEWAY: &str = "192.168.77.1/24";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostOnly {
    pub name: String,
    /// The host's address and the subnet's size, e.g. "192.168.77.1/24".
    #[serde(default = "default_gateway")]
    pub gateway: String,
}

fn default_gateway() -> String {
    DEFAULT_GATEWAY.into()
}

/// What netd offers when its config says nothing about host-only.
pub fn defaults() -> Vec<HostOnly> {
    vec![HostOnly { name: DEFAULT_NAME.into(), gateway: default_gateway() }]
}

/// A host-only subnet: the host's address in it and the prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    pub host: Ipv4Addr,
    pub prefix: u8,
}

impl Subnet {
    /// "192.168.77.1/24": a private address (RFC 1918, what vmnet takes),
    /// /16 to /29, the host neither the network's first nor its last two
    /// addresses (DHCP needs room above it).
    pub fn parse(s: &str) -> Result<Subnet, String> {
        let bad = |why: &str| Err(format!("host-only gateway {s:?}: {why}"));
        let Some((a, p)) = s.split_once('/') else { return bad("want address/prefix, e.g. 192.168.77.1/24") };
        let (Ok(host), Ok(prefix)) = (a.parse::<Ipv4Addr>(), p.parse::<u8>()) else { return bad("want address/prefix, e.g. 192.168.77.1/24") };
        if !host.is_private() {
            return bad("not a private address (10/8, 172.16/12, 192.168/16)");
        }
        if !(16..=29).contains(&prefix) {
            return bad("prefix /16 to /29");
        }
        let n = Subnet { host, prefix };
        let h = u32::from(host);
        if h == u32::from(n.network()) || h + 1 >= u32::from(n.broadcast()) {
            return bad("the host takes an address low in the subnet; DHCP hands out the ones above it");
        }
        Ok(n)
    }

    pub fn mask(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::MAX << (32 - self.prefix))
    }

    pub fn network(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.host) & u32::from(self.mask()))
    }

    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.host) | !u32::from(self.mask()))
    }

    /// DHCP's last address: the one before broadcast.
    pub fn dhcp_end(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.broadcast()) - 1)
    }

    pub fn contains(&self, a: Ipv4Addr) -> bool {
        u32::from(a) & u32::from(self.mask()) == u32::from(self.network())
    }

    pub fn overlaps(&self, o: &Subnet) -> bool {
        self.contains(o.network()) || o.contains(self.network())
    }
}

/// Check the configured networks: valid subnets, none sharing one (they
/// would land on the same host bridge). Returns them parsed, in order.
pub fn check(nets: &[HostOnly]) -> Result<Vec<Subnet>, String> {
    let mut out: Vec<Subnet> = Vec::new();
    for (i, n) in nets.iter().enumerate() {
        let s = Subnet::parse(&n.gateway)?;
        if nets[..i].iter().any(|o| o.name == n.name) {
            return Err(format!("host-only network {} twice", n.name));
        }
        if let Some(j) = out.iter().position(|o| o.overlaps(&s)) {
            return Err(format!("host-only networks {} and {} overlap", nets[j].name, n.name));
        }
        out.push(s);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(name: &str, gw: &str) -> HostOnly {
        HostOnly { name: name.into(), gateway: gw.into() }
    }

    #[test]
    fn subnets() {
        let s = Subnet::parse("192.168.77.1/24").unwrap();
        assert_eq!((s.mask(), s.network(), s.dhcp_end()), ("255.255.255.0".parse().unwrap(), "192.168.77.0".parse().unwrap(), "192.168.77.254".parse().unwrap()));
        assert!(s.contains("192.168.77.200".parse().unwrap()) && !s.contains("192.168.78.2".parse().unwrap()));
        let s = Subnet::parse("10.20.0.1/16").unwrap();
        assert_eq!(s.dhcp_end(), "10.20.255.254".parse::<Ipv4Addr>().unwrap());
        let s = Subnet::parse("172.16.5.9/29").unwrap();
        assert_eq!((s.network(), s.dhcp_end()), ("172.16.5.8".parse().unwrap(), "172.16.5.14".parse().unwrap()));
        for bad in ["192.168.77.1", "192.168.77.1/33", "192.168.77.1/8", "8.8.8.1/24", "192.168.77.0/24", "192.168.77.254/24", "192.168.77.255/24", "x/24", "192.168.77.1/30"] {
            assert!(Subnet::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn configured_networks() {
        assert_eq!(check(&defaults()).unwrap(), vec![Subnet::parse(DEFAULT_GATEWAY).unwrap()]);
        assert!(check(&[net("a", "192.168.77.1/24"), net("b", "192.168.78.1/24")]).is_ok());
        let e = check(&[net("a", "192.168.77.1/24"), net("b", "192.168.0.1/16")]).unwrap_err();
        assert!(e.contains("a and b overlap"), "{e}");
        assert!(check(&[net("a", "192.168.77.1/24"), net("a", "192.168.78.1/24")]).unwrap_err().contains("twice"));
        assert!(check(&[net("a", "192.168.77.0/24")]).is_err());
    }
}
