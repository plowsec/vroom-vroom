//! Coherence on Windows: the windows on screen, and what the host may do
//! to them.
//!
//! Coordinates are the screen's real pixels: the calls run with
//! per-monitor DPI awareness (this process is otherwise DPI-unaware, and
//! Windows would scale what we read at 125% and up). A window's place is
//! what can be seen of it (DWM's frame bounds), without the invisible
//! resize borders Windows 10 and 11 put around it.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::time::Duration;

use vroom_proto::{win_flags, GuestWindow, WindowOp};
use windows_sys::Win32::Foundation::{CloseHandle, HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows_sys::Win32::Storage::Packaging::Appx::GetPackageFullName;
use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VK_LWIN, VK_RETURN};
use windows_sys::Win32::UI::Shell::ExtractIconExW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, DispatchMessageW, DrawIconEx, FindWindowExW, GetClassLongPtrW, GetMessageW, PostThreadMessageW, SendMessageTimeoutW, TranslateMessage,
    CHILDID_SELF, DI_NORMAL, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MINIMIZEEND, GCLP_HICON, GCLP_HICONSM, HICON, ICON_BIG, ICON_SMALL, ICON_SMALL2, OBJID_WINDOW, SMTO_ABORTIFHUNG, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_GETICON, WM_QUIT,
};
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
    /// Icons made, by window: the icon handle they were made from.
    icons: HashMap<isize, (isize, Option<crate::coherence::Icon>)>,
    /// Icons of programs (Store apps' windows have none of their own).
    exe_icons: HashMap<String, Option<crate::coherence::Icon>>,
    /// The thread with the window event hooks, and its id.
    hook: Option<(u32, std::thread::JoinHandle<()>)>,
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
        WinDesktop { listed: Vec::new(), icons: HashMap::new(), exe_icons: HashMap::new(), hook: None, hidden: true, ours: false, fg_seen: 0, explain: false }
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
                WindowOp::Activate => activate(h),
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

/// Bring a window to the front with the keyboard (restored if minimized).
/// The calling thread needs a message queue.
fn activate(h: HWND) {
    // SAFETY: plain calls; a window that went makes them fail.
    unsafe {
        if IsIconic(h) != 0 {
            ShowWindowAsync(h, SW_RESTORE);
        }
        // Windows only lets the app in front hand the keyboard over: join
        // its input queue for the moment.
        let me = GetCurrentThreadId();
        let fg = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
        let joined = fg != 0 && fg != me && AttachThreadInput(me, fg, 1) != 0;
        BringWindowToTop(h);
        SetForegroundWindow(h);
        if joined {
            AttachThreadInput(me, fg, 0);
        }
    }
}

/// The app windows on screen now.
fn app_windows() -> Vec<isize> {
    let mut out = Vec::new();
    // SAFETY: plain calls; the walk is capped.
    unsafe {
        let (fg, me, scr) = (GetForegroundWindow(), GetCurrentProcessId(), screen());
        let mut h = GetTopWindow(std::ptr::null_mut());
        let mut n = 0;
        while !h.is_null() && n < 4096 {
            if entry(h, fg, me, &scr).is_ok_and(|w| w.flags & (win_flags::APP | win_flags::MINIMIZED) == win_flags::APP) {
                out.push(h as isize);
            }
            h = GetWindow(h, GW_HWNDNEXT);
            n += 1;
        }
    }
    out
}

/// After starting an app: its first window, when it comes (within a few
/// seconds), goes to the front. Windows lets an app started by a program
/// in the background open behind the others.
fn front_new(before: &[isize]) {
    // AttachThreadInput needs a message queue on this thread.
    // SAFETY: zeroed MSG is valid; nothing is removed.
    unsafe {
        let mut m: MSG = std::mem::zeroed();
        PeekMessageW(&mut m, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }
    let mut new = None;
    let mut tries = 0;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(250));
        let Some(h) = new.or_else(|| app_windows().into_iter().find(|h| !before.contains(h))) else { continue };
        new = Some(h);
        // SAFETY: a plain call.
        if unsafe { GetForegroundWindow() } as isize == h {
            return;
        }
        // Again a little later if the app (or another starting) took the
        // front back while it came up.
        if tries == 3 {
            return;
        }
        tries += 1;
        log::info!("apps: {h:#x} to the front");
        activate(h as HWND);
        std::thread::sleep(Duration::from_millis(750));
    }
}

