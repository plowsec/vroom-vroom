//! Framed messages: type u8 | len u32 LE | payload.

use crate::{Error, Result, MAX_HEADS};
pub use vroom_proto::{GuestApp, GuestWindow, WindowOp};

/// Absolute pointer range (like a USB tablet).
pub const POINTER_MAX: u16 = 32767;
/// Window scales a resize may carry, in percent (0 = none).
pub const SCALE_MIN: u32 = 100;
pub const SCALE_MAX: u32 = 500;
const MAX_MSG: usize = 16 << 20;
/// Most names a drag may carry.
pub const DRAG_ITEMS_MAX: usize = 4096;
/// Biggest guest pointer picture, in pixels on a side.
pub const CURSOR_MAX: u32 = 256;
/// Most host USB devices a VM lists (its port pairs).
pub const USB_PORTS_MAX: usize = 16;

/// A host USB device, as the viewer names it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UsbDev {
    /// "vendor:product", 4 hex digits each.
    pub id: String,
    /// Empty: none.
    pub serial: String,
    /// A label for people.
    pub name: String,
}

impl UsbDev {
    /// Plain text only, and short: these end up in the VM file and on
    /// screen.
    pub fn valid(&self) -> bool {
        let hex = |s: &str| s.len() == 4 && s.bytes().all(|b| b.is_ascii_hexdigit());
        let id = self.id.split_once(':').is_some_and(|(v, p)| hex(v) && hex(p));
        let text = |s: &str, max: usize| s.len() <= max && !s.chars().any(char::is_control);
        id && text(&self.serial, 126) && text(&self.name, 128)
    }

    fn encode(&self, p: &mut Vec<u8>) {
        for s in [&self.id, &self.serial, &self.name] {
            p.extend_from_slice(&(s.len() as u16).to_le_bytes());
            p.extend_from_slice(s.as_bytes());
        }
    }

    fn decode(p: &[u8], o: &mut usize) -> Result<UsbDev> {
        let mut f = || -> Result<String> {
            let len = u16::from_le_bytes(p.get(*o..*o + 2).ok_or(Error::Proto("short"))?.try_into().map_err(|_| Error::Proto("short"))?) as usize;
            let s = text(p.get(*o + 2..*o + 2 + len).ok_or(Error::Proto("short"))?)?;
            *o += 2 + len;
            Ok(s)
        };
        let d = UsbDev { id: f()?, serial: f()?, name: f()? };
        if !d.valid() {
            return Err(Error::Proto("usb device"));
        }
        Ok(d)
    }
}

/// A host device on one of the VM's USB port pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbPort {
    pub port: u8,
    pub dev: UsbDev,
    /// Its helper has it open (else it was unplugged from the Mac, or
    /// not found yet).
    pub here: bool,
    /// From the VM file (`[[usb]]`), connected at every start.
    pub always: bool,
}

/// Most network adapters a VM has (the config's limit).
pub const NET_PORTS_MAX: usize = 8;
/// Most port forwards one adapter lists.
pub const FORWARDS_MAX: usize = 64;

/// What a network adapter is plugged into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetKind {
    Nat,
    Bridged,
    HostOnly,
}

/// A NAT port forward: `host_addr:host_port` to the guest's `guest_port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub host_addr: String,
    pub host_port: u16,
    pub guest_port: u16,
}

/// The other end of an adapter's cable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetLive {
    pub kind: NetKind,
    /// Bridged: the Mac's interface; host-only: the network. NAT: empty.
    pub name: String,
    /// NAT: the guest may reach the host's loopback, may not reach the LAN.
    pub loopback: bool,
    pub block_private: bool,
    /// NAT: the port forwards it has.
    pub forwards: Vec<Forward>,
}

/// One network adapter of the VM, as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetPort {
    /// Its index (`[[net]]` order at start).
    pub nic: u8,
    pub live: NetLive,
    /// The cable is in.
    pub connected: bool,
    /// The MAC the guest uses.
    pub mac: [u8; 6],
}

/// Interface and network names: what the VM file allows.
fn net_name_ok(kind: NetKind, s: &str) -> bool {
    let chars = s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    match kind {
        NetKind::Nat => s.is_empty(),
        NetKind::Bridged => !s.is_empty() && s.len() <= 15 && chars,
        NetKind::HostOnly => !s.is_empty() && s.len() <= 64 && !s.starts_with('.') && chars,
    }
}

impl NetLive {
    pub fn valid(&self) -> bool {
        let fwd_ok = |f: &Forward| f.host_addr.len() <= 64 && f.host_addr.parse::<std::net::IpAddr>().is_ok();
        net_name_ok(self.kind, &self.name)
            && self.forwards.len() <= FORWARDS_MAX
            && self.forwards.iter().all(fwd_ok)
            && (self.kind == NetKind::Nat || (self.forwards.is_empty() && !self.loopback && !self.block_private))
    }

    /// kind u8 | flags u8 | name (u8 len) | forwards: n u8, then addr
    /// (u8 len), host port u16, guest port u16 each.
    fn encode(&self, p: &mut Vec<u8>) {
        p.push(self.kind as u8);
        p.push(self.loopback as u8 | (self.block_private as u8) << 1);
        p.push(self.name.len() as u8);
        p.extend_from_slice(self.name.as_bytes());
        p.push(self.forwards.len() as u8);
        for f in &self.forwards {
            p.push(f.host_addr.len() as u8);
            p.extend_from_slice(f.host_addr.as_bytes());
            p.extend_from_slice(&f.host_port.to_le_bytes());
            p.extend_from_slice(&f.guest_port.to_le_bytes());
        }
    }

