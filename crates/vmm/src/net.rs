//! Network adapters: their backends at start, and changes while the VM
//! runs. The VM window plugs an adapter's cable in or out
//! (`ToVmm::NetConnect`) and puts it on another network (`ToVmm::NetSet`):
//! same virtio-net device, same MAC, another backend behind it.
//!
//! The VMM is sandboxed by then: it can't reach vroom-netd (no Unix
//! sockets), so for bridged and host-only the app asks netd and hands us
//! the frame socket, as it hands us USB helpers. NAT is ours to make, if
//! the sandbox allows the connections it makes (a NAT adapter at start).
//! Port forwards need listening sockets, which the sandbox can't open
//! either: a NAT made later has none, and the one an adapter started with
//! is kept while the adapter is on another network, for when it comes
//! back.

use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;
use std::sync::{Arc, Mutex, MutexGuard};

use vroom_config::{Identity, NetKind as ConfigKind, NetMode, VmConfig};
use vroom_display::{Forward, NetKind, NetLive, NetPort, ToViewer};
use vroom_net::nat::{Nat, NatConfig, NatPolicy, TcpForward};
use vroom_net::socket::FrameSocket;
use vroom_net::{netd, NetBackend};
use vroom_virtio::devices::net::NetControl;

use crate::display::ViewerTx;
use crate::{Error, Result};

/// One adapter to build.
pub(crate) struct Planned {
    pub net: vroom_config::Net,
    pub mac: [u8; 6],
    /// Taken out of the VM file while the VM was suspended: it stays (the
    /// guest has it) with its cable out, until the next start.
    pub removed: bool,
}

/// The adapters to build. A resumed VM gets the ones it was saved with,
/// set up as the file says now: an adapter added meanwhile comes at the
/// next start, one removed stays unplugged until then.
pub(crate) fn plan(config: &VmConfig, saved: Option<&VmConfig>) -> Result<Vec<Planned>> {
    let n = saved.map_or(config.nets.len(), |s| s.nets.len());
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let p = match (config.nets.get(i), saved) {
            (Some(net), _) => Planned { net: net.clone(), mac: net.mac_of(&config.identity, &config.name, i)?, removed: false },
            (None, Some(s)) => {
                let net = s.nets[i].clone();
                log::info!("net: adapter {} left the VM file while suspended: unplugged until the VM restarts", i + 1);
                Planned { mac: net.mac_of(&s.identity, &s.name, i)?, net, removed: true }
            }
            (None, None) => unreachable!("n is the file's count"),
        };
        out.push(p);
    }
    if let Some(s) = saved {
        if config.nets.len() > s.nets.len() {
            log::info!("net: {} adapter(s) added while suspended come at the next start", config.nets.len() - s.nets.len());
        }
    }
    Ok(out)
}

/// What the view of an adapter says.
pub(crate) fn live_of(n: &vroom_config::Net) -> Result<NetLive> {
    Ok(match n.mode()? {
        NetMode::Nat { port_forwards } => NetLive {
            kind: NetKind::Nat,
            name: String::new(),
            loopback: n.allow_host_loopback,
            block_private: n.block_private,
            forwards: port_forwards
                .iter()
                .map(|p| Forward { host_addr: p.host_addr.clone(), host_port: p.host_port, guest_port: p.guest_port })
                .collect(),
        },
        NetMode::Bridged { interface } => {
            NetLive { kind: NetKind::Bridged, name: interface, loopback: false, block_private: false, forwards: Vec::new() }
        }
        NetMode::HostOnly { network } => {
            NetLive { kind: NetKind::HostOnly, name: network, loopback: false, block_private: false, forwards: Vec::new() }
        }
    })
}

/// "nat", "bridged:en0", "host-only:lab": what the device keeps in a
/// snapshot, to see a change at resume.
pub(crate) fn label(l: &NetLive) -> String {
    match l.kind {
        NetKind::Nat => "nat".into(),
        NetKind::Bridged => format!("bridged:{}", l.name),
        NetKind::HostOnly => format!("host-only:{}", l.name),
    }
}

/// Where the NAT of adapter `idx` lives (its own /24) and where its DNS
/// relay asks. `dns`: found before the sandbox (it can't read
/// /etc/resolv.conf after).
fn nat_config(ident: &Identity, idx: usize, mac: [u8; 6], policy: NatPolicy, dns: Option<SocketAddr>) -> Result<NatConfig> {
    let mut cfg = NatConfig::new(mac);
    cfg.dns_upstream = dns;
    // Adapter N: the first adapter's /24 plus N (10.0.(2+N).0/24 by
    // default): two NICs never share a subnet in the guest's routing
    // table.
    (cfg.guest, cfg.gateway, cfg.dns) = ident.nat_addrs(idx);
    cfg.gateway_mac = vroom_config::parse_mac(&ident.nat_gateway_mac)?;
    cfg.policy = policy;
    Ok(cfg)
}