impl crate::coherence::Desktop for WinDesktop {
    fn windows(&mut self) -> Vec<GuestWindow> {
        real_pixels(|| self.list())
    }

    fn icon(&mut self, id: u32) -> Option<crate::coherence::Icon> {
        let h = hwnd(id);
        if !self.listed.contains(&(h as isize)) {
            return None;
        }
        let listed = &self.listed;
        self.icons.retain(|w, _| listed.contains(w));
        let size = crate::coherence::ICON_SIZE as i32;
        if let Some(hi) = window_icon(h) {
            if let Some((was, icon)) = self.icons.get(&(h as isize)) {
                if *was == hi as isize {
                    return icon.clone();
                }
            }
            let icon = render(hi, size);
            self.icons.insert(h as isize, (hi as isize, icon.clone()));
            return icon;
        }
        // No icon of its own: its program's (a Store app's window shows
        // the app's, not the frame host's).
        let app = app_window(h);
        let path = exe_of(app)?;
        if self.exe_icons.len() > 64 {
            self.exe_icons.clear();
        }
        if let Some(i) = self.exe_icons.get(&path) {
            return i.clone();
        }
        // A Store app: the logo in its package (its program has none).
        if let Some(i) = package_of(app).and_then(|dir| {
            // Its program, relative to the package (which app of it).
            let (p, d) = (path.to_lowercase(), dir.to_string_lossy().to_lowercase());
            let exe = p.strip_prefix(&d).map(|e| e.trim_start_matches('\\').to_string());
            crate::apps_win::package_logo(&dir, None, exe.as_deref())
        }) {
            self.exe_icons.insert(path, Some(i.clone()));
            return Some(i);
        }
        let w = wide(&path);
        let mut big: HICON = std::ptr::null_mut();
        // SAFETY: a NUL-terminated path; one icon asked, ours to destroy.
        let icon = unsafe {
            if ExtractIconExW(w.as_ptr(), 0, &mut big, std::ptr::null_mut(), 1) > 0 && !big.is_null() {
                let i = render(big, size);
                DestroyIcon(big);
                i
            } else {
                None
            }
        };
        self.exe_icons.insert(path, icon.clone());
        icon
    }

    fn watch(&mut self, poke: Option<crate::coherence::Poke>) -> bool {
        if let Some((tid, t)) = self.hook.take() {
            // SAFETY: a plain call; the thread ends its message loop.
            unsafe { PostThreadMessageW(tid, WM_QUIT, 0, 0) };
            let _ = t.join();
        }
        let Some(poke) = poke else { return true };
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::Builder::new().name("coherence-events".into()).spawn(move || events(poke, tx));
        let Ok(t) = t else { return false };
        match rx.recv() {
            Ok((tid, true)) => {
                self.hook = Some((tid, t));
                true
            }
            _ => {
                log::warn!("coherence: no window events, looking every {:?}", crate::coherence::EVERY);
                let _ = t.join();
                false
            }
        }
    }

