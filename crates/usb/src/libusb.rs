//! The real device, through libusb (loaded at run time, like ALSA and
//! virglrenderer elsewhere: no build-time dependency, and only the helper
//! process ever loads it).
//!
//! Transfers are asynchronous: one thread runs libusb's event loop and
//! sends completions to the VMM as they come; cancelling is
//! `libusb_cancel_transfer`, whose completion still comes back.
//!
//! Host drivers: macOS, as root (vroom-netd's helper): a device macOS's
//! drivers hold is taken as soon as it is opened: libusb "captures" it,
//! re-enumerating it with macOS's drivers kept off. Linux: an interface
//! a kernel driver holds is taken from it when the guest configures the
//! device. Either way the device stays ours until the helper ends,
//! through guest resets and reconfigurations, and only then goes back to
//! the host's drivers (macOS: one more re-enumeration). Taking and
//! giving back on every guest reset would let the host mount a disk in
//! between.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::desc::{Config, DeviceDesc};
use crate::helper::{Device, Opener, Sink};
use crate::proto::{Done, Event, IsoDone, IsoPacket, IsoSubmit, Speed, Status, Submit, XferType};
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
    /// `iso_packet_desc[]`: `num_iso_packets` of them follow.
    iso: [IsoDesc; 0],
}

/// `struct libusb_iso_packet_descriptor`.
#[repr(C)]
#[derive(Clone, Copy)]
struct IsoDesc {
    length: c_uint,
    actual_length: c_uint,
    status: c_int,
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
const TYPE_ISO: u8 = 1;

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
    kernel_driver_active: fn(*mut Handle, c_int) -> c_int;
    detach_kernel_driver: fn(*mut Handle, c_int) -> c_int;
    attach_kernel_driver: fn(*mut Handle, c_int) -> c_int;
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
    /// Isochronous: the packet lengths (the packets sit back to back in
    /// `buf`). Empty otherwise.
    iso: Vec<u32>,
}

fn status_of(s: c_int, shared: &Shared) -> Status {
    match s {
        T_COMPLETED => Status::Ok,
        T_CANCELLED => Status::Cancelled,
        T_STALL => Status::Stall,
        T_OVERFLOW => Status::Babble,
        T_NO_DEVICE => {
            shared.gone.store(true, Ordering::Relaxed);
            Status::NoDevice
        }
        _ => Status::Error,
    }
}

