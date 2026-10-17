//! virtio-net. One RX/TX pair, no offloads (keeps the frame path simple:
//! every frame is a plain, fully checksummed Ethernet frame).
//!
//! While the guest runs, the VMM can pull the cable out and put it back
//! ([`NetControl::set_connected`]: the link status bit and a config
//! interrupt, as with a real cable) and change what is at the other end
//! ([`NetControl::set_backend`]: NAT, or a vroom-netd port), with the same
//! device and MAC. A new backend bounces the link, so the guest asks DHCP
//! again.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use vroom_net::mac::MacXlate;
use vroom_net::NetBackend;

use crate::worker::{Poker, QueueCtx, QueueHandler, WorkerSlot};
use crate::{types, ActivateContext, DeviceState, Error, Result, VirtioDevice, VirtioInterrupt};

const F_MTU: u64 = 1 << 3;
const F_MAC: u64 = 1 << 5;
const F_STATUS: u64 = 1 << 16;
const F_CTRL_VQ: u64 = 1 << 17;
const S_LINK_UP: u16 = 1;

const RX: usize = 0;
const TX: usize = 1;
/// The control queue: Windows' driver (NetKVM) only takes config
/// interrupts, the link changes, through the vector it gives this queue.
const CTRL: usize = 2;
/// Biggest control command we read (a MAC filter table is the largest).
const MAX_CTRL: usize = 4096;
const HDR_LEN: usize = 12;
const MTU: u16 = 1500;
/// Biggest frame we accept from the guest (no TSO, so MTU + header).
const MAX_FRAME: usize = 1514 + 4;
/// How long the link stays down when the backend changes: long enough
/// for the guest to see it go (Linux and Windows read the status from a
/// work item) and drop its address, so it asks DHCP again on the way up.
pub const BOUNCE: Duration = Duration::from_secs(2);

/// The cable and what it is plugged into: shared by the device, its
/// worker and the VMM's [`NetControl`].
struct Link {
    /// Where frames go. Here while the device is inactive too.
    backend: Option<Box<dyn NetBackend>>,
    /// The worker's poller while it runs: a new backend registers there.
    reg: Option<mio::Registry>,
    poker: Option<Poker>,
    /// While the driver has the device.
    irq: Option<Arc<dyn VirtioInterrupt>>,
    /// The cable is in (the user's choice).
    connected: bool,
    /// Down until then whatever `connected` says (a bounce).
    down_until: Option<Instant>,
    /// What the guest was last told.
    shown_up: bool,
    /// The VM is paused (or being saved): interrupts wait.
    paused: bool,
    /// The MAC, as the guest knows it.
    mac: [u8; 6],
    /// What the backend is ("nat", "bridged:en0"): a snapshot keeps it,
    /// so a resume on another one bounces the link.
    label: String,
}

impl Link {
    fn up(&self) -> bool {
        self.connected && self.down_until.is_none()
    }

    /// Tell the guest if the link changed since it last heard.
    fn update(&mut self) {
        let up = self.up();
        if up == self.shown_up || self.paused {
            return;
        }
        self.shown_up = up;
        if let Some(irq) = &self.irq {
            log::info!("net: link {} ({})", if up { "up" } else { "down" }, self.label);
            irq.signal_config_changed();
        }
    }

    fn bounce(&mut self) {
        if self.irq.is_none() {
            // No driver: nothing to tell.
            return;
        }
        self.down_until = Some(Instant::now() + BOUNCE);
        self.update();
        if let Some(p) = &self.poker {
            // The worker's next wakeup changes.
            p.poke();
        }
    }
}

fn lock(l: &Mutex<Link>) -> MutexGuard<'_, Link> {
    l.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the VMM holds to change an adapter while the guest runs.
#[derive(Clone)]
pub struct NetControl {
    link: Arc<Mutex<Link>>,
}

impl std::fmt::Debug for NetControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetControl").finish_non_exhaustive()
    }
}

impl NetControl {
    /// Plug the cable in or pull it out.
    pub fn set_connected(&self, on: bool) {
        let mut l = lock(&self.link);
        l.connected = on;
        l.update();
    }

    pub fn connected(&self) -> bool {
        lock(&self.link).connected
    }

    /// The MAC the guest uses.
    pub fn mac(&self) -> [u8; 6] {
        lock(&self.link).mac
    }

