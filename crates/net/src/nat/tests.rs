use super::*;
use mio::{Events, Poll};

const MAC: Mac = [2, 0, 0, 0, 0, 9];

fn drive(n: &mut Nat, poll: &mut Poll) {
    let mut events = Events::with_capacity(64);
    poll.poll(&mut events, Some(Duration::from_millis(2))).unwrap();
    for e in events.iter() {
        n.ready(e.token().0);
    }
    n.tick();
}

#[test]
fn fragmented_udp_round_trip_through_host_socket() {
    let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    server.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut cfg = NatConfig::new(MAC);
    cfg.dns_upstream = Some(server.local_addr().unwrap());
    let data: Vec<u8> = (0..8000).map(|i| (i % 251) as u8).collect();
    let expected = data.clone();
    let thread = std::thread::spawn(move || {
        let mut b = vec![0; 65535];
        let (len, peer) = server.recv_from(&mut b).unwrap();
        assert_eq!(&b[..len], expected);
        server.send_to(&b[..len], peer).unwrap();
    });
    let mut n = Nat::new(cfg).unwrap();
    let mut poll = Poll::new().unwrap();
    n.register(poll.registry()).unwrap();
    let frames = fragment_ipv4(build_udp(GATEWAY_MAC, MAC, (GUEST, 4444), (DNS, 53), &data), 1500, 123);
    for f in frames.iter().rev() {
        n.send_frame(f);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut reassembly = fragment::Reassembly::default();
    let mut result = None;
    while Instant::now() < deadline && result.is_none() {
        drive(&mut n, &mut poll);
        while let Some(f) = n.recv_frame() {
            assert!(f.len() <= 1514);
            if let Some(complete) = reassembly.push(&f) {
                result = Some(complete);
            }
        }
    }
    thread.join().unwrap();
    let f = result.expect("fragmented reply");
    let ip = parse_ipv4(&f).unwrap();
    let u = parse_udp(ip.payload).unwrap();
    assert_eq!((ip.src, ip.dst, u.src_port, u.dst_port), (DNS, GUEST, 53, 4444));
    assert_eq!(u.payload, data);
}

#[test]
fn tcp_large_transfer_and_half_close() {
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    server.set_nonblocking(true).unwrap();
    let mut cfg = NatConfig::new(MAC);
    cfg.dns_upstream = Some(server.local_addr().unwrap());
    let data: Vec<u8> = (0..300_000).map(|i| (i % 251) as u8).collect();
    let expected = data.clone();
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut s = loop {
            match server.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(e) => panic!("accept: {e}"),
            }
        };
        s.set_nonblocking(false).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut received = Vec::new();
        s.read_to_end(&mut received).unwrap_or_else(|e| panic!("host read after {} bytes: {e}", received.len()));
        assert_eq!(received, expected);
        s.write_all(&received).unwrap();
        s.shutdown(Shutdown::Write).unwrap();
    });
    let mut n = Nat::new(cfg).unwrap();
    let mut poll = Poll::new().unwrap();
    n.register(poll.registry()).unwrap();
    let mut pipe = Pipe::default();
    let mut icfg = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(MAC)));
    icfg.random_seed = 123;
    let epoch = Instant::now();
    let mut iface = Interface::new(icfg, &mut pipe, smol_now(epoch));
    iface.update_ip_addrs(|a| {
        a.push(IpCidr::new(IpAddress::Ipv4(GUEST), 24)).unwrap();
    });
    iface.routes_mut().add_default_ipv4_route(GATEWAY).unwrap();
    let mut sockets = SocketSet::new(Vec::new());
    let mut socket = Nat::new_tcp_socket();
    socket.connect(iface.context(), (IpAddress::Ipv4(DNS), 53), 3456).unwrap();
    let handle = sockets.add(socket);
    let mut sent = 0;
    let mut received = Vec::new();
    let mut closed = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let s = sockets.get_mut::<tcp::Socket>(handle);
        if sent < data.len() && s.can_send() {
            sent += s.send_slice(&data[sent..]).unwrap();
        }
        if sent == data.len() && !closed {
            s.close();
            closed = true;
        }
        while s.can_recv() {
            s.recv(|b| {
                received.extend_from_slice(b);
                (b.len(), ())
            })
            .unwrap();
        }
        if received.len() == data.len() && !s.may_recv() {
            break;
        }
        iface.poll(smol_now(epoch), &mut pipe, &mut sockets);
        while let Some(f) = pipe.tx.pop_front() {
            n.send_frame(&f);
        }
        drive(&mut n, &mut poll);
        while let Some(f) = n.recv_frame() {
            pipe.rx.push_back(f);
        }
    }
    thread.join().unwrap();
    assert_eq!(sent, data.len());
    assert_eq!(received, data);
}

