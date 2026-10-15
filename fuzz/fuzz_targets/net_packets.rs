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
});