    /// Frames go to `b` from now on. `port_mac`: the MAC `b` passes, if
    /// not the guest's (a vroom-netd port on macOS): frames are
    /// translated. The link bounces. Returns the backend it replaces.
    pub fn set_backend(&self, b: Box<dyn NetBackend>, port_mac: [u8; 6], label: &str) -> std::io::Result<Option<Box<dyn NetBackend>>> {
        let mut l = lock(&self.link);
        let mut b = MacXlate::wrap(b, l.mac, port_mac);
        let mut old = l.backend.take();
        if let Some(reg) = l.reg.as_ref() {
            if let Some(o) = old.as_mut() {
                o.deregister(reg);
            }
            if let Err(e) = b.register(reg) {
                b.deregister(reg);
                if let Some(o) = old.as_mut() {
                    let _ = o.register(reg);
                }
                l.backend = old;
                return Err(e);
            }
        }
        l.backend = Some(b);
        l.label = label.to_string();
        l.bounce();
        if let Some(p) = &l.poker {
            p.poke();
        }
        Ok(old)
    }

    /// NAT: where new connections may go. False if the backend isn't one.
    pub fn set_nat_policy(&self, p: &vroom_net::nat::NatPolicy) -> bool {
        lock(&self.link).backend.as_mut().is_some_and(|b| b.set_nat_policy(p))
    }

    pub fn label(&self) -> String {
        lock(&self.link).label.clone()
    }
}

#[derive(Debug)]
pub struct Net {
    id: String,
    link: Arc<Mutex<Link>>,
    worker: WorkerSlot<NetHandler>,
    /// Offer a control queue (devices restored from a snapshot taken
    /// before it existed keep their two queues).
    ctrl: bool,
}

/// What a snapshot keeps (older ones have nothing).
#[derive(Debug, Serialize, Deserialize)]
struct Snap {
    mac: [u8; 6],
    /// What the guest had been told.
    up: bool,
    label: String,
}

impl Net {
    pub fn new(id: &str, mac: [u8; 6], backend: Box<dyn NetBackend>) -> Self {
        Net::with_link(id, mac, backend, "", true)
    }

    /// `label` names the backend (see [`NetControl::label`]); `connected`
    /// false: the cable starts out pulled.
    pub fn with_link(id: &str, mac: [u8; 6], backend: Box<dyn NetBackend>, label: &str, connected: bool) -> Self {
        let link = Link {
            backend: Some(backend),
            reg: None,
            poker: None,
            irq: None,
            connected,
            down_until: None,
            shown_up: connected,
            paused: false,
            mac,
            label: label.to_string(),
        };
        Net { id: id.to_string(), link: Arc::new(Mutex::new(link)), worker: WorkerSlot::default(), ctrl: true }
    }

    /// No control queue: the layout of devices saved before it existed.
    pub fn without_ctrl(mut self) -> Self {
        self.ctrl = false;
        self
    }

    pub fn control(&self) -> NetControl {
        NetControl { link: self.link.clone() }
    }

    fn start(&mut self, ctx: ActivateContext, paused: bool) -> Result<()> {
        {
            let mut l = lock(&self.link);
            if l.backend.is_none() {
                return Err(Error::Device("net backend already in use".into()));
            }
            l.irq = Some(ctx.irq.clone());
            l.paused = paused;
        }
        let h = NetHandler { link: self.link.clone(), pending: None };
        self.worker.start(&format!("net-{}", self.id), ctx, h, paused)
    }

    /// The worker deregisters the backend's sources on its way out; the
    /// backend stays in the link for the next activation.
    fn stop(&mut self) {
        self.worker.stop();
        let mut l = lock(&self.link);
        l.irq = None;
        l.poker = None;
        l.down_until = None;
        l.shown_up = l.connected;
    }
}

#[derive(Debug)]
pub struct NetHandler {
    link: Arc<Mutex<Link>>,
    /// Frame waiting for an RX buffer.
    pending: Option<Vec<u8>>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link").field("connected", &self.connected).field("label", &self.label).finish_non_exhaustive()
    }
}