    fn decode(p: &[u8], o: &mut usize) -> Result<NetLive> {
        let short = || Error::Proto("short");
        let byte = |o: &mut usize| -> Result<u8> {
            let b = *p.get(*o).ok_or_else(short)?;
            *o += 1;
            Ok(b)
        };
        let str8 = |o: &mut usize| -> Result<String> {
            let n = *p.get(*o).ok_or_else(short)? as usize;
            let s = text(p.get(*o + 1..*o + 1 + n).ok_or_else(short)?)?;
            *o += 1 + n;
            Ok(s)
        };
        let u16_at = |o: &mut usize| -> Result<u16> {
            let b = p.get(*o..*o + 2).ok_or_else(short)?;
            *o += 2;
            Ok(u16::from_le_bytes([b[0], b[1]]))
        };
        let kind = match byte(o)? {
            0 => NetKind::Nat,
            1 => NetKind::Bridged,
            2 => NetKind::HostOnly,
            _ => return Err(Error::Proto("network mode")),
        };
        let flags = byte(o)?;
        let name = str8(o)?;
        let n = byte(o)? as usize;
        if n > FORWARDS_MAX {
            return Err(Error::Proto("port forwards"));
        }
        let mut forwards = Vec::with_capacity(n);
        for _ in 0..n {
            let host_addr = str8(o)?;
            let (host_port, guest_port) = (u16_at(o)?, u16_at(o)?);
            forwards.push(Forward { host_addr, host_port, guest_port });
        }
        let live = NetLive { kind, name, loopback: flags & 1 != 0, block_private: flags & 2 != 0, forwards };
        if flags & !3 != 0 || !live.valid() {
            return Err(Error::Proto("network adapter"));
        }
        Ok(live)
    }
}

/// VMM -> viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToViewer {
    /// Scanout mode changed (fb is tightly packed: stride = width*4).
    Mode { width: u32, height: u32 },
    /// Pixels changed in this rect.
    Damage { x: u32, y: u32, w: u32, h: u32 },
    /// Put this on the host clipboard.
    ClipboardSet(String),
    /// Human-readable status ("running", "paused", "guest agent connected").
    Status(String),
    /// VM is gone; viewer should close.
    Closed,
    /// The guest is recording from the host microphone (true) or stopped.
    Mic(bool),
    /// The host camera is on for the guest (true) or off.
    Camera(bool),
    /// The guest's pointer, for the viewer to show as its own.
    Cursor(GuestCursor),
    /// The guest is ready for files dragged over the window (`DragEnter`).
    /// `live`: it runs a drag of its own under the pointer, which the
    /// viewer drives with the mouse button; else the files go to a folder.
    DragReady { id: u32, live: bool },
    /// The dropped files are in the guest (`ok`), or why not. `place`:
    /// where they went, without a live drag.
    DropDone { id: u32, ok: bool, place: String },
    /// The host USB devices the VM has now. Sent once the VM has a USB
    /// controller to plug them into, and whenever the list changes.
    Usb(Vec<UsbPort>),
    /// The guest agent said hello: what it can do (`vroom_proto::features`).
    Agent { features: u64 },
    /// Coherence: the guest's windows, front to back (checked like the
    /// agent's message; the viewer still clips them to the screen).
    Windows(Vec<GuestWindow>),
    /// Files dragged out of the guest (answer to `ToVmm::DragOut`): their
    /// top-level names and whether each is a folder; none when the guest
    /// isn't dragging files.
    DragOutItems { id: u32, items: Vec<(String, bool)> },
    /// Coherence: the app icon of a guest window, `w` x `h` RGBA (not
    /// premultiplied), at most `vroom_proto::ICON_MAX` on a side; 0 x 0:
    /// none. Checked like the agent's message.
    WindowIcon { id: u32, w: u32, h: u32, rgba: Vec<u8> },
    /// Coherence can't work in the guest's session, and why.
    CoherenceNo(String),
    /// Coherence: the apps the guest can start by name (checked like the
    /// agent's message), for the bar to suggest.
    Apps(Vec<GuestApp>),
    /// Another guest monitor's mode (`head` 1..MAX_HEADS; head 0 is
    /// `Mode`). 0x0: the guest turned it off. Its pixels are in slot
    /// `head` of the framebuffer.
    HeadMode { head: u32, width: u32, height: u32 },
    /// Pixels changed on another monitor.
    HeadDamage { head: u32, x: u32, y: u32, w: u32, h: u32 },
    /// The network adapters, as they are now: sent at start and whenever
    /// one changes. `nat_ok`: an adapter can switch to NAT while the VM
    /// runs (its sandbox allows the connections NAT makes).
    Net { nat_ok: bool, ports: Vec<NetPort> },
}

/// What the guest's pointer looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestCursor {
    /// The guest gives none: it draws its own into the frame, or has none.
    Unset,
    /// It hid it.
    Hidden,
    /// `w` x `h` RGBA (not premultiplied), at most `CURSOR_MAX` on a side;
    /// the hot spot is inside it.
    Image { w: u32, h: u32, hot_x: u32, hot_y: u32, rgba: Vec<u8> },
}

/// Lifecycle requests from the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Pause,
    Resume,
    Snapshot,
    Reset,
    /// Ask the guest to shut down cleanly (agent / power button).
    Shutdown,
    /// Pull the plug.
    PowerOff,
    Suspend,
}

