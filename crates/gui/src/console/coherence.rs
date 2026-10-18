//! Coherence, the app's side: each guest window as a Mac window of its
//! own, showing that window's part of the shared framebuffer.
//!
//! The guest's screen is made the size of the Mac screen's free area (no
//! menu bar, Dock or our bar) so guest windows sit where their Mac windows
//! are, 1:1 in points. The guest agent reports its windows, front to back;
//! we keep one frameless Mac window per window on screen, a strip of ours
//! on top (VM name, title: the guest can't draw over it), the guest's
//! picture below. Input over the picture goes to the guest at the matching
//! place. Moving a Mac window (by the strip) moves the guest's; a guest
//! window that moves or comes to the front takes its Mac window along.
//!
//! The window list is the guest's word: it is checked by the VMM and the
//! viewer decoder (counts, sizes), and here clipped to the screen, capped
//! (`MAX_SHOWN`) and its titles cleaned. See docs/SECURITY.md.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, Model, Rgba8Pixel, SharedPixelBuffer, VecModel};
use vroom_display::{GuestCursor, GuestWindow, SharedFb, ToVmm, WindowOp, POINTER_MAX};
use vroom_proto::win_flags;

use super::{clean, with, Console, Io};
use crate::{CohApp, CohChip, CoherenceBar, CoherenceWindow, VmState};

/// Most guest windows shown at once (front first; the rest wait).
pub const MAX_SHOWN: usize = 24;
/// Our strip above each window's picture (coherence.slint's `strip`).
pub const STRIP: f64 = 20.0;
/// The Coherence bar (coherence.slint), kept clear at the bottom.
pub const BAR: f64 = 44.0;
/// Guest windows smaller than this (pixels) aren't shown.
const MIN_SIDE: u32 = 4;
/// A Mac window the user moves is left alone by guest reports this long.
const HANDS_OFF: Duration = Duration::from_millis(500);
/// Focus changes we cause are not passed on to the guest for this long.
const QUIET: Duration = Duration::from_millis(400);
/// How often we look whether the bar went to another screen, or the
/// screen changed (Dock, resolution, unplugged).
const SCREENS_EVERY: Duration = Duration::from_secs(1);
/// What of a window the user moves stays on the guest's screen (points).
const KEEP: f64 = 40.0;
/// Most apps the field suggests at once.
const SUGGEST: usize = 3;

/// A rectangle in Mac points: global, top left origin, y down (winit's
/// logical coordinates on macOS).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A rectangle of the guest screen, in its pixels, inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl GRect {
    fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && y >= self.y && x - self.x < self.w && y - self.y < self.h
    }

    /// The part of `d` ([x0, y0, x1, y1]) in this one, if any.
    fn cut(&self, d: [u32; 4]) -> Option<GRect> {
        let (x0, y0) = (d[0].max(self.x), d[1].max(self.y));
        let (x1, y1) = (d[2].min(self.x + self.w), d[3].min(self.y + self.h));
        (x1 > x0 && y1 > y0).then(|| GRect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }
}

/// Where the guest's screen lies on the Mac.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Map {
    pub area: Rect,
    pub mode: (u32, u32),
}

impl Map {
    /// Guest pixels per point, each way.
    fn k(&self) -> (f64, f64) {
        (self.mode.0 as f64 / self.area.w.max(1.0), self.mode.1 as f64 / self.area.h.max(1.0))
    }

    /// Where a guest rectangle's picture goes on the Mac (the strip goes above).
    pub fn mac(self, g: GRect) -> Rect {
        let (kx, ky) = self.k();
        Rect { x: self.area.x + g.x as f64 / kx, y: self.area.y + g.y as f64 / ky, w: g.w as f64 / kx, h: g.h as f64 / ky }
    }

    /// The other way, for moves: may reach past the screen (the guest
    /// takes that), sizes at least a pixel.
    pub fn guest(self, r: Rect) -> (i32, i32, u32, u32) {
        let (kx, ky) = self.k();
        let lim = vroom_proto::COORD_MAX as f64;
        let c = |v: f64| if v.is_finite() { v.round().clamp(-lim, lim) } else { 0.0 };
        (c((r.x - self.area.x) * kx) as i32, c((r.y - self.area.y) * ky) as i32, c(r.w * kx).max(1.0) as u32, c(r.h * ky).max(1.0) as u32)
    }

    /// The guest pointer for a point of a window's picture (`px`, `py`:
    /// points from its top left; `pic`: the picture's size in points,
    /// which is the guest window's unless the Mac window was just resized
    /// and the guest hasn't followed yet: the picture is stretched then).
    pub fn pointer(&self, g: GRect, px: f64, py: f64, pic: (f64, f64)) -> (u16, u16) {
        let k = |n: u32, len: f64| if len.is_finite() && len >= 1.0 { n as f64 / len } else { 0.0 };
        let (kx, ky) = (k(g.w, pic.0), k(g.h, pic.1));
        let (px, py) = (if px.is_finite() { px } else { 0.0 }, if py.is_finite() { py } else { 0.0 });
        let gx = (g.x as f64 + px * kx).clamp(0.0, self.mode.0.saturating_sub(1) as f64);
        let gy = (g.y as f64 + py * ky).clamp(0.0, self.mode.1.saturating_sub(1) as f64);
        self.abs(gx, gy)
    }

    fn abs(&self, gx: f64, gy: f64) -> (u16, u16) {
        // As the guest reads it: 0 is the first pixel, POINTER_MAX the last.
        let f = |v: f64, n: u32| (v / n.saturating_sub(1).max(1) as f64 * POINTER_MAX as f64).round().clamp(0.0, POINTER_MAX as f64) as u16;
        (f(gx, self.mode.0), f(gy, self.mode.1))
    }
}

/// Where the user may put a window: mostly anywhere, but some of it (and
/// all of its strip) stays on the guest's screen, or the guest would lose
/// it off its edge.
pub fn keep_on(r: Rect, area: Rect) -> Rect {
    let x = r.x.clamp(area.x - r.w + KEEP, area.x + area.w - KEEP);
    let y = r.y.clamp(area.y, (area.y + area.h - KEEP).max(area.y));
    Rect { x, y, ..r }
}

/// The guest's screen inside a Mac screen's free area: room above for the
/// top window's strip, below for our bar.
pub fn area_in(free: Rect) -> Rect {
    Rect { x: free.x, y: free.y + STRIP, w: free.w, h: (free.h - STRIP - BAR).max(200.0) }
}

