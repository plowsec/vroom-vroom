//! The real device, through libusb (loaded at run time, like ALSA and
//! virglrenderer elsewhere: no build-time dependency, and only the helper
//! process ever loads it).
//!
//! Transfers are asynchronous: one thread runs libusb's event loop and
//! sends completions to the VMM as they come; cancelling is
//! `libusb_cancel_transfer`, whose completion still comes back.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::desc::{Config, DeviceDesc};
use crate::helper::{Device, Opener, Sink};
use crate::proto::{Done, Event, Speed, Status, Submit, XferType};
use crate::Selector;

type Ctx = c_void;
type Dev = c_void;
type Handle = c_void;

#[repr(C)]
struct Transfer {
    dev_handle: *mut Handle,
    flags: u8,
    endpoint: u8,
    kind: u8,
    timeout: c_uint,
    status: c_int,
    length: c_int,
    actual_length: c_int,
    callback: Option<unsafe extern "C" fn(*mut Transfer)>,
    user_data: *mut c_void,
    buffer: *mut u8,
    num_iso_packets: c_int,
}

/// `struct libusb_device_descriptor`: the wire layout, 2-byte aligned.
#[repr(C, align(2))]
struct RawDesc([u8; 18]);

const ADD_ZERO_PACKET: u8 = 1 << 3;
const ERROR_NO_DEVICE: c_int = -4;
const ERROR_NOT_FOUND: c_int = -5;
const ERROR_PIPE: c_int = -9;
// Transfer status.
const T_COMPLETED: c_int = 0;
const T_CANCELLED: c_int = 3;
const T_STALL: c_int = 4;
const T_NO_DEVICE: c_int = 5;
const T_OVERFLOW: c_int = 6;

macro_rules! api {
    ($($name:ident: fn($($a:ty),*) $(-> $r:ty)?;)*) => {
        struct Lib {
            $($name: unsafe extern "C" fn($($a),*) $(-> $r)?,)*
        }

        impl Lib {
            /// # Safety
            /// `h` is a live dlopen handle to libusb-1.0.
            unsafe fn load(h: *mut c_void) -> Result<Lib, String> {
                Ok(Lib {
                    $($name: {
                        let n = concat!("libusb_", stringify!($name), "\0");
                        // SAFETY: n is NUL-terminated; h is live.
                        let p = unsafe { libc::dlsym(h, n.as_ptr() as *const c_char) };
                        if p.is_null() {
                            return Err(format!("libusb has no {}", n.trim_end_matches('\0')));
                        }
                        // SAFETY: the symbol is libusb's function of this
                        // signature (libusb.h, stable ABI).
                        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($a),*) $(-> $r)?>(p) }
                    },)*
                })
            }
        }
    };
}

api! {
    init: fn(*mut *mut Ctx) -> c_int;
    get_device_list: fn(*mut Ctx, *mut *mut *mut Dev) -> isize;
    free_device_list: fn(*mut *mut Dev, c_int);
    get_device_descriptor: fn(*mut Dev, *mut RawDesc) -> c_int;
    get_bus_number: fn(*mut Dev) -> u8;
    get_port_numbers: fn(*mut Dev, *mut u8, c_int) -> c_int;
    get_device_speed: fn(*mut Dev) -> c_int;
    open: fn(*mut Dev, *mut *mut Handle) -> c_int;
    close: fn(*mut Handle);
    set_auto_detach_kernel_driver: fn(*mut Handle, c_int) -> c_int;
    get_configuration: fn(*mut Handle, *mut c_int) -> c_int;
    set_configuration: fn(*mut Handle, c_int) -> c_int;
    claim_interface: fn(*mut Handle, c_int) -> c_int;
    release_interface: fn(*mut Handle, c_int) -> c_int;
    set_interface_alt_setting: fn(*mut Handle, c_int, c_int) -> c_int;
    clear_halt: fn(*mut Handle, u8) -> c_int;
    reset_device: fn(*mut Handle) -> c_int;
    control_transfer: fn(*mut Handle, u8, u8, u16, u16, *mut u8, u16, c_uint) -> c_int;
    alloc_transfer: fn(c_int) -> *mut Transfer;
    free_transfer: fn(*mut Transfer);
    submit_transfer: fn(*mut Transfer) -> c_int;
    cancel_transfer: fn(*mut Transfer) -> c_int;
    handle_events_timeout_completed: fn(*mut Ctx, *mut libc::timeval, *mut c_int) -> c_int;
    error_name: fn(c_int) -> *const c_char;
}

