//! Unprivileged ICMP echo relay. Each outstanding probe owns a datagram
//! socket; no raw sockets, helper process, or additional sandbox permissions.
use super::*;

pub(super) const MAX_PINGS: usize = 64;
const TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct Pending {
    sock: UdpSocket,
    request: Vec<u8>,
    wire: Vec<u8>,
    destination: Ipv4Addr,
    created: Instant,
}

impl Nat {
    pub(super) fn error_to_guest(&mut self, frame: &[u8], kind: u8, code: u8) {
        if let Some(reply) = icmp_error(frame, GATEWAY_MAC, GATEWAY, kind, code) {
            self.push_guest(reply);
        }
    }

    pub(super) fn handle_ping(&mut self, frame: &[u8], ip: &Ipv4<'_>) {
        let p = ip.payload;
        if p.len() < 8 || p[0] != 8 || p[1] != 0 || checksum(0, p) != 0 {
            return;
        }
        // Echo uses the same destination policy, but never the DNS/loopback aliases.
        if ip.dst == DNS || ip.dst == GATEWAY || self.resolve(ip.dst, 1).is_none() {
            self.error_to_guest(frame, 3, 13);
            return;
        }
        if self.pings.len() >= MAX_PINGS {
            return;
        }
        let result = (|| -> io::Result<Pending> {
            let sock =
                socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::ICMPV4))?;
            sock.set_nonblocking(true)?;
            sock.set_ttl(u32::from(ip.ttl.saturating_sub(1).max(1)))?;
            sock.connect(&SocketAddr::from((ip.dst, 0)).into())?;
            let mut wire = p.to_vec();
            getrandom::getrandom(&mut wire[4..8]).map_err(|e| io::Error::other(e.to_string()))?;
            wire[2..4].fill(0);
            let c = checksum(0, &wire);
            wire[2..4].copy_from_slice(&c.to_be_bytes());
            sock.send(&wire)?;
            // Linux ping sockets substitute their allocated local port for the
            // echo identifier. Darwin preserves it and includes an IP header.
            #[cfg(target_os = "linux")]
            {
                let id = sock.local_addr()?.as_socket_ipv4().ok_or_else(|| io::Error::other("ping address"))?.port();
                wire[4..6].copy_from_slice(&id.to_be_bytes());
            }
            Ok(Pending {
                sock: UdpSocket::from_std(sock.into()),
                request: frame.to_vec(),
                wire,
                destination: ip.dst,
                created: Instant::now(),
            })
        })();
        let mut pending = match result {
            Ok(p) => p,
            Err(e) => {
                log::debug!("icmp echo to {}: {e}", ip.dst);
                self.error_to_guest(frame, 3, 1);
                return;
            }
        };
        let Some(reg) = self.registry.as_ref().and_then(|r| r.try_clone().ok()) else { return };
        let tok = self.alloc_token(Owner::Ping);
        if reg.register(&mut pending.sock, Token(tok), Interest::READABLE).is_err() {
            self.owners.remove(&tok);
            return;
        }
        self.pings.insert(tok, pending);
        // Covers a reply that arrived before registration on edge-triggered pollers.
        self.ping_readable(tok);
    }

    pub(super) fn ping_readable(&mut self, tok: usize) {
        let Some(p) = self.pings.get_mut(&tok) else { return };
        let mut buf = vec![0u8; 65535];
        let mut reply = None;
        // Darwin can deliver other processes' ICMP replies too. Bound work per event.
        for _ in 0..128 {
            match p.sock.recv_from(&mut buf) {
                Ok((n, addr)) => {
                    let Some(body) = host_payload(&buf[..n]) else { continue };
                    if addr.ip() != p.destination || !matches_reply(body, &p.wire) {
                        continue;
                    }
                    let Some(original) = parse_ipv4(&p.request) else { break };
                    let mut payload = body.to_vec();
                    payload[4..8].copy_from_slice(&original.payload[4..8]);
                    reply = Some(build_icmp(self.guest_mac, GATEWAY_MAC, p.destination, GUEST, &payload));
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => break,
            }
        }
        if let Some(reply) = reply {
            self.push_guest(reply);
            self.drop_ping(tok);
        }
    }

    fn drop_ping(&mut self, tok: usize) {
        if let Some(mut p) = self.pings.remove(&tok) {
            if let Some(reg) = &self.registry {
                let _ = reg.deregister(&mut p.sock);
            }
            self.owners.remove(&tok);
        }
    }

    pub(super) fn expire_pings(&mut self) {
        let dead: Vec<_> = self.pings.iter().filter(|(_, p)| p.created.elapsed() >= TIMEOUT).map(|(&t, _)| t).collect();
        for tok in dead {
            self.drop_ping(tok);
        }
    }
}

fn host_payload(bytes: &[u8]) -> Option<&[u8]> {
    #[cfg(target_os = "macos")]
    {
        // Darwin's raw receive header has host-endian length fields; use the
        // actual datagram length, not the ordinary Ethernet/IP parser.
        if bytes.len() < 20 || bytes[0] >> 4 != 4 || bytes[9] != PROTO_ICMP {
            return None;
        }
        let ihl = (bytes[0] & 15) as usize * 4;
        if ihl < 20 {
            return None;
        }
        bytes.get(ihl..)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(bytes)
    }
}

fn matches_reply(reply: &[u8], sent: &[u8]) -> bool {
    reply.len() >= 8
        && reply.len() == sent.len()
        && reply[0] == 0
        && reply[1] == 0
        && checksum(0, reply) == 0
        && reply[4..] == sent[4..]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn echo_matching_rejects_foreign_and_corrupt_packets() {
        let sent = [8, 0, 0, 0, 0x12, 0x34, 0x56, 0x78, 42];
        let frame = build_icmp([2; 6], [4; 6], GATEWAY, GUEST, &[0, 0, 0, 0, 0x12, 0x34, 0x56, 0x78, 42]);
        let reply = parse_ipv4(&frame).unwrap().payload;
        assert!(matches_reply(reply, &sent));
        for i in [0, 1, 2, 4, 6, 8] {
            let mut bad = reply.to_vec();
            bad[i] ^= 1;
            assert!(!matches_reply(&bad, &sent));
        }
        assert!(!matches_reply(&reply[..7], &sent));
        let foreign = build_icmp([2; 6], [4; 6], GATEWAY, GUEST, &[0, 0, 0, 0, 0x12, 0x34, 0x56, 0x79, 42]);
        assert!(!matches_reply(parse_ipv4(&foreign).unwrap().payload, &sent));
    }

    #[test]
    fn outstanding_probes_expire() {
        let mut n = Nat::new(NatConfig::new([2, 0, 0, 0, 0, 9])).unwrap();
        let poll = mio::Poll::new().unwrap();
        n.register(poll.registry()).unwrap();
        let token = n.alloc_token(Owner::Ping);
        let mut sock = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        poll.registry().register(&mut sock, Token(token), Interest::READABLE).unwrap();
        n.pings.insert(
            token,
            Pending {
                sock,
                request: Vec::new(),
                wire: Vec::new(),
                destination: Ipv4Addr::LOCALHOST,
                created: Instant::now() - TIMEOUT,
            },
        );
        n.tick();
        assert!(n.pings.is_empty());
        assert!(!n.owners.contains_key(&token));
    }
}
