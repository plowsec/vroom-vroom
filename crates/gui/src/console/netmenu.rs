//! Network adapters while the VM runs: the VM window's network menu, and
//! what the settings page changes.
//!
//! The VM file says what each adapter should be. When it changes (here,
//! or on the settings page) we tell the VMM: the cable in or out
//! (`ToVmm::NetConnect`), another network (`ToVmm::NetSet`). The VMM is
//! sandboxed and can't reach vroom-netd: for bridged and host-only we ask
//! netd for the port and hand the VMM its socket, as for USB helpers. The
//! VMM says what each adapter is now (`ToViewer::Net`); the menu and the
//! settings page show that, and what waits for the next start.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use slint::{ModelRc, VecModel};
use vroom_config::{NetKind as Kind, NetMode, PortForward, VmConfig};
use vroom_display::{Forward, NetKind, NetLive, NetPort, ToVmm};
use vroom_net::netd;

use super::{with, Console};
use crate::NetMenuRow;

/// How long we keep our copy of a netd socket we handed the VMM (macOS
/// can drop a socket whose other copies all close while it is in flight).
const HOLD: Duration = Duration::from_secs(5);
/// A change the VMM never answered (netd may wait for a port that is
/// still closing: a few seconds).
const BUSY_FOR: Duration = Duration::from_secs(20);

#[derive(Default)]
pub(super) struct NetMenu {
    /// What the VMM says each adapter is now; whether it may switch one
    /// to NAT.
    live: Option<(bool, Vec<NetPort>)>,
    /// The VM file's adapters (by index) to the VM's (this run). None:
    /// added since the VM started, it comes at the next start. Running
    /// adapters no entry points to were taken out of the file.
    map: Option<Vec<Option<u8>>>,
    /// Adapters being switched, since when.
    busy: HashMap<u8, Instant>,
    held: Vec<(Instant, OwnedFd)>,
    /// The VM file's adapters when this window opened.
    start: Vec<vroom_config::Net>,
}

impl NetMenu {
    pub(super) fn new(start: Vec<vroom_config::Net>) -> Self {
        NetMenu { start, ..Default::default() }
    }
}

/// What the settings page needs to say what applies now and what later.
#[derive(Debug, Clone, Default)]
pub struct NetView {
    pub nat_ok: bool,
    pub ports: Vec<NetPort>,
    /// The VM file's adapters to the VM's (see `NetMenu::map`).
    pub map: Vec<Option<u8>>,
    /// The VM file's adapters when the window opened (what a MAC change
    /// is measured against).
    pub start: Vec<vroom_config::Net>,
}

thread_local! {
    /// NAT port forwards of adapters switched to another network, by VM
    /// and adapter: they come back with NAT (the VM file can't keep them
    /// on a bridged adapter).
    static STASH: RefCell<HashMap<(String, usize), Vec<PortForward>>> = RefCell::new(HashMap::new());
}

/// Put adapter `i` of VM `key` on mode `m` (0 NAT, 1 bridged, 2
/// host-only) in `c`, keeping its NAT port forwards aside meanwhile.
pub fn set_mode(key: &str, c: &mut VmConfig, i: usize, m: usize, iface: Option<&str>) -> Result<(), String> {
    let before = c.nets.get(i).ok_or("no such adapter")?.clone();
    crate::edit::set(c, &format!("net.{i}.mode"), &m.to_string(), iface)?;
    let after = &mut c.nets[i];
    if before.mode == Kind::Nat && after.mode != Kind::Nat && !before.port_forwards.is_empty() {
        STASH.with(|s| s.borrow_mut().insert((key.to_string(), i), before.port_forwards.clone()));
    }
    if before.mode != Kind::Nat && after.mode == Kind::Nat {
        if let Some(f) = STASH.with(|s| s.borrow_mut().remove(&(key.to_string(), i))) {
            after.port_forwards = f;
        }
    }
    Ok(())
}

/// The VM file's adapter as the VMM names it.
pub fn live_of(n: &vroom_config::Net) -> Option<NetLive> {
    let l = match n.mode().ok()? {
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
    };
    l.valid().then_some(l)
}