#[test]
fn echo_policy_ttl_and_spoofed_source() {
    let mut cfg = NatConfig::new(MAC);
    cfg.policy.block_private = true;
    let mut n = Nat::new(cfg).unwrap();
    let blocked = build_icmp(GATEWAY_MAC, MAC, GUEST, Ipv4Addr::new(192, 168, 1, 1), &[8, 0, 0, 0, 0, 1, 0, 2, 3]);
    n.send_frame(&blocked);
    let f = n.recv_frame().unwrap();
    let ip = parse_ipv4(&f).unwrap();
    assert_eq!(&ip.payload[..2], &[3, 13]);
    assert_eq!(checksum(0, ip.payload), 0);
    let mut ttl = build_icmp(GATEWAY_MAC, MAC, GUEST, Ipv4Addr::new(1, 1, 1, 1), &[8, 0, 0, 0, 0, 1, 0, 2, 3]);
    ttl[22] = 1;
    ttl[24..26].fill(0);
    let c = checksum(0, &ttl[14..34]);
    ttl[24..26].copy_from_slice(&c.to_be_bytes());
    n.send_frame(&ttl);
    let f = n.recv_frame().unwrap();
    assert_eq!(&parse_ipv4(&f).unwrap().payload[..2], &[11, 0]);
    assert!(n.pings.is_empty());
    let spoof = build_icmp(GATEWAY_MAC, MAC, Ipv4Addr::LOCALHOST, Ipv4Addr::new(1, 1, 1, 1), &[8, 0, 0, 0, 0, 1, 0, 2]);
    n.send_frame(&spoof);
    assert!(n.pings.is_empty());
    assert!(n.recv_frame().is_none());
}

#[test]
fn pending_connect_expires_and_refuses_guest() {
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = TcpStream::connect(server.local_addr().unwrap()).unwrap();
    let mut n = Nat::new(NatConfig::new(MAC)).unwrap();
    let key = FlowKey { guest_port: 123, dst: SocketAddrV4::new(DNS, 53) };
    let tok = n.alloc_token(Owner::Tcp);
    n.tcp_by_key.insert(key, tok);
    n.tcp.insert(
        tok,
        TcpConn {
            key: Some(key),
            stream,
            handle: None,
            pending_syn: Some((Vec::new(), 42)),
            to_host: Vec::new(),
            host_readable: false,
            host_eof: false,
            host_wr_closed: false,
            created: Instant::now() - CONNECT_TIMEOUT,
        },
    );
    n.tick();
    assert!(n.tcp.is_empty() && n.tcp_by_key.is_empty() && n.owners.is_empty());
    let f = n.recv_frame().unwrap();
    let t = parse_tcp(parse_ipv4(&f).unwrap().payload).unwrap();
    assert_eq!((t.ack, t.flags), (43, TCP_RST | TCP_ACK));
}

/// Opt-in real network test. Run the same exchange in a subprocess with the
/// production sandbox, since applying Seatbelt affects the entire process.
#[test]
#[ignore = "requires outbound ICMP and host permission for datagram ping sockets"]
fn sandboxed_external_echo() {
    if std::env::var_os("BULWARK_PING_SANDBOX_CHILD").is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "nat::integration_tests::sandboxed_external_echo", "--ignored", "--nocapture"])
            .env("BULWARK_PING_SANDBOX_CHILD", "1")
            .status()
            .unwrap();
        assert!(result.success());
        return;
    }
    let mut n = Nat::new(NatConfig::new(MAC)).unwrap();
    let mut poll = Poll::new().unwrap();
    n.register(poll.registry()).unwrap();
    bulwark_sandbox::apply(&bulwark_sandbox::Policy { network_outbound: true, ..Default::default() }).unwrap();
    let dst = Ipv4Addr::new(1, 1, 1, 1);
    // Large, odd-sized payload exercises fragmentation in both directions.
    let mut payload = vec![42; 4009];
    payload[..8].copy_from_slice(&[8, 0, 0, 0, 0x12, 0x34, 0, 7]);
    let fs = fragment_ipv4(build_icmp(GATEWAY_MAC, MAC, GUEST, dst, &payload), 1500, 87);
    for f in fs {
        n.send_frame(&f);
    }
    let mut reassembly = fragment::Reassembly::default();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        drive(&mut n, &mut poll);
        while let Some(f) = n.recv_frame() {
            let ip = parse_ipv4(&f).unwrap();
            if !ip.fragmented {
                panic!("unexpected ICMP {} code {}", ip.payload[0], ip.payload[1]);
            }
            if let Some(f) = reassembly.push(&f) {
                let ip = parse_ipv4(&f).unwrap();
                assert_eq!((ip.src, ip.dst, ip.proto), (dst, GUEST, PROTO_ICMP));
                assert_eq!(&ip.payload[..2], &[0, 0]);
                assert_eq!(&ip.payload[4..], &payload[4..]);
                assert_eq!(checksum(0, ip.payload), 0);
                assert!(n.pings.is_empty());
                return;
            }
        }
    }
    panic!("no echo reply under sandbox");
}