unsafe extern "C" fn on_done(t: *mut Transfer) {
    let Ok(u) = usb() else { return };
    // SAFETY: libusb hands back the transfer we submitted; user_data is
    // the XferCtx we leaked for it, reclaimed exactly once here.
    let (ctx, status, actual) = unsafe { (Box::from_raw((*t).user_data as *mut XferCtx), (*t).status, (*t).actual_length) };
    // Isochronous: each packet's result, from the descriptors libusb
    // filled in (as many as we allocated the transfer with).
    let iso: Vec<IsoDesc> = (0..ctx.iso.len())
        // SAFETY: the transfer was allocated with ctx.iso.len() packet
        // descriptors after the struct.
        .map(|i| unsafe { *std::ptr::addr_of!((*t).iso).cast::<IsoDesc>().add(i) })
        .collect();
    ctx.shared.inflight.lock().unwrap_or_else(|e| e.into_inner()).remove(&ctx.id);
    // SAFETY: the transfer is ours and finished; nothing refers to it now
    // (removed from the map above, under the lock cancel uses).
    unsafe { (u.lib.free_transfer)(t) };
    let status = status_of(status, &ctx.shared);
    if !ctx.iso.is_empty() {
        // Each packet's bytes sit at its offset in the buffer; the VMM
        // gets them back to back.
        let (mut off, mut data, mut packets) = (0usize, Vec::new(), Vec::with_capacity(iso.len()));
        for (d, &want) in iso.iter().zip(&ctx.iso) {
            let n = (d.actual_length as usize).min(want as usize);
            if ctx.is_in {
                data.extend_from_slice(&ctx.buf[off..off + n]);
            }
            packets.push(IsoPacket { status: status_of(d.status, &ctx.shared), actual: n as u32 });
            off += want as usize;
        }
        let _ = ctx.shared.sink.send(Event::IsoDone(IsoDone { id: ctx.id, status, packets, data }));
        return;
    }
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
    /// Interfaces taken from a host driver: given back when we're done.
    detached: Vec<u8>,
    /// macOS: an earlier root helper captured the device and didn't give
    /// it back. We take it over (and give it back at the end).
    captured: bool,
    /// Set when the device can't be used at all (no interface could be
    /// claimed): the opener leaves it alone from then on.
    refused: Arc<AtomicBool>,
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

    /// macOS, as root: take the whole device from macOS's drivers (one
    /// re-enumeration). A device an earlier helper captured is taken over
    /// the same way, so that our guest's resets don't hand it to macOS.
    fn capture(&mut self) {
        // SAFETY: live handle. The interface number doesn't matter on
        // macOS: the whole device is taken.
        match unsafe { (self.u.lib.detach_kernel_driver)(self.h, 0) } {
            0 => {
                log::info!("usb: device taken from macOS's drivers");
                self.detached.push(0);
            }
            rc => log::warn!("usb: can't take the device from macOS's drivers ({})", err_name(self.u, rc)),
        }
    }

    /// Take interface `a` from the host driver holding it, if any.
    fn take(&mut self, a: u8) {
        // SAFETY: live handle.
        if unsafe { (self.u.lib.kernel_driver_active)(self.h, a as c_int) } == 1 {
            self.detach(a);
        }
    }

    fn detach(&mut self, a: u8) {
        // SAFETY: live handle.
        match unsafe { (self.u.lib.detach_kernel_driver)(self.h, a as c_int) } {
            0 => {
                log::info!("usb: interface {a} taken from the host's driver");
                self.detached.push(a);
            }
            rc => log::info!("usb: interface {a}: can't take it from the host's driver ({})", err_name(self.u, rc)),
        }
    }

    /// Claim interface `a`. Right after a macOS capture the device is
    /// still coming back: a few tries.
    fn claim(&mut self, a: u8) -> c_int {
        let mut rc = 0;
        for i in 0..4 {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(250));
            }
            // SAFETY: live handle.
            rc = unsafe { (self.u.lib.claim_interface)(self.h, a as c_int) };
            if rc == 0 || self.detached.is_empty() || !cfg!(target_os = "macos") {
                break;
            }
        }
        rc
    }

    /// Hand what we took back to the host's drivers (end of use).
    fn give_back(&mut self) {
        let detached = std::mem::take(&mut self.detached);
        for &a in &detached {
            // SAFETY: live handle, interfaces released. macOS:
            // re-enumerates the device on the last one, without capture.
            let rc = unsafe { (self.u.lib.attach_kernel_driver)(self.h, a as c_int) };
            if rc != 0 {
                log::info!("usb: giving interface {a} back to the host: {}", err_name(self.u, rc));
            }
        }
        if !detached.is_empty() {
            log::info!("usb: device given back to the host's drivers");
        } else if cfg!(target_os = "macos") && self.captured {
            // Captured before us and never taken over: a plain reset
            // re-enumerates it without capture, macOS's drivers come back.
            // SAFETY: live handle.
            let rc = unsafe { (self.u.lib.reset_device)(self.h) };
            log::info!("usb: re-enumerated for macOS's drivers ({})", if rc == 0 { "ok".into() } else { err_name(self.u, rc) });
        }
        self.captured = false;
    }
}

impl UsbDevice {
    /// Hand one transfer to libusb: `ctx` carries its buffer and, for
    /// isochronous, its packet lengths.
    fn start(&mut self, ep: u8, kind: u8, flags: u8, mut ctx: Box<XferCtx>) -> Result<(), Status> {
        let len = c_int::try_from(ctx.buf.len()).map_err(|_| Status::Error)?;
        let n = ctx.iso.len();
        // SAFETY: plain allocation, with n packet descriptors.
        let t = unsafe { (self.u.lib.alloc_transfer)(n as c_int) };
        if t.is_null() {
            return Err(Status::Error);
        }
        let id = ctx.id;
        let bufp = ctx.buf.as_mut_ptr();
        // SAFETY: t is a fresh transfer we own, with room for n packet
        // descriptors; the buffer lives in ctx (heap, not moved) until
        // the callback frees it.
        unsafe {
            (*t).dev_handle = self.h;
            (*t).flags = flags;
            (*t).endpoint = ep;
            (*t).kind = kind;
            (*t).timeout = 0;
            (*t).length = len;
            (*t).callback = Some(on_done);
            (*t).buffer = bufp;
            (*t).num_iso_packets = n as c_int;
            let descs = std::ptr::addr_of_mut!((*t).iso).cast::<IsoDesc>();
            for (i, &l) in ctx.iso.iter().enumerate() {
                descs.add(i).write(IsoDesc { length: l, actual_length: 0, status: 0 });
            }
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
            log::debug!("usb: submit {id} ep {ep:#x}: {}", err_name(self.u, rc));
            return Err(map_rc(rc));
        }
        map.insert(id, t as usize);
        Ok(())
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
        let kind = match s.kind {
            XferType::Control => 0,
            XferType::Bulk => 2,
            XferType::Interrupt => 3,
            XferType::Iso => return Err(Status::Error),
        };
        let flags = if s.zero_packet { ADD_ZERO_PACKET } else { 0 };
        let ctx = Box::new(XferCtx { id: s.id, is_in, control, shared: self.shared.clone(), buf, iso: Vec::new() });
        self.start(s.ep, kind, flags, ctx)
    }