/// "NAT", "Bridged (en0)", "Host-only (lab)".
pub fn mode_text(l: &NetLive) -> String {
    match l.kind {
        NetKind::Nat => "NAT".into(),
        NetKind::Bridged => format!("Bridged ({})", l.name),
        NetKind::HostOnly => format!("Host-only ({})", l.name),
    }
}

/// The same network (the cable may differ).
fn same_net(a: &NetLive, b: &NetLive) -> bool {
    a.kind == b.kind && a.name == b.name && (a.kind != NetKind::Nat || (a.loopback == b.loopback && a.block_private == b.block_private))
}

fn netd_socket() -> std::path::PathBuf {
    std::env::var_os("VROOM_NETD_SOCKET").map(std::path::PathBuf::from).unwrap_or_else(|| netd::DEFAULT_SOCKET.into())
}

/// Save one change to the VM file. Comments and layout stay.
fn edit_file(path: &Path, f: impl FnOnce(&mut VmConfig) -> Result<(), String>) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let before = VmConfig::from_toml(&text).map_err(|e| e.to_string())?;
    let mut after = before.clone();
    f(&mut after)?;
    after.validate().map_err(|e| e.to_string())?;
    if after == before {
        return Ok(());
    }
    crate::edit::save(path, &before, &after)
}

impl Console {
    /// The VMM's word on its adapters.
    pub(super) fn net_list(&mut self, nat_ok: bool, ports: Vec<NetPort>) {
        let m = &mut self.net;
        if m.map.is_none() {
            // The file's adapters, in order, are the VM's (a resumed VM
            // may have more or fewer).
            let n = VmConfig::load_raw(&self.path).map(|c| c.nets.len()).unwrap_or(ports.len());
            m.map = Some((0..n).map(|i| ports.iter().any(|p| p.nic as usize == i).then_some(i as u8)).collect());
        }
        m.live = Some((nat_ok, ports));
        // It has the sockets it got (or refused them): let ours go.
        m.held.clear();
        m.busy.clear();
        self.net_bar();
        if self.win.get_net_open() {
            self.net_rows();
        }
    }

    /// "Network: <why>" from the VMM: a change it couldn't make.
    pub(super) fn net_refused(&mut self) {
        self.net.busy.clear();
        self.net.held.clear();
        if self.win.get_net_open() {
            self.net_rows();
        }
    }

    pub(super) fn net_tick(&mut self, now: Instant) {
        let m = &mut self.net;
        m.held.retain(|(t, _)| now.duration_since(*t) < HOLD);
        let n = m.busy.len();
        m.busy.retain(|_, t| now.duration_since(*t) < BUSY_FOR);
        if n != self.net.busy.len() && self.win.get_net_open() {
            self.net_rows();
        }
    }

    pub(super) fn net_view(&self) -> Option<NetView> {
        let (nat_ok, ports) = self.net.live.clone()?;
        Some(NetView { nat_ok, ports, map: self.net.map.clone().unwrap_or_default(), start: self.net.start.clone() })
    }

    /// The toolbar button: there if the VM has adapters, marked if a
    /// cable is out.
    fn net_bar(&self) {
        let ports = self.net.live.as_ref().map(|(_, p)| p.as_slice()).unwrap_or_default();
        self.win.set_net_shown(!ports.is_empty());
        self.win.set_net_off(ports.iter().filter(|p| !p.connected && self.file_index(p.nic).is_some()).count() as i32);
    }

    /// The VM file's index of running adapter `nic`, if it is still there.
    fn file_index(&self, nic: u8) -> Option<usize> {
        self.net.map.as_ref()?.iter().position(|m| *m == Some(nic))
    }

    /// The menu's commands; false if `cmd` isn't one.
    pub(super) fn net_command(&mut self, cmd: &str) -> bool {
        if cmd == "net-menu" {
            let open = !self.win.get_net_open();
            self.win.set_net_open(open);
            if open {
                self.win.set_usb_open(false);
                self.net_rows();
            }
            return true;
        }
        let ports = self.net.live.as_ref().map(|(_, p)| p.clone()).unwrap_or_default();
        if let Some(i) = cmd.strip_prefix("net.toggle.").and_then(|i| i.parse::<usize>().ok()) {
            if let Some(p) = ports.get(i) {
                self.net_toggle(p);
            }
        } else if let Some((i, m)) = cmd.strip_prefix("net.mode.").and_then(|r| r.split_once('.')) {
            let (Ok(i), Ok(m)) = (i.parse::<usize>(), m.parse::<usize>()) else { return true };
            if let (Some(p), true) = (ports.get(i), m < 3) {
                self.net_mode(p.nic, m);
            }
        } else {
            return false;
        }
        true
    }

