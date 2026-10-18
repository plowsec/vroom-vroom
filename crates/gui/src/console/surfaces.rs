//! The guest's screen straight from the GPU (macOS): the GPU process draws
//! each 3D frame into an IOSurface and we show that surface in a CALayer
//! over the screen's place in the window. No pixels cross processes: the
//! VMM, the GPU process and we used to copy every frame (the GPU's
//! readback, the shared frame buffer, our RGBA picture and Skia's texture),
//! about a CPU core at 2560x1600.
//!
//! The surfaces come as mach ports: a port of ours the VMM inherits when we
//! start it (vroom_ipc::mach), checked to come from that VMM, each surface
//! checked to be the size it says. A VM we reattach to, Coherence, and any
//! time something of ours is drawn over the screen (menus, messages, the
//! pause veil) use the shared frame buffer as before: the layer would hide
//! them.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use vroom_ipc::mach::Port;
use vroom_ipc::surface::{Surface, MAX_SLOTS, MSG_SURFACE};

/// The first monitor's surfaces, as the VMM handed them on.
#[derive(Debug, Default)]
pub struct Pool {
    gen: u32,
    size: (u32, u32),
    slots: [Option<Surface>; MAX_SLOTS as usize],
}

impl Pool {
    /// Surface `slot` of generation `gen`, if we have it.
    pub fn get(&self, gen: u32, slot: u32) -> Option<&Surface> {
        (self.gen == gen).then(|| self.slots.get(slot as usize)?.as_ref()).flatten()
    }
}

/// Receive surfaces from VMM `pid` on `rx` until `gone`.
pub fn inbox(rx: Port, pid: u32, gone: Arc<AtomicBool>) -> std::io::Result<Arc<Mutex<Pool>>> {
    let pool = Arc::new(Mutex::new(Pool::default()));
    let p = pool.clone();
    std::thread::Builder::new().name("vmm-surfaces".into()).spawn(move || {
        while !gone.load(Ordering::Acquire) {
            let m = match rx.recv(Some(Duration::from_millis(500))) {
                Ok(Some(m)) => m,
                Ok(None) => continue,
                Err(e) => {
                    log::warn!("surfaces: {e}");
                    return;
                }
            };
            let [head, gen, slot, w, h, _] = m.data;
            // Only our VMM, only what we asked for: the first monitor.
            if m.id != MSG_SURFACE || m.pid as u32 != pid || head != 0 || slot >= MAX_SLOTS as u64 || gen == 0 || gen > u32::MAX as u64 {
                log::warn!("surfaces: a message from pid {} refused", m.pid);
                continue;
            }
            let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else { continue };
            let Some(s) = m.port.and_then(|port| Surface::from_port(port, w, h)) else {
                log::warn!("surfaces: not the {w}x{h} surface announced");
                continue;
            };
            let mut p = p.lock().unwrap_or_else(|e| e.into_inner());
            if p.gen != gen as u32 {
                *p = Pool { gen: gen as u32, size: (w, h), ..Pool::default() };
            }
            if p.size == (w, h) {
                p.slots[slot as usize] = Some(s);
            }
        }
    })?;
    Ok(pool)
}

/// A CALayer over the guest screen's place, showing a surface.
#[derive(Debug)]
pub struct Layer {
    layer: Retained<AnyObject>,
    parent: Retained<AnyObject>,
    shown: Option<*const c_void>,
    visible: bool,
}

/// CATransaction around our changes: no implicit animations.
fn quietly(f: impl FnOnce()) {
    let Some(t) = AnyClass::get(c"CATransaction") else { return f() };
    // SAFETY: CATransaction class methods, on the main thread.
    unsafe {
        let _: () = msg_send![t, begin];
        let _: () = msg_send![t, setDisableActions: true];
    }
    f();
    // SAFETY: as above.
    unsafe {
        let _: () = msg_send![t, commit];
    }
}

impl Layer {
    /// A hidden layer on top of `view`'s (the window's NSView). Main
    /// thread only.
    pub fn new(view: *mut c_void) -> Option<Layer> {
        if view.is_null() {
            return None;
        }
        let class = AnyClass::get(c"CALayer")?;
        // SAFETY: `view` is the window's NSView, alive while the window is
        // (we are called from that window's code, on the main thread);
        // plain AppKit and Core Animation calls.
        unsafe {
            let view = view as *mut AnyObject;
            let _: () = msg_send![view, setWantsLayer: true];
            let parent: *mut AnyObject = msg_send![view, layer];
            let parent = Retained::retain(parent)?;
            let layer: Retained<AnyObject> = msg_send![class, new];
            let _: () = msg_send![&*layer, setOpaque: true];
            let _: () = msg_send![&*layer, setHidden: true];
            // Above whatever the window draws its own content with.
            let _: () = msg_send![&*layer, setZPosition: 1000.0f64];
            let gravity: *mut AnyObject = msg_send![AnyClass::get(c"NSString")?, stringWithUTF8String: c"resize".as_ptr()];
            let _: () = msg_send![&*layer, setContentsGravity: gravity];
            let _: () = msg_send![&*parent, addSublayer: &*layer];
            Some(Layer { layer, parent, shown: None, visible: false })
        }
    }