/// The loaded library and its one context, with the event thread running.
struct Usb {
    lib: Lib,
    ctx: usize,
}

static USB: OnceLock<Result<Usb, String>> = OnceLock::new();

static ONLY: OnceLock<String> = OnceLock::new();

/// Load libusb from `path` and nowhere else (the helper vroom-netd runs
/// as root: its own root-owned copy, never one a user can replace).
/// Before the first use; later calls do nothing.
pub fn only_from(path: &str) {
    let _ = ONLY.set(path.to_string());
}

fn candidates() -> Vec<String> {
    if let Some(p) = ONLY.get() {
        return vec![p.clone()];
    }
    let mut v: Vec<String> = std::env::var("VROOM_LIBUSB").into_iter().collect();
    if cfg!(target_os = "macos") {
        v.extend(["/opt/homebrew/lib/libusb-1.0.0.dylib", "/usr/local/lib/libusb-1.0.0.dylib"].map(String::from));
    } else {
        v.extend(["libusb-1.0.so.0", "libusb-1.0.so"].map(String::from));
    }
    v
}

fn load() -> Result<Usb, String> {
    let mut h = std::ptr::null_mut();
    for c in candidates() {
        let Ok(cs) = CString::new(c) else { continue };
        // SAFETY: valid C string.
        h = unsafe { libc::dlopen(cs.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if !h.is_null() {
            break;
        }
    }
    if h.is_null() {
        let hint = if cfg!(target_os = "macos") { "brew install libusb" } else { "install libusb-1.0" };
        return Err(format!("USB passthrough needs libusb ({hint})"));
    }
    // SAFETY: h is a live handle to libusb (never closed).
    let lib = unsafe { Lib::load(h)? };
    let mut ctx = std::ptr::null_mut();
    // SAFETY: out pointer to our local.
    let rc = unsafe { (lib.init)(&mut ctx) };
    if rc != 0 {
        return Err(format!("libusb_init: {rc}"));
    }
    let ctx = ctx as usize;
    std::thread::Builder::new()
        .name("usb-events".into())
        .spawn(move || loop {
            let Some(Ok(u)) = USB.get() else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            let mut tv = libc::timeval { tv_sec: 0, tv_usec: 200_000 };
            // SAFETY: ctx lives for the process; tv is ours.
            unsafe { (u.lib.handle_events_timeout_completed)(ctx as *mut Ctx, &mut tv, std::ptr::null_mut()) };
        })
        .map_err(|e| e.to_string())?;
    Ok(Usb { lib, ctx })
}

fn usb() -> Result<&'static Usb, String> {
    USB.get_or_init(load).as_ref().map_err(|e| e.clone())
}

fn err_name(u: &Usb, rc: c_int) -> String {
    // SAFETY: libusb_error_name returns a static string for any code.
    let p = unsafe { (u.lib.error_name)(rc) };
    if p.is_null() {
        return rc.to_string();
    }
    // SAFETY: NUL-terminated static string.
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Transfers in flight, shared with the completion callback.
struct Shared {
    inflight: Mutex<HashMap<u32, usize>>,
    sink: Sink,
    gone: AtomicBool,
}

/// What a transfer carries to its callback.
struct XferCtx {
    id: u32,
    is_in: bool,
    control: bool,
    shared: Arc<Shared>,
    buf: Vec<u8>,
}

unsafe extern "C" fn on_done(t: *mut Transfer) {
    let Ok(u) = usb() else { return };
    // SAFETY: libusb hands back the transfer we submitted; user_data is
    // the XferCtx we leaked for it, reclaimed exactly once here.
    let (ctx, status, actual) = unsafe { (Box::from_raw((*t).user_data as *mut XferCtx), (*t).status, (*t).actual_length) };
    ctx.shared.inflight.lock().unwrap_or_else(|e| e.into_inner()).remove(&ctx.id);
    // SAFETY: the transfer is ours and finished; nothing refers to it now
    // (removed from the map above, under the lock cancel uses).
    unsafe { (u.lib.free_transfer)(t) };
    let status = match status {
        T_COMPLETED => Status::Ok,
        T_CANCELLED => Status::Cancelled,
        T_STALL => Status::Stall,
        T_OVERFLOW => Status::Babble,
        T_NO_DEVICE => {
            ctx.shared.gone.store(true, Ordering::Relaxed);
            Status::NoDevice
        }
        _ => Status::Error,
    };
    let off = if ctx.control { 8 } else { 0 };
    let room = ctx.buf.len().saturating_sub(off);
    let actual = (actual.max(0) as usize).min(room);
    let data = if ctx.is_in { ctx.buf[off..off + actual].to_vec() } else { Vec::new() };
    let _ = ctx.shared.sink.send(Event::Done(Done { id: ctx.id, status, actual: actual as u32, data }));
}

/// An opened device.
pub struct UsbDevice {
    u: &'static Usb,
    h: *mut Handle,
    shared: Arc<Shared>,
    claimed: Vec<u8>,
    num_configs: u8,
}

impl std::fmt::Debug for UsbDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsbDevice").field("claimed", &self.claimed).finish()
    }
}