impl NetHandler {
    fn tx(&mut self, ctx: &mut QueueCtx) {
        // Everything the guest queued goes to the backend as one batch.
        let mut batch = Vec::new();
        loop {
            let chain = match ctx.queues[TX].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("net tx: {e}");
                    break;
                }
            };
            let head = chain.head();
            if let Ok((mut r, _)) = chain.into_rw() {
                let n = r.available();
                if n > HDR_LEN && n <= HDR_LEN + MAX_FRAME {
                    let mut hdr = [0u8; HDR_LEN];
                    let mut frame = vec![0u8; n - HDR_LEN];
                    if r.read_exact(&mut hdr).is_ok() && r.read_exact(&mut frame).is_ok() {
                        batch.push(frame);
                    }
                }
            }
            ctx.add_used(TX, head, 0);
        }
        let mut l = lock(&self.link);
        // No cable: what the guest sends goes nowhere.
        if batch.is_empty() || !l.up() {
            return;
        }
        if let Some(be) = l.backend.as_mut() {
            be.send_frames(&batch);
        }
    }

    fn rx(&mut self, ctx: &mut QueueCtx) {
        let mut l = lock(&self.link);
        let up = l.up();
        let Some(be) = l.backend.as_mut() else { return };
        if !up {
            // Nothing reaches an unplugged guest; nothing piles up either.
            self.pending = None;
            while be.recv_frame().is_some() {}
            return;
        }
        while let Some(frame) = self.pending.take().or_else(|| be.recv_frame()) {
            let chain = match ctx.queues[RX].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    // No buffers: hold the frame until the driver adds some.
                    self.pending = Some(frame);
                    break;
                }
                Err(e) => {
                    log::warn!("net rx: {e}");
                    break;
                }
            };
            let head = chain.head();
            let mut len = 0;
            if let Ok((_, mut w)) = chain.into_rw() {
                if w.available() >= HDR_LEN + frame.len() {
                    let mut hdr = [0u8; HDR_LEN];
                    hdr[10] = 1; // num_buffers
                    if w.write_all(&hdr).is_ok() && w.write_all(&frame).is_ok() {
                        len = w.written() as u32;
                    }
                }
            }
            ctx.add_used(RX, head, len);
        }
    }

    /// Control commands (receive mode, MAC filters, VLANs, announce
    /// acks). We pass every frame anyway: each is acknowledged and
    /// otherwise ignored.
    fn ctrl(&mut self, ctx: &mut QueueCtx) {
        if ctx.features & F_CTRL_VQ == 0 || ctx.queues.len() <= CTRL {
            return;
        }
        loop {
            let chain = match ctx.queues[CTRL].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("net ctrl: {e}");
                    break;
                }
            };
            let head = chain.head();
            let mut len = 0;
            if let Ok((r, mut w)) = chain.into_rw() {
                // class, command, data; then one byte back: 0 ok, 1 error.
                let ok = (2..=MAX_CTRL).contains(&r.available());
                if w.write_all(&[if ok { 0 } else { 1 }]).is_ok() {
                    len = w.written() as u32;
                }
            }
            ctx.add_used(CTRL, head, len);
        }
    }

    /// A bounce that is over: the link comes back.
    fn relink(&mut self) {
        let mut l = lock(&self.link);
        if l.down_until.is_some_and(|t| Instant::now() >= t) {
            l.down_until = None;
        }
        l.update();
    }
}