    fn net_rows(&mut self) {
        let Some((nat_ok, ports)) = self.net.live.clone() else { return };
        let rows: Vec<NetMenuRow> = ports
            .iter()
            .map(|p| {
                let fi = self.file_index(p.nic);
                let busy = self.net.busy.contains_key(&p.nic);
                let note = if busy {
                    "Switching...".to_string()
                } else if fi.is_none() {
                    "Removed from the VM file: unplugged until the VM restarts".into()
                } else if p.connected {
                    "Connected".into()
                } else {
                    "Cable out: the guest sees no network here".into()
                };
                NetMenuRow {
                    label: format!("Adapter {} · {}", fi.unwrap_or(p.nic as usize) + 1, mode_text(&p.live)).into(),
                    note: note.into(),
                    on: p.connected,
                    busy,
                    mode: p.live.kind as i32,
                    nat_ok,
                    removed: fi.is_none(),
                    warn: fi.is_none(),
                }
            })
            .collect();
        self.win.set_net_rows(ModelRc::new(VecModel::from(rows)));
    }

    /// Cable in or out: in the VM file (it stays so), and now.
    fn net_toggle(&mut self, p: &NetPort) {
        let Some(fi) = self.file_index(p.nic) else { return };
        let on = !p.connected;
        if let Err(e) = edit_file(&self.path, |c| crate::edit::set(c, &format!("net.{fi}.connected"), if on { "true" } else { "false" }, None)) {
            self.toast(&format!("Couldn't save: {e}"));
        }
        self.io.borrow_mut().send(ToVmm::NetConnect { nic: p.nic, on });
    }

    /// Another network for running adapter `nic`: in the VM file, then now.
    fn net_mode(&mut self, nic: u8, m: usize) {
        let Some(fi) = self.file_index(nic) else { return };
        let iface = crate::host::interfaces().into_iter().next();
        let key = self.key.clone();
        if let Err(e) = edit_file(&self.path, |c| set_mode(&key, c, fi, m, iface.as_deref())) {
            self.toast(&format!("Couldn't save: {e}"));
            return;
        }
        self.net_apply(fi);
    }

    /// Bring running adapter of file index `fi` in line with the VM file:
    /// what can change now does. The rest (port forwards, the MAC) waits
    /// for the next start; the settings page says so.
    pub(super) fn net_apply(&mut self, fi: usize) {
        let Ok(cfg) = VmConfig::load_raw(&self.path) else { return };
        let Some(n) = cfg.nets.get(fi) else { return };
        let Some(nic) = self.net.map.as_ref().and_then(|m| m.get(fi).copied().flatten()) else { return };
        let Some(p) = self.net.live.as_ref().and_then(|(_, ps)| ps.iter().find(|p| p.nic == nic).cloned()) else { return };
        let Some(want) = live_of(n) else { return };
        if n.connected != p.connected {
            self.io.borrow_mut().send(ToVmm::NetConnect { nic, on: n.connected });
        }
        if same_net(&want, &p.live) {
            return;
        }
        if want.kind == NetKind::Nat {
            self.net.busy.insert(nic, Instant::now());
            self.io.borrow_mut().send(ToVmm::NetSet { nic, live: want, port_mac: [0; 6] });
        } else {
            self.net_attach(nic, want, p.mac);
        }
        if self.win.get_net_open() {
            self.net_rows();
        }
    }

