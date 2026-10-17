//! Coherence, guest side: the host shows each of our windows as a window
//! of its own, cut out of the screen, without our desktop around it.
//!
//! While it is on we send the list of windows (`Windows`, front to back)
//! whenever it changes, and the app icon of each app window when it
//! changes (`WindowIcon`); we hide the taskbar and give windows the whole
//! screen (a maximized window then fills what the host shows). The host
//! can bring a window to the front, minimize, close or move it
//! (`WindowDo`), but only one of the last list we sent, and start an app
//! of the menu by name (`StartApp`, apps.rs). Off, or when the session
//! ends, the taskbar comes back. While on, the host also gets the list of
//! apps it can start (`Apps`), read on a thread of its own (it takes a
//! second: every icon) and again every `APPS_AGAIN`.
//!
//! We look at the windows when the desktop says something changed
//! (Windows: window events; X11: events on the root and the windows),
//! at most every `GAP`, and every `SLOW` anyway in case an event was
//! missed. A desktop without events is looked at every `EVERY`.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use vroom_proto::{win_flags, GuestApp, GuestWindow, Message, WindowOp, ICON_MAX, TITLE_MAX, WINDOWS_MAX};

/// How often the windows are looked at without events.
pub const EVERY: Duration = Duration::from_millis(40);
/// With events: at most this often (a window dragged in the guest sends
/// one per mouse move)...
pub const GAP: Duration = Duration::from_millis(10);
/// ...and at least this often, in case an event got lost.
pub const SLOW: Duration = Duration::from_millis(500);
/// Icons of windows already listed are looked at again this often (apps
/// change them: a browser shows the page's).
const ICONS_AGAIN: Duration = Duration::from_secs(2);
/// The app list is read again this often while on (apps come and go).
const APPS_AGAIN: Duration = Duration::from_secs(60);
/// The icon size we ask the desktop for, in pixels.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub const ICON_SIZE: u32 = 32;

/// Tells the session loop to call `tick` soon. Called from the desktop's
/// event thread.
pub type Poke = Arc<dyn Fn() + Send + Sync>;

/// A window's app icon: `w` x `h` RGBA, not premultiplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icon {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// The desktop, behind a trait so tests can play it.
pub trait Desktop {
    /// The windows on screen (and minimized ones), front to back.
    fn windows(&mut self) -> Vec<GuestWindow>;
    /// The app icon of a window of the last list, about `ICON_SIZE` on a
    /// side, if it has one.
    fn icon(&mut self, id: u32) -> Option<Icon>;
    fn act(&mut self, id: u32, op: WindowOp);
    /// Coherence on: taskbar away, windows get the whole screen. Off:
    /// back as it was. Called again on every look while on (Windows puts
    /// the taskbar and work area back when the screen size changes).
    fn shell(&mut self, coherence: bool);
    /// Call `poke` whenever the windows may have changed (Some), or stop
    /// (None). False: this desktop has no such events.
    fn watch(&mut self, _poke: Option<Poke>) -> bool {
        false
    }
    /// Start the app with this name, as the Start menu would (apps.rs).
    /// Returns at once.
    fn start(&mut self, _name: &str) {}
}

/// Reads the list of apps (apps.rs; tests play it).
pub type Lister = Arc<dyn Fn() -> Vec<GuestApp> + Send + Sync>;

pub struct Coherence {
    desk: Option<Box<dyn Desktop>>,
    /// Why there is no desktop (told to the host when it asks).
    why: String,
    on: bool,
    /// What the host last got.
    sent: Option<Vec<GuestWindow>>,
    /// The icons the host has, by window: a hash of the pixels.
    icons: HashMap<u32, u64>,
    icons_at: Instant,
    /// The desktop calls `poke` on changes (else we look every `EVERY`).
    watching: bool,
    /// Set by the desktop's events, cleared when we look.
    poked: Arc<AtomicBool>,
    /// Wakes the session loop (from another thread).
    wake: Poke,
    last: Instant,
    lister: Option<Lister>,
    /// The list being read, and when that started.
    apps: Option<Receiver<Vec<GuestApp>>>,
    apps_at: Option<Instant>,
    /// What the host last got.
    apps_sent: Option<Vec<GuestApp>>,
}