impl QueueHandler for NetHandler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        if q == TX {
            self.tx(ctx);
        }
        if q == CTRL {
            self.ctrl(ctx);
        }
        self.rx(ctx);
    }

    fn backend_ready(&mut self, ctx: &mut QueueCtx, token: usize) {
        if let Some(be) = lock(&self.link).backend.as_mut() {
            be.ready(token);
        }
        self.relink();
        self.rx(ctx);
    }

    fn register(&mut self, reg: &mio::Registry) -> std::io::Result<()> {
        let mut l = lock(&self.link);
        l.reg = Some(reg.try_clone()?);
        match l.backend.as_mut() {
            Some(be) => be.register(reg),
            None => Ok(()),
        }
    }

    fn deregister(&mut self, reg: &mio::Registry) {
        let mut l = lock(&self.link);
        l.reg = None;
        if let Some(be) = l.backend.as_mut() {
            be.deregister(reg);
        }
    }

    fn attach(&mut self, poker: Poker) {
        lock(&self.link).poker = Some(poker);
    }

    fn tick_interval(&mut self) -> Option<Duration> {
        let mut l = lock(&self.link);
        let relink = l.down_until.map(|t| t.saturating_duration_since(Instant::now()));
        let be = l.backend.as_mut().and_then(|b| b.poll_delay());
        match (be, relink) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn tick(&mut self, ctx: &mut QueueCtx) {
        if let Some(be) = lock(&self.link).backend.as_mut() {
            be.tick();
        }
        self.relink();
        self.rx(ctx);
    }
}

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        types::NET
    }
    fn name(&self) -> String {
        format!("net-{}", self.id)
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        if self.ctrl {
            vec![256, 256, 64]
        } else {
            vec![256, 256]
        }
    }
    fn device_features(&self) -> u64 {
        let ctrl = if self.ctrl { F_CTRL_VQ } else { 0 };
        F_MAC | F_STATUS | F_MTU | ctrl | crate::features::RING_EVENT_IDX | crate::features::RING_INDIRECT_DESC
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let (mac, up) = {
            let l = lock(&self.link);
            (l.mac, l.shown_up)
        };
        let mut c = [0u8; 12];
        c[0..6].copy_from_slice(&mac);
        c[6..8].copy_from_slice(&(if up { S_LINK_UP } else { 0 }).to_le_bytes());
        c[8..10].copy_from_slice(&1u16.to_le_bytes());
        c[10..12].copy_from_slice(&MTU.to_le_bytes());
        let o = offset as usize;
        data.fill(0);
        if o < c.len() {
            let n = data.len().min(c.len() - o);
            data[..n].copy_from_slice(&c[o..o + n]);
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        self.start(ctx, false)
    }
    fn queue_notify(&mut self, q: u16) {
        self.worker.notify(q);
    }
    fn reset(&mut self) {
        self.stop();
    }
    fn pause(&mut self) {
        lock(&self.link).paused = true;
        self.worker.pause();
    }
    fn resume(&mut self) {
        {
            let mut l = lock(&self.link);
            l.paused = false;
            // What changed while paused (or since the snapshot).
            l.update();
        }
        self.worker.resume();
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        // Connection state (NAT tables, host sockets) can't be snapshotted;
        // TCP sessions reset after restore, like after a cable unplug.
        let snap = {
            let l = lock(&self.link);
            Snap { mac: l.mac, up: l.shown_up, label: l.label.clone() }
        };
        let data = serde_json::to_vec(&snap).map_err(|e| Error::Device(e.to_string()))?;
        Ok(DeviceState { queues: self.worker.queues(), data })
    }
    fn restore_active(&mut self, ctx: ActivateContext, data: &[u8]) -> Result<()> {
        // Older snapshots: nothing saved, nothing to compare.
        let snap: Option<Snap> = if data.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(data).map_err(|e| Error::Device(format!("net snapshot: {e}")))?)
        };
        let bounce = {
            let mut l = lock(&self.link);
            match snap {
                Some(s) => {
                    if s.mac != l.mac {
                        // The guest knows the MAC it had: keep it, and
                        // translate to the one this backend passes.
                        log::info!("net: the guest keeps {} until it restarts", fmt_mac(&s.mac));
                        if let Some(b) = l.backend.take() {
                            l.backend = Some(MacXlate::wrap(b, s.mac, l.mac));
                        }
                        l.mac = s.mac;
                    }
                    l.shown_up = s.up;
                    // Another network behind it now: the guest's address
                    // is stale.
                    s.label != l.label && !l.label.is_empty()
                }
                None => false,
            }
        };
        self.start(ctx, true)?;
        if bounce {
            let mut l = lock(&self.link);
            l.down_until = Some(Instant::now() + BOUNCE);
        }
        Ok(())
    }
}

