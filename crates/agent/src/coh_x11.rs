//! Coherence on Linux with X11: the windows on screen, through the
//! window manager's EWMH properties, and what the host may do to them,
//! through the client messages a taskbar sends (EWMH again).
//!
//! The list comes from `_NET_CLIENT_LIST` (the managed windows) and the
//! root's children (their stacking order, and the pop-ups the window
//! manager leaves alone: menus, tooltips). What can be seen of a window is
//! its client area with the window manager's frame around it, less the
//! shadows GTK draws inside its windows (ewmh.rs). Events on the root and
//! on each window say when to look again.
//!
//! Wayland can't do this: there, only the compositor knows where windows
//! are, and it doesn't tell other programs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use vroom_proto::{win_flags, GuestWindow, WindowOp};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, MapState, PropMode, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::coherence::{Icon, Poke, ICON_SIZE};
use crate::ewmh::{self, Extents, Top};

macro_rules! atoms {
    ($($name:ident),* $(,)?) => {
        #[allow(non_snake_case)]
        #[derive(Clone, Copy)]
        struct Atoms { $($name: u32,)* }
        impl Atoms {
            #[allow(non_snake_case)]
            fn new(c: &RustConnection) -> Option<Atoms> {
                $(let $name = c.intern_atom(false, stringify!($name).as_bytes()).ok()?;)*
                Some(Atoms { $($name: $name.reply().ok()?.atom,)* })
            }
        }
    };
}

atoms!(
    _NET_SUPPORTING_WM_CHECK,
    _NET_CLIENT_LIST,
    _NET_CLIENT_LIST_STACKING,
    _NET_ACTIVE_WINDOW,
    _NET_CURRENT_DESKTOP,
    _NET_WM_DESKTOP,
    _NET_WM_NAME,
    _NET_WM_ICON,
    _NET_WM_STATE,
    _NET_WM_STATE_HIDDEN,
    _NET_WM_STATE_MAXIMIZED_VERT,
    _NET_WM_STATE_MAXIMIZED_HORZ,
    _NET_WM_STATE_SKIP_TASKBAR,
    _NET_WM_WINDOW_TYPE,
    _NET_WM_WINDOW_TYPE_NORMAL,
    _NET_WM_WINDOW_TYPE_DESKTOP,
    _NET_WM_WINDOW_TYPE_DOCK,
    _NET_FRAME_EXTENTS,
    _GTK_FRAME_EXTENTS,
    _NET_CLOSE_WINDOW,
    _NET_MOVERESIZE_WINDOW,
    _NET_WORKAREA,
    WM_CHANGE_STATE,
    WM_STATE,
);

/// Most properties we read are small; icons are capped (32-bit units).
const ICON_LONGS: u32 = 1 << 18;

/// What we know of a window of the last list.
#[derive(Clone, Copy)]
struct Known {
    frame: Extents,
    shadow: Extents,
    /// Managed by the window manager (else a pop-up: nothing to do to it).
    managed: bool,
    maximized: bool,
}

pub struct X11Desktop {
    c: RustConnection,
    root: Window,
    a: Atoms,
    /// The windows of the last list.
    listed: HashMap<u32, Known>,
    /// Clients' frames (the root's child holding them).
    frames: HashMap<u32, u32>,
    /// The event thread: told to stop, and woken through this window.
    events: Option<(Arc<AtomicBool>, Window, std::thread::JoinHandle<()>)>,
    dock: Dock,
}

impl X11Desktop {
    /// The session's X server, if it is X11 with an EWMH window manager.
    pub fn open() -> Result<X11Desktop, String> {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland");
        if wayland {
            return Err("the guest runs a Wayland session, where only the compositor knows its windows: log in with an X11 session (GNOME: \"Ubuntu on Xorg\")".into());
        }
        let (c, n) = x11rb::connect(None).map_err(|e| format!("no X11 display in the guest's session ({e})"))?;
        let root = c.setup().roots.get(n).ok_or("no X11 screen")?.root;
        let a = Atoms::new(&c).ok_or("the X server didn't answer")?;
        let d = X11Desktop { c, root, a, listed: HashMap::new(), frames: HashMap::new(), events: None, dock: Dock::default() };
        if d.prop32(root, a._NET_SUPPORTING_WM_CHECK, 1).is_empty() {
            return Err("the guest's window manager doesn't tell where windows are (no EWMH)".into());
        }
        Ok(d)
    }