/// Where the bar goes on a screen's free area: at the bottom, in the middle.
pub fn bar_in(free: Rect) -> Rect {
    let width = free.w.min(900.0);
    Rect { x: free.x + (free.w - width) / 2.0, y: free.y + free.h - BAR, w: width, h: BAR }
}

/// A window icon from the guest as an image (sizes were checked by the
/// protocol: at most 64 on a side, exactly that many pixels).
fn icon_image(w: u32, h: u32, rgba: &[u8]) -> Option<slint::Image> {
    if w == 0 || h == 0 || w > vroom_proto::ICON_MAX || h > vroom_proto::ICON_MAX || rgba.len() != (w * h * 4) as usize {
        return None;
    }
    Some(slint::Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba, w, h)))
}

/// The apps (indexes into `names`) that `q` may mean, best first, at
/// most `n`: the whole name, then its start, the start of one of its
/// words, then anywhere in it; a shorter name first.
pub fn suggest(names: &[String], q: &str, n: usize) -> Vec<usize> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u8, usize, String, usize)> = names
        .iter()
        .enumerate()
        .filter_map(|(i, name)| {
            let l = name.to_lowercase();
            let r = if l == q {
                0
            } else if l.starts_with(&q) {
                1
            } else if l.split(|c: char| !c.is_alphanumeric()).any(|w| !w.is_empty() && w.starts_with(&q)) {
                2
            } else if l.contains(&q) {
                3
            } else {
                return None;
            };
            Some((r, l.chars().count(), l, i))
        })
        .collect();
    hits.sort();
    hits.into_iter().take(n).map(|h| h.3).collect()
}

/// The part of `w` on a `mode` screen, if it is worth a window.
pub fn clip(w: &GuestWindow, mode: (u32, u32)) -> Option<GRect> {
    let (x0, y0) = (w.x.max(0) as i64, w.y.max(0) as i64);
    let x1 = (w.x as i64 + w.w as i64).min(mode.0 as i64);
    let y1 = (w.y as i64 + w.h as i64).min(mode.1 as i64);
    let (cw, ch) = (x1 - x0, y1 - y0);
    (cw >= MIN_SIDE as i64 && ch >= MIN_SIDE as i64).then_some(GRect { x: x0 as u32, y: y0 as u32, w: cw as u32, h: ch as u32 })
}

/// The windows to show: on screen, not minimized, front to back, at most
/// `MAX_SHOWN`.
pub fn shown(list: &[GuestWindow], mode: (u32, u32)) -> Vec<(u32, GRect)> {
    list.iter()
        .filter(|w| w.flags & win_flags::MINIMIZED == 0)
        .filter_map(|w| clip(w, mode).map(|g| (w.id, g)))
        .take(MAX_SHOWN)
        .collect()
}

/// A spot of the guest screen no shown window covers: the guest's pointer
/// waits there while the Mac's is elsewhere (Windows draws its pointer into
/// the screen; left on a window's edge, it would show there).
pub fn park(rects: &[GRect], mode: (u32, u32)) -> Option<(u32, u32)> {
    let (w, h) = (mode.0.max(1) - 1, mode.1.max(1) - 1);
    [(w, h), (0, h), (w, 0), (0, 0), (w / 2, h)].into_iter().find(|&(x, y)| !rects.iter().any(|r| r.contains(x, y)))
}

/// A guest window title as we show it.
pub fn title(raw: &str) -> String {
    let t = clean(raw);
    let t = t.trim();
    if t.chars().count() > 80 {
        t.chars().take(79).chain(['…']).collect()
    } else {
        t.to_string()
    }
}

/// The Mac screen's free area (no menu bar, no Dock) around a point.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[allow(unsafe_code)]
pub mod mac {
    use super::Rect;
    use std::ffi::{c_char, c_void};

    type Id = *mut c_void;
    type Sel = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CgRect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_msgSend();
    }

    unsafe fn sel(name: &std::ffi::CStr) -> Sel {
        // SAFETY: a NUL-terminated name.
        unsafe { sel_registerName(name.as_ptr()) }
    }

    unsafe fn id(obj: Id, s: &std::ffi::CStr) -> Id {
        // SAFETY: objc_msgSend called with the prototype of a method that
        // takes nothing and returns an object (arm64: no variadic ABI).
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(obj, sel(s))
        }
    }

    unsafe fn rect(obj: Id, s: &std::ffi::CStr) -> CgRect {
        // SAFETY: as above, for a method returning an NSRect (four doubles,
        // returned in registers on arm64).
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel) -> CgRect = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(obj, sel(s))
        }
    }

    /// (frame, free area) of the screen with the point (global points,
    /// top left origin), or the main screen's.
    pub fn screen_at(px: f64, py: f64) -> Option<(Rect, Rect)> {
        // SAFETY: AppKit calls on the main thread (the UI thread), on
        // objects AppKit returns; the array isn't kept.
        unsafe {
            let screens = id(objc_getClass(c"NSScreen".as_ptr()), c"screens");
            if screens.is_null() {
                return None;
            }
            let count: unsafe extern "C" fn(Id, Sel) -> usize = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let at: unsafe extern "C" fn(Id, Sel, usize) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let n = count(screens, sel(c"count"));
            if n == 0 {
                return None;
            }
            // Cocoa's y goes up from the bottom of the first screen.
            let main_h = rect(at(screens, sel(c"objectAtIndex:"), 0), c"frame").h;
            let flip = |r: CgRect| Rect { x: r.x, y: main_h - (r.y + r.h), w: r.w, h: r.h };
            let mut first = None;
            for i in 0..n {
                let s = at(screens, sel(c"objectAtIndex:"), i);
                let (f, v) = (flip(rect(s, c"frame")), flip(rect(s, c"visibleFrame")));
                if first.is_none() {
                    first = Some((f, v));
                }
                if px >= f.x && py >= f.y && px < f.x + f.w && py < f.y + f.h {
                    return Some((f, v));
                }
            }
            first
        }
    }

    /// Whether this app is the one in front.
    pub fn app_active() -> bool {
        // SAFETY: as above; isActive returns a BOOL.
        unsafe {
            let app = id(objc_getClass(c"NSApplication".as_ptr()), c"sharedApplication");
            if app.is_null() {
                return true;
            }
            let f: unsafe extern "C" fn(Id, Sel) -> bool = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(app, sel(c"isActive"))
        }
    }

    /// Put a window in front of the others of its level, or behind them,
    /// without giving it the keyboard. `view`: its NSView.
    pub fn order(view: *mut c_void, front: bool) {
        // SAFETY: as above; `view` is a live NSView of ours.
        unsafe {
            let win = id(view, c"window");
            if win.is_null() {
                return;
            }
            let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(win, sel(if front { c"orderFront:" } else { c"orderBack:" }), std::ptr::null_mut());
        }
    }
}

