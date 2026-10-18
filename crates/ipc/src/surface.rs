//! IOSurfaces (macOS): GPU pictures several processes can use, here the
//! guest's screen, drawn into by the GPU process and shown by the app.
//! They cross processes as mach ports ([`crate::mach`]) only: none is made
//! global, and nothing here looks one up by id.

use std::ffi::c_void;

use crate::mach::Port;

type CFTypeRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFMutableDictionaryRef = *mut c_void;
type CFStringRef = *const c_void;
type CFNumberRef = *const c_void;
type IOSurfaceRef = *const c_void;

/// The mach message that hands a monitor's surface on (GPU process to VMM,
/// VMM to app): its port, and data `[head, generation, slot, width,
/// height, 0]`.
pub const MSG_SURFACE: i32 = 0x766d_7331;
/// Surfaces per monitor and generation, at most.
pub const MAX_SLOTS: u32 = 3;
/// Monitors with surfaces, at most.
pub const MAX_HEADS: u32 = 4;

/// 'BGRA': what our surfaces hold, 4 bytes a pixel.
pub const BGRA: u32 = 0x4247_5241;
/// The largest side we make or take.
pub const MAX_SIDE: u32 = 16384;

const K_CF_NUMBER_SINT32: i32 = 3;
const K_IOSURFACE_LOCK_READ_ONLY: u32 = 1;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFDictionaryCreateMutable(alloc: CFTypeRef, cap: isize, keys: *const c_void, values: *const c_void) -> CFMutableDictionaryRef;
    fn CFDictionarySetValue(d: CFMutableDictionaryRef, k: CFTypeRef, v: CFTypeRef);
    fn CFNumberCreate(alloc: CFTypeRef, kind: i32, value: *const c_void) -> CFNumberRef;
    fn CFRelease(v: CFTypeRef);
    fn CFRetain(v: CFTypeRef) -> CFTypeRef;
}