    /// Ask vroom-netd for a port for `nic` (off the UI thread: it can take
    /// seconds), then hand the VMM its socket.
    fn net_attach(&mut self, nic: u8, want: NetLive, mac: [u8; 6]) {
        self.net.busy.insert(nic, Instant::now());
        let vm: String = self.name.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).take(64).collect();
        let attach = match want.kind {
            NetKind::Bridged => netd::Attach::Bridged { interface: want.name.clone() },
            _ => netd::Attach::HostOnly { network: want.name.clone() },
        };
        // netd takes locally administered unicast MACs only; the VMM
        // translates if the port passes another one.
        let mut mac = mac;
        mac[0] = (mac[0] | 2) & !1;
        let req = netd::Request { version: netd::VERSION, vm: if vm.is_empty() { "vm".into() } else { vm }, nic: nic as u32, attach, mac };
        let key = self.key.clone();
        std::thread::spawn(move || {
            let r = netd::attach(&netd_socket(), &req).map_err(|e| match e {
                netd::NetdError::Refused(m) => m,
                e => e.to_string(),
            });
            let _ = slint::invoke_from_event_loop(move || {
                with(&key, |c| c.net_attached(nic, want, r));
            });
        });
    }

    fn net_attached(&mut self, nic: u8, want: NetLive, r: Result<(std::os::unix::net::UnixDatagram, [u8; 6]), String>) {
        let (sock, port_mac) = match r {
            Ok(x) => x,
            Err(e) => {
                self.net.busy.remove(&nic);
                self.toast(&format!("Network: {e}"));
                if self.win.get_net_open() {
                    self.net_rows();
                }
                return;
            }
        };
        let sent = match self.io.borrow().tx.as_ref() {
            Some(tx) => vroom_display::send_fd_with(tx, &ToVmm::NetSet { nic, live: want, port_mac }.encode(), sock.as_raw_fd())
                .map_err(|e| e.to_string()),
            None => Err("the VM is gone".into()),
        };
        match sent {
            Ok(()) => self.net.held.push((Instant::now(), OwnedFd::from(sock))),
            Err(e) => {
                self.net.busy.remove(&nic);
                self.toast(&format!("Network: {e}"));
            }
        }
    }

    /// File adapter `fi` was taken out: its running adapter is unplugged
    /// now and goes at the next start; the ones after it move up.
    pub(super) fn net_removed(&mut self, fi: usize) {
        let Some(map) = self.net.map.as_mut() else { return };
        if fi >= map.len() {
            return;
        }
        if let Some(nic) = map.remove(fi) {
            self.io.borrow_mut().send(ToVmm::NetConnect { nic, on: false });
        }
        // Kept forwards are by index: the ones after it moved.
        STASH.with(|s| s.borrow_mut().retain(|(k, _), _| *k != self.key));
        self.net_bar();
    }

    /// An adapter was added to the file: it comes at the next start.
    pub(super) fn net_added(&mut self) {
        if let Some(map) = self.net.map.as_mut() {
            map.push(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VmConfig {
        VmConfig::from_toml(
            "name = \"t\"\n[boot]\ntype = \"uefi\"\nvars = \"v.fd\"\n[[net]]\nmode = \"nat\"\nport_forwards = [{ host_port = 2222, guest_port = 22 }]\n",
        )
        .unwrap()
    }

    #[test]
    fn port_forwards_come_back_with_nat() {
        let mut c = cfg();
        set_mode("k", &mut c, 0, 1, Some("en0")).unwrap();
        assert_eq!((c.nets[0].mode, c.nets[0].port_forwards.len()), (Kind::Bridged, 0));
        set_mode("k", &mut c, 0, 2, None).unwrap();
        set_mode("k", &mut c, 0, 0, None).unwrap();
        assert_eq!(c.nets[0].port_forwards.len(), 1, "back");
        c.validate().unwrap();
        // Another VM's are its own.
        set_mode("k", &mut c, 0, 1, Some("en0")).unwrap();
        let mut other = cfg();
        other.nets[0] = c.nets[0].clone();
        set_mode("other", &mut other, 0, 0, None).unwrap();
        assert!(other.nets[0].port_forwards.is_empty());
    }

    #[test]
    fn views_of_the_file() {
        let c = cfg();
        let l = live_of(&c.nets[0]).unwrap();
        assert_eq!((l.kind, l.forwards.len()), (NetKind::Nat, 1));
        assert_eq!(mode_text(&l), "NAT");
        let b = NetLive { kind: NetKind::Bridged, name: "en0".into(), loopback: false, block_private: false, forwards: vec![] };
        assert_eq!(mode_text(&b), "Bridged (en0)");
        assert!(!same_net(&l, &b));
        let mut l2 = l.clone();
        l2.forwards.clear();
        assert!(same_net(&l, &l2), "forwards wait for the next start");
        l2.block_private = true;
        assert!(!same_net(&l, &l2));
    }
}
