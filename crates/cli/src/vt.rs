//! VideoToolbox decoding for the video helper: one decompression
//! session per guest decoder, NV12 pictures out, copied into the guest's
//! layout in the output callback.
//!
//! Only parameter sets and access units that `vroom_video::nal` took
//! apart reach VideoToolbox, as length-prefixed samples.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;

use vroom_video::{Codec, Layout};

type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFStringRef = *const c_void;
type CFNumberRef = *const c_void;
type CMFormatDescriptionRef = *const c_void;
type CMBlockBufferRef = *const c_void;
type CMSampleBufferRef = *const c_void;
type CVImageBufferRef = *const c_void;
type VTDecompressionSessionRef = *const c_void;
type OSStatus = i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const CM_TIME_INVALID: CMTime = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };
const CM_TIME_VALID: u32 = 1;

#[repr(C)]
struct CMSampleTimingInfo {
    duration: CMTime,
    pts: CMTime,
    dts: CMTime,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMVideoDimensions {
    width: i32,
    height: i32,
}

type OutputCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    flags: u32,
    image: CVImageBufferRef,
    pts: CMTime,
    duration: CMTime,
);

#[repr(C)]
struct OutputCallbackRecord {
    callback: OutputCallback,
    refcon: *mut c_void,
}

#[repr(C)]
struct DictCallbacks {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeDictionaryKeyCallBacks: DictCallbacks;
    static kCFTypeDictionaryValueCallBacks: DictCallbacks;
    fn CFRelease(cf: CFTypeRef);
    fn CFDictionaryCreate(
        alloc: CFAllocatorRef,
        keys: *const *const c_void,
        values: *const *const c_void,
        n: isize,
        kcb: *const DictCallbacks,
        vcb: *const DictCallbacks,
    ) -> CFDictionaryRef;
    fn CFNumberCreate(alloc: CFAllocatorRef, kind: isize, value: *const c_void) -> CFNumberRef;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    fn CVPixelBufferLockBaseAddress(pb: CVImageBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: CVImageBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetPixelFormatType(pb: CVImageBufferRef) -> u32;
    fn CVPixelBufferGetPlaneCount(pb: CVImageBufferRef) -> usize;
    fn CVPixelBufferGetWidthOfPlane(pb: CVImageBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pb: CVImageBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetBytesPerRowOfPlane(pb: CVImageBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(pb: CVImageBufferRef, plane: usize) -> *const u8;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        alloc: CFAllocatorRef,
        count: usize,
        ptrs: *const *const u8,
        sizes: *const usize,
        nal_len: i32,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        alloc: CFAllocatorRef,
        count: usize,
        ptrs: *const *const u8,
        sizes: *const usize,
        nal_len: i32,
        extensions: CFDictionaryRef,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetDimensions(fd: CMFormatDescriptionRef) -> CMVideoDimensions;
    fn CMBlockBufferCreateWithMemoryBlock(
        alloc: CFAllocatorRef,
        block: *mut c_void,
        block_len: usize,
        block_alloc: CFAllocatorRef,
        custom: *const c_void,
        offset: usize,
        len: usize,
        flags: u32,
        out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(src: *const c_void, dst: CMBlockBufferRef, offset: usize, len: usize) -> OSStatus;
    fn CMSampleBufferCreateReady(
        alloc: CFAllocatorRef,
        data: CMBlockBufferRef,
        fd: CMFormatDescriptionRef,
        samples: isize,
        timings: isize,
        timing: *const CMSampleTimingInfo,
        sizes_n: isize,
        sizes: *const usize,
        out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn VTDecompressionSessionCreate(
        alloc: CFAllocatorRef,
        fd: CMFormatDescriptionRef,
        decoder_spec: CFDictionaryRef,
        dest_attrs: CFDictionaryRef,
        callback: *const OutputCallbackRecord,
        out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        s: VTDecompressionSessionRef,
        sample: CMSampleBufferRef,
        flags: u32,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionFinishDelayedFrames(s: VTDecompressionSessionRef) -> OSStatus;
    fn VTDecompressionSessionWaitForAsynchronousFrames(s: VTDecompressionSessionRef) -> OSStatus;
    fn VTDecompressionSessionCanAcceptFormatDescription(s: VTDecompressionSessionRef, fd: CMFormatDescriptionRef) -> u8;
    fn VTDecompressionSessionInvalidate(s: VTDecompressionSessionRef);
}

/// '420v': NV12, video range.
const NV12: u32 = 0x3432_3076;
/// Let the decoder hold pictures back and hand them out in display order.
const TEMPORAL_PROCESSING: u32 = 1 << 3;

/// A decoded picture (or a failed one), in decoding order of output.
#[derive(Debug)]
pub struct Decoded {
    /// The `refcon` given to [`Session::decode`].
    pub tag: u64,
    /// NV12 in the session's layout; `None`: the picture failed.
    pub data: Option<Vec<u8>>,
}

/// What the output callback writes to.
struct Sink {
    layout: Layout,
    out: Mutex<VecDeque<Decoded>>,
}

/// A CoreFoundation object we own a reference to.
struct Cf(CFTypeRef);

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: we own one reference to this object.
            unsafe { CFRelease(self.0) };
        }
    }
}

/// One VideoToolbox decompression session for one format (parameter
/// sets). A new format that the session can't take needs a new one.
pub struct Session {
    vt: VTDecompressionSessionRef,
    fd: Cf,
    sink: Box<Sink>,
    pub layout: Layout,
    pub visible: (u32, u32),
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("layout", &self.layout).finish()
    }
}

// SAFETY: VideoToolbox sessions may be used from any thread; we use each
// from one thread at a time (the helper's).
unsafe impl Send for Session {}

/// A format description from parameter sets (already checked by `nal`).
fn format(codec: Codec, sets: &[&[u8]]) -> Option<Cf> {
    if sets.is_empty() || sets.len() > 64 {
        return None;
    }
    let ptrs: Vec<*const u8> = sets.iter().map(|s| s.as_ptr()).collect();
    let sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
    let mut fd: CMFormatDescriptionRef = ptr::null();
    // SAFETY: ptrs/sizes describe live slices for the call; fd is written
    // by CoreMedia (owned reference) on success.
    let st = unsafe {
        match codec {
            Codec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(ptr::null(), sets.len(), ptrs.as_ptr(), sizes.as_ptr(), 4, &mut fd),
            Codec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                ptr::null(),
                sets.len(),
                ptrs.as_ptr(),
                sizes.as_ptr(),
                4,
                ptr::null(),
                &mut fd,
            ),
        }
    };
    if st != 0 || fd.is_null() {
        log::info!("video: parameter sets refused ({st})");
        return None;
    }
    Some(Cf(fd))
}

unsafe extern "C" fn on_output(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    _flags: u32,
    image: CVImageBufferRef,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: refcon is the session's Sink, alive until the session is
    // invalidated (Session::drop waits for every frame first).
    let sink = unsafe { &*(refcon as *const Sink) };
    let data = if status == 0 && !image.is_null() {
        // SAFETY: a pixel buffer VideoToolbox hands us for this call.
        unsafe { copy_nv12(image, sink.layout) }
    } else {
        None
    };
    if data.is_none() {
        log::info!("video: a picture failed to decode ({status})");
    }
    sink.out.lock().unwrap_or_else(|e| e.into_inner()).push_back(Decoded { tag: frame_refcon as u64, data });
}

/// Copy an NV12 pixel buffer into `layout` (top left; the rest black).
///
/// # Safety
/// `pb` must be a valid CVPixelBuffer.
unsafe fn copy_nv12(pb: CVImageBufferRef, layout: Layout) -> Option<Vec<u8>> {
    // SAFETY: pb is valid (caller); planes are read under the base
    // address lock, within the sizes CoreVideo reports for them.
    unsafe {
        if CVPixelBufferGetPixelFormatType(pb) != NV12 || CVPixelBufferGetPlaneCount(pb) != 2 {
            return None;
        }
        if CVPixelBufferLockBaseAddress(pb, 1) != 0 {
            return None;
        }
        let (lw, lh) = (layout.width as usize, layout.height as usize);
        let mut out = vec![0u8; layout.size() as usize];
        out[lw * lh..].fill(128);
        for plane in 0..2 {
            let base = CVPixelBufferGetBaseAddressOfPlane(pb, plane);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, plane);
            // Plane 1 is half height; its width in bytes is the luma width.
            let row = (CVPixelBufferGetWidthOfPlane(pb, plane) * (plane + 1)).min(lw).min(stride);
            let rows = CVPixelBufferGetHeightOfPlane(pb, plane).min(if plane == 0 { lh } else { lh / 2 });
            if base.is_null() {
                continue;
            }
            let dst0 = if plane == 0 { 0 } else { lw * lh };
            for y in 0..rows {
                let src = std::slice::from_raw_parts(base.add(y * stride), row);
                out[dst0 + y * lw..dst0 + y * lw + row].copy_from_slice(src);
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, 1);
        Some(out)
    }
}

impl Session {
    /// A session for these parameter sets, or None if VideoToolbox won't.
    pub fn new(codec: Codec, sets: &[&[u8]]) -> Option<Session> {
        let fd = format(codec, sets)?;
        // SAFETY: fd is a valid format description.
        let dim = unsafe { CMVideoFormatDescriptionGetDimensions(fd.0) };
        let (w, h) = (u32::try_from(dim.width).ok()?, u32::try_from(dim.height).ok()?);
        let layout = Layout::for_picture(w, h)?;
        let sink = Box::new(Sink { layout, out: Mutex::new(VecDeque::new()) });
        let pixfmt = NV12 as i32;
        // SAFETY: CF objects created and released here; the callback record
        // points at our boxed sink, which outlives the session.
        unsafe {
            let num = Cf(CFNumberCreate(ptr::null(), 3, &pixfmt as *const i32 as *const c_void));
            let keys = [kCVPixelBufferPixelFormatTypeKey];
            let vals = [num.0];
            let attrs = Cf(CFDictionaryCreate(
                ptr::null(),
                keys.as_ptr(),
                vals.as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            ));
            let rec = OutputCallbackRecord { callback: on_output, refcon: &*sink as *const Sink as *mut c_void };
            let mut vt: VTDecompressionSessionRef = ptr::null();
            let st = VTDecompressionSessionCreate(ptr::null(), fd.0, ptr::null(), attrs.0, &rec, &mut vt);
            if st != 0 || vt.is_null() {
                log::info!("video: no decompression session ({st})");
                return None;
            }
            Some(Session { vt, fd, sink, layout, visible: (w, h) })
        }
    }