    fn prop32(&self, w: Window, p: u32, len: u32) -> Vec<u32> {
        self.c
            .get_property(false, w, p, AtomEnum::ANY, 0, len)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default()
    }

    fn extents(v: &[u32]) -> Extents {
        if v.len() == 4 {
            [v[0], v[1], v[2], v[3]]
        } else {
            [0; 4]
        }
    }

    fn list(&mut self) -> Option<Vec<GuestWindow>> {
        let (c, a, root) = (&self.c, self.a, self.root);
        let clients: Vec<u32> = self.prop32(root, a._NET_CLIENT_LIST, 1024).into_iter().take(512).collect();
        let active = self.prop32(root, a._NET_ACTIVE_WINDOW, 1).first().copied().unwrap_or(0);
        let desk = self.prop32(root, a._NET_CURRENT_DESKTOP, 1).first().copied();
        self.frames.retain(|k, _| clients.contains(k));
        // Frames of new clients: the root's child above each.
        for &cl in &clients {
            if self.frames.contains_key(&cl) {
                continue;
            }
            let mut w = cl;
            for _ in 0..8 {
                let Some(t) = c.query_tree(w).ok().and_then(|q| q.reply().ok()) else { break };
                if t.parent == root || t.parent == 0 {
                    break;
                }
                w = t.parent;
            }
            self.frames.insert(cl, w);
        }
        let tree = c.query_tree(root).ok()?.reply().ok()?.children;
        let scr = c.get_geometry(root).ok()?.reply().ok()?;
        let (sw, sh) = (scr.width as i32, scr.height as i32);
        let by_frame: HashMap<u32, u32> = self.frames.iter().map(|(cl, f)| (*f, *cl)).collect();
        // Minimized clients: mutter keeps them mapped, under its guard window.
        let states: Vec<_> = clients.iter().map(|w| c.get_property(false, *w, a._NET_WM_STATE, AtomEnum::ATOM, 0, 64).ok()).collect();
        let minimized: Vec<u32> = clients
            .iter()
            .zip(states)
            .filter_map(|(w, s)| {
                let r = s?.reply().ok()?;
                let hidden = r.value32()?.any(|x| x == a._NET_WM_STATE_HIDDEN);
                hidden.then_some(*w)
            })
            .collect();
        // Frames (on screen or not) and pop-ups (the root's children no
        // window manager looks after, on screen).
        let tree: Vec<u32> = tree.into_iter().take(2048).collect();
        let attrs: Vec<_> = tree.iter().map(|w| c.get_window_attributes(*w).ok()).collect();
        let tree: Vec<(u32, Top)> = tree
            .iter()
            .zip(attrs)
            .map(|(w, a)| {
                let a = a.and_then(|a| a.reply().ok());
                let shown = a.as_ref().is_some_and(|a| a.map_state == MapState::VIEWABLE);
                let kind = match by_frame.get(w) {
                    Some(cl) => Top::Frame(*cl, shown && !minimized.contains(cl)),
                    None if shown && a.is_some_and(|a| a.override_redirect) => Top::Popup,
                    None => Top::Other,
                };
                (*w, kind)
            })
            .collect();
        let order = ewmh::order(&tree, &clients);
        let popups: Vec<u32> = tree.iter().filter(|(_, t)| *t == Top::Popup).map(|(w, _)| *w).collect();

        // Everything about each window, asked all at once.
        let ask = |w: u32, p: u32, len: u32| c.get_property(false, w, p, AtomEnum::ANY, 0, len).ok();
        struct Asked<'a> {
            geo: Option<x11rb::cookie::Cookie<'a, RustConnection, x11rb::protocol::xproto::GetGeometryReply>>,
            at: Option<x11rb::cookie::Cookie<'a, RustConnection, x11rb::protocol::xproto::TranslateCoordinatesReply>>,
            props: Vec<Option<x11rb::cookie::Cookie<'a, RustConnection, x11rb::protocol::xproto::GetPropertyReply>>>,
        }
        let names = [a._NET_WM_NAME, u32::from(AtomEnum::WM_NAME), a._NET_WM_STATE, a._NET_WM_WINDOW_TYPE, u32::from(AtomEnum::WM_TRANSIENT_FOR), a._NET_FRAME_EXTENTS, a._GTK_FRAME_EXTENTS, a._NET_WM_DESKTOP];
        let asked: Vec<Asked> = order
            .iter()
            .take(256)
            .map(|&w| Asked { geo: c.get_geometry(w).ok(), at: c.translate_coordinates(w, root, 0, 0).ok(), props: names.iter().map(|p| ask(w, *p, 256)).collect() })
            .collect();
        let mut out = Vec::new();
        let mut listed = HashMap::new();
        for (&w, q) in order.iter().zip(asked) {
            let geo = q.geo.and_then(|g| g.reply().ok());
            let at = q.at.and_then(|t| t.reply().ok());
            let props: Vec<Option<x11rb::protocol::xproto::GetPropertyReply>> = q.props.into_iter().map(|p| p.and_then(|p| p.reply().ok())).collect();
            let (Some(geo), Some(at)) = (geo, at) else { continue };
            let v32 = |i: usize| -> Vec<u32> { props[i].as_ref().and_then(|r| r.value32().map(|v| v.collect())).unwrap_or_default() };
            let text = |i: usize| props[i].as_ref().map(|r| String::from_utf8_lossy(&r.value).into_owned()).unwrap_or_default();
            let managed = clients.contains(&w);
            let state = v32(2);
            let types = v32(3);
            if types.contains(&a._NET_WM_WINDOW_TYPE_DESKTOP) || types.contains(&a._NET_WM_WINDOW_TYPE_DOCK) {
                continue;
            }
            // On another workspace (0xFFFFFFFF: on all of them).
            if let (Some(d), Some(&wd)) = (desk, v32(7).first()) {
                if managed && wd != d && wd != u32::MAX {
                    continue;
                }
            }
            let (frame, shadow) = (if managed { Self::extents(&v32(5)) } else { [0; 4] }, Self::extents(&v32(6)));
            let (x, y, ww, hh) = ewmh::visible((at.dst_x as i32, at.dst_y as i32, geo.width as u32, geo.height as u32), frame, shadow);
            let hidden = state.contains(&a._NET_WM_STATE_HIDDEN);
            // Off the screen, or a speck (GNOME keeps a 1x1 window at -200,-200).
            let off = x >= sw || y >= sh || x as i64 + ww as i64 <= 0 || y as i64 + hh as i64 <= 0;
            if !hidden && (off || ww < 2 || hh < 2) {
                continue;
            }
            let maximized = state.contains(&a._NET_WM_STATE_MAXIMIZED_VERT) && state.contains(&a._NET_WM_STATE_MAXIMIZED_HORZ);
            if !managed && !popups.contains(&w) {
                continue;
            }
            let mut title = text(0);
            if title.is_empty() {
                title = text(1);
            }
            let normal = types.is_empty() || types.contains(&a._NET_WM_WINDOW_TYPE_NORMAL);
            let app = managed && ewmh::is_app(normal, !v32(4).is_empty(), state.contains(&a._NET_WM_STATE_SKIP_TASKBAR)) && !title.is_empty();
            let mut flags = 0;
            if w == active {
                flags |= win_flags::FOREGROUND;
            }
            if hidden {
                flags |= win_flags::MINIMIZED;
            }
            if app {
                flags |= win_flags::APP;
            }
            if maximized {
                flags |= win_flags::MAXIMIZED;
            }
            // Minimized windows: listed (to bring back) only if they're apps.
            if hidden && !app {
                continue;
            }
            listed.insert(w, Known { frame, shadow, managed, maximized });
            out.push(GuestWindow { id: w, x, y, w: ww, h: hh, flags, title });
            if out.len() == vroom_proto::WINDOWS_MAX {
                break;
            }
        }
        self.listed = listed;
        Some(out)
    }

    /// A client message to the root window, as pagers send them.
    fn tell(&self, w: Window, ty: u32, data: [u32; 5]) {
        let ev = ClientMessageEvent::new(32, w, ty, data);
        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
        let _ = self.c.send_event(false, self.root, mask, ev);
    }
}

