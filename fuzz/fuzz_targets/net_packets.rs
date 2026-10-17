//! Frames a guest sends to the NAT: every parser layer.
#![no_main]
use vroom_net::packet::*;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|f: &[u8]| {
    let _ = parse_eth(f);
    let _ = parse_arp(f);
    if let Some(ip) = parse_ipv4(f) {
        let _ = parse_udp(ip.payload);
        let _ = parse_tcp(ip.payload);
    }
    let _ = checksum(0, f);
    let _ = icmp_error(f, [2, 0, 0, 0, 0, 1], std::net::Ipv4Addr::new(10, 0, 2, 2), 3, 3);
    // MAC translation (an adapter switched to a vmnet port): both ways,
    // with the MACs taken from the frame so the rewrites happen.
    let mut g = f.to_vec();
    let mac = |o: usize| -> [u8; 6] { std::array::from_fn(|i| f.get(o + i).copied().unwrap_or(0)) };
    let (guest, port) = (mac(6), mac(0));
    vroom_net::mac::outgoing(&mut g, guest, port);
    assert_eq!(g.len(), f.len());
    vroom_net::mac::incoming(&mut g, port, guest);
    assert_eq!(g.len(), f.len());
});
