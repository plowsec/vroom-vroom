//! Userspace NAT ("slirp"-style), unprivileged.
//!
//! The guest sees a /24 with a gateway (10.0.2.2) and a DNS relay
//! (10.0.2.3). We terminate its TCP connections in smoltcp and proxy them
//! over normal host sockets; UDP flows map to connected host UDP sockets.
//!
//! Policy (secure by default): the guest can reach the internet but NOT
//! the host's loopback services, link-local, multicast or our own subnet.
//! Every table is capped so a hostile guest can't exhaust host memory.

mod dhcp;
mod fragment;
mod ping;

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use mio::net::{TcpListener, TcpStream, UdpSocket};
use mio::{Interest, Registry, Token};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint};

use crate::packet::*;
use crate::NetBackend;

pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
pub const DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
pub const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
const GATEWAY_MAC: Mac = [0x02, 0x42, 0x57, 0x4b, 0x00, 0x01];

const MAX_TCP: usize = 512;
const MAX_UDP: usize = 256;
const MAX_TO_GUEST: usize = 4096;
const TCP_BUF: usize = 64 << 10;
const UDP_IDLE: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Default)]
pub struct NatPolicy {
    /// Let the guest reach host 127.0.0.1 through the gateway address.
    pub allow_host_loopback: bool,
    /// Block RFC1918 / CGNAT destinations (isolate from the LAN).
    pub block_private: bool,
}

#[derive(Debug, Clone)]
pub struct TcpForward {
    pub host: SocketAddr,
    pub guest_port: u16,
}

#[derive(Debug, Clone)]
pub struct NatConfig {
    pub guest_mac: Mac,
    pub policy: NatPolicy,
    /// Where DNS queries to 10.0.2.3 go. None = DNS disabled.
    pub dns_upstream: Option<SocketAddr>,
    pub forwards: Vec<TcpForward>,
}

impl NatConfig {
    pub fn new(guest_mac: Mac) -> Self {
        NatConfig { guest_mac, policy: NatPolicy::default(), dns_upstream: host_dns(), forwards: Vec::new() }
    }
}

/// First IPv4 nameserver from /etc/resolv.conf.
pub fn host_dns() -> Option<SocketAddr> {
    let s = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    s.lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|v| v.trim().parse::<Ipv4Addr>().ok())
        .map(|ip| SocketAddr::V4(SocketAddrV4::new(ip, 53)))
        .next()
}

/// A UDP socket connected to `dst`, without an explicit bind: connect()
/// picks the ephemeral port. Keeps the sandbox free of "bind" rights, so a
/// compromised VMM can't open listening UDP ports.
fn connected_udp(dst: SocketAddr) -> io::Result<UdpSocket> {
    let domain = if dst.is_ipv4() { socket2::Domain::IPV4 } else { socket2::Domain::IPV6 };
    let s = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    s.set_nonblocking(true)?;
    s.connect(&dst.into())?;
    Ok(UdpSocket::from_std(s.into()))
}

/// smoltcp <-> frame queues.
#[derive(Default)]
struct Pipe {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

struct RxTok(Vec<u8>);
struct TxTok<'a>(&'a mut VecDeque<Vec<u8>>);

impl RxToken for RxTok {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for TxTok<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0u8; len];
        let r = f(&mut b);
        self.0.push_back(b);
        r
    }
}

impl Device for Pipe {
    type RxToken<'a> = RxTok;
    type TxToken<'a> = TxTok<'a>;

    fn receive(&mut self, _t: smoltcp::time::Instant) -> Option<(RxTok, TxTok<'_>)> {
        let f = self.rx.pop_front()?;
        Some((RxTok(f), TxTok(&mut self.tx)))
    }

