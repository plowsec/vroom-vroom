//! X11 desktops: drags with the XDND protocol.
//!
//! Mac -> guest (`start`): like on Windows, a small window of ours goes
//! under the pointer; the host presses the guest's button on it, and we
//! become the source of an XDND drag of the staged files. The host moves
//! the pointer with the Mac's mouse; the app under it (Files, the
//! desktop, a browser) sees an ordinary drag of files and, when the host
//! lets go of the button, gets the drop as a list of file URIs.
//!
//! Guest -> Mac (`probe`): the host's mouse left the VM window with the
//! button held. A window of ours that says it takes drops (XdndAware)
//! goes under the pointer; if an app is dragging files, the drag enters
//! it and we ask for the list (`text/uri-list`). We never accept the
//! drop: the host gives the drag up here with Escape.
//!
//! Wayland sessions keep drags between their own clients: a program
//! outside can't start or see one there, so neither works (files dropped
//! from the Mac land on the Desktop instead).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, GrabMode, PropMode,
    SelectionNotifyEvent, StackMode, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE};

/// Side of our window under the pointer, in pixels.
const SIDE: u16 = 64;
/// No press from the host by then: give up.
const WAIT_PRESS: Duration = Duration::from_secs(15);
/// No drag entered our drop target by then: there is none.
const WAIT_ENTER: Duration = Duration::from_millis(1500);
/// The host has the files by then, or never will.
const KEEP: Duration = Duration::from_secs(30);
/// The app the files were dropped on says it is done by then.
const WAIT_FINISHED: Duration = Duration::from_secs(30);
/// Escape on X servers with evdev / XKB key codes (all current ones).
const KEY_ESCAPE: u8 = 9;
/// The XDND version we speak.
const XDND_VERSION: u32 = 5;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

struct Atoms {
    aware: Atom,
    enter: Atom,
    position: Atom,
    status: Atom,
    leave: Atom,
    drop: Atom,
    finished: Atom,
    selection: Atom,
    type_list: Atom,
    copy: Atom,
    uri_list: Atom,
    targets: Atom,
    data: Atom,
}

impl Atoms {
    fn new(c: &RustConnection) -> Res<Atoms> {
        let a = |n: &str| -> Res<Atom> { Ok(c.intern_atom(false, n.as_bytes())?.reply()?.atom) };
        Ok(Atoms {
            aware: a("XdndAware")?,
            enter: a("XdndEnter")?,
            position: a("XdndPosition")?,
            status: a("XdndStatus")?,
            leave: a("XdndLeave")?,
            drop: a("XdndDrop")?,
            finished: a("XdndFinished")?,
            selection: a("XdndSelection")?,
            type_list: a("XdndTypeList")?,
            copy: a("XdndActionCopy")?,
            uri_list: a("text/uri-list")?,
            targets: a("TARGETS")?,
            data: a("VROOM_XDND_DATA")?,
        })
    }
}

fn wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
}

/// A connection, the root window and the pointer on it.
fn connect() -> Res<(RustConnection, Window, i16, i16)> {
    let (c, screen) = x11rb::connect(None)?;
    let root = c.setup().roots.get(screen).ok_or("no screen")?.root;
    let p = c.query_pointer(root)?.reply()?;
    Ok((c, root, p.root_x, p.root_y))
}

/// An invisible window of ours under the pointer that takes clicks.
fn window_at(c: &RustConnection, root: Window, x: i16, y: i16, events: EventMask) -> Res<Window> {
    let w = c.generate_id()?;
    let half = (SIDE / 2) as i16;
    c.create_window(
        COPY_DEPTH_FROM_PARENT,
        w,
        root,
        x - half,
        y - half,
        SIDE,
        SIDE,
        0,
        WindowClass::INPUT_ONLY,
        COPY_FROM_PARENT,
        &CreateWindowAux::new().override_redirect(1).event_mask(events),
    )?;
    c.map_window(w)?;
    c.configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
    c.flush()?;
    Ok(w)
}

fn move_to(c: &RustConnection, w: Window, x: i16, y: i16) -> Res<()> {
    let half = (SIDE / 2) as i32;
    c.configure_window(w, &ConfigureWindowAux::new().x(x as i32 - half).y(y as i32 - half).stack_mode(StackMode::ABOVE))?;
    c.flush()?;
    Ok(())
}

fn client_message(c: &RustConnection, to: Window, ty: Atom, data: [u32; 5]) -> Res<()> {
    let ev = ClientMessageEvent::new(32, to, ty, data);
    c.send_event(false, to, EventMask::NO_EVENT, ev)?;
    c.flush()?;
    Ok(())
}