/// Elsewhere: the whole first monitor winit knows, less a guessed menu bar.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub mod mac {
    use super::Rect;
    pub fn screen_at(_x: f64, _y: f64) -> Option<(Rect, Rect)> {
        None
    }
    pub fn app_active() -> bool {
        true
    }
    pub fn order(_view: *mut std::ffi::c_void, _front: bool) {}
}

/// Put a winit window in front of ours (or behind everything) without
/// taking the keyboard.
fn order(w: &slint::Window, front: bool) {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    w.with_winit_window(|w| {
        if let Ok(h) = w.window_handle() {
            if let RawWindowHandle::AppKit(a) = h.as_raw() {
                mac::order(a.ns_view.as_ptr(), front);
            }
        }
    });
}

/// Which of our windows has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Win(u32),
    Bar,
}

struct CohWin {
    win: CoherenceWindow,
    /// The guest window's part of the screen, as last reported.
    g: GRect,
    /// Where we put its picture (points).
    at: Option<Rect>,
    /// The user moved or resized it lately (by the strip, the edges).
    user: Option<Instant>,
    /// The last move we asked of the guest, so a burst sends each once.
    sent: Option<(i32, i32, u32, u32)>,
    img: Option<SharedPixelBuffer<Rgba8Pixel>>,
    titled: bool,
    born: Instant,
}

pub struct Coherence {
    /// The guest's screen on the Mac (points); the mode once known.
    area: Rect,
    /// The other guest monitors, off while Coherence is on: the guest
    /// has one screen then (its windows gather there).
    extras: Vec<usize>,
    map: Option<Map>,
    pub(super) bar: CoherenceBar,
    chips: Rc<VecModel<CohChip>>,
    /// The apps the guest can start (names checked by the protocol;
    /// cleaned here), and the field's suggestions from them.
    apps: Vec<(String, Option<slint::Image>)>,
    suggestions: Rc<VecModel<CohApp>>,
    /// What the field holds.
    query: String,
    /// Chips in the order their windows first came.
    chip_order: Vec<u32>,
    wins: HashMap<u32, CohWin>,
    list: Vec<GuestWindow>,
    /// Front to back, as last put in order on the Mac.
    order: Vec<u32>,
    focus: Option<Target>,
    quiet_until: Instant,
    /// The guest's window in front.
    fg: Option<u32>,
    /// App icons, by guest window (only of windows in the list).
    icons: HashMap<u32, slint::Image>,
    /// The Mac screen the guest's screen is on (its frame), and its free area.
    screen: Rect,
    free: Rect,
    screens_at: Instant,
    /// Where the bar was at the last look (it may be on its way somewhere).
    bar_was: Option<(f64, f64)>,
}

impl Coherence {
    fn rects(&self) -> Vec<GRect> {
        self.wins.values().map(|w| w.g).collect()
    }
}

/// A guest window's Mac window: its callbacks find the VM by key and the
/// window by id (it may be gone by then).
fn wire_win(key: &str, io: &Rc<RefCell<Io>>, w: &CoherenceWindow, id: u32) {
    let k = key.to_string();
    w.on_mouse_move(move |x, y| {
        with(&k, |c| c.coh_pointer(id, x as f64, y as f64));
    });
    let i = io.clone();
    w.on_button(move |code, down| i.borrow_mut().button(code as u16, down));
    let i = io.clone();
    w.on_wheel(move |dx, dy| super::wheel(&i, dx, dy));
    let k = key.to_string();
    w.on_pointer_over(move |over| {
        let k = k.clone();
        // After Slint is done with the event (it sets its own pointer).
        slint::Timer::single_shot(Duration::ZERO, move || {
            with(&k, |c| c.coh_over(id, over));
        });
    });
    let k = key.to_string();
    w.on_drag(move || {
        // Now, inside the press: macOS drags with the event it's handling.
        let win = with(&k, |c| {
            let cw = c.coh.as_mut()?.wins.get_mut(&id)?;
            cw.user = Some(Instant::now());
            Some(cw.win.clone_strong())
        });
        if let Some(Some(win)) = win {
            win.window().with_winit_window(|w| {
                let _ = w.drag_window();
            });
        }
    });
    let i = io.clone();
    w.window().on_close_requested(move || {
        // Cmd+W and the like: ask the guest window to close.
        i.borrow_mut().send(ToVmm::Window { id, op: WindowOp::Close });
        slint::CloseRequestResponse::KeepWindowShown
    });
    let (k, i) = (key.to_string(), io.clone());
    w.window().on_winit_window_event(move |_w, ev| winit_event(&k, &i, Target::Win(id), ev));
}

/// Which keys and focus changes a Coherence window passes on.
fn winit_event(key: &str, io: &Rc<RefCell<Io>>, target: Target, ev: &WindowEvent) -> EventResult {
    match ev {
        WindowEvent::Focused(f) => {
            let (key, f) = (key.to_string(), *f);
            // After the event: focus moves between our windows in pairs.
            slint::Timer::single_shot(Duration::ZERO, move || {
                with(&key, |c| c.coh_focus(target, f));
            });
            EventResult::Propagate
        }
        WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
            if let Target::Win(id) = target {
                let key = key.to_string();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    with(&key, |c| c.coh_moved(id));
                });
            }
            EventResult::Propagate
        }
        _ => super::input_event(key, io, ev),
    }
}