    fn transmit(&mut self, _t: smoltcp::time::Instant) -> Option<TxTok<'_>> {
        (self.tx.len() < MAX_TO_GUEST).then(|| TxTok(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1514;
        c
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FlowKey {
    guest_port: u16,
    dst: SocketAddrV4,
}

struct TcpConn {
    key: Option<FlowKey>,
    stream: TcpStream,
    handle: Option<SocketHandle>,
    /// Outbound connect in progress; holds the guest's SYN to replay.
    pending_syn: Option<(Vec<u8>, u32)>,
    to_host: Vec<u8>,
    host_readable: bool,
    host_eof: bool,
    host_wr_closed: bool,
    created: Instant,
}

struct UdpFlow {
    key: FlowKey,
    sock: UdpSocket,
    last: Instant,
}

enum Owner {
    Tcp,
    Ping,
    Udp(FlowKey),
    Listener(usize),
}

pub struct Nat {
    cfg: NatConfig,
    guest_mac: Mac,
    to_guest: VecDeque<Vec<u8>>,
    pipe: Pipe,
    iface: Interface,
    sockets: SocketSet<'static>,
    epoch: Instant,
    registry: Option<Registry>,
    next_token: usize,
    owners: HashMap<usize, Owner>,
    tcp: HashMap<usize, TcpConn>,
    tcp_by_key: HashMap<FlowKey, usize>,
    udp: HashMap<FlowKey, (usize, UdpFlow)>,
    listeners: Vec<(TcpListener, u16)>,
    next_local_port: u16,
    fragments: fragment::Reassembly,
    next_ip_id: u16,
    pings: HashMap<usize, ping::Pending>,
}

impl std::fmt::Debug for Nat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nat").field("tcp", &self.tcp.len()).field("udp", &self.udp.len()).finish()
    }
}

fn smol_now(epoch: Instant) -> smoltcp::time::Instant {
    smoltcp::time::Instant::from_micros(epoch.elapsed().as_micros() as i64)
}

impl Nat {
    pub fn new(cfg: NatConfig) -> io::Result<Nat> {
        let mut pipe = Pipe::default();
        let mut icfg = IfaceConfig::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        let mut seed = [0u8; 8];
        getrandom::getrandom(&mut seed).map_err(|e| io::Error::other(e.to_string()))?;
        icfg.random_seed = u64::from_le_bytes(seed);
        let epoch = Instant::now();
        let mut iface = Interface::new(icfg, &mut pipe, smol_now(epoch));
        iface.update_ip_addrs(|a| {
            let _ = a.push(IpCidr::new(IpAddress::Ipv4(GATEWAY), 24));
        });
        let _ = iface.routes_mut().add_default_ipv4_route(GATEWAY);
        // Accept TCP for any destination: that's how we proxy.
        iface.set_any_ip(true);
        let mut listeners = Vec::new();
        for f in &cfg.forwards {
            let l = TcpListener::bind(f.host)?;
            listeners.push((l, f.guest_port));
        }
        Ok(Nat {
            guest_mac: cfg.guest_mac,
            cfg,
            to_guest: VecDeque::new(),
            pipe,
            iface,
            sockets: SocketSet::new(Vec::new()),
            epoch,
            registry: None,
            next_token: 0,
            owners: HashMap::new(),
            tcp: HashMap::new(),
            tcp_by_key: HashMap::new(),
            udp: HashMap::new(),
            listeners,
            next_local_port: 40000,
            fragments: fragment::Reassembly::default(),
            next_ip_id: u16::from_le_bytes([seed[0], seed[1]]),
            pings: HashMap::new(),
        })
    }