    /// Put it over `(x, y, w, h)` (logical pixels from the window's
    /// top left), showing `surface`, or hide it.
    pub fn set(&mut self, surface: Option<&Surface>, (x, y, w, h): (f64, f64, f64, f64), visible: bool) {
        let ptr = surface.map(|s| s.as_ptr());
        let visible = visible && ptr.is_some() && w > 0.0 && h > 0.0;
        quietly(|| {
            // SAFETY: our layer and its parent, on the main thread; the
            // surface stays alive in the pool while it is shown (the pool
            // keeps every surface of the generation shown).
            unsafe {
                let bounds: NSRect = msg_send![&*self.parent, bounds];
                let flipped: bool = msg_send![&*self.parent, isGeometryFlipped];
                let ly = if flipped { y } else { bounds.size.height - y - h };
                let frame = NSRect::new(NSPoint::new(x, ly), NSSize::new(w, h));
                let _: () = msg_send![&*self.layer, setFrame: frame];
                if ptr != self.shown {
                    let _: () = msg_send![&*self.layer, setContents: ptr.unwrap_or(std::ptr::null()) as *const AnyObject];
                    self.shown = ptr;
                }
                if visible != self.visible {
                    let _: () = msg_send![&*self.layer, setHidden: !visible];
                    self.visible = visible;
                }
            }
        });
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        // SAFETY: our layer, taken out of its parent.
        unsafe {
            let _: () = msg_send![&*self.layer, removeFromSuperlayer];
        }
    }
}

impl super::Console {
    /// Follow the window: the layer over the screen's place with the
    /// latest frame, shown unless something is drawn over the screen;
    /// surfaces from the VMM only while the layer can show them.
    pub(super) fn surface_update(&mut self) {
        use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use slint::winit_030::WinitWindowAccessor;
        use slint::ComponentHandle;
        if self.surf.is_none() {
            return;
        }
        // Tooltips hang from the toolbar over the screen: step aside while
        // the mouse is there (and a little below, where they show).
        let sf = self.win.window().scale_factor() as f64;
        let in_bar = {
            let mut io = self.io.borrow_mut();
            io.bar_px = (self.win.get_bar_height() as f64 + 4.0) * sf;
            io.in_bar
        };
        let covered = self.win.get_screen_covered() || in_bar;
        let want = !covered && self.coh.is_none() && self.state == crate::VmState::Running && self.mode != (0, 0);
        let Some(s) = self.surf.as_mut() else { return };
        if s.layer.is_none() && want {
            let view = self.win.window().with_winit_window(|w| match w.window_handle().ok()?.as_raw() {
                RawWindowHandle::AppKit(h) => Some(h.ns_view.as_ptr()),
                _ => None,
            });
            s.layer = view.flatten().and_then(Layer::new);
            if s.layer.is_none() {
                log::warn!("surfaces: no layer on the window: pixels as before");
                self.surf = None;
                return;
            }
        }
        let want = want && s.layer.is_some();
        if want != s.on {
            s.on = want;
            if !want {
                // Back to pixels: the picture first, so nothing old shows
                // while the first frame comes back.
                self.picture_from_surface();
            }
            self.io.borrow_mut().send(vroom_display::ToVmm::Surfaces(want));
        }
        let Some(s) = self.surf.as_mut() else { return };
        let rect = (self.win.get_screen_x() as f64, self.win.get_screen_y() as f64, self.win.get_screen_width() as f64, self.win.get_screen_height() as f64);
        let pool = s.pool.lock().unwrap_or_else(|e| e.into_inner());
        let surface = s.shown.and_then(|(g, slot)| pool.get(g, slot));
        if let Some(l) = s.layer.as_mut() {
            l.set(surface, rect, s.on && !covered);
        }
    }

    /// The latest frame's surface into our picture of the screen (the
    /// thumbnail, Slint's own drawing, shots).
    pub(super) fn picture_from_surface(&mut self) {
        let Some(s) = self.surf.as_ref() else { return };
        let Some((g, slot)) = s.shown else { return };
        let pool = s.pool.lock().unwrap_or_else(|e| e.into_inner());
        let Some(surface) = pool.get(g, slot) else { return };
        let (w, h) = (surface.width(), surface.height());
        let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
        let px = buf.make_mut_bytes();
        if !surface.read_rows(0, h, px, w as usize * 4) {
            return;
        }
        drop(pool);
        for p in px.as_chunks_mut::<4>().0 {
            // BGRA (alpha unused) to RGBA, opaque.
            *p = [p[2], p[1], p[0], 0xff];
        }
        self.img = Some(buf.clone());
        self.win.set_frame(slint::Image::from_rgba8(buf));
    }
}