/// Viewer -> VMM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToVmm {
    /// Linux evdev key code.
    Key { code: u16, down: bool },
    /// Absolute pointer, 0..=POINTER_MAX on both axes.
    Pointer { x: u16, y: u16 },
    /// evdev button code (BTN_LEFT = 0x110 ...).
    Button { code: u16, down: bool },
    Wheel { dx: i16, dy: i16 },
    /// Window content size; guests that can follow will. `scale` 0: in
    /// logical pixels. Else (HiDPI) in physical pixels, and the window's
    /// scale in percent (200 on Retina), for the guest's UI.
    Resize { width: u32, height: u32, scale: u32 },
    Focus(bool),
    /// Host clipboard changed (only sent while focused).
    Clipboard(String),
    Command(Command),
    /// Files are dragged over the window: their top-level names, and
    /// whether each is a folder. Nothing else goes until they are dropped.
    DragEnter { id: u32, items: Vec<(String, bool)> },
    /// The drag left the window, or the drop was given up.
    DragCancel { id: u32 },
    /// Dropped: comes with a pipe (SCM_RIGHTS) the viewer writes the
    /// files into, as guest agent frames (DropEntry, DropData, DropEnd).
    Drop { id: u32 },
    /// Plug a host USB device in: comes with the socket of the helper the
    /// viewer started for it (SCM_RIGHTS), which the VMM talks to as it
    /// does to the helpers it starts itself.
    UsbAttach(UsbDev),
    /// Pull the device on this port pair out; its helper is told to go.
    UsbDetach { port: u8 },
    /// Coherence on or off (the guest agent shows and hides its taskbar).
    Coherence(bool),
    /// Coherence: do this to a guest window.
    Window { id: u32, op: WindowOp },
    /// The mouse left the window with the guest's button held (`end`
    /// false): is the guest dragging files out? Or (`end`) that drag is
    /// over, the guest may forget it.
    DragOut { id: u32, end: bool },
    /// The Mac drop wants item `item` of a drag out: comes with a pipe
    /// (SCM_RIGHTS) the VMM writes it into, as guest agent frames
    /// (DropEntry, DropData, DropEnd, or DragOutFail), checked.
    DragOutGet { id: u32, item: u32 },
    /// Start the guest app with this name (`vroom_proto::line_ok`).
    StartApp(String),
    /// Another monitor's window changed size (as `Resize`, for monitor
    /// `head` 1..MAX_HEADS). 0x0: its window is closed, unplug it.
    HeadResize { head: u32, width: u32, height: u32, scale: u32 },
    /// Absolute pointer on another monitor's window, 0..=POINTER_MAX.
    HeadPointer { head: u32, x: u16, y: u16 },
    /// Plug network adapter `nic`'s cable in, or pull it out.
    NetConnect { nic: u8, on: bool },
    /// Put adapter `nic` on another network, same device and MAC. Bridged
    /// and host-only come with the vroom-netd socket the viewer asked for
    /// (SCM_RIGHTS); `port_mac` is the MAC that port passes (netd's
    /// answer). NAT: none, `port_mac` unused; `forwards` are what the VM
    /// file asks for.
    NetSet { nic: u8, live: NetLive, port_mac: [u8; 6] },
}