impl crate::coherence::Desktop for X11Desktop {
    fn windows(&mut self) -> Vec<GuestWindow> {
        self.list().unwrap_or_default()
    }

    fn icon(&mut self, id: u32) -> Option<Icon> {
        if !self.listed.get(&id).is_some_and(|k| k.managed) {
            return None;
        }
        let data = self.prop32(id, self.a._NET_WM_ICON, ICON_LONGS);
        ewmh::pick_icon(&data, ICON_SIZE)
    }

    fn act(&mut self, id: u32, op: WindowOp) {
        let Some(k) = self.listed.get(&id).copied() else { return };
        if !k.managed {
            return;
        }
        let a = self.a;
        // Source 2: a pager or taskbar, which the window manager obeys.
        match op {
            WindowOp::Activate => self.tell(id, a._NET_ACTIVE_WINDOW, [2, 0, 0, 0, 0]),
            // ICCCM: IconicState.
            WindowOp::Minimize => self.tell(id, a.WM_CHANGE_STATE, [3, 0, 0, 0, 0]),
            WindowOp::Close => self.tell(id, a._NET_CLOSE_WINDOW, [0, 2, 0, 0, 0]),
            WindowOp::Move { x, y, w, h } => {
                if k.maximized {
                    // _NET_WM_STATE_REMOVE both.
                    self.tell(id, a._NET_WM_STATE, [0, a._NET_WM_STATE_MAXIMIZED_VERT, a._NET_WM_STATE_MAXIMIZED_HORZ, 2, 0]);
                }
                let (cx, cy, cw, ch) = ewmh::client((x, y, w, h), k.frame, k.shadow);
                // Static gravity (the client's own corner), x, y, w, h given, from a pager.
                let flags = 10 | 0xf << 8 | 2 << 12;
                self.tell(id, a._NET_MOVERESIZE_WINDOW, [flags, cx as u32, cy as u32, cw, ch]);
            }
        }
        let _ = self.c.flush();
    }

