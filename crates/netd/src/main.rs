//! vroom-netd: vroom's one privileged helper.
//!
//! Runs as root and does two things for unprivileged vroom processes:
//!
//! - networking: owns the real network attachments (vmnet on macOS,
//!   tap+bridge on Linux) and hands each sandboxed VMM a socket that
//!   carries Ethernet frames. The VMM never gets privileges or interface
//!   handles.
//! - USB devices a macOS driver holds: starts the USB helper as root for
//!   one device the app names, and hands back its socket (`usb.rs`).
//!
//! Hardening: strict request parsing, admin allow-lists, socket group
//! access control, per-user limits, and source-MAC anti-spoofing on every
//! guest frame.

mod dhcp;
mod hostonly;
mod port;
mod usb;

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hostonly::{HostOnly, Subnet};
use port::Port;
use serde::Deserialize;
use vroom_net::netd::{read_line, Attach, HostOnlyNet, Hello, PortInfo, Request, Response, Status, UsbRequest, VERSION};

const MAX_PORTS_PER_USER: usize = 32;
/// Connections handled at once (each holds a thread): a local user can't
/// starve the daemon by opening sockets and saying nothing.
const MAX_CONNS: usize = 64;
const MAX_FRAME: usize = 65536;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsbConfig {
    /// Serve USB devices a host driver holds (off unless set).
    #[serde(default)]
    enabled: bool,
    /// Who may ask. macOS: `admin`, the users who could use sudo anyway.
    #[serde(default = "default_usb_group")]
    group: String,
}

impl Default for UsbConfig {
    fn default() -> Self {
        UsbConfig { enabled: false, group: default_usb_group() }
    }
}

fn default_usb_group() -> String {
    if cfg!(target_os = "macos") { "admin" } else { "vroom" }.into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_socket")]
    socket: PathBuf,
    /// Unix group allowed to connect.
    #[serde(default = "default_group")]
    group: String,
    /// Host interfaces VMs may bridge onto (Linux: bridge names). A name
    /// ending in `*` stands for all that start that way (`en*`).
    #[serde(default)]
    bridge_interfaces: Vec<String>,
    /// None in the file: one, `lab` on 192.168.77.1/24.
    #[serde(default = "hostonly::defaults")]
    host_only: Vec<HostOnly>,
    #[serde(default)]
    usb: UsbConfig,
}

fn default_socket() -> PathBuf {
    vroom_net::netd::DEFAULT_SOCKET.into()
}

fn default_group() -> String {
    if cfg!(target_os = "macos") { "staff" } else { "vroom" }.into()
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// `pattern` is a name, or a prefix followed by `*`.
fn iface_allowed(patterns: &[String], iface: &str) -> bool {
    valid_name(iface)
        && patterns.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => iface.starts_with(prefix) && iface.len() > prefix.len(),
            None => p == iface,
        })
}

struct State {
    cfg: Config,
    /// `cfg.host_only`'s subnets, in order.
    subnets: Vec<Subnet>,
    /// Their DHCP leases, where netd serves them (Linux).
    pools: Vec<Mutex<dhcp::Pool>>,
    /// uid -> open ports
    per_user: Mutex<HashMap<u32, usize>>,
    /// Open ports by id, with their user: what `status` lists.
    ports: Mutex<HashMap<u64, (u32, PortInfo)>>,
    next_port: AtomicU64,
    usb: usb::Usb,
}

impl State {
    fn new(cfg: Config, usb: usb::Usb) -> Result<State, String> {
        let subnets = hostonly::check(&cfg.host_only)?;
        let pools = subnets.iter().map(|s| Mutex::new(dhcp::Pool::new(*s))).collect();
        Ok(State { cfg, subnets, pools, per_user: Mutex::new(HashMap::new()), ports: Mutex::new(HashMap::new()), next_port: AtomicU64::new(0), usb })
    }