impl Console {
    /// Why Coherence can't start now; empty if it can.
    pub(super) fn coherence_why(&self) -> &'static str {
        if self.features == 0 {
            "Coherence needs the guest tools (they aren't running)"
        } else if self.features & vroom_proto::features::COHERENCE == 0 {
            if self.windows_guest {
                "Coherence needs newer guest tools: run vroom-setup in the guest"
            } else {
                "Coherence needs newer guest tools: install the current vroom-agent in the guest"
            }
        } else if self.state != VmState::Running {
            "Coherence needs the VM running"
        } else {
            ""
        }
    }

    pub(super) fn coherence_toggle(&mut self) {
        if self.coh.is_some() {
            self.coherence_stop();
        } else {
            self.coherence_start();
        }
    }

    fn coherence_start(&mut self) {
        let why = self.coherence_why();
        if !why.is_empty() {
            self.toast(&format!("{why}."));
            return;
        }
        let w = self.win.window();
        let sf = w.scale_factor() as f64;
        // The screen the VM window is on.
        let center = w.with_winit_window(|w| {
            let p = w.outer_position().ok()?.to_logical::<f64>(sf);
            let s = w.outer_size().to_logical::<f64>(sf);
            Some((p.x + s.width / 2.0, p.y + s.height / 2.0))
        });
        let (cx, cy) = center.flatten().unwrap_or((0.0, 0.0));
        let (screen, free) = match mac::screen_at(cx, cy) {
            Some(s) => s,
            None => {
                let Some(m) = w.with_winit_window(|w| w.current_monitor()).flatten() else {
                    self.toast("Coherence: can't tell how big the screen is.");
                    return;
                };
                let (p, s) = (m.position().to_logical::<f64>(m.scale_factor()), m.size().to_logical::<f64>(m.scale_factor()));
                let f = Rect { x: p.x, y: p.y, w: s.width, h: s.height };
                (f, Rect { y: p.y + 25.0, h: s.height - 25.0, ..f })
            }
        };
        let area = area_in(free);
        let bar = match CoherenceBar::new() {
            Ok(b) => b,
            Err(e) => {
                self.toast(&format!("Coherence: {e}"));
                return;
            }
        };
        let chips = Rc::new(VecModel::<CohChip>::default());
        let suggestions = Rc::new(VecModel::<CohApp>::default());
        bar.set_suggestions(suggestions.clone().into());
        bar.set_vm_name(self.name.clone().into());
        bar.set_os(self.win.get_os());
        bar.set_chips(chips.clone().into());
        bar.set_note("Waiting for the guest's windows...".into());
        bar.set_can_start(self.features & vroom_proto::features::START_APP != 0);
        self.wire_bar(&bar);
        let bar_at = bar_in(free);
        bar.window().set_size(slint::LogicalSize::new(bar_at.w as f32, BAR as f32));
        log::info!("{}: coherence on, guest screen {:.0}x{:.0} points at {:.0},{:.0}", self.name, area.w, area.h, area.x, area.y);
        // The guest's screen takes the free area, in pixels.
        let (width_px, height_px, scale) = super::guest_size((area.w * sf) as u32, (area.h * sf) as u32, sf as f32, self.hidpi);
        {
            let mut io = self.io.borrow_mut();
            io.release_all();
            io.send(ToVmm::Resize { width: width_px, height: height_px, scale });
            io.send(ToVmm::Coherence(true));
        }
        self.pending_resize = None;
        let extras = self.extras_pause();
        self.coh = Some(Coherence {
            area,
            extras,
            map: (self.mode != (0, 0)).then_some(Map { area, mode: self.mode }),
            bar,
            chips,
            apps: Vec::new(),
            suggestions,
            query: String::new(),
            chip_order: Vec::new(),
            wins: HashMap::new(),
            list: Vec::new(),
            order: Vec::new(),
            focus: None,
            quiet_until: Instant::now() + QUIET,
            fg: None,
            icons: HashMap::new(),
            screen,
            free,
            screens_at: Instant::now(),
            bar_was: None,
        });
        let _ = self.win.hide();
        if let Some(c) = &self.coh {
            let _ = c.bar.show();
            // Once shown: before, macOS places it where it likes.
            let bar = c.bar.as_weak();
            slint::Timer::single_shot(Duration::from_millis(100), move || {
                if let Some(b) = bar.upgrade() {
                    set_frame(b.window(), bar_at);
                    log::debug!("coherence bar to {bar_at:?}, now at {:?}", b.window().with_winit_window(|w| w.outer_position().ok().map(|p| p.to_logical::<f64>(w.scale_factor()))));
                }
            });
        }
    }

    /// A Coherence window by title (part of it), else the guest's in front.
    pub(super) fn coh_window(&self, title: &str) -> Option<CoherenceWindow> {
        let c = self.coh.as_ref()?;
        let id = match title {
            "" => c.fg?,
            t => c.list.iter().find(|w| w.title.contains(t))?.id,
        };
        c.wins.get(&id).map(|w| w.win.clone_strong())
    }

    /// The VM is gone: our windows go with it.
    pub(super) fn coh_close(&mut self) {
        if let Some(c) = self.coh.take() {
            for w in c.wins.values() {
                let _ = w.win.hide();
            }
            let _ = c.bar.hide();
        }
    }

    pub(super) fn coherence_stop(&mut self) {
        let Some(c) = self.coh.take() else { return };
        log::info!("{}: coherence off", self.name);
        for w in c.wins.values() {
            let _ = w.win.hide();
        }
        let _ = c.bar.hide();
        {
            let mut io = self.io.borrow_mut();
            io.release_all();
            io.send(ToVmm::Coherence(false));
        }
        let _ = self.win.show();
        self.win.window().with_winit_window(|w| w.focus_window());
        // The guest goes back to the VM window's size.
        self.pending_resize = Some(Instant::now());
        self.extras_resume(&c.extras);
    }

    fn wire_bar(&self, bar: &CoherenceBar) {
        let i = self.io.clone();
        bar.on_start(move || {
            let mut io = i.borrow_mut();
            // The Windows key opens Start.
            io.key(125, true);
            io.key(125, false);
        });
        let w = bar.as_weak();
        bar.on_drag(move || {
            if let Some(b) = w.upgrade() {
                b.window().with_winit_window(|w| {
                    let _ = w.drag_window();
                });
            }
        });
        let i = self.io.clone();
        bar.on_chip(move |id| i.borrow_mut().send(ToVmm::Window { id: id as u32, op: WindowOp::Activate }));
        let i = self.io.clone();
        bar.on_start_app(move |name| {
            let name = name.trim();
            if vroom_proto::line_ok(name) {
                i.borrow_mut().send(ToVmm::StartApp(name.to_string()));
            }
        });
        let k = self.key.clone();
        bar.on_typed(move |text| {
            with(&k, |c| c.coh_suggest(&text));
        });
        let k = self.key.clone();
        bar.on_exit(move || {
            let k = k.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                with(&k, |c| c.coherence_stop());
            });
        });
        let k = self.key.clone();
        bar.window().on_close_requested(move || {
            let k = k.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                with(&k, |c| c.coherence_stop());
            });
            slint::CloseRequestResponse::KeepWindowShown
        });
        let (k, i, b) = (self.key.clone(), self.io.clone(), bar.as_weak());
        bar.window().on_winit_window_event(move |_w, ev| {
            // Typing an app's name: the keys are the field's, not the guest's.
            let keys = matches!(ev, WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_));
            if keys && b.upgrade().is_some_and(|b| b.get_typing()) {
                return EventResult::Propagate;
            }
            winit_event(&k, &i, Target::Bar, ev)
        });
    }

    /// A new window list from the guest.
    pub(super) fn coh_windows(&mut self, list: Vec<GuestWindow>) {
        let Some(c) = self.coh.as_mut() else { return };
        c.fg = list.iter().find(|w| w.flags & win_flags::FOREGROUND != 0).map(|w| w.id);
        c.icons.retain(|id, _| list.iter().any(|w| w.id == *id));
        c.list = list;
        self.coh_layout();
    }

    /// App icons from the guest, for windows of the list.
    pub(super) fn coh_icons(&mut self, icons: std::collections::BTreeMap<u32, (u32, u32, Vec<u8>)>) {
        let Some(c) = self.coh.as_mut() else { return };
        for (id, (w, h, rgba)) in icons {
            if !c.list.iter().any(|g| g.id == id) {
                continue;
            }
            match icon_image(w, h, &rgba) {
                Some(i) => c.icons.insert(id, i),
                None => c.icons.remove(&id),
            };
        }
        self.coh_layout();
    }

    /// The apps the guest can start: the field suggests them.
    pub(super) fn coh_apps(&mut self, list: Vec<vroom_display::GuestApp>) {
        let Some(c) = self.coh.as_mut() else { return };
        log::debug!("{}: coherence: {} apps to start", self.name, list.len());
        c.apps = list
            .into_iter()
            .map(|a| {
                let icon = icon_image(a.w, a.h, &a.rgba);
                (title(&a.name), icon)
            })
            .filter(|(n, _)| !n.is_empty())
            .collect();
        let q = c.query.clone();
        self.coh_suggest(&q);
    }

    /// What the field suggests for `text`.
    fn coh_suggest(&mut self, text: &str) {
        let Some(c) = self.coh.as_mut() else { return };
        c.query = text.to_string();
        let names: Vec<String> = c.apps.iter().map(|a| a.0.clone()).collect();
        let picks: Vec<CohApp> = suggest(&names, text, SUGGEST)
            .into_iter()
            .map(|i| {
                let (name, icon) = &c.apps[i];
                CohApp { name: name.as_str().into(), has_icon: icon.is_some(), icon: icon.clone().unwrap_or_default() }
            })
            .collect();
        c.suggestions.set_vec(picks);
        if c.bar.get_pick() >= c.suggestions.row_count() as i32 {
            c.bar.set_pick(0);
        }
    }

    /// The guest says Coherence can't work there.
    pub(super) fn coh_no(&mut self, why: &str) {
        if self.coh.is_none() {
            return;
        }
        self.coherence_stop();
        self.toast(&format!("Coherence can't work in this guest: {}.", clean(why).trim_end_matches('.')));
    }

    /// Bring the Mac windows in line with the guest's list and screen:
    /// which exist, where, in what order, which has the keyboard, the bar,
    /// the pictures.
    fn coh_layout(&mut self) {
        let (name, os, key, mode, io) = (self.name.clone(), self.win.get_os(), self.key.clone(), self.mode, self.io.clone());
        let Some(c) = self.coh.as_mut() else { return };
        c.map = (mode != (0, 0)).then_some(Map { area: c.area, mode });
        let want = match c.map {
            Some(m) => shown(&c.list, m.mode),
            None => Vec::new(),
        };
        let now = Instant::now();
        // Gone, or off the screen now.
        let gone: Vec<u32> = c.wins.keys().filter(|id| !want.iter().any(|(w, _)| w == *id)).copied().collect();
        for id in gone {
            c.quiet_until = now + QUIET;
            if let Some(w) = c.wins.remove(&id) {
                let _ = w.win.hide();
            }
            if c.focus == Some(Target::Win(id)) {
                c.focus = None;
            }
        }
        let active = mac::app_active();
        for (id, g) in &want {
            let gw = c.list.iter().find(|w| w.id == *id);
            let w = match c.wins.entry(*id) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let Ok(win) = CoherenceWindow::new() else { continue };
                    win.set_vm_name(name.clone().into());
                    win.set_os(os.clone());
                    wire_win(&key, &io, &win, *id);
                    c.quiet_until = now + QUIET;
                    e.insert(CohWin { win, g: *g, at: None, user: None, sent: None, img: None, titled: false, born: now })
                }
            };
            if w.g != *g {
                w.g = *g;
                w.img = None;
            }
            w.win.set_win_title(gw.map(|w| title(&w.title)).unwrap_or_default().into());
            w.win.set_front(c.fg == Some(*id));
            w.titled = gw.is_some_and(|w| !w.title.trim().is_empty());
            let first = w.at.is_none();
            if first || w.user.is_none_or(|t| now.duration_since(t) > HANDS_OFF) {
                place(w, c.map, first);
            }
            if first {
                // Placed before it shows; behind other apps' windows if
                // vroom isn't the app in front (a guest can't pop windows
                // over the user's work).
                let _ = w.win.show();
                place(w, c.map, true);
                if !active {
                    order(w.win.window(), false);
                }
            }
        }
        let ids: Vec<u32> = want.iter().map(|(id, _)| *id).collect();
        // The guest's order, front to back: put the Mac windows the same
        // way, back ones first (only while vroom is in front: ordering
        // windows of an app in the back would put them over the user's).
        if active && c.order != ids {
            for id in ids.iter().rev() {
                if let Some(w) = c.wins.get(id) {
                    order(w.win.window(), true);
                }
            }
            c.order = ids.clone();
        }
        // The guest's window in front gets the keyboard on the Mac too,
        // while the user is in the VM.
        if let (Some(fg), Some(_)) = (c.fg, c.focus) {
            if c.focus != Some(Target::Win(fg)) {
                if let Some(w) = c.wins.get(&fg) {
                    c.quiet_until = now + QUIET;
                    w.win.window().with_winit_window(|w| w.focus_window());
                }
            }
        }
        // The bar: app windows in the order they came.
        let apps: Vec<&GuestWindow> = c.list.iter().filter(|w| w.flags & win_flags::APP != 0).collect();
        c.chip_order.retain(|id| apps.iter().any(|w| w.id == *id));
        for w in &apps {
            if !c.chip_order.contains(&w.id) {
                c.chip_order.push(w.id);
            }
        }
        let chips: Vec<CohChip> = c
            .chip_order
            .iter()
            .filter_map(|id| apps.iter().find(|w| w.id == *id))
            .take(12)
            .map(|w| {
                let icon = c.icons.get(&w.id).cloned();
                CohChip {
                    id: w.id as i32,
                    title: title(&w.title).into(),
                    front: c.fg == Some(w.id),
                    minimized: w.flags & win_flags::MINIMIZED != 0,
                    has_icon: icon.is_some(),
                    icon: icon.unwrap_or_default(),
                }
            })
            .collect();
        let same = c.chips.row_count() == chips.len()
            && c.chips.iter().zip(&chips).all(|(a, b)| (a.id, &a.title, a.front, a.minimized, a.has_icon) == (b.id, &b.title, b.front, b.minimized, b.has_icon) && a.icon == b.icon);
        if !same {
            c.chips.set_vec(chips);
        }
        let note = if c.list.is_empty() { "No windows open in the guest (or nobody is logged in)" } else { "" };
        c.bar.set_note(note.into());
        // Pictures for new or moved windows (the rest follow damage).
        self.coh_pictures(Some([0, 0, 0, 0]));
    }

    /// Copy the guest screen into the windows' pictures: where `damage`
    /// ([x0, y0, x1, y1]) touches them, or all of them.
    pub(super) fn coh_pictures(&mut self, damage: Option<[u32; 4]>) {
        let (Some(fb), Some(c)) = (self.fb.as_ref(), self.coh.as_mut()) else { return };
        let mode = self.mode;
        if mode == (0, 0) {
            return;
        }
        for w in c.wins.values_mut() {
            let g = w.g;
            if g.x + g.w > mode.0 || g.y + g.h > mode.1 {
                continue;
            }
            let fresh = w.img.as_ref().is_none_or(|b| (b.width(), b.height()) != (g.w, g.h));
            let part = match (fresh, damage) {
                (true, _) | (false, None) => Some(g),
                (false, Some(d)) => g.cut(d),
            };
            let Some(p) = part else { continue };
            let mut buf = match w.img.take() {
                Some(b) if !fresh => {
                    // Ours alone again: only the changed rows are written.
                    w.win.set_frame(slint::Image::default());
                    b
                }
                _ => SharedPixelBuffer::new(g.w, g.h),
            };
            crop(fb, mode.0, g, p, buf.make_mut_bytes());
            w.img = Some(buf.clone());
            w.win.set_frame(slint::Image::from_rgba8(buf));
        }
    }

    /// The guest's screen changed size: everything moves.
    pub(super) fn coh_mode(&mut self) {
        if self.coh.is_some() {
            self.coh_layout();
            self.coh_pictures(None);
        }
    }

    fn coh_pointer(&mut self, id: u32, x: f64, y: f64) {
        let Some(c) = self.coh.as_ref() else { return };
        let (Some(m), Some(w)) = (c.map, c.wins.get(&id)) else { return };
        let win = w.win.window();
        let s = win.size().to_logical(win.scale_factor());
        let (x, y) = m.pointer(w.g, x, y, (s.width as f64, s.height as f64 - STRIP));
        self.io.borrow_mut().send(ToVmm::Pointer { x, y });
    }

    /// The mouse came over a window's picture, or left it.
    fn coh_over(&mut self, id: u32, over: bool) {
        let draws = self.draws_pointer;
        let cursor = self.cursor.clone();
        let host = self.host_cursors.last().map(|(_, _, c)| c.clone());
        let Some(c) = self.coh.as_ref() else { return };
        let Some(w) = c.wins.get(&id) else { return };
        if !over {
            // Park the guest's pointer where no window shows it.
            if let (Some(m), Some((x, y))) = (c.map, c.map.and_then(|m| park(&c.rects(), m.mode))) {
                let (px, py) = m.abs(x as f64, y as f64);
                self.io.borrow_mut().send(ToVmm::Pointer { x: px, y: py });
            }
            return;
        }
        w.win.window().with_winit_window(|w| match (&cursor, host) {
            (GuestCursor::Image { .. }, Some(h)) => {
                w.set_cursor(h);
                w.set_cursor_visible(true);
            }
            (GuestCursor::Hidden, _) | (GuestCursor::Unset, _) if draws || matches!(cursor, GuestCursor::Hidden) => w.set_cursor_visible(false),
            _ => {
                w.set_cursor(slint::winit_030::winit::window::CursorIcon::Default);
                w.set_cursor_visible(true);
            }
        });
    }

    /// One of our windows got or lost the keyboard.
    fn coh_focus(&mut self, t: Target, focused: bool) {
        let Some(c) = self.coh.as_mut() else { return };
        if focused {
            let was = c.focus.replace(t);
            if was.is_none() {
                let mut io = self.io.borrow_mut();
                io.focused = true;
                io.send(ToVmm::Focus(true));
            }
            // The user picked a guest window on the Mac (a click, Cmd+`):
            // bring it to the front in the guest too. Not menus and the
            // like (no title): activating one would close it.
            if let Target::Win(id) = t {
                let titled = c.wins.get(&id).is_some_and(|w| w.titled);
                if Instant::now() >= c.quiet_until && c.fg != Some(id) && titled {
                    self.io.borrow_mut().send(ToVmm::Window { id, op: WindowOp::Activate });
                }
            }
            self.clip_seen = None;
            return;
        }
        if c.focus != Some(t) {
            return;
        }
        c.focus = None;
        // Unless another of ours takes it right away, the VM lost the keyboard.
        let k = self.key.clone();
        slint::Timer::single_shot(Duration::from_millis(60), move || {
            with(&k, |c| {
                if c.coh.as_ref().is_some_and(|h| h.focus.is_none()) {
                    let mut io = c.io.borrow_mut();
                    io.focused = false;
                    io.release_all();
                    io.send(ToVmm::Focus(false));
                }
            });
        });
    }

    /// A Mac window moved or changed size: by us, or by the user. Then
    /// the guest window follows.
    fn coh_moved(&mut self, id: u32) {
        let Some(c) = self.coh.as_mut() else { return };
        let Some(m) = c.map else { return };
        let Some(w) = c.wins.get_mut(&id) else { return };
        let Some(now) = picture_rect(&w.win) else { return };
        // Ours, or macOS placing a new window before we could.
        let ours = w.at.is_some_and(|a| close(a, now)) || w.born.elapsed() < Duration::from_secs(1);
        if ours {
            return;
        }
        w.user = Some(Instant::now());
        // Dragged to another screen, or off this one: the guest keeps it
        // on its screen, and the window goes back there once let go.
        let r = m.guest(keep_on(now, m.area));
        if w.sent == Some(r) {
            return;
        }
        w.sent = Some(r);
        let (x, y, gw, gh) = r;
        self.io.borrow_mut().send(ToVmm::Window { id, op: WindowOp::Move { x, y, w: gw, h: gh } });
    }

    /// Every tick: windows the user stopped moving go back to where the
    /// guest says they are.
    pub(super) fn coh_tick(&mut self) {
        let Some(c) = self.coh.as_mut() else { return };
        let now = Instant::now();
        for w in c.wins.values_mut() {
            if w.user.is_some_and(|t| now.duration_since(t) > HANDS_OFF) {
                w.user = None;
                w.sent = None;
            }
            if w.user.is_none() {
                place(w, c.map, false);
            }
        }
        if self.state != VmState::Running && self.state != VmState::Paused {
            self.coherence_stop();
            return;
        }
        self.coh_screens();
    }

    /// The guest's screen follows the bar to another Mac screen, and the
    /// screen's free area when it changes (the Dock moved or hid, another
    /// resolution, the screen unplugged).
    fn coh_screens(&mut self) {
        let Some(c) = self.coh.as_mut() else { return };
        if c.screens_at.elapsed() < SCREENS_EVERY {
            return;
        }
        c.screens_at = Instant::now();
        let bw = c.bar.window();
        let at = bw
            .with_winit_window(|w| {
                let sf = w.scale_factor();
                let p = w.outer_position().ok()?.to_logical::<f64>(sf);
                let s = w.outer_size().to_logical::<f64>(sf);
                Some((p.x + s.width / 2.0, p.y + s.height / 2.0))
            })
            .flatten();
        let Some(at) = at else { return };
        // Still on its way: wait until it stays put.
        if c.bar_was.replace(at) != Some(at) {
            return;
        }
        let Some((screen, free)) = mac::screen_at(at.0, at.1) else { return };
        if screen == c.screen && free == c.free {
            return;
        }
        let moved = screen != c.screen;
        log::info!("{}: coherence: guest screen to {free:?} ({})", self.name, if moved { "another screen" } else { "the screen changed" });
        let area = area_in(free);
        (c.screen, c.free, c.area) = (screen, free, area);
        let bar_at = bar_in(free);
        bw.set_size(slint::LogicalSize::new(bar_at.w as f32, BAR as f32));
        set_frame(bw, bar_at);
        c.bar_was = None;
        let sf = bw.scale_factor() as f64;
        let (width_px, height_px, scale) = super::guest_size((area.w * sf) as u32, (area.h * sf) as u32, sf as f32, self.hidpi);
        self.io.borrow_mut().send(ToVmm::Resize { width: width_px, height: height_px, scale });
        // Placed again with the new mode (coh_mode); the map changes now.
        self.coh_layout();
    }
}

