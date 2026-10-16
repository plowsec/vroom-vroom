//! Coherence on Windows: the windows on screen, and what the host may do
//! to them.
//!
//! Coordinates are the screen's real pixels: the calls run with
//! per-monitor DPI awareness (this process is otherwise DPI-unaware, and
//! Windows would scale what we read at 125% and up). A window's place is
//! what can be seen of it (DWM's frame bounds), without the invisible
//! resize borders Windows 10 and 11 put around it.

#![allow(unsafe_code)]

use std::ffi::c_void;

use vroom_proto::{win_flags, GuestWindow, WindowOp};
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId};
use windows_sys::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, FindWindowW, GetAncestor, GetTopWindow, GA_ROOT, GW_HWNDNEXT, GetClassNameW, GetForegroundWindow, GetSystemMetrics, GetWindow, GetWindowLongPtrW,
    GetWindowPlacement, GetWindowRect, GetWindowThreadProcessId, InternalGetWindowText, IsIconic, IsWindow, IsWindowVisible, IsZoomed,
    PeekMessageW, PostMessageW, SetForegroundWindow, SetWindowPos, ShowWindowAsync, SystemParametersInfoW, GWL_EXSTYLE, GWL_STYLE, GW_OWNER,
    MSG, PM_NOREMOVE, SC_CLOSE, SM_CXSCREEN, SM_CYSCREEN, SPIF_SENDCHANGE, SPI_GETWORKAREA, SPI_SETWORKAREA, SWP_ASYNCWINDOWPOS,
    SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_MINIMIZE, SW_RESTORE, SW_SHOWNA, WINDOWPLACEMENT, WM_SYSCOMMAND, WS_CAPTION,
    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

/// Windows that are the desktop itself, or ours.
const SKIP: &[&str] = &["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd", "VroomDropSource"];

pub struct WinDesktop {
    /// The handles of the last list (the host may only touch those).
    listed: Vec<isize>,
    /// The taskbar may be hidden (by us, or by an agent that crashed).
    hidden: bool,
    /// This process hid it.
    ours: bool,
    /// The last window in front we said we leave out (logs).
    fg_seen: isize,
    /// Log what the next list leaves out (Coherence just went on).
    explain: bool,
}

/// Runs `f` with per-monitor DPI awareness on this thread.
fn real_pixels<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: plain calls; the old context goes back.
    let old: DPI_AWARENESS_CONTEXT = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let r = f();
    if !old.is_null() {
        // SAFETY: as above.
        unsafe { SetThreadDpiAwarenessContext(old) };
    }
    r
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn hwnd(id: u32) -> HWND {
    // Window handles are 32-bit values, sign-extended on 64-bit Windows.
    id as i32 as isize as HWND
}