fn fmt_mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Debug, Default)]
    struct Irq {
        config: AtomicU32,
    }

    impl VirtioInterrupt for Irq {
        fn signal_used_queue(&self, _q: u16) {}
        fn signal_config_changed(&self) {
            self.config.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[derive(Debug, Default)]
    struct Pipe {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        incoming: Vec<Vec<u8>>,
    }

    impl NetBackend for Pipe {
        fn send_frame(&mut self, f: &[u8]) {
            self.sent.lock().unwrap().push(f.to_vec());
        }
        fn recv_frame(&mut self) -> Option<Vec<u8>> {
            self.incoming.pop()
        }
        fn register(&mut self, _reg: &mio::Registry) -> std::io::Result<()> {
            Ok(())
        }
        fn ready(&mut self, _token: usize) {}
    }

    fn status(n: &Net) -> u16 {
        let mut b = [0u8; 2];
        n.read_config(6, &mut b);
        u16::from_le_bytes(b)
    }

    fn active(n: &mut Net) -> Arc<Irq> {
        let irq = Arc::new(Irq::default());
        let mem = vroom_memory::GuestMemory::new(&[(vroom_memory::GuestAddress(0), 1 << 16)]).unwrap();
        let ctx = ActivateContext { mem, queues: vec![crate::Queue::new(256), crate::Queue::new(256)], irq: irq.clone(), features: F_STATUS };
        n.start(ctx, false).unwrap();
        irq
    }

    #[test]
    fn cable_out_and_in() {
        let mut n = Net::with_link("eth0", [2, 0, 0, 0, 0, 1], Box::new(Pipe::default()), "nat", false);
        assert_eq!(status(&n), 0, "starts unplugged");
        let irq = active(&mut n);
        let c = n.control();
        c.set_connected(true);
        assert_eq!(status(&n), S_LINK_UP);
        assert_eq!(irq.config.load(Ordering::SeqCst), 1);
        c.set_connected(true);
        assert_eq!(irq.config.load(Ordering::SeqCst), 1, "no change, no interrupt");
        // Paused: the guest hears about it on resume.
        n.pause();
        c.set_connected(false);
        assert_eq!(irq.config.load(Ordering::SeqCst), 1);
        n.resume();
        assert_eq!((status(&n), irq.config.load(Ordering::SeqCst)), (0, 2));
        n.reset();
    }

    #[test]
    fn a_new_backend_bounces_the_link_and_frames_follow() {
        let mut n = Net::with_link("eth0", [2, 0, 0, 0, 0, 1], Box::new(Pipe::default()), "nat", true);
        let irq = active(&mut n);
        let sent = Arc::new(Mutex::new(Vec::new()));
        let c = n.control();
        let old = c.set_backend(Box::new(Pipe { sent: sent.clone(), incoming: Vec::new() }), [2, 0, 0, 0, 0, 1], "bridged:en0").unwrap();
        assert!(old.is_some());
        assert_eq!((status(&n), irq.config.load(Ordering::SeqCst)), (0, 1), "down for a while");
        assert_eq!(c.label(), "bridged:en0");
        lock(&n.link).down_until = Some(Instant::now());
        n.worker.with(|_, h| h.relink());
        assert_eq!((status(&n), irq.config.load(Ordering::SeqCst)), (S_LINK_UP, 2), "and back");
        // Unplugged: nothing goes out.
        c.set_connected(false);
        lock(&n.link).backend.as_mut().unwrap().send_frame(&[1; 60]);
        assert_eq!(sent.lock().unwrap().len(), 1);
        n.reset();
    }

    #[test]
    fn a_control_queue_unless_restoring_an_older_layout() {
        let n = Net::new("eth0", [2, 0, 0, 0, 0, 1], Box::new(Pipe::default()));
        assert_eq!((n.queue_max_sizes().len(), n.device_features() & F_CTRL_VQ), (3, F_CTRL_VQ));
        let n = n.without_ctrl();
        assert_eq!((n.queue_max_sizes().len(), n.device_features() & F_CTRL_VQ), (2, 0));
    }

    #[test]
    fn restore_keeps_the_mac_the_guest_knows() {
        let mut n = Net::with_link("eth0", [2, 0, 0, 0, 0, 1], Box::new(Pipe::default()), "nat", true);
        let mut saved = Net::with_link("eth0", [2, 0, 0, 0, 0, 9], Box::new(Pipe::default()), "bridged:en0", true);
        let irq = active(&mut saved);
        let st = saved.save_active().unwrap();
        saved.reset();
        let mem = vroom_memory::GuestMemory::new(&[(vroom_memory::GuestAddress(0), 1 << 16)]).unwrap();
        let ctx = ActivateContext { mem, queues: st.queues, irq: irq.clone(), features: F_STATUS };
        n.restore_active(ctx, &st.data).unwrap();
        let mut mac = [0u8; 6];
        n.read_config(0, &mut mac);
        assert_eq!(mac, [2, 0, 0, 0, 0, 9]);
        // Another network than at the save: down, then up after resume.
        n.resume();
        assert_eq!(status(&n), 0);
        let mem = vroom_memory::GuestMemory::new(&[(vroom_memory::GuestAddress(0), 1 << 16)]).unwrap();
        let ctx = ActivateContext { mem, queues: vec![crate::Queue::new(256), crate::Queue::new(256)], irq, features: 0 };
        assert!(n.restore_active(ctx, b"{\"mac\":[1,2]}").is_err(), "checked");
        n.reset();
    }
}