    fn start(&mut self, name: &str) {
        // The Start menu's shortcut or app of that name. On a thread: the
        // menu is read again (a new app is found), and Start may take a
        // moment to open and to search.
        let name = name.to_string();
        let _ = std::thread::Builder::new().name("start-app".into()).spawn(move || {
            let before = app_windows();
            if crate::apps::start(&name) {
                front_new(&before);
                return;
            }
            // Not a command line: Start's search runs those too.
            if !crate::apps::plain(&name) {
                log::info!("apps: not searching Start for {name:?}");
                return;
            }
            // None by that name: as a user would, the Start menu, the name
            // in its search, Enter. Typed as characters, whatever the
            // keyboard layout. Start's search may pick something else
            // (a web search).
            log::info!("apps: searching Start for {name:?}");
            let name: Vec<u16> = name.encode_utf16().collect();
            send_keys(&[key(VK_LWIN, false), key(VK_LWIN, true)]);
            std::thread::sleep(Duration::from_millis(700));
            let typed: Vec<INPUT> = name.iter().flat_map(|&u| [unicode(u, false), unicode(u, true)]).collect();
            send_keys(&typed);
            std::thread::sleep(Duration::from_millis(900));
            send_keys(&[key(VK_RETURN, false), key(VK_RETURN, true)]);
        });
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

thread_local! {
    /// The event thread's poke (hook callbacks have no context pointer).
    static POKE: RefCell<Option<crate::coherence::Poke>> = const { RefCell::new(None) };
}

/// The event thread: window event hooks (out of context: Windows calls
/// us on this thread, from its message loop) until WM_QUIT.
fn events(poke: crate::coherence::Poke, ready: std::sync::mpsc::Sender<(u32, bool)>) {
    POKE.with(|p| *p.borrow_mut() = Some(poke));
    // SAFETY: plain calls; the hooks are removed before the thread ends.
    unsafe {
        let mut m: MSG = std::mem::zeroed();
        PeekMessageW(&mut m, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        // System events (foreground, menus, move and size, minimize),
        // then object events (create .. uncloaked).
        let a = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, std::ptr::null_mut(), Some(on_event), 0, 0, flags);
        let b = SetWinEventHook(EVENT_OBJECT_CREATE, EVENT_OBJECT_UNCLOAKED, std::ptr::null_mut(), Some(on_event), 0, 0, flags);
        let ok = !a.is_null() && !b.is_null();
        let _ = ready.send((GetCurrentThreadId(), ok));
        if ok {
            while GetMessageW(&mut m, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&m);
                DispatchMessageW(&m);
            }
        }
        for h in [a, b] {
            if !h.is_null() {
                UnhookWinEvent(h);
            }
        }
    }
}

unsafe extern "system" fn on_event(_hook: HWINEVENTHOOK, event: u32, h: HWND, obj: i32, child: i32, _thread: u32, _time: u32) {
    let wanted = if event < EVENT_OBJECT_CREATE {
        true
    } else {
        // Top-level windows only (a destroyed or hidden one may have no
        // root any more). Not the caret, the pointer, controls inside
        // windows: they come by the thousand.
        // SAFETY: a plain call; a stale handle makes it return null.
        let root = unsafe { GetAncestor(h, GA_ROOT) };
        let gone = matches!(event, EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE) && root.is_null();
        obj == OBJID_WINDOW && child == CHILDID_SELF as i32 && !h.is_null() && (root == h || gone)
    };
    if wanted {
        POKE.with(|p| {
            if let Some(p) = p.borrow().as_ref() {
                p();
            }
        });
    }
}

/// A window's icon handle (not ours to destroy), if it has one.
fn window_icon(h: HWND) -> Option<HICON> {
    let mut r: usize = 0;
    for which in [ICON_BIG, ICON_SMALL2, ICON_SMALL] {
        // SAFETY: a plain call; a hung app makes it give up after 50 ms.
        let ok = unsafe { SendMessageTimeoutW(h, WM_GETICON, which as usize, 0, SMTO_ABORTIFHUNG, 50, &mut r) };
        if ok != 0 && r != 0 {
            return Some(r as HICON);
        }
    }
    for which in [GCLP_HICON, GCLP_HICONSM] {
        // SAFETY: a plain call.
        let c = unsafe { GetClassLongPtrW(h, which) };
        if c != 0 {
            return Some(c as HICON);
        }
    }
    None
}

/// The window whose program a window shows: a Store app's frame holds
/// the app's own window.
fn app_window(h: HWND) -> HWND {
    if class(h) != "ApplicationFrameWindow" {
        return h;
    }
    let core = wide("Windows.UI.Core.CoreWindow");
    // SAFETY: a NUL-terminated class name.
    let c = unsafe { FindWindowExW(h, std::ptr::null_mut(), core.as_ptr(), std::ptr::null()) };
    if c.is_null() {
        h
    } else {
        c
    }
}

/// The install folder of the package a window's program came in (Store
/// apps), if it did.
fn package_of(h: HWND) -> Option<std::path::PathBuf> {
    let mut pid = 0u32;
    // SAFETY: out pointers and buffers with their lengths; the process
    // handle is closed below.
    unsafe {
        GetWindowThreadProcessId(h, &mut pid);
        if pid == 0 {
            return None;
        }
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if p.is_null() {
            return None;
        }
        let mut b = [0u16; 256];
        let mut n = b.len() as u32;
        let r = GetPackageFullName(p, &mut n, b.as_mut_ptr());
        CloseHandle(p);
        // Not packaged: APPMODEL_ERROR_NO_PACKAGE.
        if r != 0 || n == 0 || n as usize > b.len() {
            return None;
        }
        crate::apps_win::package_path(&b[..n as usize])
    }
}

/// The program a window belongs to (full path).
fn exe_of(h: HWND) -> Option<String> {
    let mut pid = 0u32;
    // SAFETY: out pointer; the process handle is closed below.
    unsafe {
        GetWindowThreadProcessId(h, &mut pid);
        if pid == 0 {
            return None;
        }
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if p.is_null() {
            return None;
        }
        let mut b = [0u16; 1024];
        let mut n = b.len() as u32;
        let ok = QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, b.as_mut_ptr(), &mut n);
        CloseHandle(p);
        (ok != 0).then(|| String::from_utf16_lossy(&b[..n as usize]))
    }
}