impl Coherence {
    /// `wake`: makes the session loop call `tick` (from another thread).
    pub fn new(wake: Poke) -> Coherence {
        let (mut desk, why) = match desktop() {
            Ok(d) => (Some(d), String::new()),
            Err(e) => (None, e),
        };
        // An agent that died in Coherence left the taskbar hidden.
        if let Some(d) = desk.as_mut() {
            d.shell(false);
        }
        let lister: Option<Lister> = Coherence::starts_apps().then(|| Arc::new(crate::apps::list) as Lister);
        Coherence::make(desk, why, wake, lister)
    }

    fn make(desk: Option<Box<dyn Desktop>>, why: String, wake: Poke, lister: Option<Lister>) -> Coherence {
        let now = Instant::now();
        Coherence {
            desk,
            why,
            on: false,
            sent: None,
            icons: HashMap::new(),
            icons_at: now,
            watching: false,
            poked: Arc::default(),
            wake,
            last: now,
            lister,
            apps: None,
            apps_at: None,
            apps_sent: None,
        }
    }

    #[cfg(test)]
    fn with(desk: Box<dyn Desktop>) -> Coherence {
        Coherence::make(Some(desk), String::new(), Arc::new(|| {}), None)
    }

    /// Whether this guest can do it at all (Linux: if its session is X11,
    /// which only the session helper knows).
    pub fn supported() -> bool {
        cfg!(any(windows, target_os = "linux"))
    }

    /// Whether we can start apps for the host.
    pub fn starts_apps() -> bool {
        cfg!(any(windows, target_os = "linux"))
    }

    /// Read the app list on a thread (unless that's underway); the
    /// session loop is woken when it's done.
    fn read_apps(&mut self) {
        let Some(lister) = self.lister.clone() else { return };
        if self.apps.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = self.wake.clone();
        let t = std::thread::Builder::new().name("app-list".into()).spawn(move || {
            let t0 = Instant::now();
            let list = lister();
            log::info!("coherence: {} apps to start ({:?})", list.len(), t0.elapsed());
            let _ = tx.send(list);
            wake();
        });
        if t.is_ok() {
            self.apps = Some(rx);
            self.apps_at = Some(Instant::now());
        }
    }