fn class(h: HWND) -> String {
    let mut b = [0u16; 64];
    // SAFETY: buffer and its length.
    let n = unsafe { GetClassNameW(h, b.as_mut_ptr(), b.len() as i32) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

fn title(h: HWND) -> String {
    let mut b = [0u16; 260];
    // SAFETY: buffer and its length. Unlike GetWindowText this never
    // sends the window a message (a hung app can't stall us).
    let n = unsafe { InternalGetWindowText(h, b.as_mut_ptr(), b.len() as i32) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

/// What can be seen of a window, else its whole rectangle.
fn bounds(h: HWND) -> Option<RECT> {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: out pointer to a RECT, its size.
    let hr = unsafe { DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS as u32, &mut r as *mut RECT as *mut c_void, std::mem::size_of::<RECT>() as u32) };
    // SAFETY: out pointer.
    if hr < 0 && unsafe { GetWindowRect(h, &mut r) } == 0 {
        return None;
    }
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

fn cloaked(h: HWND) -> bool {
    let mut c = 0u32;
    // SAFETY: out pointer to a u32, its size.
    let hr = unsafe { DwmGetWindowAttribute(h, DWMWA_CLOAKED as u32, &mut c as *mut u32 as *mut c_void, 4) };
    hr >= 0 && c != 0
}

fn screen() -> RECT {
    // SAFETY: plain calls.
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    RECT { left: 0, top: 0, right: w, bottom: h }
}

fn same(a: &RECT, b: &RECT) -> bool {
    (a.left, a.top, a.right, a.bottom) == (b.left, b.top, b.right, b.bottom)
}

fn work_area() -> RECT {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: out pointer to a RECT.
    unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut r as *mut RECT as *mut c_void, 0) };
    r
}

fn set_work_area(mut r: RECT) {
    // SAFETY: pointer to a RECT; maximized windows follow (SENDCHANGE).
    unsafe { SystemParametersInfoW(SPI_SETWORKAREA, 0, &mut r as *mut RECT as *mut c_void, SPIF_SENDCHANGE) };
}

/// One window as the host sees it, or why it is left out.
fn entry(h: HWND, fg: HWND, me: u32, scr: &RECT) -> Result<GuestWindow, &'static str> {
    // SAFETY: plain calls on a handle from the walk (one that went
    // meanwhile makes them fail, nothing worse).
    let (visible, iconic, zoomed) = unsafe { (IsWindowVisible(h) != 0, IsIconic(h) != 0, IsZoomed(h) != 0) };
    if !visible {
        return Err("hidden");
    }
    if cloaked(h) {
        return Err("cloaked");
    }
    let mut pid = 0u32;
    // SAFETY: out pointer.
    unsafe { GetWindowThreadProcessId(h, &mut pid) };
    if pid == me || SKIP.contains(&class(h).as_str()) {
        return Err("desktop or ours");
    }
    // SAFETY: plain calls.
    let (style, ex, owner) = unsafe { (GetWindowLongPtrW(h, GWL_STYLE) as u32, GetWindowLongPtrW(h, GWL_EXSTYLE) as u32, GetWindow(h, GW_OWNER)) };
    let title = title(h);
    let app = (owner.is_null() && ex & WS_EX_TOOLWINDOW == 0 && style & WS_CAPTION == WS_CAPTION || ex & WS_EX_APPWINDOW != 0) && !title.is_empty();
    let r = if iconic {
        // Where it goes back to; the host only lists it.
        // SAFETY: zeroed WINDOWPLACEMENT with its length set.
        let mut p: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
        p.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        // SAFETY: out pointer.
        if !app || unsafe { GetWindowPlacement(h, &mut p) } == 0 {
            return Err("minimized");
        }
        let r = p.rcNormalPosition;
        if r.right <= r.left || r.bottom <= r.top {
            return Err("empty");
        }
        r
    } else {
        let r = bounds(h).ok_or("empty")?;
        if r.right <= scr.left || r.bottom <= scr.top || r.left >= scr.right || r.top >= scr.bottom {
            return Err("off the screen");
        }
        if r.right - r.left < 2 || r.bottom - r.top < 2 {
            return Err("too small");
        }
        r
    };
    let mut flags = 0;
    if h == fg {
        flags |= win_flags::FOREGROUND;
    }
    if iconic {
        flags |= win_flags::MINIMIZED;
    }
    if app {
        flags |= win_flags::APP;
    }
    if zoomed {
        flags |= win_flags::MAXIMIZED;
    }
    Ok(GuestWindow { id: h as isize as u32, x: r.left, y: r.top, w: (r.right - r.left) as u32, h: (r.bottom - r.top) as u32, flags, title })
}

impl WinDesktop {
    pub fn new() -> WinDesktop {
        // AttachThreadInput needs a message queue on this thread.
        // SAFETY: zeroed MSG is valid; nothing is removed.
        unsafe {
            let mut m: MSG = std::mem::zeroed();
            PeekMessageW(&mut m, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
        }
        // Not ours yet; shell(false) at start shows it if a crashed agent hid it.
        WinDesktop { listed: Vec::new(), hidden: true, ours: false, fg_seen: 0, explain: false }
    }

    fn list(&mut self) -> Vec<GuestWindow> {
        // Top-level windows front to back. Not EnumWindows: since Windows
        // 8 it leaves out Store apps' and the shell's own (the Start menu).
        let mut all: Vec<HWND> = Vec::new();
        // SAFETY: plain calls; the walk is capped (the order may change under it).
        let mut h = unsafe { GetTopWindow(std::ptr::null_mut()) };
        while !h.is_null() && all.len() < 4096 {
            all.push(h);
            // SAFETY: as above.
            h = unsafe { GetWindow(h, GW_HWNDNEXT) };
        }
        // SAFETY: plain calls.
        let (fg, me, scr) = unsafe { (GetForegroundWindow(), GetCurrentProcessId(), screen()) };
        // The Start menu and search (Windows 11) are in front without being
        // in the walk: they come first.
        // SAFETY: plain call.
        let root = unsafe { GetAncestor(fg, GA_ROOT) };
        let front = if root.is_null() { fg } else { root };
        if !front.is_null() && !all.contains(&front) {
            all.insert(0, front);
        }
        let mut out = Vec::new();
        for h in all {
            match entry(h, fg, me, &scr) {
                Ok(w) => out.push(w),
                Err(why) if h == fg && self.fg_seen != h as isize => {
                    // Once per window: why the one in front isn't shown.
                    log::info!("coherence: window in front {:#x} ({}, {:?}) left out: {why}", h as isize, class(h), title(h));
                    self.fg_seen = h as isize;
                }
                // The first list after Coherence goes on: what's left out
                // and why (only windows with a title, the rest is noise).
                Err(why) if self.explain && why != "hidden" && !title(h).is_empty() => {
                    log::info!("coherence: {:#x} ({}, {:?}) left out: {why}", h as isize, class(h), title(h));
                }
                Err(_) => {}
            }
            if out.len() == vroom_proto::WINDOWS_MAX {
                break;
            }
        }
        self.listed = out.iter().map(|w| hwnd(w.id) as isize).collect();
        self.explain = false;
        out
    }

    fn act_on(&mut self, h: HWND, op: WindowOp) {
        // SAFETY: plain calls on a window we listed (checked by the caller
        // and IsWindow); none of them wait for the window's app.
        unsafe {
            if IsWindow(h) == 0 {
                return;
            }
            match op {
                WindowOp::Activate => {
                    if IsIconic(h) != 0 {
                        ShowWindowAsync(h, SW_RESTORE);
                    }
                    // Windows only lets the app in front hand the keyboard
                    // over: join its input queue for the moment.
                    let me = GetCurrentThreadId();
                    let fg = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
                    let joined = fg != 0 && fg != me && AttachThreadInput(me, fg, 1) != 0;
                    BringWindowToTop(h);
                    SetForegroundWindow(h);
                    if joined {
                        AttachThreadInput(me, fg, 0);
                    }
                }
                WindowOp::Minimize => {
                    ShowWindowAsync(h, SW_MINIMIZE);
                }
                WindowOp::Close => {
                    PostMessageW(h, WM_SYSCOMMAND, SC_CLOSE as usize, 0);
                }
                WindowOp::Move { x, y, w, h: ht } => {
                    // The host speaks of what can be seen; Windows of the
                    // window with its invisible borders.
                    let mut wr = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                    let Some(vis) = bounds(h) else { return };
                    if GetWindowRect(h, &mut wr) == 0 {
                        return;
                    }
                    let (l, t) = (vis.left - wr.left, vis.top - wr.top);
                    let (r, b) = (wr.right - vis.right, wr.bottom - vis.bottom);
                    if IsZoomed(h) != 0 {
                        ShowWindowAsync(h, SW_RESTORE);
                    }
                    SetWindowPos(h, std::ptr::null_mut(), x - l, y - t, w as i32 + l + r, ht as i32 + t + b, SWP_NOZORDER | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
                }
            }
        }
    }
}

impl crate::coherence::Desktop for WinDesktop {
    fn windows(&mut self) -> Vec<GuestWindow> {
        real_pixels(|| self.list())
    }

    fn act(&mut self, id: u32, op: WindowOp) {
        let h = hwnd(id);
        if !self.listed.contains(&(h as isize)) {
            return;
        }
        log::debug!("coherence: {op:?} on {id:#x}");
        real_pixels(|| self.act_on(h, op));
    }

    fn shell(&mut self, on: bool) {
        let class = wide("Shell_TrayWnd");
        real_pixels(|| {
            // SAFETY: plain calls on the taskbar window, if there is one.
            unsafe {
                let tray = FindWindowW(class.as_ptr(), std::ptr::null());
                if on {
                    if !tray.is_null() && IsWindowVisible(tray) != 0 {
                        ShowWindowAsync(tray, SW_HIDE);
                    }
                    self.hidden = true;
                    if !std::mem::replace(&mut self.ours, true) {
                        // Just turned on (logs say what's left out).
                        self.explain = true;
                    }
                    // Windows puts the taskbar's share back on screen changes.
                    let s = screen();
                    if !same(&work_area(), &s) {
                        set_work_area(s);
                    }
                } else if std::mem::replace(&mut self.hidden, false) {
                    if tray.is_null() {
                        return;
                    }
                    let was_hidden = IsWindowVisible(tray) == 0;
                    if was_hidden {
                        ShowWindowAsync(tray, SW_SHOWNA);
                    }
                    // At start, a visible taskbar means nothing to undo
                    // (with auto-hide on, the work area is the screen).
                    if !std::mem::replace(&mut self.ours, false) && !was_hidden {
                        return;
                    }
                    // The screen less the taskbar (at the bottom, or wherever
                    // the user put it).
                    let (s, mut t) = (screen(), RECT { left: 0, top: 0, right: 0, bottom: 0 });
                    if GetWindowRect(tray, &mut t) == 0 {
                        return;
                    }
                    let mut w = s;
                    if t.top > s.top && t.left <= s.left && t.right >= s.right {
                        w.bottom = t.top;
                    } else if t.bottom < s.bottom && t.left <= s.left && t.right >= s.right {
                        w.top = t.bottom;
                    } else if t.left > s.left {
                        w.right = t.left;
                    } else if t.right < s.right {
                        w.left = t.right;
                    }
                    if same(&work_area(), &s) {
                        set_work_area(w);
                    }
                }
            }
        });
    }
}