fn frame(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(ty);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn get_u32(p: &[u8], i: usize) -> Result<u32> {
    let b = p.get(i * 4..i * 4 + 4).ok_or(Error::Proto("short"))?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// A monitor other than the first.
fn head(h: u32) -> Result<u32> {
    if (1..MAX_HEADS as u32).contains(&h) {
        Ok(h)
    } else {
        Err(Error::Proto("monitor"))
    }
}

/// id u32, then dir u8 | len u16 | name for each item.
fn items_payload(id: u32, items: &[(String, bool)]) -> Vec<u8> {
    let mut p = id.to_le_bytes().to_vec();
    for (name, dir) in items {
        p.push(*dir as u8);
        p.extend_from_slice(&(name.len() as u16).to_le_bytes());
        p.extend_from_slice(name.as_bytes());
    }
    p
}

fn parse_items(p: &[u8]) -> Result<(u32, Vec<(String, bool)>)> {
    let id = get_u32(p, 0)?;
    let mut items = Vec::new();
    let mut o = 4;
    while o < p.len() {
        if items.len() >= DRAG_ITEMS_MAX {
            return Err(Error::Proto("drag items"));
        }
        let len = u16::from_le_bytes(p.get(o + 1..o + 3).ok_or(Error::Proto("short"))?.try_into().map_err(|_| Error::Proto("short"))?) as usize;
        let name = text(p.get(o + 3..o + 3 + len).ok_or(Error::Proto("short"))?)?;
        items.push((name, p[o] != 0));
        o += 3 + len;
    }
    Ok((id, items))
}

fn text(p: &[u8]) -> Result<String> {
    std::str::from_utf8(p).map(|s| s.to_string()).map_err(|_| Error::Proto("utf-8"))
}

/// One short line of plain text (`vroom_proto::line_ok`).
fn line(p: &[u8]) -> Result<String> {
    let s = text(p)?;
    if !vroom_proto::line_ok(&s) {
        return Err(Error::Proto("line"));
    }
    Ok(s)
}

impl ToViewer {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ToViewer::Mode { width, height } => frame(1, &u32s(&[*width, *height])),
            ToViewer::Damage { x, y, w, h } => frame(2, &u32s(&[*x, *y, *w, *h])),
            ToViewer::ClipboardSet(s) => frame(3, s.as_bytes()),
            ToViewer::Status(s) => frame(4, s.as_bytes()),
            ToViewer::Closed => frame(5, &[]),
            ToViewer::Mic(on) => frame(6, &[*on as u8]),
            ToViewer::Cursor(GuestCursor::Unset) => frame(7, &[0]),
            ToViewer::Cursor(GuestCursor::Hidden) => frame(7, &[1]),
            ToViewer::Cursor(GuestCursor::Image { w, h, hot_x, hot_y, rgba }) => {
                let mut p = vec![2];
                p.extend_from_slice(&u32s(&[*w, *h, *hot_x, *hot_y]));
                p.extend_from_slice(rgba);
                frame(7, &p)
            }
            ToViewer::Camera(on) => frame(10, &[*on as u8]),
            ToViewer::DragReady { id, live } => {
                let mut p = id.to_le_bytes().to_vec();
                p.push(*live as u8);
                frame(8, &p)
            }
            ToViewer::DropDone { id, ok, place } => {
                let mut p = id.to_le_bytes().to_vec();
                p.push(*ok as u8);
                p.extend_from_slice(place.as_bytes());
                frame(9, &p)
            }
            ToViewer::Usb(ports) => {
                let mut p = vec![ports.len() as u8];
                for u in ports {
                    p.push(u.port);
                    p.push(u.here as u8 | (u.always as u8) << 1);
                    u.dev.encode(&mut p);
                }
                frame(11, &p)
            }
            ToViewer::Agent { features } => frame(12, &features.to_le_bytes()),
            ToViewer::Windows(list) => frame(13, &vroom_proto::encode_windows(list)),
            ToViewer::DragOutItems { id, items } => frame(14, &items_payload(*id, items)),
            ToViewer::WindowIcon { id, w, h, rgba } => frame(18, &vroom_proto::encode_icon(*id, *w, *h, rgba)),
            ToViewer::CoherenceNo(s) => frame(19, s.as_bytes()),
            // Well above the others (other work adds types in the 20s).
            ToViewer::Apps(list) => frame(60, &vroom_proto::encode_apps(list)),
            ToViewer::HeadMode { head, width, height } => frame(16, &u32s(&[*head, *width, *height])),
            ToViewer::HeadDamage { head, x, y, w, h } => frame(17, &u32s(&[*head, *x, *y, *w, *h])),
            ToViewer::Net { nat_ok, ports } => {
                let mut p = vec![*nat_ok as u8, ports.len() as u8];
                for n in ports {
                    p.push(n.nic);
                    p.push(n.connected as u8);
                    p.extend_from_slice(&n.mac);
                    n.live.encode(&mut p);
                }
                frame(40, &p)
            }
        }
    }

    fn decode(ty: u8, p: &[u8]) -> Result<Self> {
        Ok(match ty {
            16 if p.len() == 12 => ToViewer::HeadMode { head: head(get_u32(p, 0)?)?, width: get_u32(p, 1)?, height: get_u32(p, 2)? },
            17 if p.len() == 20 => {
                ToViewer::HeadDamage { head: head(get_u32(p, 0)?)?, x: get_u32(p, 1)?, y: get_u32(p, 2)?, w: get_u32(p, 3)?, h: get_u32(p, 4)? }
            }
            1 => ToViewer::Mode { width: get_u32(p, 0)?, height: get_u32(p, 1)? },
            2 => ToViewer::Damage { x: get_u32(p, 0)?, y: get_u32(p, 1)?, w: get_u32(p, 2)?, h: get_u32(p, 3)? },
            3 => ToViewer::ClipboardSet(text(p)?),
            4 => ToViewer::Status(text(p)?),
            5 => ToViewer::Closed,
            6 => ToViewer::Mic(*p.first().ok_or(Error::Proto("short"))? != 0),
            7 => ToViewer::Cursor(match p.first() {
                Some(0) => GuestCursor::Unset,
                Some(1) => GuestCursor::Hidden,
                Some(2) => {
                    let q = &p[1..];
                    let (w, h, hot_x, hot_y) = (get_u32(q, 0)?, get_u32(q, 1)?, get_u32(q, 2)?, get_u32(q, 3)?);
                    if w == 0 || h == 0 || w > CURSOR_MAX || h > CURSOR_MAX || hot_x >= w || hot_y >= h {
                        return Err(Error::Proto("cursor size"));
                    }
                    let rgba = &q[16..];
                    if rgba.len() != w as usize * h as usize * 4 {
                        return Err(Error::Proto("cursor size"));
                    }
                    GuestCursor::Image { w, h, hot_x, hot_y, rgba: rgba.to_vec() }
                }
                _ => return Err(Error::Proto("unknown cursor")),
            }),
            10 => ToViewer::Camera(*p.first().ok_or(Error::Proto("short"))? != 0),
            8 if p.len() == 5 => ToViewer::DragReady { id: get_u32(p, 0)?, live: p[4] != 0 },
            14 => {
                let (id, items) = parse_items(p)?;
                ToViewer::DragOutItems { id, items }
            }
            9 if p.len() >= 5 => ToViewer::DropDone { id: get_u32(p, 0)?, ok: p[4] != 0, place: text(&p[5..])?.chars().take(256).collect() },
            11 => {
                let n = *p.first().ok_or(Error::Proto("short"))? as usize;
                if n > USB_PORTS_MAX {
                    return Err(Error::Proto("usb ports"));
                }
                let mut o = 1;
                let mut ports = Vec::with_capacity(n);
                for _ in 0..n {
                    let (port, flags) = (*p.get(o).ok_or(Error::Proto("short"))?, *p.get(o + 1).ok_or(Error::Proto("short"))?);
                    o += 2;
                    let dev = UsbDev::decode(p, &mut o)?;
                    ports.push(UsbPort { port, dev, here: flags & 1 != 0, always: flags & 2 != 0 });
                }
                if o != p.len() {
                    return Err(Error::Proto("usb ports"));
                }
                ToViewer::Usb(ports)
            }
            12 if p.len() == 8 => ToViewer::Agent { features: u64::from(get_u32(p, 0)?) | u64::from(get_u32(p, 1)?) << 32 },
            13 => ToViewer::Windows(vroom_proto::decode_windows(p).ok_or(Error::Proto("windows"))?),
            18 => {
                let (id, w, h, rgba) = vroom_proto::decode_icon(p).ok_or(Error::Proto("window icon"))?;
                ToViewer::WindowIcon { id, w, h, rgba }
            }
            19 => ToViewer::CoherenceNo(line(p)?),
            60 => ToViewer::Apps(vroom_proto::decode_apps(p).ok_or(Error::Proto("apps"))?),
            40 => {
                let (nat_ok, n) = match p {
                    [f @ (0 | 1), n, ..] => (*f != 0, *n as usize),
                    _ => return Err(Error::Proto("network adapters")),
                };
                if n > NET_PORTS_MAX {
                    return Err(Error::Proto("network adapters"));
                }
                let mut o = 2;
                let mut ports = Vec::with_capacity(n);
                for _ in 0..n {
                    let head = p.get(o..o + 8).ok_or(Error::Proto("short"))?;
                    let (nic, flags) = (head[0], head[1]);
                    if nic as usize >= NET_PORTS_MAX || flags > 1 || ports.iter().any(|x: &NetPort| x.nic == nic) {
                        return Err(Error::Proto("network adapters"));
                    }
                    let mut mac = [0u8; 6];
                    mac.copy_from_slice(&head[2..8]);
                    o += 8;
                    let live = NetLive::decode(p, &mut o)?;
                    ports.push(NetPort { nic, live, connected: flags != 0, mac });
                }
                if o != p.len() {
                    return Err(Error::Proto("network adapters"));
                }
                ToViewer::Net { nat_ok, ports }
            }
            _ => return Err(Error::Proto("unknown viewer message")),
        })
    }
}