    /// The app list, if one was read (and is news to the host); time to
    /// read it again.
    fn send_apps(&mut self, out: &mut dyn Write) -> io::Result<()> {
        if let Some(rx) = self.apps.as_ref() {
            match rx.try_recv() {
                Ok(list) => {
                    self.apps = None;
                    if self.on && self.apps_sent.as_ref() != Some(&list) {
                        out.write_all(&Message::Apps(list.clone()).encode())?;
                        self.apps_sent = Some(list);
                    }
                }
                Err(TryRecvError::Disconnected) => self.apps = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.on && self.apps_at.is_some_and(|t| t.elapsed() >= APPS_AGAIN) {
            self.read_apps();
        }
        Ok(())
    }

    /// How long the session loop may wait before calling `tick`.
    pub fn wait(&self) -> Option<Duration> {
        if !self.on {
            return None;
        }
        let every = if self.poked.load(Ordering::Acquire) {
            GAP
        } else if self.watching {
            SLOW
        } else {
            EVERY
        };
        Some((self.last + every).saturating_duration_since(Instant::now()))
    }

    /// Whether `m` is ours; then it is handled, answers go to `out`.
    pub fn handle(&mut self, m: &Message, out: &mut dyn Write) -> io::Result<bool> {
        match m {
            Message::Coherence { on } => {
                if self.desk.is_none() && *on {
                    // The session may have come up since (a display to talk to).
                    match desktop() {
                        Ok(d) => self.desk = Some(d),
                        Err(e) => self.why = e,
                    }
                }
                let Some(d) = self.desk.as_mut() else {
                    // Nothing to show: say so rather than leave the host waiting.
                    if *on {
                        log::info!("coherence: not here: {}", self.why);
                        if vroom_proto::line_ok(&self.why) {
                            out.write_all(&Message::CoherenceNo(self.why.clone()).encode())?;
                        }
                    }
                    out.write_all(&Message::Windows(Vec::new()).encode())?;
                    return Ok(true);
                };
                if *on != self.on {
                    log::info!("coherence {}", if *on { "on" } else { "off" });
                    d.shell(*on);
                    let poke: Option<Poke> = on.then(|| {
                        let (poked, wake) = (self.poked.clone(), self.wake.clone());
                        Arc::new(move || {
                            if !poked.swap(true, Ordering::AcqRel) {
                                wake();
                            }
                        }) as Poke
                    });
                    self.watching = d.watch(poke) && *on;
                }
                if *on && !self.on {
                    // The host's bar is new: it gets the list again.
                    self.apps_sent = None;
                    self.read_apps();
                }
                self.on = *on;
                self.sent = None;
                self.icons.clear();
                self.look(out)?;
            }
            Message::WindowDo { id, op } => {
                let listed = self.sent.as_ref().is_some_and(|l| l.iter().any(|w| w.id == *id));
                if let (true, true, Some(d)) = (self.on, listed, self.desk.as_mut()) {
                    d.act(*id, *op);
                    // Show the result soon, not at the next change.
                    self.look(out)?;
                }
            }
            Message::StartApp(name) => {
                if let Some(d) = self.desk.as_mut() {
                    log::info!("coherence: starting {name:?}");
                    d.start(name);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Look at the windows if it's time (something changed, or a while
    /// went by); tell the host what changed.
    pub fn tick(&mut self, out: &mut dyn Write) -> io::Result<()> {
        if !self.on {
            return Ok(());
        }
        self.send_apps(out)?;
        if self.wait().is_some_and(|w| !w.is_zero()) {
            return Ok(());
        }
        self.look(out)
    }

    /// Look now.
    fn look(&mut self, out: &mut dyn Write) -> io::Result<()> {
        let (true, Some(d)) = (self.on, self.desk.as_mut()) else { return Ok(()) };
        self.poked.store(false, Ordering::Release);
        self.last = Instant::now();
        d.shell(true);
        let list = tidy(d.windows());
        if self.sent.as_ref() != Some(&list) {
            out.write_all(&Message::Windows(list.clone()).encode())?;
            self.sent = Some(list);
        }
        self.send_icons(out)
    }

    /// Icons of new app windows, and now and then all of them again: the
    /// ones that changed go out.
    fn send_icons(&mut self, out: &mut dyn Write) -> io::Result<()> {
        let (Some(list), Some(d)) = (self.sent.as_ref(), self.desk.as_mut()) else { return Ok(()) };
        self.icons.retain(|id, _| list.iter().any(|w| w.id == *id));
        let again = self.icons_at.elapsed() >= ICONS_AGAIN;
        if again {
            self.icons_at = Instant::now();
        }
        for w in list.iter().filter(|w| w.flags & win_flags::APP != 0) {
            let had = self.icons.get(&w.id).copied();
            if had.is_some() && !again {
                continue;
            }
            let icon = d.icon(w.id).filter(icon_ok);
            let h = icon.as_ref().map(|i| {
                let mut s = std::collections::hash_map::DefaultHasher::new();
                (i.w, i.h, &i.rgba).hash(&mut s);
                s.finish()
            });
            // 0: none (and the host never had one: nothing to say).
            let h = h.unwrap_or(0);
            if had == Some(h) || had.is_none() && h == 0 {
                self.icons.insert(w.id, h);
                continue;
            }
            let m = match icon {
                Some(i) => Message::WindowIcon { id: w.id, w: i.w, h: i.h, rgba: i.rgba },
                None => Message::WindowIcon { id: w.id, w: 0, h: 0, rgba: Vec::new() },
            };
            out.write_all(&m.encode())?;
            self.icons.insert(w.id, h);
        }
        Ok(())
    }
}

/// What the protocol takes.
fn icon_ok(i: &Icon) -> bool {
    (1..=ICON_MAX).contains(&i.w) && (1..=ICON_MAX).contains(&i.h) && i.rgba.len() == (i.w * i.h * 4) as usize
}

impl Drop for Coherence {
    fn drop(&mut self) {
        if let (true, Some(d)) = (self.on, self.desk.as_mut()) {
            d.watch(None);
            d.shell(false);
        }
    }
}

/// What the protocol takes: the first `WINDOWS_MAX`, one entry per id,
/// titles cut to `TITLE_MAX` bytes (whole characters), sizes kept sane.
pub fn tidy(list: Vec<GuestWindow>) -> Vec<GuestWindow> {
    let mut out: Vec<GuestWindow> = Vec::with_capacity(list.len().min(WINDOWS_MAX));
    for mut w in list {
        if out.len() == WINDOWS_MAX {
            break;
        }
        if out.iter().any(|o| o.id == w.id) {
            continue;
        }
        let lim = vroom_proto::COORD_MAX;
        w.x = w.x.clamp(-lim, lim);
        w.y = w.y.clamp(-lim, lim);
        w.w = w.w.clamp(1, lim as u32);
        w.h = w.h.clamp(1, lim as u32);
        w.flags &= vroom_proto::win_flags::ALL;
        w.title = w.title.chars().filter(|c| *c != '\0').collect();
        if w.title.len() > TITLE_MAX {
            let mut end = TITLE_MAX;
            while !w.title.is_char_boundary(end) {
                end -= 1;
            }
            w.title.truncate(end);
        }
        out.push(w);
    }
    out
}

/// Scale an RGBA picture to `size` x `size` (nearest pixel), keeping its
/// shape: icons come in all sizes (X11 apps give several, up to 256 and
/// more).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn fit(w: u32, h: u32, rgba: &[u8], size: u32) -> Option<Icon> {
    if w == 0 || h == 0 || rgba.len() != (w as usize * h as usize * 4) {
        return None;
    }
    if w <= size && h <= size {
        return Some(Icon { w, h, rgba: rgba.to_vec() });
    }
    let k = size as f64 / w.max(h) as f64;
    let (nw, nh) = (((w as f64 * k).round() as u32).clamp(1, size), ((h as f64 * k).round() as u32).clamp(1, size));
    let mut out = Vec::with_capacity((nw * nh * 4) as usize);
    for y in 0..nh {
        // The middle of each target pixel, back in the source.
        let sy = (((y as f64 + 0.5) / k) as u32).min(h - 1);
        for x in 0..nw {
            let sx = (((x as f64 + 0.5) / k) as u32).min(w - 1);
            let o = (sy as usize * w as usize + sx as usize) * 4;
            out.extend_from_slice(&rgba[o..o + 4]);
        }
    }
    Some(Icon { w: nw, h: nh, rgba: out })
}

#[cfg(windows)]
fn desktop() -> Result<Box<dyn Desktop>, String> {
    Ok(Box::new(crate::coh_win::WinDesktop::new()))
}

#[cfg(target_os = "linux")]
fn desktop() -> Result<Box<dyn Desktop>, String> {
    crate::coh_x11::X11Desktop::open().map(|d| Box::new(d) as Box<dyn Desktop>)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn desktop() -> Result<Box<dyn Desktop>, String> {
    Err("this guest system has no Coherence".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use vroom_proto::Decoder;

    #[derive(Default)]
    struct Seen {
        list: Vec<GuestWindow>,
        icons: HashMap<u32, Icon>,
        icon_asks: usize,
        acts: Vec<(u32, WindowOp)>,
        shell: Vec<bool>,
        poke: Option<Poke>,
        started: Vec<String>,
    }
    struct Fake(Arc<Mutex<Seen>>, bool);
    impl Desktop for Fake {
        fn windows(&mut self) -> Vec<GuestWindow> {
            self.0.lock().unwrap().list.clone()
        }
        fn icon(&mut self, id: u32) -> Option<Icon> {
            let mut s = self.0.lock().unwrap();
            s.icon_asks += 1;
            s.icons.get(&id).cloned()
        }
        fn act(&mut self, id: u32, op: WindowOp) {
            self.0.lock().unwrap().acts.push((id, op));
        }
        fn shell(&mut self, on: bool) {
            let mut s = self.0.lock().unwrap();
            if s.shell.last() != Some(&on) {
                s.shell.push(on);
            }
        }
        fn watch(&mut self, poke: Option<Poke>) -> bool {
            self.0.lock().unwrap().poke = poke;
            self.1
        }
        fn start(&mut self, name: &str) {
            self.0.lock().unwrap().started.push(name.into());
        }
    }

    fn msgs(b: &[u8]) -> Vec<Message> {
        let mut d = Decoder::new(1 << 20);
        d.push(b);
        std::iter::from_fn(|| d.next().unwrap()).collect()
    }

    fn win(id: u32, title: &str) -> GuestWindow {
        GuestWindow { id, x: 0, y: 0, w: 100, h: 100, flags: 0, title: title.into() }
    }

    fn app(id: u32) -> GuestWindow {
        GuestWindow { flags: win_flags::APP, ..win(id, "app") }
    }

    #[test]
    fn lists_go_out_when_they_change_and_only_listed_windows_are_touched() {
        let seen = Arc::new(Mutex::new(Seen { list: vec![win(1, "a")], ..Default::default() }));
        let mut c = Coherence::with(Box::new(Fake(seen.clone(), false)));
        let mut out = Vec::new();
        // Off: nothing is looked at, nothing done.
        c.tick(&mut out).unwrap();
        assert!(c.handle(&Message::WindowDo { id: 1, op: WindowOp::Close }, &mut out).unwrap());
        assert!(out.is_empty() && seen.lock().unwrap().acts.is_empty());
        assert_eq!(c.wait(), None);

        c.handle(&Message::Coherence { on: true }, &mut out).unwrap();
        assert!(c.wait().is_some_and(|w| w <= EVERY));
        c.last -= EVERY;
        c.tick(&mut out).unwrap();
        assert_eq!(msgs(&out), vec![Message::Windows(vec![win(1, "a")])], "once, not on every look");
        out.clear();
        seen.lock().unwrap().list.push(win(2, "b"));
        c.last -= EVERY;
        c.tick(&mut out).unwrap();
        assert_eq!(msgs(&out), vec![Message::Windows(vec![win(1, "a"), win(2, "b")])]);

        out.clear();
        c.handle(&Message::WindowDo { id: 2, op: WindowOp::Activate }, &mut out).unwrap();
        c.handle(&Message::WindowDo { id: 99, op: WindowOp::Close }, &mut out).unwrap();
        assert_eq!(seen.lock().unwrap().acts, vec![(2, WindowOp::Activate)]);

        c.handle(&Message::StartApp("Paint".into()), &mut out).unwrap();
        assert_eq!(seen.lock().unwrap().started, vec!["Paint".to_string()]);

        c.handle(&Message::Coherence { on: false }, &mut out).unwrap();
        c.handle(&Message::Coherence { on: true }, &mut out).unwrap();
        drop(c);
        // Taskbar hidden, back, hidden, and back when the session goes.
        assert_eq!(seen.lock().unwrap().shell, vec![true, false, true, false]);
    }

    #[test]
    fn events_wake_us_and_a_slow_look_is_kept() {
        let seen = Arc::new(Mutex::new(Seen { list: vec![win(1, "a")], ..Default::default() }));
        let woken = Arc::new(Mutex::new(0));
        let w = woken.clone();
        let mut c = Coherence::make(Some(Box::new(Fake(seen.clone(), true))), String::new(), Arc::new(move || *w.lock().unwrap() += 1), None);
        let mut out = Vec::new();
        c.handle(&Message::Coherence { on: true }, &mut out).unwrap();
        assert!(c.wait().unwrap() > EVERY, "events: only a slow look");
        out.clear();
        let poke = seen.lock().unwrap().poke.clone().unwrap();
        seen.lock().unwrap().list.push(win(2, "b"));
        // A burst of events wakes the loop once.
        for _ in 0..50 {
            poke();
        }
        assert_eq!(*woken.lock().unwrap(), 1);
        assert!(c.wait().unwrap() <= GAP);
        c.last -= GAP;
        c.tick(&mut out).unwrap();
        assert_eq!(msgs(&out).len(), 1);
        // Looked: the next poke wakes again.
        poke();
        assert_eq!(*woken.lock().unwrap(), 2);
        c.handle(&Message::Coherence { on: false }, &mut out).unwrap();
        assert!(seen.lock().unwrap().poke.is_none(), "off: no more events");
    }

    #[test]
    fn icons_go_out_once_and_when_they_change() {
        let red = Icon { w: 1, h: 1, rgba: vec![255, 0, 0, 255] };
        let blue = Icon { w: 1, h: 1, rgba: vec![0, 0, 255, 255] };
        let seen = Arc::new(Mutex::new(Seen { list: vec![app(1), win(2, "menu"), app(3)], ..Default::default() }));
        seen.lock().unwrap().icons.insert(1, red.clone());
        seen.lock().unwrap().icons.insert(2, red.clone());
        // Too big for the protocol: never sent.
        seen.lock().unwrap().icons.insert(3, Icon { w: ICON_MAX + 1, h: 1, rgba: vec![0; (ICON_MAX as usize + 1) * 4] });
        let mut c = Coherence::with(Box::new(Fake(seen.clone(), true)));
        let mut out = Vec::new();
        c.handle(&Message::Coherence { on: true }, &mut out).unwrap();
        let m = msgs(&out);
        assert_eq!(m[1..], [Message::WindowIcon { id: 1, w: 1, h: 1, rgba: red.rgba.clone() }], "app windows only, valid ones only");
        out.clear();
        c.last -= SLOW;
        c.tick(&mut out).unwrap();
        assert!(out.is_empty(), "nothing new");
        // It changes; seen at the next round.
        seen.lock().unwrap().icons.insert(1, blue.clone());
        c.icons_at -= ICONS_AGAIN;
        c.last -= SLOW;
        c.tick(&mut out).unwrap();
        assert_eq!(msgs(&out), vec![Message::WindowIcon { id: 1, w: 1, h: 1, rgba: blue.rgba.clone() }]);
        // It goes: the host is told.
        out.clear();
        seen.lock().unwrap().icons.remove(&1);
        c.icons_at -= ICONS_AGAIN;
        c.last -= SLOW;
        c.tick(&mut out).unwrap();
        assert_eq!(msgs(&out), vec![Message::WindowIcon { id: 1, w: 0, h: 0, rgba: Vec::new() }]);
    }

    #[test]
    fn no_desktop_says_why() {
        let mut c = Coherence::make(None, "a Wayland session".into(), Arc::new(|| {}), None);
        let mut out = Vec::new();
        c.why = "a Wayland session".into();
        c.handle(&Message::Coherence { on: true }, &mut out).unwrap();
        let m = msgs(&out);
        // (On a host with a desktop of its own, a retry may have found one.)
        if c.desk.is_none() {
            assert!(matches!(&m[0], Message::CoherenceNo(_)));
            assert_eq!(m.last(), Some(&Message::Windows(Vec::new())));
        }
    }

    #[test]
    fn the_app_list_goes_out_when_read_and_when_it_changes() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let apps = Arc::new(Mutex::new(vec![GuestApp { name: "Calculator".into(), w: 0, h: 0, rgba: Vec::new() }]));
        let a = apps.clone();
        let (wtx, wrx) = std::sync::mpsc::channel();
        let wtx = Mutex::new(wtx);
        let wake: Poke = Arc::new(move || {
            let _ = wtx.lock().unwrap().send(());
        });
        let mut c = Coherence::make(Some(Box::new(Fake(seen, true))), String::new(), wake, Some(Arc::new(move || a.lock().unwrap().clone())));
        let mut out = Vec::new();
        c.handle(&Message::Coherence { on: true }, &mut out).unwrap();
        wrx.recv_timeout(Duration::from_secs(5)).unwrap();
        out.clear();
        c.tick(&mut out).unwrap();
        assert_eq!(msgs(&out), vec![Message::Apps(apps.lock().unwrap().clone())]);
        // Read again, the same: nothing.
        out.clear();
        c.apps_at = Some(Instant::now() - APPS_AGAIN);
        c.tick(&mut out).unwrap();
        wrx.recv_timeout(Duration::from_secs(5)).unwrap();
        c.tick(&mut out).unwrap();
        assert!(msgs(&out).is_empty());
        // A new app.
        apps.lock().unwrap().push(GuestApp { name: "Paint".into(), w: 0, h: 0, rgba: Vec::new() });
        c.apps_at = Some(Instant::now() - APPS_AGAIN);
        c.tick(&mut out).unwrap();
        wrx.recv_timeout(Duration::from_secs(5)).unwrap();
        c.tick(&mut out).unwrap();
        assert!(matches!(&msgs(&out)[..], [Message::Apps(l)] if l.len() == 2));
        // Off, on: the host gets it again.
        out.clear();
        c.handle(&Message::Coherence { on: false }, &mut out).unwrap();
        c.handle(&Message::Coherence { on: true }, &mut out).unwrap();
        wrx.recv_timeout(Duration::from_secs(5)).unwrap();
        out.clear();
        c.tick(&mut out).unwrap();
        assert!(matches!(&msgs(&out)[..], [Message::Apps(_)]));
    }

    #[test]
    fn icons_are_made_to_fit() {
        let big: Vec<u8> = (0..256u32 * 128).flat_map(|i| [(i % 256) as u8, 0, 0, 255]).collect();
        let i = fit(256, 128, &big, 32).unwrap();
        assert_eq!((i.w, i.h, i.rgba.len()), (32, 16, 32 * 16 * 4));
        assert_eq!(fit(16, 16, &[0; 1024], 32).unwrap().w, 16, "small ones stay");
        assert!(fit(2, 2, &[0; 15], 32).is_none());
        assert!(fit(0, 2, &[], 32).is_none());
    }

    #[test]
    fn tidy_caps_what_goes_out() {
        let mut list: Vec<GuestWindow> = (0..WINDOWS_MAX as u32 + 5).map(|i| win(i, "x")).collect();
        list.insert(1, win(0, "same id"));
        list[0].title = "é".repeat(TITLE_MAX);
        list[0].w = 0;
        list[0].x = i32::MIN;
        list[0].flags = u32::MAX;
        let t = tidy(list);
        assert_eq!(t.len(), WINDOWS_MAX);
        assert!(t[0].title.len() <= TITLE_MAX && t[0].w == 1 && t[0].x == -vroom_proto::COORD_MAX);
        assert_eq!(t[0].flags, vroom_proto::win_flags::ALL);
        assert_eq!(t[1].id, 1);
        // What tidy gives always decodes.
        assert!(vroom_proto::decode_windows(&vroom_proto::encode_windows(&t)).is_some());
    }
}