// SAFETY: the handle is only used from the thread holding the helper's
// device lock (and libusb handles are thread-safe anyway).
unsafe impl Send for UsbDevice {}

fn map_rc(rc: c_int) -> Status {
    match rc {
        ERROR_PIPE => Status::Stall,
        ERROR_NO_DEVICE => Status::NoDevice,
        _ => Status::Error,
    }
}

impl UsbDevice {
    fn release_all(&mut self) {
        for i in std::mem::take(&mut self.claimed) {
            // SAFETY: live handle.
            unsafe { (self.u.lib.release_interface)(self.h, i as c_int) };
        }
    }
}

impl Device for UsbDevice {
    fn submit(&mut self, s: Submit) -> Result<(), Status> {
        let is_in = s.is_in();
        let control = s.kind == XferType::Control;
        let mut buf = Vec::with_capacity(8 + s.len as usize);
        if control {
            buf.extend_from_slice(&s.setup);
        }
        if is_in {
            buf.resize(buf.len() + s.len as usize, 0);
        } else {
            buf.extend_from_slice(&s.data);
        }
        let len = c_int::try_from(buf.len()).map_err(|_| Status::Error)?;
        // SAFETY: plain allocation.
        let t = unsafe { (self.u.lib.alloc_transfer)(0) };
        if t.is_null() {
            return Err(Status::Error);
        }
        let mut ctx = Box::new(XferCtx { id: s.id, is_in, control, shared: self.shared.clone(), buf });
        let bufp = ctx.buf.as_mut_ptr();
        let kind = match s.kind {
            XferType::Control => 0,
            XferType::Bulk => 2,
            XferType::Interrupt => 3,
        };
        // SAFETY: t is a fresh transfer we own; the buffer lives in ctx
        // (heap, not moved) until the callback frees it.
        unsafe {
            (*t).dev_handle = self.h;
            (*t).flags = if s.zero_packet { ADD_ZERO_PACKET } else { 0 };
            (*t).endpoint = s.ep;
            (*t).kind = kind;
            (*t).timeout = 0;
            (*t).length = len;
            (*t).callback = Some(on_done);
            (*t).buffer = bufp;
            (*t).num_iso_packets = 0;
        }
        let mut map = self.shared.inflight.lock().unwrap_or_else(|e| e.into_inner());
        let raw = Box::into_raw(ctx);
        // SAFETY: t is ours; raw is reclaimed by on_done or below.
        let rc = unsafe {
            (*t).user_data = raw.cast();
            (self.u.lib.submit_transfer)(t)
        };
        if rc != 0 {
            // SAFETY: never submitted: both are still ours alone.
            unsafe {
                drop(Box::from_raw(raw));
                (self.u.lib.free_transfer)(t);
            }
            if rc == ERROR_NO_DEVICE {
                self.shared.gone.store(true, Ordering::Relaxed);
            }
            log::debug!("usb: submit {} ep {:#x}: {}", s.id, s.ep, err_name(self.u, rc));
            return Err(map_rc(rc));
        }
        map.insert(s.id, t as usize);
        Ok(())
    }