impl ToVmm {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ToVmm::Key { code, down } => frame(1, &u32s(&[*code as u32, *down as u32])),
            ToVmm::Pointer { x, y } => frame(2, &u32s(&[*x as u32, *y as u32])),
            ToVmm::Button { code, down } => frame(3, &u32s(&[*code as u32, *down as u32])),
            ToVmm::Wheel { dx, dy } => frame(4, &u32s(&[*dx as u16 as u32, *dy as u16 as u32])),
            ToVmm::Resize { width, height, scale } => frame(5, &u32s(&[*width, *height, *scale])),
            ToVmm::Focus(f) => frame(6, &[*f as u8]),
            ToVmm::Clipboard(s) => frame(7, s.as_bytes()),
            ToVmm::Command(c) => frame(8, &[*c as u8]),
            ToVmm::DragEnter { id, items } => frame(9, &items_payload(*id, items)),
            ToVmm::DragCancel { id } => frame(10, &id.to_le_bytes()),
            ToVmm::Drop { id } => frame(11, &id.to_le_bytes()),
            ToVmm::UsbAttach(d) => {
                let mut p = Vec::new();
                d.encode(&mut p);
                frame(12, &p)
            }
            ToVmm::UsbDetach { port } => frame(13, &[*port]),
            ToVmm::Coherence(on) => frame(14, &[*on as u8]),
            ToVmm::Window { id, op } => {
                let mut p = id.to_le_bytes().to_vec();
                p.extend_from_slice(&op.encode());
                frame(15, &p)
            }
            ToVmm::DragOut { id, end } => {
                let mut p = id.to_le_bytes().to_vec();
                p.push(*end as u8);
                frame(16, &p)
            }
            ToVmm::DragOutGet { id, item } => frame(17, &u32s(&[*id, *item])),
            ToVmm::StartApp(s) => frame(20, s.as_bytes()),
            ToVmm::HeadResize { head, width, height, scale } => frame(18, &u32s(&[*head, *width, *height, *scale])),
            ToVmm::HeadPointer { head, x, y } => frame(19, &u32s(&[*head, *x as u32, *y as u32])),
            ToVmm::NetConnect { nic, on } => frame(40, &[*nic, *on as u8]),
            ToVmm::NetSet { nic, live, port_mac } => {
                let mut p = vec![*nic];
                p.extend_from_slice(port_mac);
                live.encode(&mut p);
                frame(41, &p)
            }
        }
    }

    fn decode(ty: u8, p: &[u8]) -> Result<Self> {
        let small = |v: u32| u16::try_from(v).map_err(|_| Error::Proto("range"));
        Ok(match ty {
            1 => ToVmm::Key { code: small(get_u32(p, 0)?)?, down: get_u32(p, 1)? != 0 },
            2 => {
                let (x, y) = (small(get_u32(p, 0)?)?, small(get_u32(p, 1)?)?);
                ToVmm::Pointer { x: x.min(POINTER_MAX), y: y.min(POINTER_MAX) }
            }
            3 => ToVmm::Button { code: small(get_u32(p, 0)?)?, down: get_u32(p, 1)? != 0 },
            4 => ToVmm::Wheel { dx: get_u32(p, 0)? as u16 as i16, dy: get_u32(p, 1)? as u16 as i16 },
            5 => {
                // The scale is optional (older viewers, scripts): 0.
                let scale = if p.len() >= 12 { get_u32(p, 2)? } else { 0 };
                if scale != 0 && !(SCALE_MIN..=SCALE_MAX).contains(&scale) {
                    return Err(Error::Proto("scale"));
                }
                ToVmm::Resize { width: get_u32(p, 0)?, height: get_u32(p, 1)?, scale }
            }
            6 => ToVmm::Focus(*p.first().ok_or(Error::Proto("short"))? != 0),
            7 => ToVmm::Clipboard(text(p)?),
            8 => ToVmm::Command(match p.first() {
                Some(0) => Command::Pause,
                Some(1) => Command::Resume,
                Some(2) => Command::Snapshot,
                Some(3) => Command::Reset,
                Some(4) => Command::Shutdown,
                Some(5) => Command::PowerOff,
                Some(6) => Command::Suspend,
                _ => return Err(Error::Proto("unknown command")),
            }),
            9 => {
                let (id, items) = parse_items(p)?;
                if items.is_empty() {
                    return Err(Error::Proto("drag items"));
                }
                ToVmm::DragEnter { id, items }
            }
            10 if p.len() == 4 => ToVmm::DragCancel { id: get_u32(p, 0)? },
            11 if p.len() == 4 => ToVmm::Drop { id: get_u32(p, 0)? },
            12 => {
                let mut o = 0;
                let d = UsbDev::decode(p, &mut o)?;
                if o != p.len() {
                    return Err(Error::Proto("usb device"));
                }
                ToVmm::UsbAttach(d)
            }
            13 if p.len() == 1 => ToVmm::UsbDetach { port: p[0] },
            14 if p.len() == 1 => ToVmm::Coherence(p[0] != 0),
            15 if p.len() > 4 => ToVmm::Window { id: get_u32(p, 0)?, op: WindowOp::decode(&p[4..]).ok_or(Error::Proto("window op"))? },
            16 if p.len() == 5 => ToVmm::DragOut { id: get_u32(p, 0)?, end: p[4] != 0 },
            17 if p.len() == 8 => ToVmm::DragOutGet { id: get_u32(p, 0)?, item: get_u32(p, 1)? },
            20 => ToVmm::StartApp(line(p)?),
            18 if p.len() == 16 => {
                let (width, height, scale) = (get_u32(p, 1)?, get_u32(p, 2)?, get_u32(p, 3)?);
                if scale != 0 && !(SCALE_MIN..=SCALE_MAX).contains(&scale) {
                    return Err(Error::Proto("scale"));
                }
                if (width == 0) != (height == 0) {
                    return Err(Error::Proto("size"));
                }
                ToVmm::HeadResize { head: head(get_u32(p, 0)?)?, width, height, scale }
            }
            19 if p.len() == 12 => {
                let (x, y) = (small(get_u32(p, 1)?)?, small(get_u32(p, 2)?)?);
                ToVmm::HeadPointer { head: head(get_u32(p, 0)?)?, x: x.min(POINTER_MAX), y: y.min(POINTER_MAX) }
            }
            40 if p.len() == 2 && (p[0] as usize) < NET_PORTS_MAX && p[1] <= 1 => ToVmm::NetConnect { nic: p[0], on: p[1] != 0 },
            41 if p.len() >= 7 && (p[0] as usize) < NET_PORTS_MAX => {
                let mut port_mac = [0u8; 6];
                port_mac.copy_from_slice(&p[1..7]);
                let mut o = 7;
                let live = NetLive::decode(p, &mut o)?;
                if o != p.len() || port_mac[0] & 1 != 0 {
                    return Err(Error::Proto("network adapter"));
                }
                ToVmm::NetSet { nic: p[0], live, port_mac }
            }
            _ => return Err(Error::Proto("unknown vmm message")),
        })
    }
}