    /// Where a guest destination really goes, or None if policy says no.
    fn resolve(&self, dst: Ipv4Addr, port: u16) -> Option<SocketAddr> {
        if dst == DNS {
            return if port == 53 { self.cfg.dns_upstream } else { None };
        }
        if dst == GATEWAY {
            return self
                .cfg
                .policy
                .allow_host_loopback
                .then(|| SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)));
        }
        let o = dst.octets();
        let in_subnet = o[0] == 10 && o[1] == 0 && o[2] == 2;
        let cgnat = o[0] == 100 && (64..128).contains(&o[1]);
        if dst.is_loopback()
            || dst.is_unspecified()
            || dst.is_broadcast()
            || dst.is_multicast()
            || dst.is_link_local()
            || in_subnet
            || port == 0
        {
            return None;
        }
        if self.cfg.policy.block_private && (dst.is_private() || cgnat) {
            return None;
        }
        Some(SocketAddr::V4(SocketAddrV4::new(dst, port)))
    }

    fn alloc_token(&mut self, owner: Owner) -> usize {
        let base = crate::TOKEN_BASE;
        let t = base + self.next_token;
        self.next_token += 1;
        self.owners.insert(t, owner);
        t
    }

    fn push_guest(&mut self, f: Vec<u8>) {
        let frames = if f.len() > 1514 {
            self.next_ip_id = self.next_ip_id.wrapping_add(1);
            fragment_ipv4(f, 1500, self.next_ip_id)
        } else {
            vec![f]
        };
        // Queue the whole datagram or none of it.
        if self.to_guest.len() + frames.len() <= MAX_TO_GUEST {
            self.to_guest.extend(frames);
        }
    }

    fn handle_arp(&mut self, frame: &[u8]) {
        let Some(a) = parse_arp(frame) else { return };
        // Only speak for our own two addresses. Answering for everything
        // (smoltcp any_ip) would break the guest's duplicate address checks.
        if a.target_ip == GATEWAY || a.target_ip == DNS {
            if a.op == 1 && a.target_ip == DNS {
                self.push_guest(arp_reply(&a, DNS));
                return;
            }
            self.pipe.rx.push_back(frame.to_vec());
        }
    }

    fn handle_dhcp(&mut self, eth: &Eth, payload: &[u8]) {
        let lease = dhcp::Lease { client: GUEST, server: GATEWAY, mask: NETMASK, dns: DNS, seconds: 86400 };
        if let Some(reply) = dhcp::handle(payload, &lease) {
            let f = build_udp(eth.src, GATEWAY_MAC, (GATEWAY, 67), (Ipv4Addr::BROADCAST, 68), &reply);
            self.push_guest(f);
        }
    }

    fn handle_udp(&mut self, ip: &Ipv4<'_>, u: &Udp<'_>) {
        let key = FlowKey { guest_port: u.src_port, dst: SocketAddrV4::new(ip.dst, u.dst_port) };
        if !self.udp.contains_key(&key) {
            let Some(real) = self.resolve(ip.dst, u.dst_port) else { return };
            if self.udp.len() >= MAX_UDP {
                self.expire_udp(true);
            }
            let Some(reg) = self.registry.as_ref().and_then(|r| r.try_clone().ok()) else { return };
            let Ok(mut sock) = connected_udp(real) else { return };
            let t = self.alloc_token(Owner::Udp(key));
            if reg.register(&mut sock, Token(t), Interest::READABLE).is_err() {
                self.owners.remove(&t);
                return;
            }
            self.udp.insert(key, (t, UdpFlow { key, sock, last: Instant::now() }));
        }
        if let Some((_, flow)) = self.udp.get_mut(&key) {
            flow.last = Instant::now();
            let _ = flow.sock.send(u.payload);
        }
    }

    fn udp_readable(&mut self, key: FlowKey) {
        let mut buf = vec![0u8; 65536];
        let mut out = Vec::new();
        if let Some((_, flow)) = self.udp.get_mut(&key) {
            loop {
                match flow.sock.recv(&mut buf) {
                    Ok(n) => {
                        flow.last = Instant::now();
                        if n <= 65507 {
                            out.push(build_udp(
                                self.guest_mac,
                                GATEWAY_MAC,
                                (*flow.key.dst.ip(), flow.key.dst.port()),
                                (GUEST, flow.key.guest_port),
                                &buf[..n],
                            ));
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        for f in out {
            self.push_guest(f);
        }
    }

    fn expire_udp(&mut self, force_one: bool) {
        let now = Instant::now();
        let mut dead: Vec<FlowKey> =
            self.udp.iter().filter(|(_, (_, f))| now - f.last > UDP_IDLE).map(|(k, _)| *k).collect();
        if dead.is_empty() && force_one {
            if let Some(k) = self.udp.iter().min_by_key(|(_, (_, f))| f.last).map(|(k, _)| *k) {
                dead.push(k);
            }
        }
        for k in dead {
            if let Some((t, mut f)) = self.udp.remove(&k) {
                if let Some(r) = &self.registry {
                    let _ = r.deregister(&mut f.sock);
                }
                self.owners.remove(&t);
            }
        }
    }

    fn handle_tcp(&mut self, frame: &[u8], ip: &Ipv4<'_>, t: &Tcp) {
        let key = FlowKey { guest_port: t.src_port, dst: SocketAddrV4::new(ip.dst, t.dst_port) };
        let is_syn = t.flags & TCP_SYN != 0 && t.flags & TCP_ACK == 0;
        if let Some(&tok) = self.tcp_by_key.get(&key) {
            let stale = self.tcp.get(&tok).is_some_and(|c| {
                c.handle.is_some_and(|h| {
                    matches!(self.sockets.get::<tcp::Socket>(h).state(), tcp::State::Closed | tcp::State::TimeWait)
                })
            });
            if !(is_syn && stale) {
                if let Some(c) = self.tcp.get_mut(&tok) {
                    if c.pending_syn.is_some() {
                        if is_syn {
                            c.pending_syn = Some((frame.to_vec(), t.seq));
                        }
                        return;
                    }
                }
                self.pipe.rx.push_back(frame.to_vec());
                return;
            }
            self.drop_tcp(tok);
        }
        if !is_syn {
            // No connection: let smoltcp answer with RST.
            self.pipe.rx.push_back(frame.to_vec());
            return;
        }
        let refuse = |nat: &mut Nat| {
            let rst = build_tcp_rst(
                nat.guest_mac,
                GATEWAY_MAC,
                (ip.dst, t.dst_port),
                (ip.src, t.src_port),
                t.seq.wrapping_add(1),
            );
            nat.push_guest(rst);
        };
        if self.tcp.len() >= MAX_TCP {
            return refuse(self);
        }
        let Some(real) = self.resolve(ip.dst, t.dst_port) else { return refuse(self) };
        let Some(reg) = self.registry.as_ref().and_then(|r| r.try_clone().ok()) else { return };
        let Ok(mut stream) = TcpStream::connect(real) else { return refuse(self) };
        let tok = self.alloc_token(Owner::Tcp);
        if reg.register(&mut stream, Token(tok), Interest::READABLE | Interest::WRITABLE).is_err() {
            self.owners.remove(&tok);
            return refuse(self);
        }
        self.tcp.insert(
            tok,
            TcpConn {
                key: Some(key),
                stream,
                handle: None,
                pending_syn: Some((frame.to_vec(), t.seq)),
                to_host: Vec::new(),
                host_readable: false,
                host_eof: false,
                host_wr_closed: false,
                created: Instant::now(),
            },
        );
        self.tcp_by_key.insert(key, tok);
    }

    fn new_tcp_socket() -> tcp::Socket<'static> {
        let mut s =
            tcp::Socket::new(tcp::SocketBuffer::new(vec![0u8; TCP_BUF]), tcp::SocketBuffer::new(vec![0u8; TCP_BUF]));
        s.set_nagle_enabled(false);
        s
    }

    /// Host side of an outbound connection became ready.
    fn tcp_event(&mut self, tok: usize) {
        let Some(c) = self.tcp.get_mut(&tok) else { return };
        c.host_readable = true;
        let Some((syn, seq)) = c.pending_syn.clone() else { return };
        let failed = match c.stream.take_error() {
            Ok(Some(_)) | Err(_) => true,
            Ok(None) => match c.stream.peer_addr() {
                Ok(_) => false,
                Err(e) if e.kind() == io::ErrorKind::NotConnected => return,
                Err(_) => true,
            },
        };
        let Some(key) = c.key else { return };
        if failed {
            let rst = build_tcp_rst(
                self.guest_mac,
                GATEWAY_MAC,
                (*key.dst.ip(), key.dst.port()),
                (GUEST, key.guest_port),
                seq.wrapping_add(1),
            );
            self.push_guest(rst);
            self.drop_tcp(tok);
            return;
        }
        let mut s = Self::new_tcp_socket();
        if s.listen(IpListenEndpoint { addr: Some(IpAddress::Ipv4(*key.dst.ip())), port: key.dst.port() }).is_err() {
            self.drop_tcp(tok);
            return;
        }
        let h = self.sockets.add(s);
        if let Some(c) = self.tcp.get_mut(&tok) {
            c.handle = Some(h);
            c.pending_syn = None;
        }
        self.pipe.rx.push_back(syn);
    }

    fn accept_forward(&mut self, idx: usize) {
        loop {
            let Some((l, guest_port)) = self.listeners.get(idx) else { return };
            let guest_port = *guest_port;
            let (mut stream, _) = match l.accept() {
                Ok(x) => x,
                Err(_) => return,
            };
            if self.tcp.len() >= MAX_TCP {
                continue;
            }
            let Some(reg) = self.registry.as_ref().and_then(|r| r.try_clone().ok()) else { return };
            let mut s = Self::new_tcp_socket();
            let local = self.next_local_port;
            self.next_local_port = if local >= 60000 { 40000 } else { local + 1 };
            let remote = IpEndpoint::new(IpAddress::Ipv4(GUEST), guest_port);
            if s.connect(self.iface.context(), remote, (IpAddress::Ipv4(GATEWAY), local)).is_err() {
                continue;
            }
            let tok = self.alloc_token(Owner::Tcp);
            if reg.register(&mut stream, Token(tok), Interest::READABLE | Interest::WRITABLE).is_err() {
                self.owners.remove(&tok);
                continue;
            }
            let h = self.sockets.add(s);
            self.tcp.insert(
                tok,
                TcpConn {
                    key: None,
                    stream,
                    handle: Some(h),
                    pending_syn: None,
                    to_host: Vec::new(),
                    host_readable: true,
                    host_eof: false,
                    host_wr_closed: false,
                    created: Instant::now(),
                },
            );
        }
    }

    fn drop_tcp(&mut self, tok: usize) {
        if let Some(mut c) = self.tcp.remove(&tok) {
            if let Some(r) = &self.registry {
                let _ = r.deregister(&mut c.stream);
            }
            if let Some(h) = c.handle {
                self.sockets.get_mut::<tcp::Socket>(h).abort();
                self.sockets.remove(h);
            }
            if let Some(k) = c.key {
                self.tcp_by_key.remove(&k);
            }
            self.owners.remove(&tok);
        }
    }

    /// Move bytes between smoltcp sockets and host streams.
    fn pump_tcp(&mut self) {
        let mut dead = Vec::new();
        let mut buf = vec![0u8; 16 << 10];
        for (&tok, c) in self.tcp.iter_mut() {
            let Some(h) = c.handle else { continue };
            let s = self.sockets.get_mut::<tcp::Socket>(h);
            // guest -> host
            while c.to_host.len() < TCP_BUF && s.can_recv() {
                let room = TCP_BUF - c.to_host.len();
                let r = s.recv(|b| {
                    let n = b.len().min(room);
                    c.to_host.extend_from_slice(&b[..n]);
                    (n, n)
                });
                if !matches!(r, Ok(n) if n > 0) {
                    break;
                }
            }
            while !c.to_host.is_empty() {
                match c.stream.write(&c.to_host) {
                    Ok(0) => break,
                    Ok(n) => {
                        c.to_host.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        s.abort();
                        dead.push(tok);
                        break;
                    }
                }
            }
            let established_once =
                !matches!(s.state(), tcp::State::Listen | tcp::State::SynReceived | tcp::State::SynSent);
            if established_once && !s.may_recv() && c.to_host.is_empty() && !c.host_wr_closed {
                let _ = c.stream.shutdown(Shutdown::Write);
                c.host_wr_closed = true;
            }
            // host -> guest
            while c.host_readable && !c.host_eof && s.can_send() {
                let space = (s.send_capacity() - s.send_queue()).min(buf.len());
                if space == 0 {
                    break;
                }
                match c.stream.read(&mut buf[..space]) {
                    Ok(0) => {
                        c.host_eof = true;
                        s.close();
                    }
                    Ok(n) => {
                        let _ = s.send_slice(&buf[..n]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => c.host_readable = false,
                    Err(_) => {
                        s.abort();
                        dead.push(tok);
                        break;
                    }
                }
            }
            if matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait) && established_once {
                dead.push(tok);
            }
        }
        for t in dead {
            self.drop_tcp(t);
        }
    }

    fn expire_connects(&mut self) {
        let dead: Vec<_> = self
            .tcp
            .iter()
            .filter(|(_, c)| c.pending_syn.is_some() && c.created.elapsed() >= CONNECT_TIMEOUT)
            .map(|(&t, _)| t)
            .collect();
        for t in dead {
            if let Some(c) = self.tcp.get(&t) {
                if let (Some(k), Some((_, seq))) = (c.key, &c.pending_syn) {
                    self.push_guest(build_tcp_rst(
                        self.guest_mac,
                        GATEWAY_MAC,
                        (*k.dst.ip(), k.dst.port()),
                        (GUEST, k.guest_port),
                        seq.wrapping_add(1),
                    ));
                }
            }
            self.drop_tcp(t);
        }
    }

    fn poll_stack(&mut self) {
        for _ in 0..4 {
            let now = smol_now(self.epoch);
            self.iface.poll(now, &mut self.pipe, &mut self.sockets);
            self.pump_tcp();
            let now = smol_now(self.epoch);
            self.iface.poll(now, &mut self.pipe, &mut self.sockets);
            if self.pipe.rx.is_empty() {
                break;
            }
        }
        while let Some(f) = self.pipe.tx.pop_front() {
            self.push_guest(f);
        }
    }
}

fn arp_reply(req: &Arp, ip: Ipv4Addr) -> Vec<u8> {
    let mut f = Vec::with_capacity(42);
    f.extend_from_slice(&req.sender_mac);
    f.extend_from_slice(&GATEWAY_MAC);
    f.extend_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    f.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]);
    f.extend_from_slice(&GATEWAY_MAC);
    f.extend_from_slice(&ip.octets());
    f.extend_from_slice(&req.sender_mac);
    f.extend_from_slice(&req.sender_ip.octets());
    f
}

impl NetBackend for Nat {
    fn send_frame(&mut self, frame: &[u8]) {
        let Some(eth) = parse_eth(frame) else { return };
        if eth.src != self.guest_mac {
            return; // one configured guest per backend
        }
        match eth.ethertype {
            ETHERTYPE_ARP => self.handle_arp(frame),
            ETHERTYPE_IPV4 => {
                let Some(ip) = parse_ipv4(frame) else { return };
                if ip.fragmented {
                    if eth.src == self.guest_mac && ip.src == GUEST {
                        if let Some(complete) = self.fragments.push(frame) {
                            self.send_frame(&complete);
                        }
                    }
                    return;
                }
                let dhcp = ip.proto == PROTO_UDP
                    && parse_udp(ip.payload).is_some_and(|u| u.src_port == 68 && u.dst_port == 67);
                if eth.src != self.guest_mac || (!dhcp && ip.src != GUEST) {
                    return;
                }
                if !dhcp && ip.ttl <= 1 && ip.dst != GATEWAY && ip.dst != DNS {
                    self.error_to_guest(frame, 11, 0);
                    return;
                }
                match ip.proto {
                    PROTO_UDP => {
                        if let Some(u) = parse_udp(ip.payload) {
                            if dhcp {
                                self.handle_dhcp(&eth, u.payload);
                            } else {
                                self.handle_udp(&ip, &u);
                            }
                        }
                    }
                    PROTO_TCP => {
                        if let Some(t) = parse_tcp(ip.payload) {
                            self.handle_tcp(frame, &ip, &t);
                        }
                    }
                    PROTO_ICMP if ip.dst == GATEWAY => self.pipe.rx.push_back(frame.to_vec()),
                    PROTO_ICMP => self.handle_ping(frame, &ip),
                    _ => {}
                }
            }
            _ => {} // IPv6 etc: not offered on the NAT network.
        }
        self.poll_stack();
    }

    fn recv_frame(&mut self) -> Option<Vec<u8>> {
        self.to_guest.pop_front()
    }

    fn register(&mut self, reg: &Registry) -> io::Result<()> {
        self.registry = Some(reg.try_clone()?);
        for i in 0..self.listeners.len() {
            let t = self.alloc_token(Owner::Listener(i));
            reg.register(&mut self.listeners[i].0, Token(t), Interest::READABLE)?;
        }
        // Flows that outlived a device reset (the guest re-activated the
        // NIC): back on the new poller under their old tokens.
        for (&t, c) in self.tcp.iter_mut() {
            let _ = reg.register(&mut c.stream, Token(t), Interest::READABLE | Interest::WRITABLE);
        }
        for (t, f) in self.udp.values_mut() {
            let _ = reg.register(&mut f.sock, Token(*t), Interest::READABLE);
        }
        for (&t, p) in self.pings.iter_mut() {
            let _ = reg.register(&mut p.sock, Token(t), Interest::READABLE);
        }
        Ok(())
    }

    fn deregister(&mut self, reg: &Registry) {
        for (l, _) in self.listeners.iter_mut() {
            let _ = reg.deregister(l);
        }
        self.owners.retain(|_, o| !matches!(o, Owner::Listener(_)));
        for c in self.tcp.values_mut() {
            let _ = reg.deregister(&mut c.stream);
        }
        for (_, f) in self.udp.values_mut() {
            let _ = reg.deregister(&mut f.sock);
        }
        for p in self.pings.values_mut() {
            let _ = reg.deregister(&mut p.sock);
        }
        self.registry = None;
    }

    fn ready(&mut self, token: usize) {
        if token == crate::RESUME_TOKEN {
            for c in self.tcp.values_mut() {
                c.host_readable = true;
            }
            let keys: Vec<FlowKey> = self.udp.keys().copied().collect();
            for k in keys {
                self.udp_readable(k);
            }
            let tokens: Vec<_> = self.pings.keys().copied().collect();
            for t in tokens {
                self.ping_readable(t);
            }
            for i in 0..self.listeners.len() {
                self.accept_forward(i);
            }
        } else {
            match self.owners.get(&token) {
                Some(Owner::Tcp) => self.tcp_event(token),
                Some(Owner::Ping) => self.ping_readable(token),
                Some(Owner::Udp(k)) => {
                    let k = *k;
                    self.udp_readable(k)
                }
                Some(Owner::Listener(i)) => {
                    let i = *i;
                    self.accept_forward(i)
                }
                None => {}
            }
        }
        self.poll_stack();
    }

    fn poll_delay(&mut self) -> Option<Duration> {
        let now = smol_now(self.epoch);
        let d = self.iface.poll_delay(now, &self.sockets).map(|d| Duration::from_micros(d.total_micros()));
        let cap = Duration::from_secs(1);
        Some(d.map_or(cap, |d| d.min(cap)))
    }

    fn tick(&mut self) {
        self.fragments.expire();
        self.expire_pings();
        self.expire_connects();
        self.expire_udp(false);
        self.poll_stack();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nat() -> Nat {
        Nat::new(NatConfig::new([2, 0, 0, 0, 0, 9])).unwrap()
    }

    #[test]
    fn policy() {
        let mut n = nat();
        assert!(n.resolve(Ipv4Addr::new(1, 1, 1, 1), 443).is_some());
        assert!(n.resolve(Ipv4Addr::LOCALHOST, 22).is_none());
        assert!(n.resolve(GATEWAY, 22).is_none());
        assert!(n.resolve(Ipv4Addr::new(10, 0, 2, 99), 22).is_none());
        assert!(n.resolve(Ipv4Addr::new(169, 254, 169, 254), 80).is_none());
        assert!(n.resolve(Ipv4Addr::new(224, 0, 0, 1), 80).is_none());
        assert!(n.resolve(DNS, 80).is_none());
        assert!(n.resolve(Ipv4Addr::new(192, 168, 1, 1), 80).is_some());
        n.cfg.policy.block_private = true;
        assert!(n.resolve(Ipv4Addr::new(192, 168, 1, 1), 80).is_none());
        n.cfg.policy.allow_host_loopback = true;
        assert_eq!(n.resolve(GATEWAY, 22), Some("127.0.0.1:22".parse().unwrap()));
    }

    #[test]
    fn dhcp_and_arp() {
        let mut n = nat();
        // DHCP discover from the guest.
        let mut d = vec![0u8; 240];
        d[0] = 1;
        d[1] = 1;
        d[2] = 6;
        d[236..240].copy_from_slice(&[99, 130, 83, 99]);
        d.extend_from_slice(&[53, 1, 1, 255]);
        let f = build_udp([0xff; 6], [2, 0, 0, 0, 0, 9], (Ipv4Addr::UNSPECIFIED, 68), (Ipv4Addr::BROADCAST, 67), &d);
        n.send_frame(&f);
        let r = n.recv_frame().unwrap();
        let ip = parse_ipv4(&r).unwrap();
        let u = parse_udp(ip.payload).unwrap();
        assert_eq!(u.dst_port, 68);
        assert_eq!(&u.payload[16..20], &GUEST.octets());

        // ARP for gateway: answered. ARP for a random IP (guest DAD): ignored.
        let arp = |target: Ipv4Addr| {
            let mut f = vec![0xff; 6];
            f.extend_from_slice(&[2, 0, 0, 0, 0, 9]);
            f.extend_from_slice(&[0x08, 0x06, 0, 1, 8, 0, 6, 4, 0, 1, 2, 0, 0, 0, 0, 9, 10, 0, 2, 15]);
            f.extend_from_slice(&[0; 6]);
            f.extend_from_slice(&target.octets());
            f
        };
        n.send_frame(&arp(GATEWAY));
        let r = n.recv_frame().expect("arp reply for gateway");
        assert_eq!(parse_arp(&r).unwrap().op, 2);
        n.send_frame(&arp(Ipv4Addr::new(10, 0, 2, 15)));
        assert!(n.recv_frame().is_none());
        n.send_frame(&arp(DNS));
        assert_eq!(parse_arp(&n.recv_frame().unwrap()).unwrap().sender_ip, DNS);
    }

    #[test]
    fn fuzz_garbage_frames() {
        let mut n = nat();
        let mut x: u32 = 12345;
        for len in 0..600 {
            let mut f = vec![0u8; len];
            for b in f.iter_mut() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *b = x as u8;
            }
            if len > 14 {
                f[12] = 0x08;
                f[13] = if len % 2 == 0 { 0x00 } else { 0x06 };
            }
            n.send_frame(&f);
        }
    }
}

#[cfg(test)]
mod live {
    use super::*;
    use mio::{Events, Poll};

    /// Needs network: `cargo test -p bulwark-net live -- --ignored`
    #[test]
    #[ignore]
    fn first_dns_query_answers() {
        let mut n = Nat::new(NatConfig::new([2, 0, 0, 0, 0, 9])).unwrap();
        let mut poll = Poll::new().unwrap();
        n.register(poll.registry()).unwrap();
        // DNS query for example.com A, id 0x1234.
        let mut q = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for l in ["example", "com"] {
            q.push(l.len() as u8);
            q.extend_from_slice(l.as_bytes());
        }
        q.extend_from_slice(&[0, 0, 1, 0, 1]);
        let f = build_udp(GATEWAY_MAC, [2, 0, 0, 0, 0, 9], (GUEST, 5555), (DNS, 53), &q);
        let t0 = Instant::now();
        n.send_frame(&f);
        let mut events = Events::with_capacity(8);
        while t0.elapsed() < Duration::from_secs(3) {
            poll.poll(&mut events, Some(Duration::from_millis(50))).unwrap();
            for e in events.iter() {
                n.ready(e.token().0);
            }
            if let Some(r) = n.recv_frame() {
                let ip = parse_ipv4(&r).unwrap();
                let u = parse_udp(ip.payload).unwrap();
                assert_eq!(&u.payload[..2], &[0x12, 0x34]);
                eprintln!("answer after {:?}", t0.elapsed());
                return;
            }
        }
        panic!("no DNS answer in 3s");
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod integration_tests;