/// Put a window where the guest's says (its picture, the strip above).
/// Checked again on every tick too: until macOS has made the window,
/// Slint keeps a place for it that macOS doesn't always follow.
fn place(w: &mut CohWin, map: Option<Map>, first: bool) {
    let Some(m) = map else { return };
    let r = m.mac(w.g);
    let there = picture_rect(&w.win).is_some_and(|now| close(now, r));
    if !first && there && w.at.is_some_and(|a| close(a, r)) {
        return;
    }
    w.at = Some(r);
    set_frame(w.win.window(), Rect { x: r.x, y: r.y - STRIP, w: r.w, h: r.h + STRIP });
}

/// Put a window's frame here (points), through winit once it exists.
fn set_frame(win: &slint::Window, r: Rect) {
    use slint::winit_030::winit::dpi::{LogicalPosition, LogicalSize};
    let done = win.with_winit_window(|w| {
        w.set_outer_position(LogicalPosition::new(r.x, r.y));
        let _ = w.request_inner_size(LogicalSize::new(r.w, r.h));
    });
    if done.is_none() {
        win.set_position(slint::LogicalPosition::new(r.x as f32, r.y as f32));
        win.set_size(slint::LogicalSize::new(r.w as f32, r.h as f32));
    }
}

/// Where a window's picture is now (points).
fn picture_rect(w: &CoherenceWindow) -> Option<Rect> {
    w.window()
        .with_winit_window(|w| {
            let sf = w.scale_factor();
            let p = w.outer_position().ok()?.to_logical::<f64>(sf);
            let s = w.inner_size().to_logical::<f64>(sf);
            Some(Rect { x: p.x, y: p.y + STRIP, w: s.width, h: s.height - STRIP })
        })
        .flatten()
}

