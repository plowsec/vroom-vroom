//! Windows: a drag of the staged files, started under the pointer.
//!
//! A tiny, almost transparent window goes under the pointer; the host
//! then presses the guest's mouse button on it. On that press we start a
//! shell drag (`SHDoDragDrop`, with the shell's own data object for the
//! files and its default drop source: Explorer shows the files' icons,
//! "Copy to Documents" and so on). The host moves the pointer with the
//! user's and lets go of the button once the files are written, and the
//! app under the pointer gets an ordinary drop. Escape (sent by the host
//! when the drag leaves the window) gives it up.
//!
//! Two things Windows needs before that press lands on our window: it
//! must be in front (Windows keeps new windows of a background process
//! behind the active app, topmost or not), and it must have been composed
//! once (until then clicks go to whatever is under it).

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmFlush;
use windows_sys::Win32::Graphics::Gdi::{GetStockObject, UpdateWindow, BLACK_BRUSH};
use windows_sys::Win32::System::Com::{
    DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::{
    OleInitialize, OleUninitialize, DROPEFFECT_COPY, DROPEFFECT_MOVE,
};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::{
    ILCreateFromPathW, ILFindLastID, ILFree, SHCreateDataObject, SHDoDragDrop,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, KillTimer, PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, SetLayeredWindowAttributes, SetTimer,
    SetWindowPos, TranslateMessage, HWND_TOPMOST, LWA_ALPHA, MSG, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, WM_CLOSE, WM_DESTROY, WM_LBUTTONDOWN, WM_TIMER, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

/// {0000010e-0000-0000-C000-000000000046}
const IID_IDATAOBJECT: GUID = GUID {
    data1: 0x0000_010e,
    data2: 0,
    data3: 0,
    data4: [0xc0, 0, 0, 0, 0, 0, 0, 0x46],
};
/// Side of the window under the pointer, in pixels.
const SIDE: i32 = 64;
/// Timers: give up waiting for the press; follow the pointer.
const GIVE_UP: usize = 1;
const FOLLOW: usize = 2;
/// No press from the host by then: give up.
const WAIT_MS: u32 = 15_000;

pub struct WinDrag {
    hwnd: Arc<AtomicIsize>,
}

impl crate::drop::LiveDrag for WinDrag {
    fn cancel(&mut self) {
        // Before the press: close the window. During the drag the host's
        // Escape ends it.
        let h = self.hwnd.load(Ordering::Acquire);
        if h != 0 {
            // SAFETY: posting to a window of ours (or a gone one: fails).
            unsafe { PostMessageW(h as HWND, WM_CLOSE, 0, 0) };
        }
    }
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// The data object for the staged items (IDataObject*), or null.
unsafe fn data_object(dir: &Path, items: &[(String, bool)]) -> *mut c_void {
    // SAFETY: shell calls on pidls we make and free here.
    unsafe {
        let folder = ILCreateFromPathW(wide(dir).as_ptr());
        if folder.is_null() {
            return std::ptr::null_mut();
        }
        let abs: Vec<*mut ITEMIDLIST> = items
            .iter()
            .map(|(n, _)| ILCreateFromPathW(wide(&dir.join(n)).as_ptr()))
            .collect();
        let mut obj: *mut c_void = std::ptr::null_mut();
        if abs.iter().all(|p| !p.is_null()) {
            let children: Vec<*const ITEMIDLIST> = abs
                .iter()
                .map(|&p| ILFindLastID(p) as *const ITEMIDLIST)
                .collect();
            let hr = SHCreateDataObject(
                folder,
                children.len() as u32,
                children.as_ptr(),
                std::ptr::null_mut(),
                &IID_IDATAOBJECT,
                &mut obj,
            );
            if hr < 0 {
                log::warn!("drop: SHCreateDataObject: {hr:#x}");
                obj = std::ptr::null_mut();
            } else {
                prefer_copy(obj);
            }
        }
        for p in abs.into_iter().filter(|p| !p.is_null()) {
            ILFree(p);
        }
        ILFree(folder);
        obj
    }
}

/// Ask drop targets to copy ("Copy to Documents"), not move: Explorer
/// moves between folders of one drive by default, and the files come
/// from the Mac. (Move stays allowed: some targets only take a move, and
/// it only empties our staging folder.)
unsafe fn prefer_copy(obj: *mut c_void) {
    // SAFETY: IDataObject::SetData (vtable slot 7) on a live data object,
    // which takes the memory (fRelease).
    unsafe {
        let name: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
        let cf = RegisterClipboardFormatW(name.as_ptr());
        let mem = GlobalAlloc(GMEM_MOVEABLE, 4);
        if cf == 0 || mem.is_null() {
            return;
        }
        let p = GlobalLock(mem) as *mut u32;
        if p.is_null() {
            return;
        }
        *p = DROPEFFECT_COPY;
        GlobalUnlock(mem);
        let mut fmt = FORMATETC {
            cfFormat: cf as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL as u32,
        };
        let mut med = STGMEDIUM {
            tymed: TYMED_HGLOBAL as u32,
            u: STGMEDIUM_0 { hGlobal: mem },
            pUnkForRelease: std::ptr::null_mut(),
        };
        let vtbl = *(obj as *const *const usize);
        type SetData =
            unsafe extern "system" fn(*mut c_void, *mut FORMATETC, *mut STGMEDIUM, i32) -> i32;
        let f: SetData = std::mem::transmute(*vtbl.add(7));
        let hr = f(obj, &mut fmt, &mut med, 1);
        if hr < 0 {
            log::debug!("drop: SetData(Preferred DropEffect): {hr:#x}");
        }
    }
}

/// IUnknown::Release.
unsafe fn release(obj: *mut c_void) {
    // SAFETY: obj is a live COM object; slot 2 of IUnknown is Release.
    unsafe {
        let vtbl = *(obj as *const *const usize);
        let f: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(2));
        f(obj);
    }
}

thread_local! {
    static OBJ: std::cell::Cell<*mut c_void> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: our own window, on its thread; OBJ is that thread's data object.
    unsafe {
        match msg {
            WM_LBUTTONDOWN => {
                KillTimer(hwnd, GIVE_UP);
                KillTimer(hwnd, FOLLOW);
                let obj = OBJ.with(|o| o.get());
                if !obj.is_null() {
                    let mut effect = 0u32;
                    // Modal until the button goes up (drop) or Escape (cancel).
                    // Move too: Explorer moves between folders of one drive by
                    // default, and a move only empties our staging folder.
                    let hr = SHDoDragDrop(
                        hwnd,
                        obj,
                        std::ptr::null_mut(),
                        DROPEFFECT_COPY | DROPEFFECT_MOVE,
                        &mut effect,
                    );
                    log::info!("drop: drag ended ({hr:#x}, effect {effect})");
                }
                DestroyWindow(hwnd);
                0
            }
            WM_TIMER if wp == FOLLOW => {
                // Stay under the pointer: the host moves it as the user's
                // mouse moves, and presses wherever it is then.
                let mut pt = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pt);
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    pt.x - SIDE / 2,
                    pt.y - SIDE / 2,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE,
                );
                0
            }
            WM_TIMER | WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Show `w` over the app in front. Windows keeps new windows of a
/// process in the background behind the active app, topmost or not (it
/// guards the foreground); joining the active app's input queue for a
/// moment lets us put ours in front of it.
unsafe fn to_front(w: HWND) {
    // SAFETY: plain calls; the attachment is undone before returning.
    unsafe {
        let me = GetCurrentThreadId();
        let fg = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
        let joined = fg != 0 && fg != me && AttachThreadInput(me, fg, 1) != 0;
        SetWindowPos(
            w,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        SetForegroundWindow(w);
        if joined {
            AttachThreadInput(me, fg, 0);
        }
    }
}

/// Put the drag window under the pointer; None if Windows won't.
pub fn start(dir: &Path, items: &[(String, bool)]) -> Option<WinDrag> {
    let (dir, items) = (dir.to_path_buf(), items.to_vec());
    let hwnd = Arc::new(AtomicIsize::new(0));
    let h2 = hwnd.clone();
    let (tx, rx) = mpsc::channel::<bool>();
    std::thread::Builder::new()
        .name("drag".into())
        .spawn(move || drag_thread(&dir, &items, &h2, tx))
        .ok()?;
    rx.recv_timeout(Duration::from_secs(5))
        .ok()
        .filter(|&ok| ok)
        .map(|_| WinDrag { hwnd })
}

/// The drag window's thread: shows it, runs its messages until it goes.
fn drag_thread(dir: &Path, items: &[(String, bool)], h2: &AtomicIsize, tx: mpsc::Sender<bool>) {
    // SAFETY: plain Win32 calls on this thread's own window, data object
    // and message loop.
    unsafe {
        if OleInitialize(std::ptr::null()) < 0 {
            let _ = tx.send(false);
            return;
        }
        let obj = data_object(dir, items);
        if obj.is_null() {
            let _ = tx.send(false);
            OleUninitialize();
            return;
        }
        OBJ.with(|o| o.set(obj));
        let class: Vec<u16> = "VroomDropSource\0".encode_utf16().collect();
        let inst = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: inst,
            lpszClassName: class.as_ptr(),
            // Painted: Windows lets clicks through the parts of a layered
            // window that were never drawn.
            hbrBackground: GetStockObject(BLACK_BRUSH),
            ..std::mem::zeroed()
        };
        // Fails harmlessly when already registered.
        RegisterClassW(&wc);
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        let w = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            pt.x - SIDE / 2,
            pt.y - SIDE / 2,
            SIDE,
            SIDE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            inst,
            std::ptr::null(),
        );
        if w.is_null() {
            let _ = tx.send(false);
        } else {
            // Alpha 1, not 0: a fully transparent window lets clicks through.
            log::debug!("drop: drag window at {},{}", pt.x, pt.y);
            // Alpha 1, not 0: a fully transparent window lets clicks through.
            SetLayeredWindowAttributes(w, 0, 1, LWA_ALPHA);
            to_front(w);
            // Windows routes clicks to a new window only once it has been
            // composed: before that the host's press goes to the app under
            // it. Wait for two compositions, then say we are ready.
            UpdateWindow(w);
            DwmFlush();
            DwmFlush();
            SetTimer(w, GIVE_UP, WAIT_MS, None);
            SetTimer(w, FOLLOW, 10, None);
            h2.store(w as isize, Ordering::Release);
            let _ = tx.send(true);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            h2.store(0, Ordering::Release);
        }
        OBJ.with(|o| o.set(std::ptr::null_mut()));
        release(obj);
        OleUninitialize();
    }
}