    fn cancel(&mut self, id: u32) {
        let map = self.shared.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&t) = map.get(&id) {
            // SAFETY: still in the map, so its callback hasn't run (the
            // callback removes it under this lock before freeing).
            unsafe { (self.u.lib.cancel_transfer)(t as *mut Transfer) };
        }
    }

    fn in_flight(&self) -> usize {
        self.shared.inflight.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn busy(&self, id: u32) -> bool {
        self.shared.inflight.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&id)
    }

    fn reset(&mut self) -> Result<(), Status> {
        self.release_all();
        // SAFETY: live handle.
        let rc = unsafe { (self.u.lib.reset_device)(self.h) };
        if rc == ERROR_NOT_FOUND || rc == ERROR_NO_DEVICE {
            // It came back as a new device: the watcher reattaches it.
            self.shared.gone.store(true, Ordering::Relaxed);
        }
        if rc != 0 {
            log::info!("usb: reset: {}", err_name(self.u, rc));
            return Err(map_rc(rc));
        }
        Ok(())
    }

    fn config_descriptor(&mut self, value: u8) -> Option<Vec<u8>> {
        for idx in 0..self.num_configs.max(1) {
            let mut b = vec![0u8; 4096];
            // SAFETY: b is writable for the length we pass.
            let n = unsafe { (self.u.lib.control_transfer)(self.h, 0x80, 6, 0x0200 | idx as u16, 0, b.as_mut_ptr(), b.len() as u16, 2000) };
            if n < 9 {
                continue;
            }
            b.truncate(n as usize);
            if b[5] == value {
                return Some(b);
            }
        }
        None
    }

    fn set_configuration(&mut self, value: u8, cfg: Option<&Config>) -> Result<(), Status> {
        self.release_all();
        let want = if value == 0 { -1 } else { value as c_int };
        // SAFETY: live handle.
        let rc = unsafe { (self.u.lib.set_configuration)(self.h, want) };
        if rc != 0 {
            // macOS lets us use a device someone else opened, but not
            // change its configuration: fine if it already is that one.
            let mut cur = -1;
            // SAFETY: out pointer to our local.
            let ok = unsafe { (self.u.lib.get_configuration)(self.h, &mut cur) } == 0 && cur == value as c_int;
            if !ok {
                log::warn!("usb: set configuration {value}: {}", err_name(self.u, rc));
                return Err(map_rc(rc));
            }
        }
        for a in cfg.map(|c| c.interfaces()).unwrap_or_default() {
            // SAFETY: live handle.
            let rc = unsafe { (self.u.lib.claim_interface)(self.h, a as c_int) };
            if rc == 0 {
                self.claimed.push(a);
            } else {
                let class = cfg.and_then(|c| c.alt(a, 0)).map(|x| x.class).unwrap_or(0);
                log::warn!(
                    "usb: interface {a} (class {class:#04x}) is in use by the host ({}): the guest can't use it",
                    err_name(self.u, rc)
                );
            }
        }
        Ok(())
    }

    fn set_interface(&mut self, iface: u8, alt: u8) -> Result<(), Status> {
        // SAFETY: live handle.
        match unsafe { (self.u.lib.set_interface_alt_setting)(self.h, iface as c_int, alt as c_int) } {
            0 => Ok(()),
            rc => Err(map_rc(rc)),
        }
    }

    fn clear_halt(&mut self, ep: u8) -> Result<(), Status> {
        // SAFETY: live handle.
        match unsafe { (self.u.lib.clear_halt)(self.h, ep) } {
            0 => Ok(()),
            rc => Err(map_rc(rc)),
        }
    }

    fn gone(&self) -> bool {
        self.shared.gone.load(Ordering::Relaxed)
    }
}