/// Stream decoder, generic over direction.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    fn next_raw(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > MAX_MSG {
            // Unrecoverable framing: drop everything.
            self.buf.clear();
            return Err(Error::Proto("message too large"));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let ty = self.buf[0];
        let p = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Ok(Some((ty, p)))
    }

    pub fn next_to_vmm(&mut self) -> Result<Option<ToVmm>> {
        match self.next_raw()? {
            Some((t, p)) => ToVmm::decode(t, &p).map(Some),
            None => Ok(None),
        }
    }

    pub fn next_to_viewer(&mut self) -> Result<Option<ToViewer>> {
        match self.next_raw()? {
            Some((t, p)) => ToViewer::decode(t, &p).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let vm = [
            ToVmm::Key { code: 30, down: true },
            ToVmm::Pointer { x: 100, y: 32767 },
            ToVmm::Button { code: 0x110, down: false },
            ToVmm::Wheel { dx: -1, dy: 3 },
            ToVmm::Resize { width: 1920, height: 1080, scale: 0 },
            ToVmm::Resize { width: 3456, height: 2234, scale: 200 },
            ToVmm::Focus(true),
            ToVmm::Clipboard("x".into()),
            ToVmm::Command(Command::Snapshot),
            ToVmm::DragEnter { id: 4, items: vec![("a.txt".into(), false), ("dir".into(), true)] },
            ToVmm::DragCancel { id: 4 },
            ToVmm::Drop { id: 4 },
            ToVmm::UsbAttach(UsbDev { id: "1209:000a".into(), serial: "A1".into(), name: "dev board".into() }),
            ToVmm::UsbDetach { port: 3 },
            ToVmm::Coherence(true),
            ToVmm::Window { id: 9, op: WindowOp::Activate },
            ToVmm::Window { id: 9, op: WindowOp::Move { x: -3, y: 4, w: 100, h: 50 } },
            ToVmm::DragOut { id: 5, end: false },
            ToVmm::DragOut { id: 5, end: true },
            ToVmm::DragOutGet { id: 5, item: 2 },
            ToVmm::StartApp("Paint".into()),
            ToVmm::HeadResize { head: 1, width: 1920, height: 1080, scale: 200 },
            ToVmm::HeadResize { head: 3, width: 0, height: 0, scale: 0 },
            ToVmm::HeadPointer { head: 2, x: 5, y: 32767 },
        ];
        let mut d = Decoder::default();
        for m in &vm {
            d.push(&m.encode());
        }
        for m in &vm {
            assert_eq!(d.next_to_vmm().unwrap().as_ref(), Some(m));
        }
        let v = [
            ToViewer::Mode { width: 800, height: 600 },
            ToViewer::Damage { x: 1, y: 2, w: 3, h: 4 },
            ToViewer::Closed,
            ToViewer::Mic(true),
            ToViewer::Cursor(GuestCursor::Hidden),
            ToViewer::Cursor(GuestCursor::Image { w: 2, h: 1, hot_x: 1, hot_y: 0, rgba: vec![1, 2, 3, 4, 5, 6, 7, 8] }),
            ToViewer::Cursor(GuestCursor::Unset),
            ToViewer::DragReady { id: 4, live: true },
            ToViewer::Camera(true),
            ToViewer::DropDone { id: 4, ok: false, place: "Desktop".into() },
            ToViewer::Usb(vec![]),
            ToViewer::Usb(vec![
                UsbPort { port: 0, dev: UsbDev { id: "1209:000a".into(), serial: String::new(), name: "x".into() }, here: true, always: true },
                UsbPort { port: 5, dev: UsbDev { id: "05ac:12a8".into(), serial: "s".into(), name: String::new() }, here: false, always: false },
            ]),
            ToViewer::Agent { features: 1 << 40 | 128 },
            ToViewer::Windows(vec![GuestWindow { id: 1, x: 0, y: -2, w: 3, h: 4, flags: 4, title: "Paint".into() }]),
            ToViewer::DragOutItems { id: 5, items: vec![("a b.txt".into(), false), ("dir".into(), true)] },
            ToViewer::DragOutItems { id: 6, items: Vec::new() },
            ToViewer::WindowIcon { id: 1, w: 1, h: 1, rgba: vec![1, 2, 3, 4] },
            ToViewer::WindowIcon { id: 1, w: 0, h: 0, rgba: Vec::new() },
            ToViewer::CoherenceNo("a Wayland session".into()),
            ToViewer::Apps(vec![GuestApp { name: "Paint".into(), w: 1, h: 1, rgba: vec![1, 2, 3, 4] }]),
            ToViewer::HeadMode { head: 1, width: 1024, height: 768 },
            ToViewer::HeadMode { head: 3, width: 0, height: 0 },
            ToViewer::HeadDamage { head: 2, x: 1, y: 2, w: 3, h: 4 },
        ];
        for m in &v {
            d.push(&m.encode());
        }
        for m in &v {
            assert_eq!(d.next_to_viewer().unwrap().as_ref(), Some(m));
        }
    }

    #[test]
    fn hostile_lengths() {
        let mut d = Decoder::default();
        d.push(&[1, 0xff, 0xff, 0xff, 0xff]);
        assert!(d.next_to_vmm().is_err());
        d.push(&[2, 4, 0, 0, 0, 1, 2, 3, 4]); // pointer with 1 field
        assert!(d.next_to_vmm().is_err());
        // resize without a scale: logical pixels
        d.push(&[5, 8, 0, 0, 0, 0, 4, 0, 0, 0, 3, 0, 0]);
        assert_eq!(d.next_to_vmm().unwrap(), Some(ToVmm::Resize { width: 1024, height: 768, scale: 0 }));
        d.push(&ToVmm::Resize { width: 1, height: 1, scale: 99 }.encode());
        assert!(d.next_to_vmm().is_err());
        // Cursors: pixels must match the size, the hot spot be inside.
        let img = |w: u32, h: u32, hx: u32, n: usize| {
            let mut p = vec![2u8];
            p.extend(u32s(&[w, h, hx, 0]));
            p.extend(vec![0u8; n]);
            frame(7, &p)
        };
        d.push(&img(2, 2, 0, 15));
        assert!(d.next_to_viewer().is_err());
        d.push(&img(2, 2, 2, 16));
        assert!(d.next_to_viewer().is_err());
        d.push(&img(CURSOR_MAX + 1, 1, 0, (CURSOR_MAX as usize + 1) * 4));
        assert!(d.next_to_viewer().is_err());
        d.push(&frame(7, &[2, 1, 0]));
        assert!(d.next_to_viewer().is_err());
        d.push(&img(2, 2, 1, 16));
        assert!(matches!(d.next_to_viewer().unwrap(), Some(ToViewer::Cursor(GuestCursor::Image { w: 2, h: 2, .. }))));
        // Monitors: 1..MAX_HEADS only, sizes both or neither.
        for h in [0, MAX_HEADS as u32, u32::MAX] {
            d.push(&ToViewer::HeadMode { head: h, width: 1, height: 1 }.encode());
            assert!(d.next_to_viewer().is_err(), "{h}");
            d.push(&ToVmm::HeadPointer { head: h, x: 0, y: 0 }.encode());
            assert!(d.next_to_vmm().is_err(), "{h}");
        }
        d.push(&ToVmm::HeadResize { head: 1, width: 0, height: 5, scale: 0 }.encode());
        assert!(d.next_to_vmm().is_err());
        d.push(&ToVmm::HeadResize { head: 1, width: 5, height: 5, scale: 7 }.encode());
        assert!(d.next_to_vmm().is_err());
        d.push(&frame(19, &u32s(&[1, 1 << 16, 0])));
        assert!(d.next_to_vmm().is_err(), "pointer out of range");
    }

    #[test]
    fn usb_devices_are_checked() {
        let dev = |id: &str, serial: &str, name: &str| UsbDev { id: id.into(), serial: serial.into(), name: name.into() };
        let mut d = Decoder::default();
        for bad in [dev("1209:0a", "", ""), dev("1209:000g", "", ""), dev("1209000a", "", ""), dev("1209:000a", "a\nb", ""), dev("1209:000a", "", &"n".repeat(129))] {
            assert!(!bad.valid(), "{bad:?}");
            d.push(&ToVmm::UsbAttach(bad).encode());
            assert!(d.next_to_vmm().is_err());
        }
        // Trailing bytes, a list longer than it says, too many ports.
        let mut p = Vec::new();
        dev("1209:000a", "", "").encode(&mut p);
        p.push(0);
        d.push(&frame(12, &p));
        assert!(d.next_to_vmm().is_err());
        d.push(&frame(11, &[1]));
        assert!(d.next_to_viewer().is_err());
        d.push(&frame(11, &[USB_PORTS_MAX as u8 + 1]));
        assert!(d.next_to_viewer().is_err());
        d.push(&frame(13, &[1, 2]));
        assert!(d.next_to_vmm().is_err());
    }

    fn nat(forwards: Vec<Forward>) -> NetLive {
        NetLive { kind: NetKind::Nat, name: String::new(), loopback: false, block_private: true, forwards }
    }

    #[test]
    fn network_adapters_roundtrip_and_are_checked() {
        let fwd = Forward { host_addr: "127.0.0.1".into(), host_port: 2222, guest_port: 22 };
        let bridged = NetLive { kind: NetKind::Bridged, name: "en0".into(), loopback: false, block_private: false, forwards: vec![] };
        let msgs = [
            ToVmm::NetConnect { nic: 1, on: false },
            ToVmm::NetSet { nic: 0, live: nat(vec![fwd.clone()]), port_mac: [0; 6] },
            ToVmm::NetSet { nic: 7, live: bridged.clone(), port_mac: [0x3e, 1, 2, 3, 4, 5] },
        ];
        let mut d = Decoder::default();
        for m in &msgs {
            d.push(&m.encode());
            assert_eq!(d.next_to_vmm().unwrap().as_ref(), Some(m));
        }
        let v = ToViewer::Net {
            nat_ok: true,
            ports: vec![
                NetPort { nic: 0, live: nat(vec![fwd.clone()]), connected: true, mac: [2, 1, 2, 3, 4, 5] },
                NetPort { nic: 1, live: bridged.clone(), connected: false, mac: [2, 1, 2, 3, 4, 6] },
            ],
        };
        d.push(&v.encode());
        assert_eq!(d.next_to_viewer().unwrap(), Some(v));
        // Bad: adapter 8, a name for NAT, no name for bridged, odd names,
        // forwards on a bridged adapter, a bad address, a multicast MAC,
        // trailing bytes, the same adapter twice.
        let bad_live = [
            NetLive { name: "en0".into(), ..nat(vec![]) },
            NetLive { name: String::new(), ..bridged.clone() },
            NetLive { name: "en0/../x".into(), ..bridged.clone() },
            NetLive { name: "a".repeat(16), ..bridged.clone() },
            NetLive { forwards: vec![fwd.clone()], ..bridged.clone() },
            NetLive { block_private: true, ..bridged.clone() },
            nat(vec![Forward { host_addr: "localhost".into(), ..fwd.clone() }]),
        ];
        for l in bad_live {
            assert!(!l.valid(), "{l:?}");
            d.push(&ToVmm::NetSet { nic: 0, live: l, port_mac: [0; 6] }.encode());
            assert!(d.next_to_vmm().is_err());
        }
        d.push(&ToVmm::NetConnect { nic: 8, on: true }.encode());
        assert!(d.next_to_vmm().is_err());
        d.push(&ToVmm::NetSet { nic: 0, live: bridged.clone(), port_mac: [1, 0, 0, 0, 0, 0] }.encode());
        assert!(d.next_to_vmm().is_err());
        let mut f = ToVmm::NetConnect { nic: 0, on: true }.encode();
        f[1] = 3;
        f.push(0);
        d.push(&f);
        assert!(d.next_to_vmm().is_err());
        let port = |nic| NetPort { nic, live: bridged.clone(), connected: true, mac: [2; 6] };
        d.push(&ToViewer::Net { nat_ok: false, ports: vec![port(1), port(1)] }.encode());
        assert!(d.next_to_viewer().is_err());
        d.push(&ToViewer::Net { nat_ok: false, ports: (0..9).map(port).collect() }.encode());
        assert!(d.next_to_viewer().is_err());
        d.push(&frame(40, &[1, 1, 0, 1]));
        assert!(d.next_to_viewer().is_err());
    }

    #[test]
    fn icons_and_lines_are_checked() {
        let mut d = Decoder::default();
        d.push(&frame(18, &vroom_proto::encode_icon(1, 2, 2, &[0; 12])));
        assert!(d.next_to_viewer().is_err());
        d.push(&frame(18, &vroom_proto::encode_icon(1, 65, 1, &[0; 260])));
        assert!(d.next_to_viewer().is_err());
        for bad in ["", "a\nb", &"x".repeat(vroom_proto::START_MAX + 1)] {
            d.push(&frame(20, bad.as_bytes()));
            assert!(d.next_to_vmm().is_err(), "{bad:?}");
            d.push(&frame(19, bad.as_bytes()));
            assert!(d.next_to_viewer().is_err(), "{bad:?}");
        }
    }
}