    fn submit_iso(&mut self, s: IsoSubmit) -> Result<(), Status> {
        let is_in = s.is_in();
        let buf = if is_in { vec![0u8; s.len()] } else { s.data };
        let ctx = Box::new(XferCtx { id: s.id, is_in, control: false, shared: self.shared.clone(), buf, iso: s.packets });
        self.start(s.ep, TYPE_ISO, 0, ctx)
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
        let wanted = cfg.map(|c| c.interfaces()).unwrap_or_default();
        let mut failed = Vec::new();
        for &a in &wanted {
            self.take(a);
            let rc = self.claim(a);
            if rc == 0 {
                self.claimed.push(a);
            } else {
                let class = cfg.and_then(|c| c.alt(a, 0)).map(|x| x.class).unwrap_or(0);
                log::warn!("usb: interface {a} (class {class:#04x}): {}", why_unclaimed(class, !self.detached.is_empty(), &err_name(self.u, rc)));
                failed.push(a);
            }
        }
        if !wanted.is_empty() && failed.len() == wanted.len() {
            // Nothing the guest could use: a device whose every transfer
            // fails only makes it reset and retry (firmware too). Refuse
            // it, give it back to the host now, and leave it alone.
            let back = if self.detached.is_empty() && !self.captured { "" } else { ", giving it back to the host" };
            log::warn!("usb: no interface of the device can be used: unplugging it from the VM{back}");
            self.give_back();
            self.refused.store(true, Ordering::Relaxed);
            self.shared.gone.store(true, Ordering::Relaxed);
            return Err(Status::Stall);
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

/// Why an interface couldn't be claimed, in words.
fn why_unclaimed(class: u8, taken: bool, err: &str) -> String {
    let root = {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    };
    if cfg!(target_os = "macos") && class == 8 && (taken || root) {
        // macOS 15 lets a program open USB storage only with its Full
        // Disk Access (or Removable Volumes) permission, root included.
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "vroom-netd".into());
        format!(
            "macOS refused USB storage to this program ({err}): give {exe} Full Disk Access \
             (System Settings, Privacy & Security, Full Disk Access)"
        )
    } else if cfg!(target_os = "macos") && !root {
        format!("in use by a macOS driver ({err}): only vroom-netd's root helper can take it")
    } else {
        format!("in use by the host ({err}): the guest can't use it")
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
        self.give_back();
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
    /// The device turned out unusable (see `UsbDevice::refused`).
    refused: Arc<AtomicBool>,
    /// Said why we can't open it (once).
    told: bool,
}

impl LibusbOpener {
    /// Loads libusb: fails if it isn't installed.
    pub fn new(sel: Selector) -> Result<LibusbOpener, String> {
        usb()?;
        Ok(LibusbOpener { sel, at: None, refused: Arc::new(AtomicBool::new(false)), told: false })
    }

    /// The host lists the device, but we couldn't open it: say why, once.
    fn tell(&mut self, listed: &[crate::list::HostDevice]) {
        let Some(d) = listed.first().filter(|_| !self.told) else { return };
        self.told = true;
        if d.captured {
            log::warn!(
                "usb: {} was taken from macOS by a root helper and not given back: only root can open it now \
                 (vroom-netd's helper), or unplug it and plug it back in",
                d.id()
            );
        } else {
            log::warn!("usb: {} is plugged in, but it can't be opened", d.id());
        }
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
        if self.refused.load(Ordering::Relaxed) {
            return None;
        }
        // What the host lists (registry / sysfs, no device opened). A
        // serial number picks the device by where that says it is.
        let listed: Vec<crate::list::HostDevice> = crate::list::list()
            .into_iter()
            .filter(|d| d.vendor == self.sel.vendor && d.product == self.sel.product && d.class != 9)
            .filter(|d| self.sel.serial.is_none() || d.serial == self.sel.serial)
            .collect();
        let wanted: Option<Vec<(u8, Vec<u8>)>> = self.sel.serial.as_ref().map(|_| listed.iter().map(|d| (d.bus, d.ports.clone())).collect());
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
                if !self.told {
                    log::warn!("usb: open {:04x}:{:04x}: {}", desc.vendor, desc.product, err_name(u, rc));
                    self.told = true;
                }
                continue;
            }
            let here = listed.iter().find(|l| (l.bus, &l.ports) == (bus, &ports));
            let captured = here.is_some_and(|l| l.captured);
            let held = here.is_some_and(|l| !l.claimed_by.is_empty());
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
            let mut dev = UsbDevice {
                u,
                h,
                shared,
                claimed: Vec::new(),
                num_configs: desc.configs,
                detached: Vec::new(),
                captured,
                refused: self.refused.clone(),
            };
            // SAFETY: geteuid has no preconditions.
            if cfg!(target_os = "macos") && (captured || held) && unsafe { libc::geteuid() } == 0 {
                dev.capture();
            }
            self.told = false;
            return Some((Box::new(dev), speed, desc.vendor, desc.product));
        }
        // libusb leaves out what it can't open (macOS: a device captured
        // for root).
        self.tell(&listed);
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