    fn shell(&mut self, on: bool) {
        self.dock.set(on);
    }

    fn watch(&mut self, poke: Option<Poke>) -> bool {
        if let Some((stop, wake, t)) = self.events.take() {
            stop.store(true, Ordering::Release);
            // Any change on its window wakes the event thread.
            let _ = self.c.change_property8(PropMode::REPLACE, wake, AtomEnum::WM_NAME, AtomEnum::STRING, b"stop");
            let _ = self.c.flush();
            let _ = t.join();
        }
        let Some(poke) = poke else { return true };
        let Ok((c, n)) = x11rb::connect(None) else { return false };
        let Some(root) = c.setup().roots.get(n).map(|s| s.root) else { return false };
        let Ok(wake) = c.generate_id() else { return false };
        let aux = CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE);
        if c.create_window(0, wake, root, -10, -10, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &aux).is_err() || c.flush().is_err() {
            return false;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let a = self.a;
        match std::thread::Builder::new().name("coherence-events".into()).spawn(move || events(c, root, a, s, poke)) {
            Ok(t) => {
                self.events = Some((stop, wake, t));
                true
            }
            Err(_) => false,
        }
    }
}

impl Drop for X11Desktop {
    fn drop(&mut self) {
        crate::coherence::Desktop::watch(self, None);
    }
}