impl Drop for UsbDevice {
    fn drop(&mut self) {
        let ids: Vec<u32> = self.shared.inflight.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
        for id in ids {
            self.cancel(id);
        }
        let t0 = Instant::now();
        while self.in_flight() > 0 {
            if t0.elapsed() > Duration::from_secs(3) {
                // Closing with transfers pending would free memory libusb
                // still uses: leak the handle instead.
                log::warn!("usb: transfers didn't finish, leaving the device open");
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.release_all();
        // SAFETY: live handle, nothing in flight.
        unsafe { (self.u.lib.close)(self.h) };
    }
}

#[derive(Debug)]
/// Finds the device in libusb's list (by vendor and product, and by its
/// place on the bus when a serial number picks one of several).
pub struct LibusbOpener {
    sel: Selector,
    at: Option<(u8, Vec<u8>)>,
}

impl LibusbOpener {
    /// Loads libusb: fails if it isn't installed.
    pub fn new(sel: Selector) -> Result<LibusbOpener, String> {
        usb()?;
        Ok(LibusbOpener { sel, at: None })
    }
}

struct DevList {
    u: &'static Usb,
    list: *mut *mut Dev,
    n: usize,
}

impl DevList {
    fn get(u: &'static Usb) -> Option<DevList> {
        let mut list = std::ptr::null_mut();
        // SAFETY: out pointer; the context is live.
        let n = unsafe { (u.lib.get_device_list)(u.ctx as *mut Ctx, &mut list) };
        (n >= 0 && !list.is_null()).then_some(DevList { u, list, n: n as usize })
    }

    fn devices(&self) -> impl Iterator<Item = *mut Dev> + '_ {
        // SAFETY: the list holds n device pointers while we own it.
        (0..self.n).map(|i| unsafe { *self.list.add(i) })
    }

    fn info(&self, d: *mut Dev) -> Option<(DeviceDesc, u8, Vec<u8>)> {
        let mut raw = RawDesc([0; 18]);
        // SAFETY: d is in our list; raw has room for the descriptor
        // struct (18 bytes, same layout as on the wire).
        if unsafe { (self.u.lib.get_device_descriptor)(d, &mut raw) } != 0 {
            return None;
        }
        let mut ports = [0u8; 8];
        // SAFETY: d is live; ports has room for 8 (USB allows 7).
        let n = unsafe { (self.u.lib.get_port_numbers)(d, ports.as_mut_ptr(), ports.len() as c_int) };
        // SAFETY: d is live.
        let bus = unsafe { (self.u.lib.get_bus_number)(d) };
        // The struct fields are host-endian; descriptors on the wire are
        // little-endian: same thing on every host we run on.
        let desc = DeviceDesc::parse(&raw.0)?;
        Some((desc, bus, ports[..n.max(0) as usize].to_vec()))
    }
}

impl Drop for DevList {
    fn drop(&mut self) {
        // SAFETY: we own the list; unref the devices in it.
        unsafe { (self.u.lib.free_device_list)(self.list, 1) };
    }
}

impl Opener for LibusbOpener {
    fn open(&mut self, sink: &Sink) -> Option<(Box<dyn Device>, Speed, u16, u16)> {
        let u = usb().ok()?;
        // A serial number picks the device by where the host listing
        // (registry / sysfs, no device opened) says it is.
        let wanted: Option<Vec<(u8, Vec<u8>)>> = self.sel.serial.as_ref().map(|s| {
            crate::list::list()
                .into_iter()
                .filter(|d| d.vendor == self.sel.vendor && d.product == self.sel.product && d.serial.as_ref() == Some(s))
                .map(|d| (d.bus, d.ports))
                .collect()
        });
        let list = DevList::get(u)?;
        for d in list.devices() {
            let Some((desc, bus, ports)) = list.info(d) else { continue };
            if desc.vendor != self.sel.vendor || desc.product != self.sel.product || desc.class == 9 {
                continue;
            }
            if wanted.as_ref().is_some_and(|w| !w.contains(&(bus, ports.clone()))) {
                continue;
            }
            let mut h = std::ptr::null_mut();
            // SAFETY: d is in our list (ref held); out pointer to our local.
            let rc = unsafe { (u.lib.open)(d, &mut h) };
            if rc != 0 {
                log::warn!("usb: open {:04x}:{:04x}: {}", desc.vendor, desc.product, err_name(u, rc));
                continue;
            }
            // Linux: take interfaces from kernel drivers when claiming
            // (allowed with access to the device node). macOS: works for
            // root or with Apple's VM entitlement only; else claiming fails.
            // SAFETY: live handle.
            unsafe { (u.lib.set_auto_detach_kernel_driver)(h, 1) };
            // SAFETY: d is live.
            let speed = match unsafe { (u.lib.get_device_speed)(d) } {
                1 => Speed::Low,
                3 => Speed::High,
                4 => Speed::Super,
                5 => Speed::SuperPlus,
                _ => Speed::Full,
            };
            self.at = Some((bus, ports));
            let shared = Arc::new(Shared { inflight: Mutex::new(HashMap::new()), sink: sink.clone(), gone: AtomicBool::new(false) });
            let dev = UsbDevice { u, h, shared, claimed: Vec::new(), num_configs: desc.configs };
            return Some((Box::new(dev), speed, desc.vendor, desc.product));
        }
        None
    }

    fn present(&mut self) -> bool {
        let (Ok(u), Some(at)) = (usb(), self.at.as_ref()) else { return false };
        let Some(list) = DevList::get(u) else { return true };
        let found = list.devices().any(|d| {
            list.info(d).is_some_and(|(desc, bus, ports)| desc.vendor == self.sel.vendor && desc.product == self.sel.product && (bus, &ports) == (at.0, &at.1))
        });
        found
    }
}