    /// The leased address and host name of `mac` on host-only `network`,
    /// where netd serves DHCP.
    fn lease(&self, network: &str, mac: &[u8; 6]) -> Option<(std::net::Ipv4Addr, String)> {
        let i = self.cfg.host_only.iter().position(|n| n.name == network)?;
        self.pools[i].lock().unwrap_or_else(|e| e.into_inner()).of(mac)
    }
}

fn peer_uid(s: &UnixStream) -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: valid fd, out pointers.
        (unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
    }
    #[cfg(target_os = "linux")]
    {
        let mut c = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: valid fd, correctly sized out buffer.
        let r = unsafe {
            libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut c as *mut _ as *mut _, &mut len)
        };
        (r == 0).then_some(c.uid)
    }
}

fn reply_json(s: &mut UnixStream, r: &impl serde::Serialize) {
    if let Ok(mut l) = serde_json::to_string(r) {
        l.push('\n');
        let _ = s.write_all(l.as_bytes());
    }
}

fn reply(s: &mut UnixStream, r: &Response) {
    reply_json(s, r)
}

fn refuse(s: &mut UnixStream, msg: &str) {
    log::warn!("refused: {msg}");
    reply(s, &Response { ok: false, error: msg.into(), mac: [0; 6], mtu: 0 });
}

/// netd answering a port's DHCP requests: the network's pool, and the
/// host's MAC on it (the replies' source).
struct Dhcp<'a> {
    pool: &'a Mutex<dhcp::Pool>,
    server: [u8; 6],
}

fn open_port<'a>(st: &'a State, req: &Request) -> Result<(Arc<dyn Port>, Option<Dhcp<'a>>), String> {
    let id = port::uuid_of(&format!("{}/{}", req.vm, req.nic));
    match &req.attach {
        Attach::Bridged { interface } => {
            if !iface_allowed(&st.cfg.bridge_interfaces, interface) {
                return Err(format!("bridging onto {interface} is not allowed (netd.toml bridge_interfaces)"));
            }
            #[cfg(target_os = "macos")]
            {
                let _ = &req.mac;
                port::vmnet::Vmnet::bridged(interface, id).map(|p| (Arc::new(p) as Arc<dyn Port>, None)).map_err(|e| e.to_string())
            }
            #[cfg(target_os = "linux")]
            {
                let _ = id;
                port::tap::Tap::open(interface, req.mac).map(|p| (Arc::new(p) as Arc<dyn Port>, None)).map_err(|e| e.to_string())
            }
        }
        Attach::HostOnly { network } => {
            let Some(i) = st.cfg.host_only.iter().position(|n| &n.name == network) else {
                let have: Vec<&str> = st.cfg.host_only.iter().map(|n| n.name.as_str()).collect();
                return Err(format!("no host-only network {network} in netd.toml (it has: {})", if have.is_empty() { "none".into() } else { have.join(", ") }));
            };
            #[cfg(target_os = "macos")]
            {
                // vmnet's bootpd serves DHCP.
                port::vmnet::Vmnet::host_only(&st.subnets[i], id).map(|p| (Arc::new(p) as Arc<dyn Port>, None)).map_err(|e| e.to_string())
            }
            #[cfg(target_os = "linux")]
            {
                let _ = id;
                let net = &st.cfg.host_only[i];
                let br = format!("bwh-{}", &network[..network.len().min(10)]);
                port::tap::ensure_bridge(&br, &net.gateway).map_err(|e| e.to_string())?;
                // Nothing else serves DHCP on the bridge: we do.
                let server = port::tap::mac_of(&br).unwrap_or(port::FALLBACK_SERVER_MAC);
                let tap = port::tap::Tap::open(&br, req.mac).map_err(|e| e.to_string())?;
                Ok((Arc::new(tap) as Arc<dyn Port>, Some(Dhcp { pool: &st.pools[i], server })))
            }
        }
    }
}

/// A send that failed for room, not because the VMM is gone (macOS:
/// ENOBUFS once the other end's buffer is full, whatever its blocking).
fn buffer_full(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(libc::ENOBUFS)
}