    /// Can this session go on with a stream using these parameter sets?
    pub fn accepts(&self, codec: Codec, sets: &[&[u8]]) -> Option<bool> {
        let fd = format(codec, sets)?;
        // SAFETY: both valid.
        let dim = unsafe { CMVideoFormatDescriptionGetDimensions(fd.0) };
        if (dim.width as u32, dim.height as u32) != self.visible {
            return Some(false);
        }
        // SAFETY: both valid.
        Some(unsafe { VTDecompressionSessionCanAcceptFormatDescription(self.vt, fd.0) } != 0)
    }

    /// Decode one access unit (length-prefixed NAL units). `tag` comes
    /// back with its picture. False: VideoToolbox refused it outright.
    pub fn decode(&mut self, au: &[u8], tag: u64, order: i64) -> bool {
        if au.is_empty() {
            return false;
        }
        // SAFETY: the block buffer gets its own copy of `au`; the sample
        // buffer and block buffer are released before returning.
        unsafe {
            let mut bb: CMBlockBufferRef = ptr::null();
            if CMBlockBufferCreateWithMemoryBlock(ptr::null(), ptr::null_mut(), au.len(), ptr::null(), ptr::null(), 0, au.len(), 1, &mut bb) != 0 || bb.is_null() {
                return false;
            }
            let bb = Cf(bb);
            if CMBlockBufferReplaceDataBytes(au.as_ptr() as *const c_void, bb.0, 0, au.len()) != 0 {
                return false;
            }
            let t = CMTime { value: order, timescale: 1000, flags: CM_TIME_VALID, epoch: 0 };
            let timing = CMSampleTimingInfo { duration: CM_TIME_INVALID, pts: t, dts: CM_TIME_INVALID };
            let size = au.len();
            let mut sb: CMSampleBufferRef = ptr::null();
            if CMSampleBufferCreateReady(ptr::null(), bb.0, self.fd.0, 1, 1, &timing, 1, &size, &mut sb) != 0 || sb.is_null() {
                return false;
            }
            let sb = Cf(sb);
            let mut info = 0u32;
            let st = VTDecompressionSessionDecodeFrame(self.vt, sb.0, TEMPORAL_PROCESSING, tag as *mut c_void, &mut info);
            if st != 0 {
                log::debug!("video: decode refused ({st})");
            }
            st == 0
        }
    }

    /// Hand out every picture held back for reordering.
    pub fn finish(&mut self) {
        // SAFETY: valid session.
        unsafe {
            VTDecompressionSessionFinishDelayedFrames(self.vt);
            VTDecompressionSessionWaitForAsynchronousFrames(self.vt);
        }
    }

    /// Pictures done so far, in display order.
    pub fn take(&mut self) -> Vec<Decoded> {
        self.sink.out.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: valid session; after invalidation no callback runs, so
        // the sink may go.
        unsafe {
            VTDecompressionSessionWaitForAsynchronousFrames(self.vt);
            VTDecompressionSessionInvalidate(self.vt);
            CFRelease(self.vt);
        }
    }
}