/// The next event, or None after `until`.
fn next_event(c: &RustConnection, until: Instant, stop: &AtomicBool) -> Res<Option<Event>> {
    loop {
        if let Some(e) = c.poll_for_event()? {
            return Ok(Some(e));
        }
        if Instant::now() >= until || stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

// --- Mac -> guest: we are the drag source --------------------------------

pub struct X11Drag {
    stop: Arc<AtomicBool>,
}

impl crate::drop::LiveDrag for X11Drag {
    fn cancel(&mut self) {
        // Before the press: our window goes. During the drag the host's
        // Escape ends it.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Put our window under the pointer for a drag of the staged items;
/// None where the desktop can't.
pub fn start(dir: &Path, items: &[(String, bool)]) -> Option<X11Drag> {
    if wayland() {
        return None;
    }
    let uris: Vec<PathBuf> = items.iter().map(|(n, _)| dir.join(n)).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let (tx, rx) = mpsc::channel::<bool>();
    std::thread::Builder::new()
        .name("drag".into())
        .spawn(move || {
            if let Err(e) = source(&uris, &s2, &tx) {
                log::warn!("drop: x11 drag: {e}");
                let _ = tx.send(false);
            }
        })
        .ok()?;
    rx.recv_timeout(Duration::from_secs(5)).ok().filter(|&ok| ok).map(|_| X11Drag { stop })
}

/// Where the pointer's drop would go: the window under it that takes
/// XDND drops, and its version.
fn target_at(c: &RustConnection, a: &Atoms, root: Window, us: Window, x: i16, y: i16) -> Res<Option<(Window, u32)>> {
    let mut w = root;
    for _ in 0..16 {
        let child = c.translate_coordinates(root, w, x, y)?.reply()?.child;
        if child == NONE || child == us {
            return Ok(None);
        }
        let p = c.get_property(false, child, a.aware, AtomEnum::ATOM, 0, 1)?.reply()?;
        if let Some(v) = p.value32().and_then(|mut v| v.next()) {
            return Ok(Some((child, v.min(XDND_VERSION))));
        }
        w = child;
    }
    Ok(None)
}

fn source(paths: &[PathBuf], stop: &AtomicBool, ready: &mpsc::Sender<bool>) -> Res<()> {
    let (c, root, mut x, mut y) = connect()?;
    let a = Atoms::new(&c)?;
    let us = window_at(&c, root, x, y, EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::KEY_PRESS)?;
    let _ = ready.send(true);
    let list = uri_list(paths);
    let res = (|| -> Res<()> {
        // Follow the pointer until the host presses on us.
        let t0 = Instant::now();
        let time = loop {
            if stop.load(Ordering::Relaxed) || t0.elapsed() > WAIT_PRESS {
                return Ok(());
            }
            match c.poll_for_event()? {
                Some(Event::ButtonPress(e)) if e.detail == 1 => break e.time,
                Some(_) => continue,
                None => {}
            }
            let p = c.query_pointer(root)?.reply()?;
            if (p.root_x, p.root_y) != (x, y) {
                (x, y) = (p.root_x, p.root_y);
                move_to(&c, us, x, y)?;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        // Pressed: the pointer is ours until the button goes up. Get out
        // of the way (still mapped: the grab stays), take Escape.
        c.configure_window(us, &ConfigureWindowAux::new().x(-200).y(-200).width(1).height(1))?;
        c.grab_keyboard(false, us, time, GrabMode::ASYNC, GrabMode::ASYNC)?;
        c.set_selection_owner(us, a.selection, time)?;
        c.flush()?;
        log::info!("drop: x11 drag started");
        let mut target: Option<(Window, u32)> = None;
        let (mut waiting, mut pending, mut accepted) = (false, None::<(i16, i16, u32)>, false);
        let position = |t: Window, x: i16, y: i16, time: u32| -> Res<()> {
            client_message(&c, t, a.position, [us, 0, ((x as u16 as u32) << 16) | y as u16 as u32, time, a.copy])
        };
        let mut dropped_at = None::<Instant>;
        loop {
            let until = dropped_at.map_or(Instant::now() + Duration::from_secs(600), |t| t + WAIT_FINISHED);
            let Some(ev) = next_event(&c, until, &AtomicBool::new(false))? else {
                if let Some((t, _)) = target {
                    client_message(&c, t, a.leave, [us, 0, 0, 0, 0])?;
                }
                return Ok(());
            };
            match ev {
                Event::MotionNotify(e) if dropped_at.is_none() => {
                    let now = target_at(&c, &a, root, us, e.root_x, e.root_y)?;
                    if now.map(|t| t.0) != target.map(|t| t.0) {
                        if let Some((t, _)) = target {
                            client_message(&c, t, a.leave, [us, 0, 0, 0, 0])?;
                        }
                        target = now;
                        (waiting, pending, accepted) = (false, None, false);
                        if let Some((t, v)) = target {
                            client_message(&c, t, a.enter, [us, v << 24, a.uri_list, 0, 0])?;
                        }
                    }
                    if let Some((t, _)) = target {
                        if waiting {
                            pending = Some((e.root_x, e.root_y, e.time));
                        } else {
                            position(t, e.root_x, e.root_y, e.time)?;
                            waiting = true;
                        }
                    }
                }
                Event::ClientMessage(e) if e.type_ == a.status => {
                    let d = e.data.as_data32();
                    if target.is_some_and(|t| t.0 == d[0]) {
                        accepted = d[1] & 1 != 0;
                        waiting = false;
                        if let (Some((t, _)), Some((x, y, time))) = (target, pending.take()) {
                            position(t, x, y, time)?;
                            waiting = true;
                        }
                    }
                }
                Event::ButtonRelease(e) if e.detail == 1 && dropped_at.is_none() => {
                    c.ungrab_keyboard(e.time)?;
                    match target {
                        Some((t, _)) if accepted => {
                            client_message(&c, t, a.drop, [us, 0, e.time, 0, 0])?;
                            log::info!("drop: x11 drop on window {t:#x}");
                            dropped_at = Some(Instant::now());
                        }
                        Some((t, _)) => {
                            client_message(&c, t, a.leave, [us, 0, 0, 0, 0])?;
                            return Ok(());
                        }
                        None => return Ok(()),
                    }
                }
                Event::KeyPress(e) if e.detail == KEY_ESCAPE && dropped_at.is_none() => {
                    if let Some((t, _)) = target {
                        client_message(&c, t, a.leave, [us, 0, 0, 0, 0])?;
                    }
                    log::info!("drop: x11 drag given up");
                    return Ok(());
                }
                Event::ClientMessage(e) if e.type_ == a.finished => {
                    log::info!("drop: x11 drop finished");
                    return Ok(());
                }
                Event::SelectionRequest(r) if r.selection == a.selection => {
                    let prop = if r.property == NONE { r.target } else { r.property };
                    let ok = if r.target == a.uri_list {
                        c.change_property8(PropMode::REPLACE, r.requestor, prop, a.uri_list, list.as_bytes())?;
                        true
                    } else if r.target == a.targets {
                        c.change_property32(PropMode::REPLACE, r.requestor, prop, AtomEnum::ATOM, &[a.targets, a.uri_list])?;
                        true
                    } else {
                        false
                    };
                    let ev = SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: r.time,
                        requestor: r.requestor,
                        selection: r.selection,
                        target: r.target,
                        property: if ok { prop } else { NONE },
                    };
                    c.send_event(false, r.requestor, EventMask::NO_EVENT, ev)?;
                    c.flush()?;
                }
                _ => {}
            }
        }
    })();
    let _ = c.destroy_window(us);
    let _ = c.flush();
    res
}

// --- Guest -> Mac: we are a drop target ----------------------------------

/// Find the files of a drag going on under the pointer, if any.
pub fn probe() -> Result<crate::dragout::Found, String> {
    if wayland() {
        return Err("a Wayland desktop keeps its drags to itself".into());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
    std::thread::Builder::new()
        .name("drag-out-target".into())
        .spawn(move || {
            if let Err(e) = target(&s2, tx) {
                log::warn!("drag out: x11: {e}");
            }
        })
        .map_err(|e| e.to_string())?;
    let paths = rx.recv_timeout(WAIT_ENTER + Duration::from_secs(3)).unwrap_or_default();
    if paths.is_empty() {
        stop.store(true, Ordering::Relaxed);
        return Ok(crate::dragout::Found { paths, close: None });
    }
    Ok(crate::dragout::Found { paths, close: Some(Box::new(move || stop.store(true, Ordering::Relaxed))) })
}

fn target(stop: &AtomicBool, tx: mpsc::Sender<Vec<PathBuf>>) -> Res<()> {
    let (c, root, x, y) = connect()?;
    let a = Atoms::new(&c)?;
    let us = window_at(&c, root, x, y, EventMask::PROPERTY_CHANGE)?;
    c.change_property32(PropMode::REPLACE, us, a.aware, AtomEnum::ATOM, &[XDND_VERSION])?;
    // The drag looks for a target when the pointer moves: move it a
    // little, and back.
    c.warp_pointer(NONE, root, 0, 0, 0, 0, x - 2, y - 2)?;
    c.warp_pointer(NONE, root, 0, 0, 0, 0, x, y)?;
    c.flush()?;
    let mut tx = Some(tx);
    let res = (|| -> Res<()> {
        let mut until = Instant::now() + WAIT_ENTER;
        let mut source: Option<Window> = None;
        let mut asked = false;
        while let Some(ev) = next_event(&c, until, stop)? {
            match ev {
                Event::ClientMessage(e) if e.type_ == a.enter => {
                    let d = e.data.as_data32();
                    let mut types = vec![d[2], d[3], d[4]];
                    if d[1] & 1 != 0 {
                        let p = c.get_property(false, d[0], a.type_list, AtomEnum::ATOM, 0, 64)?.reply()?;
                        types = p.value32().map(|v| v.collect()).unwrap_or_default();
                    }
                    if !types.contains(&a.uri_list) {
                        // Not files: nothing for us.
                        log::info!("drag out: x11 drag without files");
                        return Ok(());
                    }
                    source = Some(d[0]);
                    until = Instant::now() + KEEP;
                }
                Event::ClientMessage(e) if e.type_ == a.position => {
                    let d = e.data.as_data32();
                    if source != Some(d[0]) {
                        continue;
                    }
                    if !asked {
                        asked = true;
                        c.convert_selection(us, a.selection, a.uri_list, a.data, d[3])?;
                    }
                    client_message(&c, d[0], a.status, [us, 1, 0, 0, a.copy])?;
                }
                Event::SelectionNotify(e) if e.requestor == us => {
                    let paths = if e.property == NONE {
                        Vec::new()
                    } else {
                        let p = c.get_property(true, us, a.data, AtomEnum::ANY, 0, 1 << 20)?.reply()?;
                        uri_paths(&String::from_utf8_lossy(&p.value))
                    };
                    log::info!("drag out: an x11 drag of {} file(s) entered", paths.len());
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(paths);
                    }
                }
                Event::ClientMessage(e) if e.type_ == a.leave => return Ok(()),
                Event::ClientMessage(e) if e.type_ == a.drop => {
                    // We take nothing: the files go to the Mac.
                    let d = e.data.as_data32();
                    client_message(&c, d[0], a.finished, [us, 0, 0, 0, 0])?;
                    return Ok(());
                }
                _ => {}
            }
        }
        Ok(())
    })();
    let _ = c.destroy_window(us);
    let _ = c.flush();
    res
}

/// `text/uri-list` for these paths.
fn uri_list(paths: &[PathBuf]) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::new();
    for p in paths {
        s.push_str("file://");
        for &b in p.as_os_str().as_bytes() {
            if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
                s.push(b as char);
            } else {
                s.push_str(&format!("%{b:02X}"));
            }
        }
        s.push_str("\r\n");
    }
    s
}

/// The local files in a `text/uri-list`.
fn uri_paths(list: &str) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut out = Vec::new();
    for l in list.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let Some(rest) = l.strip_prefix("file://") else { continue };
        // "file:///p" or "file://localhost/p"; another host isn't ours.
        let path = match rest.find('/') {
            Some(0) => rest,
            Some(i) if &rest[..i] == "localhost" => &rest[i..],
            _ => continue,
        };
        let b = path.as_bytes();
        let mut v = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if b[i] == b'%' && i + 2 < b.len() {
                if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                    v.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
            }
            v.push(b[i]);
            i += 1;
        }
        out.push(PathBuf::from(std::ffi::OsString::from_vec(v)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_both_ways() {
        let p = vec![PathBuf::from("/home/u/.cache/vroom-drop/1/a b%.txt"), PathBuf::from("/tmp/é")];
        let l = uri_list(&p);
        assert_eq!(l, "file:///home/u/.cache/vroom-drop/1/a%20b%25.txt\r\nfile:///tmp/%C3%A9\r\n");
        assert_eq!(uri_paths(&l), p);
        assert_eq!(
            uri_paths("# comment\nfile://localhost/x%2Fy\nhttp://example.com/z\nfile://other/w\nfile:///end%"),
            vec![PathBuf::from("/x/y"), PathBuf::from("/end%")]
        );
    }
}
