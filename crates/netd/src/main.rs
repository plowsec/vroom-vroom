//! bulwark-netd: privileged network helper.
//!
//! Runs as root, owns the real network attachments (vmnet on macOS,
//! tap+bridge on Linux) and hands each sandboxed VMM a socket that carries
//! Ethernet frames. The VMM never gets privileges or interface handles.
//!
//! Hardening: strict request parsing, admin allow-lists, socket group
//! access control, per-user port limits, and source-MAC anti-spoofing on
//! every guest frame.

mod port;

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bulwark_net::netd::{read_line, Attach, Request, Response, VERSION};
use port::Port;
use serde::Deserialize;

const MAX_PORTS_PER_USER: usize = 32;
const MAX_FRAME: usize = 65536;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostOnly {
    name: String,
    /// e.g. "192.168.77.1/24" (Linux: bridge address; vmnet picks its own).
    #[serde(default = "default_gateway")]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    gateway: String,
}

fn default_gateway() -> String {
    "192.168.77.1/24".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_socket")]
    socket: PathBuf,
    /// Unix group allowed to connect.
    #[serde(default = "default_group")]
    group: String,
    /// Host interfaces VMs may bridge onto (Linux: bridge names).
    #[serde(default)]
    bridge_interfaces: Vec<String>,
    #[serde(default)]
    host_only: Vec<HostOnly>,
}

fn default_socket() -> PathBuf {
    bulwark_net::netd::DEFAULT_SOCKET.into()
}

fn default_group() -> String {
    if cfg!(target_os = "macos") { "staff" } else { "bulwark" }.into()
}

fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

struct State {
    cfg: Config,
    /// uid -> open ports
    per_user: Mutex<HashMap<u32, usize>>,
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

fn reply(s: &mut UnixStream, r: &Response) {
    if let Ok(mut l) = serde_json::to_string(r) {
        l.push('\n');
        let _ = s.write_all(l.as_bytes());
    }
}

fn refuse(s: &mut UnixStream, msg: &str) {
    log::warn!("refused: {msg}");
    reply(s, &Response { ok: false, error: msg.into(), mac: [0; 6], mtu: 0 });
}

fn open_port(st: &State, req: &Request) -> Result<Arc<dyn Port>, String> {
    let id = port::uuid_of(&format!("{}/{}", req.vm, req.nic));
    match &req.attach {
        Attach::Bridged { interface } => {
            if !st.cfg.bridge_interfaces.contains(interface) {
                return Err(format!("bridging onto {interface} is not allowed (netd.toml bridge_interfaces)"));
            }
            #[cfg(target_os = "macos")]
            {
                let _ = &req.mac;
                port::vmnet::Vmnet::start(Some(interface), id, None).map(|p| Arc::new(p) as Arc<dyn Port>).map_err(|e| e.to_string())
            }
            #[cfg(target_os = "linux")]
            {
                let _ = id;
                port::tap::Tap::open(interface, req.mac).map(|p| Arc::new(p) as Arc<dyn Port>).map_err(|e| e.to_string())
            }
        }
        Attach::HostOnly { network } => {
            let Some(net) = st.cfg.host_only.iter().find(|n| &n.name == network) else {
                return Err(format!("unknown host-only network {network}"));
            };
            #[cfg(target_os = "macos")]
            {
                let _ = net;
                let netid = port::uuid_of(&format!("hostonly/{network}"));
                port::vmnet::Vmnet::start(None, id, Some(netid)).map(|p| Arc::new(p) as Arc<dyn Port>).map_err(|e| e.to_string())
            }
            #[cfg(target_os = "linux")]
            {
                let _ = id;
                let br = format!("bwh-{}", &network[..network.len().min(10)]);
                port::tap::ensure_bridge(&br, &net.gateway).map_err(|e| e.to_string())?;
                port::tap::Tap::open(&br, req.mac).map(|p| Arc::new(p) as Arc<dyn Port>).map_err(|e| e.to_string())
            }
        }
    }
}

/// Shuttle frames until the VMM goes away.
fn pump(guest: UnixDatagram, port: Arc<dyn Port>) {
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
                    if (dst == mac || dst[0] & 1 == 1) && g2.send(&buf[..n]).is_err() {
                        break;
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
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => return refuse(&mut s, &format!("bad request: {e}")),
    };
    if req.version != VERSION || !valid_name(&req.vm) || req.nic >= 16 || req.mac[0] & 1 != 0 {
        return refuse(&mut s, "invalid request");
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
    let port = match open_port(&st, &req) {
        Ok(p) => p,
        Err(e) => {
            release(&st);
            return refuse(&mut s, &e);
        }
    };
    let (vmm_end, our_end) = match UnixDatagram::pair() {
        Ok(p) => p,
        Err(e) => {
            release(&st);
            return refuse(&mut s, &e.to_string());
        }
    };
    log::info!("uid {uid}: {} nic{} {:?} mac {:02x?}", req.vm, req.nic, req.attach, port.mac());
    reply(&mut s, &Response { ok: true, error: String::new(), mac: port.mac(), mtu: port.mtu() });
    if bulwark_ipc::send_fd(&s, vmm_end.as_raw_fd()).is_err() {
        release(&st);
        return;
    }
    drop(vmm_end);
    pump(our_end, port);
    log::info!("uid {uid}: {} nic{} detached", req.vm, req.nic);
    release(&st);
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
    let path = std::env::args().nth(1).unwrap_or_else(|| "/etc/bulwark/netd.toml".into());
    let cfg: Config = match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|s| toml::from_str(&s).map_err(|e| e.to_string())) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bulwark-netd: {path}: {e}");
            std::process::exit(2);
        }
    };
    if cfg.host_only.iter().any(|n| !valid_name(&n.name)) || cfg.bridge_interfaces.iter().any(|i| !valid_name(i)) {
        eprintln!("bulwark-netd: invalid names in config");
        std::process::exit(2);
    }
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        log::warn!("not running as root: vmnet / tap will fail");
    }
    if let Some(dir) = cfg.socket.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(&cfg.socket);
    let listener = match UnixListener::bind(&cfg.socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("bulwark-netd: bind {}: {e}", cfg.socket.display());
            std::process::exit(1);
        }
    };
    let _ = std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o660));
    if let (Some(gid), Ok(c)) = (group_gid(&cfg.group), std::ffi::CString::new(cfg.socket.as_os_str().as_encoded_bytes())) {
        // SAFETY: valid path; chown to root:group.
        unsafe { libc::chown(c.as_ptr(), 0, gid) };
    }
    log::info!("listening on {} (group {})", cfg.socket.display(), cfg.group);
    let st = Arc::new(State { cfg, per_user: Mutex::new(HashMap::new()) });
    for conn in listener.incoming() {
        match conn {
            Ok(s) => {
                let st = st.clone();
                std::thread::spawn(move || handle(st, s));
            }
            Err(e) => log::warn!("accept: {e}"),
        }
    }
}