/// The event thread: what changes on the root (windows mapped, moved,
/// stacked, the active one) and on each window (title, state, icon).
fn events(c: RustConnection, root: Window, a: Atoms, stop: Arc<AtomicBool>, poke: Poke) {
    let watch = |w: Window, mask: EventMask| {
        let _ = c.change_window_attributes(w, &ChangeWindowAttributesAux::new().event_mask(mask));
    };
    watch(root, EventMask::SUBSTRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE);
    let mut seen: Vec<u32> = Vec::new();
    let follow = |seen: &mut Vec<u32>| {
        let list: Vec<u32> = c
            .get_property(false, root, a._NET_CLIENT_LIST, AtomEnum::ANY, 0, 1024)
            .ok()
            .and_then(|r| r.reply().ok())
            .and_then(|r| r.value32().map(|v| v.take(512).collect()))
            .unwrap_or_default();
        for w in &list {
            if !seen.contains(w) {
                watch(*w, EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY);
            }
        }
        *seen = list;
        let _ = c.flush();
    };
    follow(&mut seen);
    let on_root = [a._NET_CLIENT_LIST, a._NET_CLIENT_LIST_STACKING, a._NET_ACTIVE_WINDOW, a._NET_CURRENT_DESKTOP, a._NET_WORKAREA];
    let on_win = [a._NET_WM_NAME, u32::from(AtomEnum::WM_NAME), a._NET_WM_STATE, a._NET_WM_ICON, a._NET_FRAME_EXTENTS, a._GTK_FRAME_EXTENTS, a._NET_WM_DESKTOP, a.WM_STATE];
    while let Ok(ev) = c.wait_for_event() {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let wanted = match ev {
            Event::PropertyNotify(p) if p.window == root => {
                if p.atom == a._NET_CLIENT_LIST {
                    follow(&mut seen);
                }
                on_root.contains(&p.atom)
            }
            Event::PropertyNotify(p) => on_win.contains(&p.atom),
            Event::ConfigureNotify(_) | Event::MapNotify(_) | Event::UnmapNotify(_) | Event::DestroyNotify(_) | Event::ReparentNotify(_) | Event::CreateNotify(_) => true,
            _ => false,
        };
        if wanted {
            poke();
        }
    }
}

/// Ubuntu's dock (dash-to-dock) keeps a strip of the screen for itself;
/// in Coherence it hides unless a window leaves room for it. What it was
/// is noted in the session's runtime directory, so an agent that died
/// puts it back next time.
#[derive(Default)]
struct Dock {
    /// What we last did (None: nothing yet).
    ours: Option<bool>,
}

/// The dock's setting, read and written in dconf: the dock reads its own
/// copy of the schema, whose default (Ubuntu's: fixed) `gsettings` on the
/// command line doesn't see.
const DOCK: &str = "/org/gnome/shell/extensions/dash-to-dock/dock-fixed";

impl Dock {
    fn note() -> Option<std::path::PathBuf> {
        std::env::var_os("XDG_RUNTIME_DIR").map(|d| std::path::PathBuf::from(d).join("vroom-coherence-dock"))
    }

    fn dconf(args: &[&str]) -> Option<String> {
        let o = std::process::Command::new("dconf").args(args).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    fn set(&mut self, on: bool) {
        if Some(on) == self.ours {
            return;
        }
        self.ours = Some(on);
        let Some(note) = Dock::note() else { return };
        if on {
            // "" (the default: fixed on Ubuntu) or "true": hide it, and
            // note what to put back.
            let Some(was) = Dock::dconf(&["read", DOCK]) else { return };
            if was != "false" {
                let _ = std::fs::write(&note, if was.is_empty() { "default" } else { "true" });
                Dock::dconf(&["write", DOCK, "false"]);
            }
        } else {
            match std::fs::read_to_string(&note).as_deref() {
                Ok("default") => {
                    Dock::dconf(&["reset", DOCK]);
                }
                Ok("true") => {
                    Dock::dconf(&["write", DOCK, "true"]);
                }
                _ => {}
            }
            let _ = std::fs::remove_file(&note);
        }
    }
}