#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    static kIOSurfaceWidth: CFStringRef;
    static kIOSurfaceHeight: CFStringRef;
    static kIOSurfaceBytesPerElement: CFStringRef;
    static kIOSurfacePixelFormat: CFStringRef;
    fn IOSurfaceCreate(props: CFDictionaryRef) -> IOSurfaceRef;
    fn IOSurfaceCreateMachPort(s: IOSurfaceRef) -> u32;
    fn IOSurfaceLookupFromMachPort(port: u32) -> IOSurfaceRef;
    fn IOSurfaceGetWidth(s: IOSurfaceRef) -> usize;
    fn IOSurfaceGetHeight(s: IOSurfaceRef) -> usize;
    fn IOSurfaceGetBytesPerElement(s: IOSurfaceRef) -> usize;
    fn IOSurfaceGetBytesPerRow(s: IOSurfaceRef) -> usize;
    fn IOSurfaceGetAllocSize(s: IOSurfaceRef) -> usize;
    fn IOSurfaceGetPixelFormat(s: IOSurfaceRef) -> u32;
    fn IOSurfaceGetPlaneCount(s: IOSurfaceRef) -> usize;
    fn IOSurfaceIsInUse(s: IOSurfaceRef) -> u8;
    fn IOSurfaceLock(s: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(s: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(s: IOSurfaceRef) -> *mut c_void;
}

/// One reference to an IOSurface.
#[derive(Debug)]
pub struct Surface(IOSurfaceRef);

// SAFETY: IOSurfaceRef is a thread-safe CoreFoundation object.
unsafe impl Send for Surface {}
// SAFETY: see above; we only call thread-safe IOSurface functions.
unsafe impl Sync for Surface {}

impl Surface {
    /// A new BGRA surface, `w` x `h`, private to this process until sent.
    pub fn new(w: u32, h: u32) -> Option<Surface> {
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return None;
        }
        // SAFETY: CoreFoundation calls with valid arguments; every object
        // we create is released below (the dictionary retains what it
        // holds).
        unsafe {
            let d = CFDictionaryCreateMutable(std::ptr::null(), 0, &kCFTypeDictionaryKeyCallBacks as *const _, &kCFTypeDictionaryValueCallBacks as *const _);
            if d.is_null() {
                return None;
            }
            for (k, v) in [(kIOSurfaceWidth, w as i32), (kIOSurfaceHeight, h as i32), (kIOSurfaceBytesPerElement, 4), (kIOSurfacePixelFormat, BGRA as i32)] {
                let n = CFNumberCreate(std::ptr::null(), K_CF_NUMBER_SINT32, &v as *const i32 as *const c_void);
                CFDictionarySetValue(d, k, n);
                CFRelease(n);
            }
            let s = IOSurfaceCreate(d);
            CFRelease(d);
            (!s.is_null()).then_some(Surface(s))
        }
    }

    /// The surface a send right names (taken), checked to be what we make:
    /// one plane, BGRA, `w` x `h`, its rows inside its memory.
    pub fn from_port(port: Port, w: u32, h: u32) -> Option<Surface> {
        // SAFETY: a send right we own; the lookup takes its own reference
        // to the surface, the port is ours to give back after.
        let s = unsafe { IOSurfaceLookupFromMachPort(port.name()) };
        drop(port);
        if s.is_null() {
            return None;
        }
        let s = Surface(s);
        let ok = s.width() == w
            && s.height() == h
            && w > 0
            && h > 0
            && w <= MAX_SIDE
            && h <= MAX_SIDE
            // SAFETY: a valid surface (whole block).
            && unsafe { IOSurfaceGetBytesPerElement(s.0) == 4 && IOSurfaceGetPixelFormat(s.0) == BGRA && IOSurfaceGetPlaneCount(s.0) <= 1 }
            && s.row_bytes() >= w as usize * 4
            && s.row_bytes().checked_mul(h as usize).is_some_and(|n| n <= s.alloc_size());
        ok.then_some(s)
    }

    /// A send right to it, for another process.
    pub fn port(&self) -> Option<Port> {
        // SAFETY: a valid surface; the result is a send right we own.
        let p = unsafe { IOSurfaceCreateMachPort(self.0) };
        // SAFETY: a fresh send right, ours alone.
        (p != 0).then(|| unsafe { Port::from_send_right(p) })
    }

    pub fn width(&self) -> u32 {
        // SAFETY: a valid surface.
        unsafe { IOSurfaceGetWidth(self.0) as u32 }
    }

    pub fn height(&self) -> u32 {
        // SAFETY: a valid surface.
        unsafe { IOSurfaceGetHeight(self.0) as u32 }
    }

    pub fn row_bytes(&self) -> usize {
        // SAFETY: a valid surface.
        unsafe { IOSurfaceGetBytesPerRow(self.0) }
    }

    fn alloc_size(&self) -> usize {
        // SAFETY: a valid surface.
        unsafe { IOSurfaceGetAllocSize(self.0) }
    }

    /// Something (the window server, showing it) uses it now.
    pub fn in_use(&self) -> bool {
        // SAFETY: a valid surface.
        unsafe { IOSurfaceIsInUse(self.0) != 0 }
    }

    /// The IOSurfaceRef, for a CALayer or GL; valid while `self` lives.
    pub fn as_ptr(&self) -> *const c_void {
        self.0
    }

    /// Copy rows `y0..y0 + rows` (BGRX, `w * 4` bytes each) into `out`,
    /// `stride` bytes apart, with the CPU. False if it doesn't fit.
    pub fn read_rows(&self, y0: u32, rows: u32, out: &mut [u8], stride: usize) -> bool {
        let (w, h) = (self.width() as usize, self.height() as usize);
        let row = w * 4;
        let end = y0 as usize + rows as usize;
        if end > h || stride < row || (rows > 0 && (rows as usize - 1) * stride + row > out.len()) {
            return false;
        }
        let src_stride = self.row_bytes();
        // SAFETY: locked read-only around the reads; every read is inside
        // the surface (rows and row length checked against its size above,
        // and from_port/new checked rows fit its memory).
        unsafe {
            if IOSurfaceLock(self.0, K_IOSURFACE_LOCK_READ_ONLY, std::ptr::null_mut()) != 0 {
                return false;
            }
            let base = IOSurfaceGetBaseAddress(self.0) as *const u8;
            if !base.is_null() {
                for i in 0..rows as usize {
                    let s = base.add((y0 as usize + i) * src_stride);
                    std::ptr::copy_nonoverlapping(s, out.as_mut_ptr().add(i * stride), row);
                }
            }
            IOSurfaceUnlock(self.0, K_IOSURFACE_LOCK_READ_ONLY, std::ptr::null_mut());
            !base.is_null()
        }
    }
}

impl Clone for Surface {
    fn clone(&self) -> Surface {
        // SAFETY: a valid CF object; one more reference.
        Surface(unsafe { CFRetain(self.0) })
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: the reference we hold.
        unsafe { CFRelease(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surface_crosses_as_a_port_and_is_checked() {
        let s = Surface::new(64, 32).unwrap();
        assert_eq!((s.width(), s.height()), (64, 32));
        assert!(s.row_bytes() >= 256);
        let back = Surface::from_port(s.port().unwrap(), 64, 32).expect("same surface");
        assert_eq!((back.width(), back.height()), (64, 32));
        // Not what was announced: refused.
        assert!(Surface::from_port(s.port().unwrap(), 64, 33).is_none());
        let mut px = vec![1u8; 64 * 4 * 2];
        assert!(back.read_rows(30, 2, &mut px, 256));
        assert!(!back.read_rows(31, 2, &mut px, 256));
        assert!(Surface::new(0, 1).is_none() && Surface::new(MAX_SIDE + 1, 1).is_none());
    }
}
