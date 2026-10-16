//! Coherence, guest side: the host shows each of our windows as a window
//! of its own, cut out of the screen, without our desktop around it.
//!
//! While it is on we send the list of windows (`Windows`, front to back)
//! whenever it changes, hide the taskbar and give windows the whole
//! screen (a maximized window then fills what the host shows). The host
//! can bring a window to the front, minimize, close or move it
//! (`WindowDo`), but only one of the last list we sent. Off, or when the
//! session ends, the taskbar comes back.

use std::io::{self, Write};
use std::time::Duration;

use vroom_proto::{GuestWindow, Message, WindowOp, TITLE_MAX, WINDOWS_MAX};

/// How often the windows are looked at while Coherence is on.
pub const EVERY: Duration = Duration::from_millis(40);

/// The desktop, behind a trait so tests can play it.
pub trait Desktop {
    /// The windows on screen (and minimized ones), front to back.
    fn windows(&mut self) -> Vec<GuestWindow>;
    fn act(&mut self, id: u32, op: WindowOp);
    /// Coherence on: taskbar away, windows get the whole screen. Off:
    /// back as it was. Called again on every look while on (Windows puts
    /// the taskbar and work area back when the screen size changes).
    fn shell(&mut self, coherence: bool);
}

pub struct Coherence {
    desk: Option<Box<dyn Desktop>>,
    on: bool,
    /// What the host last got.
    sent: Option<Vec<GuestWindow>>,
}

impl Coherence {
    pub fn new() -> Coherence {
        let mut desk = desktop();
        // An agent that died in Coherence left the taskbar hidden.
        if let Some(d) = desk.as_mut() {
            d.shell(false);
        }
        Coherence { desk, on: false, sent: None }
    }

    #[cfg(test)]
    fn with(desk: Box<dyn Desktop>) -> Coherence {
        Coherence { desk: Some(desk), on: false, sent: None }
    }

    /// Whether this desktop can do it at all.
    pub fn supported() -> bool {
        cfg!(windows)
    }

    /// How long the session loop may wait before calling `tick`.
    pub fn wait(&self) -> Option<Duration> {
        self.on.then_some(EVERY)
    }

    /// Whether `m` is ours; then it is handled, answers go to `out`.
    pub fn handle(&mut self, m: &Message, out: &mut dyn Write) -> io::Result<bool> {
        match m {
            Message::Coherence { on } => {
                let Some(d) = self.desk.as_mut() else {
                    // Nothing to show: say so rather than leave the host waiting.
                    out.write_all(&Message::Windows(Vec::new()).encode())?;
                    return Ok(true);
                };
                if *on != self.on {
                    log::info!("coherence {}", if *on { "on" } else { "off" });
                    d.shell(*on);
                }
                self.on = *on;
                self.sent = None;
                self.tick(out)?;
            }
            Message::WindowDo { id, op } => {
                let listed = self.sent.as_ref().is_some_and(|l| l.iter().any(|w| w.id == *id));
                if let (true, true, Some(d)) = (self.on, listed, self.desk.as_mut()) {
                    d.act(*id, *op);
                    // Show the result soon, not at the next change.
                    self.tick(out)?;
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Look at the windows; tell the host if they changed.
    pub fn tick(&mut self, out: &mut dyn Write) -> io::Result<()> {
        let (true, Some(d)) = (self.on, self.desk.as_mut()) else { return Ok(()) };
        d.shell(true);
        let list = tidy(d.windows());
        if self.sent.as_ref() != Some(&list) {
            out.write_all(&Message::Windows(list.clone()).encode())?;
            self.sent = Some(list);
        }
        Ok(())
    }
}

impl Drop for Coherence {
    fn drop(&mut self) {
        if let (true, Some(d)) = (self.on, self.desk.as_mut()) {
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

#[cfg(windows)]
fn desktop() -> Option<Box<dyn Desktop>> {
    Some(Box::new(crate::coh_win::WinDesktop::new()))
}

#[cfg(not(windows))]
fn desktop() -> Option<Box<dyn Desktop>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use vroom_proto::Decoder;

    #[derive(Default)]
    struct Seen {
        list: Vec<GuestWindow>,
        acts: Vec<(u32, WindowOp)>,
        shell: Vec<bool>,
    }
    struct Fake(Arc<Mutex<Seen>>);
    impl Desktop for Fake {
        fn windows(&mut self) -> Vec<GuestWindow> {
            self.0.lock().unwrap().list.clone()
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
    }

    fn msgs(b: &[u8]) -> Vec<Message> {
        let mut d = Decoder::new(1 << 20);
        d.push(b);
        std::iter::from_fn(|| d.next().unwrap()).collect()
    }

    fn win(id: u32, title: &str) -> GuestWindow {
        GuestWindow { id, x: 0, y: 0, w: 100, h: 100, flags: 0, title: title.into() }
    }

    #[test]
    fn lists_go_out_when_they_change_and_only_listed_windows_are_touched() {
        let seen = Arc::new(Mutex::new(Seen { list: vec![win(1, "a")], ..Default::default() }));
        let mut c = Coherence::with(Box::new(Fake(seen.clone())));
        let mut out = Vec::new();
        // Off: nothing is looked at, nothing done.
        c.tick(&mut out).unwrap();
        assert!(c.handle(&Message::WindowDo { id: 1, op: WindowOp::Close }, &mut out).unwrap());
        assert!(out.is_empty() && seen.lock().unwrap().acts.is_empty());
        assert_eq!(c.wait(), None);

        c.handle(&Message::Coherence { on: true }, &mut out).unwrap();
        assert_eq!(c.wait(), Some(EVERY));
        c.tick(&mut out).unwrap();
        assert_eq!(msgs(&out), vec![Message::Windows(vec![win(1, "a")])], "once, not on every look");
        out.clear();
        seen.lock().unwrap().list.push(win(2, "b"));
        c.tick(&mut out).unwrap();
        assert_eq!(msgs(&out), vec![Message::Windows(vec![win(1, "a"), win(2, "b")])]);

        out.clear();
        c.handle(&Message::WindowDo { id: 2, op: WindowOp::Activate }, &mut out).unwrap();
        c.handle(&Message::WindowDo { id: 99, op: WindowOp::Close }, &mut out).unwrap();
        assert_eq!(seen.lock().unwrap().acts, vec![(2, WindowOp::Activate)]);

        c.handle(&Message::Coherence { on: false }, &mut out).unwrap();
        c.handle(&Message::Coherence { on: true }, &mut out).unwrap();
        drop(c);
        // Taskbar hidden, back, hidden, and back when the session goes.
        assert_eq!(seen.lock().unwrap().shell, vec![true, false, true, false]);
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