/// An icon as `size` x `size` RGBA. Drawn twice, on black and on white:
/// how much the two differ is how transparent a pixel is, whatever kind of
/// icon it is (with an alpha channel or a mask).
pub(crate) fn render(icon: HICON, size: i32) -> Option<crate::coherence::Icon> {
    let n = (size * size) as usize;
    let draw = |fill: u8| -> Option<Vec<u8>> {
        // SAFETY: a DIB section of `n` 32-bit pixels, top-down, selected
        // into a memory DC; everything is freed before returning.
        unsafe {
            let dc = CreateCompatibleDC(std::ptr::null_mut());
            if dc.is_null() {
                return None;
            }
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            let mut out = None;
            if !bmp.is_null() && !bits.is_null() {
                let old = SelectObject(dc, bmp);
                let px = std::slice::from_raw_parts_mut(bits as *mut u8, n * 4);
                px.fill(fill);
                if DrawIconEx(dc, 0, 0, icon, size, size, 0, std::ptr::null_mut(), DI_NORMAL) != 0 {
                    GdiFlush();
                    out = Some(px.to_vec());
                }
                SelectObject(dc, old);
                DeleteObject(bmp);
            }
            DeleteDC(dc);
            out
        }
    };
    let (black, white) = (draw(0)?, draw(255)?);
    let mut rgba = vec![0u8; n * 4];
    for i in 0..n {
        let (b, w) = (&black[i * 4..i * 4 + 3], &white[i * 4..i * 4 + 3]);
        let diff = (0..3).map(|c| w[c].saturating_sub(b[c])).max().unwrap_or(255);
        let a = 255 - diff as u32;
        if a == 0 {
            continue;
        }
        // On black the colour came out times alpha: undo that (BGR in, RGB out).
        let un = |v: u8| ((v as u32 * 255 + a / 2) / a).min(255) as u8;
        rgba[i * 4..i * 4 + 4].copy_from_slice(&[un(b[2]), un(b[1]), un(b[0]), a as u8]);
    }
    rgba.chunks(4).any(|p| p[3] != 0).then_some(crate::coherence::Icon { w: size as u32, h: size as u32, rgba })
}

fn key(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { 0 }, time: 0, dwExtraInfo: 0 } },
    }
}

fn unicode(u: u16, up: bool) -> INPUT {
    let f = KEYEVENTF_UNICODE | if up { KEYEVENTF_KEYUP } else { 0 };
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: 0, wScan: u, dwFlags: f, time: 0, dwExtraInfo: 0 } } }
}

fn send_keys(k: &[INPUT]) {
    // SAFETY: a slice of INPUTs and their size.
    unsafe { SendInput(k.len() as u32, k.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
}
