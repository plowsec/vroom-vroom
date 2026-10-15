//! NAT pieces fed straight from guest frames without touching the host
//! network: the DHCP server and IPv4 fragment reassembly.
#![no_main]
use std::net::Ipv4Addr;

use bulwark_net::nat::fuzzing::{dhcp, Lease, Reassembly};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let lease = Lease {
        client: Ipv4Addr::new(10, 0, 2, 15),
        server: Ipv4Addr::new(10, 0, 2, 2),
        mask: Ipv4Addr::new(255, 255, 255, 0),
        dns: Ipv4Addr::new(10, 0, 2, 3),
        seconds: 86400,
    };
    if let Some(r) = dhcp(data, &lease) {
        assert!(r.len() <= 1500);
    }
    // Length-prefixed frames into the reassembler.
    let mut re = Reassembly::default();
    let mut rest = data;
    while rest.len() > 2 {
        let n = (u16::from_le_bytes([rest[0], rest[1]]) as usize).min(rest.len() - 2);
        if let Some(d) = re.push(&rest[2..2 + n]) {
            assert!(d.len() <= 65535 + 14);
        }
        rest = &rest[2 + n..];
    }
});