/// Shuttle frames until the VMM goes away. With `dhcp`, the guest's DHCP
/// requests are answered here and go no further.
fn pump(guest: UnixDatagram, port: Arc<dyn Port>, dhcp: Option<Dhcp<'_>>) {
    let mac = port.mac();
    let stop = Arc::new(AtomicBool::new(false));
    let g2 = match guest.try_clone() {
        Ok(g) => g,
        Err(_) => return,
    };
    let (p2, s2) = (port.clone(), stop.clone());
    let down = std::thread::spawn(move || {
        let mut buf = vec![0u8; MAX_FRAME];
        while !s2.load(Ordering::Relaxed) {
            match p2.recv(&mut buf, Duration::from_millis(500)) {
                Ok(0) => {}
                Ok(n) if n >= 14 => {
                    // Only frames for this guest (or broadcast/multicast).
                    let dst = &buf[0..6];
                    if dst != mac && dst[0] & 1 == 0 {
                        continue;
                    }
                    match g2.send(&buf[..n]) {
                        Ok(_) => {}
                        // The VMM is behind (or its NIC isn't up yet):
                        // drop the frame, as a full NIC queue would.
                        Err(e) if buffer_full(&e) => {}
                        Err(_) => break,
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        s2.store(true, Ordering::Relaxed);
    });
    let _ = guest.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf = vec![0u8; MAX_FRAME];
    while !stop.load(Ordering::Relaxed) {
        match guest.recv(&mut buf) {
            Ok(0) => break,
            Ok(n) if n >= 14 && n <= 14 + port.mtu() as usize + 4 => {
                // Anti-spoofing: a VM may only send as itself.
                if buf[6..12] != mac {
                    continue;
                }
                if let Some(d) = &dhcp {
                    if dhcp::request(&buf[..n]).is_some() {
                        let r = d.pool.lock().unwrap_or_else(|e| e.into_inner()).answer(&buf[..n], d.server, Instant::now());
                        if let Some(r) = r {
                            let _ = guest.send(&r);
                        }
                        continue;
                    }
                }
                let _ = port.send(&buf[..n]);
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = down.join();
}

fn handle(st: Arc<State>, mut s: UnixStream) {
    let uid = peer_uid(&s).unwrap_or(u32::MAX);
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let line = match s.try_clone().map(BufReader::new).and_then(|mut r| read_line(&mut r)) {
        Ok(l) => l,
        Err(_) => return,
    };
    match serde_json::from_str::<Hello>(&line) {
        Ok(Hello::Net(req)) => net(&st, s, uid, req),
        Ok(Hello::Usb(req)) => usb_request(&st, s, uid, &req),
        Ok(Hello::Status(r)) if r.version == VERSION && r.status => status(&st, s, uid),
        Ok(Hello::Status(_)) => refuse(&mut s, "invalid request"),
        Err(e) => refuse(&mut s, &format!("bad request: {e}")),
    }
}

fn status(st: &State, mut s: UnixStream, uid: u32) {
    let why = st.usb.may(uid).err();
    let r = Status {
        ok: true,
        version: VERSION,
        bridge_interfaces: st.cfg.bridge_interfaces.clone(),
        host_only: st.cfg.host_only.iter().map(|h| h.name.clone()).collect(),
        networks: st.cfg.host_only.iter().map(|h| HostOnlyNet { name: h.name.clone(), gateway: h.gateway.clone() }).collect(),
        host_only_dhcp: true,
        ports: {
            let ports = st.ports.lock().unwrap_or_else(|e| e.into_inner());
            let mut mine: Vec<PortInfo> = ports.values().filter(|(u, _)| *u == uid).map(|(_, p)| p.clone()).collect();
            for p in &mut mine {
                if let Attach::HostOnly { network } = &p.attach {
                    if let Some((ip, name)) = st.lease(network, &p.mac) {
                        (p.ip, p.host_name) = (Some(ip), name);
                    }
                }
            }
            mine.sort_by(|a, b| (&a.vm, a.nic).cmp(&(&b.vm, b.nic)));
            mine
        },
        usb: why.is_none(),
        usb_why: why.unwrap_or_default(),
    };
    reply_json(&mut s, &r);
}

fn usb_request(st: &Arc<State>, mut s: UnixStream, uid: u32, req: &UsbRequest) {
    let g = match st.usb.grant(uid, req) {
        Ok(g) => g,
        Err(e) => {
            log::warn!("uid {uid}: usb {} for {}: {e}", req.usb.id, req.vm);
            return refuse(&mut s, &e);
        }
    };
    log::info!("uid {uid}: usb {} for {}", req.usb.id, req.vm);
    let usb::Grant { sock, mut helper, dev, uid } = g;
    reply(&mut s, &Response { ok: true, error: String::new(), mac: [0; 6], mtu: 0 });
    if let Err(e) = vroom_ipc::send_fd(&s, sock.as_raw_fd()) {
        log::warn!("uid {uid}: usb: {e}");
    }
    // Ours goes once the caller has its copy (it hangs up then): macOS
    // can disconnect a socket whose last other copy closes while it is
    // still in flight. From then on only the caller's copy keeps the
    // helper going.
    let _ = std::io::Read::read(&mut s, &mut [0u8; 1]);
    drop(sock);
    drop(s);
    let st = st.clone();
    std::thread::spawn(move || {
        helper.wait();
        log::info!("uid {uid}: usb {:04x}:{:04x} helper ended", dev.0, dev.1);
        st.usb.release(uid, &dev);
    });
}

fn net(st: &State, mut s: UnixStream, uid: u32, req: Request) {
    if req.version != VERSION || !valid_name(&req.vm) || req.nic >= 16 {
        return refuse(&mut s, "invalid request");
    }
    // Unicast and locally administered only: a VM (or a compromised VMM)
    // can't take the MAC of a real device on the LAN, e.g. the router's.
    if req.mac[0] & 0x03 != 0x02 {
        return refuse(&mut s, "mac must be a locally administered unicast address (x2:..., x6:..., xA:..., xE:...)");
    }
    {
        let mut pu = st.per_user.lock().unwrap_or_else(|e| e.into_inner());
        let n = pu.entry(uid).or_default();
        if *n >= MAX_PORTS_PER_USER {
            return refuse(&mut s, "too many ports for this user");
        }
        *n += 1;
    }
    let release = |st: &State| {
        let mut pu = st.per_user.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = pu.get_mut(&uid) {
            *n = n.saturating_sub(1);
        }
    };
    let (port, dhcp) = match open_port(st, &req) {
        Ok(p) => p,
        Err(e) => {
            release(st);
            return refuse(&mut s, &e);
        }
    };
    let (vmm_end, our_end) = match UnixDatagram::pair() {
        Ok(p) => p,
        Err(e) => {
            release(st);
            return refuse(&mut s, &e.to_string());
        }
    };
    // Room for bursts both ways (macOS: 4 KiB, two frames, by default).
    vroom_net::socket::grow_buffers(&vmm_end);
    vroom_net::socket::grow_buffers(&our_end);
    log::info!("uid {uid}: {} nic{} {:?} mac {:02x?}", req.vm, req.nic, req.attach, port.mac());
    reply(&mut s, &Response { ok: true, error: String::new(), mac: port.mac(), mtu: port.mtu() });
    if vroom_ipc::send_fd(&s, vmm_end.as_raw_fd()).is_err() {
        release(st);
        return;
    }
    // Not before the VMM has its copy (it hangs up then): see usb_request.
    let _ = std::io::Read::read(&mut s, &mut [0u8; 1]);
    drop(vmm_end);
    drop(s);
    let pid = st.next_port.fetch_add(1, Ordering::Relaxed);
    let info = PortInfo { vm: req.vm.clone(), nic: req.nic, attach: req.attach.clone(), mac: port.mac(), ip: None, host_name: String::new() };
    st.ports.lock().unwrap_or_else(|e| e.into_inner()).insert(pid, (uid, info));
    pump(our_end, port, dhcp);
    st.ports.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
    log::info!("uid {uid}: {} nic{} detached", req.vm, req.nic);
    release(st);
}

fn group_gid(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: valid C string; getgrnam returns static storage or NULL.
    let g = unsafe { libc::getgrnam(c.as_ptr()) };
    // SAFETY: non-null pointer from getgrnam.
    (!g.is_null()).then(|| unsafe { (*g).gr_gid })
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("usb-host") {
        if let Err(e) = usb::host_main(&args[2..]) {
            eprintln!("vroom-netd usb-host: {e}");
            std::process::exit(1);
        }
        return;
    }
    let path = args.get(1).cloned().unwrap_or_else(|| "/etc/vroom/netd.toml".into());
    let cfg: Config = match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|s| toml::from_str(&s).map_err(|e| e.to_string())) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vroom-netd: {path}: {e}");
            std::process::exit(2);
        }
    };
    let pattern_ok = |p: &String| valid_name(p.strip_suffix('*').unwrap_or(p));
    if cfg.host_only.iter().any(|n| !valid_name(&n.name)) || !cfg.bridge_interfaces.iter().all(pattern_ok) || !valid_name(&cfg.usb.group) {
        eprintln!("vroom-netd: invalid names in config");
        std::process::exit(2);
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        log::warn!("not running as root: vmnet / tap and USB devices will fail");
    }
    if let Some(dir) = cfg.socket.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(&cfg.socket);
    let listener = match UnixListener::bind(&cfg.socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vroom-netd: bind {}: {e}", cfg.socket.display());
            std::process::exit(1);
        }
    };
    let _ = std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o660));
    if let (Some(gid), Ok(c)) = (group_gid(&cfg.group), std::ffi::CString::new(cfg.socket.as_os_str().as_encoded_bytes())) {
        // SAFETY: valid path; chown to root:group.
        unsafe { libc::chown(c.as_ptr(), 0, gid) };
    }
    log::info!("listening on {} (group {}; usb {})", cfg.socket.display(), cfg.group, if cfg.usb.enabled { &cfg.usb.group } else { "off" });
    // The USB helper is this program; libusb is our own copy next to it.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/usr/local/libexec/vroom/vroom-netd"));
    let lib = exe.with_file_name("libusb-1.0.0.dylib");
    let spawner = usb::Exec { libusb: (cfg!(target_os = "macos") && lib.exists()).then_some(lib), exe };
    let usb = usb::Usb::new(cfg.usb.enabled, cfg.usb.group.clone(), Box::new(spawner), Box::new(vroom_usb::list::list), Box::new(usb::in_group));
    let st = match State::new(cfg, usb) {
        Ok(st) => Arc::new(st),
        Err(e) => {
            eprintln!("vroom-netd: {path}: {e}");
            std::process::exit(2);
        }
    };
    for (n, s) in st.cfg.host_only.iter().zip(&st.subnets) {
        log::info!("host-only {}: host {}/{}, DHCP to {}", n.name, s.host, s.prefix, s.dhcp_end());
    }
    serve(listener, st);
}

fn serve(listener: UnixListener, st: Arc<State>) {
    let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for conn in listener.incoming() {
        match conn {
            Ok(mut s) => {
                if live.load(Ordering::Acquire) >= MAX_CONNS {
                    refuse(&mut s, "busy");
                    continue;
                }
                live.fetch_add(1, Ordering::AcqRel);
                let (st, live) = (st.clone(), live.clone());
                std::thread::spawn(move || {
                    handle(st, s);
                    live.fetch_sub(1, Ordering::AcqRel);
                });
            }
            Err(e) => log::warn!("accept: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vroom_net::netd::UsbWanted;

    /// The pretend serial port, served in-process.
    struct Fake;
    struct Thread(Option<std::thread::JoinHandle<()>>);
    impl usb::Running for Thread {
        fn wait(&mut self) {
            if let Some(t) = self.0.take() {
                let _ = t.join();
            }
        }
    }
    impl usb::Spawn for Fake {
        fn spawn(&self, dev: &usb::Device, sock: UnixStream) -> std::io::Result<Box<dyn usb::Running>> {
            let (v, p) = (dev.0, dev.1);
            let t = std::thread::spawn(move || {
                let _ = vroom_usb::helper::serve(sock, Box::new(vroom_usb::fake::FakeOpener::new(vroom_usb::fake::FakeKind::Acm, v, p, None)));
            });
            Ok(Box::new(Thread(Some(t))))
        }
    }

    fn state(usb_on: bool) -> Arc<State> {
        let cfg: Config = toml::from_str(
            r#"
bridge_interfaces = ["en*", "br0"]
[[host_only]]
name = "lab"
"#,
        )
        .unwrap();
        let lister = || {
            vec![vroom_usb::list::HostDevice {
                vendor: 0x1209,
                product: 0x000a,
                manufacturer: None,
                name: None,
                serial: None,
                class: 0,
                speed: None,
                bus: 1,
                ports: vec![2],
                interface_classes: vec![2, 10],
                claimed_by: vec!["AppleUSBCDCACMData".into()],
                captured: false,
            }]
        };
        // SAFETY: getuid has no preconditions.
        let me = unsafe { libc::getuid() };
        let usb = usb::Usb::new(usb_on, "admin".into(), Box::new(Fake), Box::new(lister), Box::new(move |uid, _| uid == me));
        Arc::new(State::new(cfg, usb).unwrap())
    }

    /// One connection to the daemon, served on a thread.
    fn connect(st: &Arc<State>) -> UnixStream {
        let (client, server) = UnixStream::pair().unwrap();
        let st = st.clone();
        std::thread::spawn(move || handle(st, server));
        client
    }

    #[test]
    fn config_defaults_keep_usb_off() {
        let c: Config = toml::from_str("").unwrap();
        assert!(!c.usb.enabled);
        assert!(toml::from_str::<Config>("[usb]\nenabled = true\nevil = 1").is_err());
    }

    #[test]
    fn host_only_out_of_the_box() {
        // No [[host_only]]: `lab`, the host on 192.168.77.1/24.
        let c: Config = toml::from_str("").unwrap();
        assert_eq!((c.host_only.len(), c.host_only[0].name.as_str(), c.host_only[0].gateway.as_str()), (1, "lab", "192.168.77.1/24"));
        // An empty list means none.
        assert!(toml::from_str::<Config>("host_only = []").unwrap().host_only.is_empty());
        // Overlapping or bad subnets keep netd from starting.
        let usb = || usb::Usb::new(false, "admin".into(), Box::new(Fake), Box::new(Vec::new), Box::new(|_, _| false));
        let two = "[[host_only]]\nname = \"a\"\n[[host_only]]\nname = \"b\"\ngateway = \"192.168.77.9/24\"\n";
        assert!(State::new(toml::from_str(two).unwrap(), usb()).is_err_and(|e| e.contains("overlap")));
        assert!(State::new(toml::from_str("[[host_only]]\nname = \"a\"\ngateway = \"1.2.3.1/24\"").unwrap(), usb()).is_err());
    }

    #[test]
    fn interface_patterns() {
        let p = vec!["en*".to_string(), "br0".into()];
        assert!(iface_allowed(&p, "en0") && iface_allowed(&p, "en12") && iface_allowed(&p, "br0"));
        assert!(!iface_allowed(&p, "en") && !iface_allowed(&p, "br01") && !iface_allowed(&p, "lo0") && !iface_allowed(&p, "en0/../x"));
    }

    #[test]
    fn status_says_what_is_offered() {
        let st = state(true);
        let s = vroom_net::netd::status_on(&connect(&st)).unwrap();
        assert_eq!((s.bridge_interfaces.len(), s.host_only.as_slice(), s.usb), (2, &["lab".to_string()][..], true));
        assert_eq!(s.networks, vec![HostOnlyNet { name: "lab".into(), gateway: "192.168.77.1/24".into() }]);
        assert!(s.host_only_dhcp);
        // The user's open ports, not others'.
        // SAFETY: getuid has no preconditions.
        let me = unsafe { libc::getuid() };
        let port = |vm: &str| PortInfo { vm: vm.into(), nic: 0, attach: Attach::HostOnly { network: "lab".into() }, mac: [2, 0, 0, 0, 0, 1], ip: None, host_name: String::new() };
        st.ports.lock().unwrap().extend([(1, (me, port("mine"))), (2, (me.wrapping_add(1), port("theirs")))]);
        assert_eq!(vroom_net::netd::status_on(&connect(&st)).unwrap().ports, vec![port("mine")]);
        // A lease netd gave it shows.
        st.pools[0].lock().unwrap().answer(&discover([2, 0, 0, 0, 0, 1], Some("deb")), [2, 0, 0, 0, 0, 0xee], Instant::now()).unwrap();
        let p = vroom_net::netd::status_on(&connect(&st)).unwrap().ports.remove(0);
        assert!(p.ip.is_some_and(|ip| st.subnets[0].contains(ip)), "{p:?}");
        st.ports.lock().unwrap().clear();
        st.ports.lock().unwrap().insert(1, (me, port("mine")));
        // A user's every port, with the longest names, still fits.
        let long = "v".repeat(64);
        st.ports.lock().unwrap().extend((10..10 + MAX_PORTS_PER_USER as u64).map(|i| (i, (me, port(&long)))));
        assert_eq!(vroom_net::netd::status_on(&connect(&st)).unwrap().ports.len(), MAX_PORTS_PER_USER + 1);
        let st = state(false);
        let s = vroom_net::netd::status_on(&connect(&st)).unwrap();
        assert!(!s.usb && s.usb_why.contains("turned off"));
    }

    /// A guest's DHCPDISCOVER frame, with a host name option if given.
    fn discover(mac: [u8; 6], name: Option<&str>) -> Vec<u8> {
        let mut b = vec![0u8; 240];
        (b[0], b[1], b[2]) = (1, 1, 6);
        b[28..34].copy_from_slice(&mac);
        b[236..240].copy_from_slice(&[99, 130, 83, 99]);
        b.extend_from_slice(&[53, 1, 1]);
        if let Some(n) = name {
            b.extend_from_slice(&[12, n.len() as u8]);
            b.extend_from_slice(n.as_bytes());
        }
        b.push(255);
        vroom_net::packet::build_udp([0xff; 6], mac, (std::net::Ipv4Addr::UNSPECIFIED, 68), (std::net::Ipv4Addr::BROADCAST, 67), &b)
    }

    /// A port that keeps what it is sent and has nothing to give.
    struct Sink([u8; 6], Mutex<Vec<Vec<u8>>>);
    impl Port for Sink {
        fn mac(&self) -> [u8; 6] {
            self.0
        }
        fn mtu(&self) -> u16 {
            1500
        }
        fn send(&self, f: &[u8]) -> std::io::Result<()> {
            self.1.lock().unwrap().push(f.to_vec());
            Ok(())
        }
        fn recv(&self, _: &mut [u8], t: Duration) -> std::io::Result<usize> {
            std::thread::sleep(t.min(Duration::from_millis(20)));
            Ok(0)
        }
    }

    #[test]
    fn pump_answers_dhcp_and_passes_the_rest() {
        let st = state(false);
        let mac = [2, 0, 0, 0, 0, 7];
        let port = Arc::new(Sink(mac, Mutex::new(Vec::new())));
        let (vmm, ours) = UnixDatagram::pair().unwrap();
        vmm.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        std::thread::scope(|s| {
            let p = port.clone() as Arc<dyn Port>;
            let st = &st;
            s.spawn(move || pump(ours, p, Some(Dhcp { pool: &st.pools[0], server: [2, 0, 0, 0, 0, 0xee] })));
            vmm.send(&discover(mac, None)).unwrap();
            let mut buf = [0u8; 2048];
            let n = vmm.recv(&mut buf).unwrap();
            let ip = vroom_net::packet::parse_ipv4(&buf[..n]).unwrap();
            assert_eq!(ip.src, st.subnets[0].host);
            // Not DHCP: on to the bridge. Another MAC's: dropped.
            let other = vroom_net::packet::build_udp([0xff; 6], mac, (ip.dst, 5000), (ip.dst, 5001), b"hi");
            vmm.send(&other).unwrap();
            vmm.send(&discover([2, 0, 0, 0, 0, 8], None)).unwrap();
            let t0 = Instant::now();
            while port.1.lock().unwrap().is_empty() && t0.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(vmm);
        });
        assert_eq!(*port.1.lock().unwrap(), vec![vroom_net::packet::build_udp([0xff; 6], mac, (std::net::Ipv4Addr::BROADCAST, 5000), (std::net::Ipv4Addr::BROADCAST, 5001), b"hi")]);
        assert!(st.pools[0].lock().unwrap().of(&mac).is_some());
    }

    #[test]
    fn usb_device_comes_back_as_a_helper_socket() {
        let st = state(true);
        let req = vroom_net::netd::UsbRequest { version: VERSION, vm: "dev".into(), usb: UsbWanted { id: "1209:000a".into(), serial: None } };
        let sock = vroom_net::netd::usb_on(&connect(&st), &req).unwrap();
        let mut r = std::io::BufReader::new(sock.try_clone().unwrap());
        let ev = vroom_usb::proto::read_event(&mut r).unwrap().unwrap();
        assert!(matches!(ev, vroom_usb::proto::Event::Attached { vendor: 0x1209, product: 0x000a, .. }), "{ev:?}");
        // A second one for the same device is refused while this lives.
        let e = vroom_net::netd::usb_on(&connect(&st), &req).unwrap_err();
        assert!(e.to_string().contains("in use"), "{e}");
        // Anything not quite right is refused, with a reason.
        let bad = vroom_net::netd::UsbRequest { usb: UsbWanted { id: "1209:000b".into(), serial: None }, ..req.clone() };
        assert!(vroom_net::netd::usb_on(&connect(&st), &bad).unwrap_err().to_string().contains("plugged in"));
        let mut raw = connect(&st);
        raw.write_all(b"{\"version\":1,\"vm\":\"dev\",\"usb\":{\"id\":\"1209:000a\",\"argv\":[\"-x\"]}}\n").unwrap();
        let mut l = String::new();
        std::io::Read::read_to_string(&mut raw, &mut l).unwrap();
        assert!(l.contains("\"ok\":false"), "{l}");
        // The VM is done with it: the device is free again.
        drop(r);
        sock.shutdown(std::net::Shutdown::Both).unwrap();
        drop(sock);
        let t0 = std::time::Instant::now();
        loop {
            match vroom_net::netd::usb_on(&connect(&st), &req) {
                Ok(_) => break,
                Err(e) if t0.elapsed() < Duration::from_secs(5) => {
                    assert!(e.to_string().contains("in use"), "{e}");
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => panic!("{e}"),
            }
        }
    }

    #[test]
    fn usb_refused_when_off() {
        let st = state(false);
        let req = vroom_net::netd::UsbRequest { version: VERSION, vm: "dev".into(), usb: UsbWanted { id: "1209:000a".into(), serial: None } };
        assert!(vroom_net::netd::usb_on(&connect(&st), &req).unwrap_err().to_string().contains("turned off"));
    }
}