fn close(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() < 1.5 && (a.y - b.y).abs() < 1.5 && (a.w - b.w).abs() < 1.5 && (a.h - b.h).abs() < 1.5
}

/// Copy part `p` of window `g` from the screen (`fb_w` wide, XRGB) into
/// its picture (RGBA, `g.w` wide).
fn crop(fb: &SharedFb, fb_w: u32, g: GRect, p: GRect, out: &mut [u8]) {
    let stride = g.w as usize * 4;
    let off = (p.y - g.y) as usize * stride + (p.x - g.x) as usize * 4;
    let Some(dst) = out.get_mut(off..) else { return };
    fb.read_rect(fb_w, p.x, p.y, p.w, p.h, dst, stride);
    let row = p.w as usize * 4;
    for r in 0..p.h as usize {
        if let Some(px) = dst.get_mut(r * stride..r * stride + row) {
            super::xrgb_to_rgba(px);
        }
    }
}

#[cfg(test)]
mod suggest_tests {
    use super::suggest;

    #[test]
    fn names_are_suggested_best_first() {
        let names: Vec<String> = ["Notepad++", "Notepad", "Calculator", "Visual Studio Code", "Paint", "Paint 3D", "Snipping Tool"].iter().map(|s| s.to_string()).collect();
        let s = |q: &str| suggest(&names, q, 3).into_iter().map(|i| names[i].as_str()).collect::<Vec<_>>();
        assert_eq!(s("notepad"), ["Notepad", "Notepad++"]);
        assert_eq!(s("Note"), ["Notepad", "Notepad++"]);
        assert_eq!(s("paint"), ["Paint", "Paint 3D"]);
        assert_eq!(s("code"), ["Visual Studio Code"]);
        assert_eq!(s("tool"), ["Snipping Tool"]);
        assert_eq!(s("a"), ["Paint", "Notepad", "Paint 3D"], "inside a name: shorter first");
        assert_eq!(s("s"), ["Snipping Tool", "Visual Studio Code"], "a name's start before a word's");
        assert!(s("").is_empty() && s("  ").is_empty() && s("zz").is_empty());
        assert_eq!(suggest(&names, "p", 1).len(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gw(id: u32, x: i32, y: i32, w: u32, h: u32, flags: u32) -> GuestWindow {
        GuestWindow { id, x, y, w, h, flags, title: format!("w{id}") }
    }

    #[test]
    fn windows_are_clipped_to_the_screen() {
        let mode = (1000, 800);
        assert_eq!(clip(&gw(1, -8, -8, 100, 50, 0), mode), Some(GRect { x: 0, y: 0, w: 92, h: 42 }));
        assert_eq!(clip(&gw(1, 950, 790, 100, 50, 0), mode), Some(GRect { x: 950, y: 790, w: 50, h: 10 }));
        assert_eq!(clip(&gw(1, 998, 0, 100, 50, 0), mode), None, "2 pixels on screen");
        assert_eq!(clip(&gw(1, -70000, 0, 65536, 50, 0), mode), None);
        assert_eq!(clip(&gw(1, 0, 0, 65536, 65536, 0), mode), Some(GRect { x: 0, y: 0, w: 1000, h: 800 }), "never more than the screen");
        let list: Vec<GuestWindow> = (0..40).map(|i| gw(i, 10, 10, 100, 100, if i == 0 { win_flags::MINIMIZED } else { 0 })).collect();
        let s = shown(&list, mode);
        assert_eq!(s.len(), MAX_SHOWN);
        assert_eq!(s[0].0, 1, "minimized windows aren't shown; front first");
    }

    #[test]
    fn points_and_pixels_map_both_ways() {
        // A Retina screen's free area, the guest at 2x.
        let m = Map { area: Rect { x: 0.0, y: 58.0, w: 1728.0, h: 1000.0 }, mode: (3456, 2000) };
        let g = GRect { x: 200, y: 100, w: 800, h: 600 };
        let r = m.mac(g);
        assert_eq!(r, Rect { x: 100.0, y: 108.0, w: 400.0, h: 300.0 });
        assert_eq!(m.guest(r), (200, 100, 800, 600));
        // A mode that isn't the area's shape (Windows' pixel cap).
        let m2 = Map { mode: (2650, 1534), ..m };
        let back = m2.guest(m2.mac(g));
        assert_eq!(back, (200, 100, 800, 600));
        // Pointer: the picture's top left is the window's; the far
        // corner of the screen is the pointer's end.
        let pic = (r.w, r.h);
        assert_eq!(m.pointer(g, 0.0, 0.0, pic), m.abs(200.0, 100.0));
        assert_eq!(m.pointer(g, 200.0, 150.0, pic), m.abs(600.0, 400.0), "the middle");
        // A picture stretched (the Mac window resized ahead of the guest's).
        assert_eq!(m.pointer(g, 400.0, 300.0, (800.0, 600.0)), m.abs(600.0, 400.0));
        assert_eq!(m.pointer(GRect { x: 3455, y: 1999, w: 1, h: 1 }, 50.0, 50.0, (0.5, 0.5)), (POINTER_MAX, POINTER_MAX));
        assert_eq!(m.pointer(g, f64::NAN, -5.0, pic).1, m.abs(0.0, 90.0).1);
        assert_eq!(m.pointer(g, 10.0, 10.0, (f64::NAN, 0.0)), m.abs(200.0, 100.0), "no size: the corner");
        // Junk from the Mac side stays in range.
        let (x, _, w, _) = m.guest(Rect { x: f64::INFINITY, y: 0.0, w: -3.0, h: 1e12 });
        assert_eq!((x, w), (0, 1));
    }

    #[test]
    fn the_pointer_parks_where_no_window_is() {
        let mode = (100, 100);
        assert_eq!(park(&[], mode), Some((99, 99)));
        let full = GRect { x: 0, y: 0, w: 100, h: 100 };
        assert_eq!(park(&[full], mode), None);
        let right = GRect { x: 50, y: 0, w: 50, h: 100 };
        assert_eq!(park(&[right], mode), Some((0, 99)));
    }

    #[test]
    fn damage_reaches_only_its_windows() {
        let g = GRect { x: 10, y: 10, w: 20, h: 20 };
        assert_eq!(g.cut([0, 0, 15, 15]), Some(GRect { x: 10, y: 10, w: 5, h: 5 }));
        assert_eq!(g.cut([30, 0, 40, 40]), None);
        assert_eq!(g.cut([0, 0, 5, 5]), None, "before it: no overflow");
        // A picture made from part of the screen.
        let fb = SharedFb::create().unwrap();
        fb.write_rect(4, 0, 0, 4, 2, &[0x33, 0x22, 0x11, 0x00].repeat(8), 16);
        let mut out = vec![0u8; 2 * 2 * 4];
        crop(&fb, 4, GRect { x: 2, y: 0, w: 2, h: 2 }, GRect { x: 3, y: 1, w: 1, h: 1 }, &mut out);
        assert_eq!(&out[12..16], &[0x11, 0x22, 0x33, 0xff]);
        assert_eq!(&out[..12], &[0; 12]);
    }

    #[test]
    fn moved_windows_stay_on_the_guest_screen() {
        let area = Rect { x: 0.0, y: 45.0, w: 1000.0, h: 700.0 };
        let r = Rect { x: 100.0, y: 100.0, w: 300.0, h: 200.0 };
        assert_eq!(keep_on(r, area), r, "inside: as it is");
        // Dragged onto a screen to the right, or above the top.
        assert_eq!(keep_on(Rect { x: 1500.0, y: -300.0, ..r }, area), Rect { x: 960.0, y: 45.0, ..r });
        assert_eq!(keep_on(Rect { x: -2000.0, y: 2000.0, ..r }, area), Rect { x: -260.0, y: 705.0, ..r });
        let free = Rect { x: 1728.0, y: 25.0, w: 2560.0, h: 1390.0 };
        assert_eq!(area_in(free), Rect { x: 1728.0, y: 45.0, w: 2560.0, h: 1326.0 });
        assert_eq!(bar_in(free), Rect { x: 1728.0 + 830.0, y: 25.0 + 1390.0 - BAR, w: 900.0, h: BAR });
    }

    #[test]
    fn icons_are_checked_again() {
        assert!(icon_image(2, 2, &[0; 16]).is_some());
        assert!(icon_image(2, 2, &[0; 15]).is_none());
        assert!(icon_image(65, 1, &[0; 260]).is_none());
        assert!(icon_image(0, 0, &[]).is_none());
    }

    #[test]
    fn titles_are_cleaned_and_short() {
        assert_eq!(title("a\u{202e}b\nc"), "abc");
        assert_eq!(title(&"x".repeat(300)).chars().count(), 80);
    }
}