/// The backend adapter `idx` starts with, and the MAC the guest must use
/// (vroom-netd's on macOS: vmnet picks it).
pub(crate) fn boot_backend(
    vm: &str,
    ident: &Identity,
    idx: usize,
    n: &vroom_config::Net,
    mac: [u8; 6],
    dns: Option<SocketAddr>,
) -> Result<(Box<dyn NetBackend>, [u8; 6])> {
    let via_netd = |attach: netd::Attach| -> Result<(Box<dyn NetBackend>, [u8; 6])> {
        let sock = std::env::var_os("VROOM_NETD_SOCKET")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| netd::DEFAULT_SOCKET.into());
        let req = netd::Request { version: netd::VERSION, vm: vm.to_string(), nic: idx as u32, attach, mac };
        let (s, assigned) = netd::attach(&sock, &req).map_err(|e| Error::Setup(e.to_string()))?;
        let fs = FrameSocket::new(s).map_err(|e| Error::Setup(format!("netd socket: {e}")))?;
        Ok((Box::new(fs), assigned))
    };
    match n.mode()? {
        NetMode::Nat { port_forwards } => {
            let policy = NatPolicy { allow_host_loopback: n.allow_host_loopback, block_private: n.block_private };
            let mut cfg = nat_config(ident, idx, mac, policy, dns)?;
            for p in port_forwards {
                let ip: std::net::IpAddr =
                    p.host_addr.parse().map_err(|_| Error::Setup(format!("bad host_addr {}", p.host_addr)))?;
                cfg.forwards.push(TcpForward { host: (ip, p.host_port).into(), guest_port: p.guest_port });
            }
            let nat = Nat::new(cfg).map_err(|e| Error::Setup(format!("nat: {e}")))?;
            Ok((Box::new(nat), mac))
        }
        NetMode::Bridged { interface } => via_netd(netd::Attach::Bridged { interface }),
        NetMode::HostOnly { network } => via_netd(netd::Attach::HostOnly { network }),
    }
}

/// One adapter, while the VM runs.
pub(crate) struct Nic {
    pub ctl: NetControl,
    pub live: NetLive,
    /// The NAT it started with, while it is on another network: it holds
    /// the port forwards' listening sockets, which can't be opened again.
    parked: Option<(Box<dyn NetBackend>, Vec<Forward>)>,
}

impl Nic {
    pub fn new(ctl: NetControl, live: NetLive) -> Self {
        Nic { ctl, live, parked: None }
    }
}

/// What the sandbox leaves this VMM, for adapters changed later.
#[derive(Debug, Clone, Default)]
pub(crate) struct Reach {
    /// Connections out (a NAT adapter at start, or no sandbox).
    pub nat: bool,
    /// The host's loopback too (`allow_host_loopback` at start).
    pub loopback: bool,
    pub dns: Option<SocketAddr>,
}

impl Reach {
    /// What the sandbox of a VM with these adapters allows (the CLI
    /// builds it the same way).
    pub fn of<'a>(nets: impl Iterator<Item = &'a vroom_config::Net> + Clone, sandbox: bool) -> Reach {
        let nat = |f: fn(&vroom_config::Net) -> bool| !sandbox || nets.clone().any(|n| n.mode == ConfigKind::Nat && f(n));
        Reach { nat: nat(|_| true), loopback: nat(|n| n.allow_host_loopback), dns: vroom_net::nat::host_dns() }
    }
}

/// The VM's adapters, for the viewer to change.
pub(crate) struct NetPorts {
    nics: Mutex<Vec<Nic>>,
    viewer: Option<ViewerTx>,
    reach: Reach,
    ident: Identity,
}

impl NetPorts {
    pub fn new(nics: Vec<Nic>, viewer: Option<ViewerTx>, reach: Reach, ident: Identity) -> Arc<NetPorts> {
        let me = Arc::new(NetPorts { nics: Mutex::new(nics), viewer, reach, ident });
        me.tell();
        me
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Nic>> {
        self.nics.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The viewer's list, now.
    fn tell(&self) {
        let Some(v) = &self.viewer else { return };
        let ports = self
            .lock()
            .iter()
            .enumerate()
            .map(|(i, n)| NetPort { nic: i as u8, live: n.live.clone(), connected: n.ctl.connected(), mac: n.ctl.mac() })
            .collect();
        v.send(&ToViewer::Net { nat_ok: self.reach.nat, ports });
    }

    /// Cable in or out.
    pub fn connect(&self, nic: u8, on: bool) -> std::result::Result<(), String> {
        {
            let nics = self.lock();
            let n = nics.get(nic as usize).ok_or("no such network adapter")?;
            n.ctl.set_connected(on);
        }
        log::info!("net: adapter {} {}", nic + 1, if on { "connected" } else { "disconnected" });
        self.tell();
        Ok(())
    }

    /// Put adapter `nic` on `want`. Bridged and host-only: `sock` is the
    /// vroom-netd port the app asked for, passing `port_mac`.
    pub fn set(&self, nic: u8, want: NetLive, port_mac: [u8; 6], sock: Option<OwnedFd>) -> std::result::Result<(), String> {
        if !want.valid() {
            return Err("not a network setting".into());
        }
        let i = nic as usize;
        let mut nics = self.lock();
        let n = nics.get_mut(i).ok_or("no such network adapter")?;
        let label = label(&want);
        match want.kind {
            NetKind::Nat => {
                let policy = NatPolicy { allow_host_loopback: want.loopback && self.reach.loopback, block_private: want.block_private };
                if n.live.kind == NetKind::Nat {
                    // The same NAT: new rules from the next connection on.
                    // Its port forwards stay as they are.
                    n.ctl.set_nat_policy(&policy);
                } else {
                    let (mut b, forwards) = match n.parked.take() {
                        Some((b, f)) if f == want.forwards => (b, f),
                        _ if !self.reach.nat => {
                            return Err("NAT needs the VM restarted: it started without a NAT adapter, so its sandbox allows no connections out".into())
                        }
                        _ => {
                            let mac = n.ctl.mac();
                            let cfg = nat_config(&self.ident, i, mac, policy.clone(), self.reach.dns).map_err(|e| e.to_string())?;
                            (Box::new(Nat::new(cfg).map_err(|e| format!("NAT: {e}"))?) as Box<dyn NetBackend>, Vec::new())
                        }
                    };
                    b.set_nat_policy(&policy);
                    let mac = n.ctl.mac();
                    n.ctl.set_backend(b, mac, &label).map_err(|e| e.to_string())?;
                    n.live.forwards = forwards;
                }
                n.live.kind = NetKind::Nat;
                n.live.name.clear();
                n.live.loopback = policy.allow_host_loopback;
                n.live.block_private = policy.block_private;
            }
            NetKind::Bridged | NetKind::HostOnly => {
                let fd = sock.ok_or("no vroom-netd socket came with it")?;
                let fs = FrameSocket::new(UnixDatagram::from(fd)).map_err(|e| e.to_string())?;
                let old = n.ctl.set_backend(Box::new(fs), port_mac, &label).map_err(|e| e.to_string())?;
                if n.live.kind == NetKind::Nat && !n.live.forwards.is_empty() {
                    n.parked = old.map(|b| (b, n.live.forwards.clone()));
                }
                n.live = NetLive { forwards: Vec::new(), loopback: false, block_private: false, ..want };
            }
        }
        log::info!("net: adapter {} now {label}", i + 1);
        drop(nics);
        self.tell();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(nets: &str) -> VmConfig {
        VmConfig::from_toml(&format!("name = \"t\"\n[boot]\ntype = \"uefi\"\nvars = \"v.fd\"\n{nets}")).unwrap()
    }

    #[test]
    fn a_resume_keeps_the_adapters_it_was_saved_with() {
        let two = cfg("[[net]]\nmode = \"nat\"\n[[net]]\nmode = \"host-only\"\nnetwork = \"lab\"\n");
        let one = cfg("[[net]]\nmode = \"bridged\"\ninterface = \"en0\"\nconnected = false\n");
        // Booting: the file.
        let p = plan(&two, None).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].mac, two.identity.nic_mac("t", 1));
        // Resumed with one removed: it stays, unplugged; the first is as
        // the file says now.
        let p = plan(&one, Some(&two)).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].net.mode, p[0].net.connected, p[0].removed), (ConfigKind::Bridged, false, false));
        assert_eq!((p[1].net.mode, p[1].removed), (ConfigKind::HostOnly, true));
        // Resumed with one added: not yet.
        assert_eq!(plan(&two, Some(&one)).unwrap().len(), 1);
    }

    #[test]
    fn views_and_labels() {
        let c = cfg("[[net]]\nmode = \"nat\"\nblock_private = true\nport_forwards = [{ host_port = 2222, guest_port = 22 }]\n");
        let l = live_of(&c.nets[0]).unwrap();
        assert!(l.valid());
        assert_eq!((l.kind, l.block_private, l.forwards.len()), (NetKind::Nat, true, 1));
        assert_eq!(label(&l), "nat");
        let c = cfg("[[net]]\nmode = \"host-only\"\nnetwork = \"lab\"\n");
        assert_eq!(label(&live_of(&c.nets[0]).unwrap()), "host-only:lab");
        // A VM with only host-only can't make a NAT later.
        let r = Reach::of(c.nets.iter(), true);
        assert!(!r.nat && !r.loopback);
        assert!(Reach::of(c.nets.iter(), false).nat, "no sandbox: no limit");
    }
}
