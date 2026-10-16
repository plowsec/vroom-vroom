//! VideoToolbox decoding for the video helper: one decompression
//! session per stream format, pictures kept as VideoToolbox's own pixel
//! buffers until they are written into a guest buffer.
//!
//! Only what `vroom_video` took apart reaches VideoToolbox: H.264 / HEVC
//! parameter sets and length-prefixed access units, VP9 frames whose
//! header we read, AV1 temporal units and their sequence header.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::Mutex;

use vroom_video::vpx::{self, Config};
use vroom_video::{Codec, Layout, PixFmt};

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
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFDictionaryCreate(
        alloc: CFAllocatorRef,
        keys: *const *const c_void,
        values: *const *const c_void,
        n: isize,
        kcb: *const DictCallbacks,
        vcb: *const DictCallbacks,
    ) -> CFDictionaryRef;
    fn CFNumberCreate(alloc: CFAllocatorRef, kind: isize, value: *const c_void) -> CFNumberRef;
    fn CFDataCreate(alloc: CFAllocatorRef, bytes: *const u8, len: isize) -> CFTypeRef;
    fn CFStringCreateWithBytes(alloc: CFAllocatorRef, bytes: *const u8, len: isize, encoding: u32, external: u8) -> CFStringRef;
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
    static kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: CFStringRef;
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
    fn CMVideoFormatDescriptionCreate(alloc: CFAllocatorRef, codec: u32, width: i32, height: i32, extensions: CFDictionaryRef, out: *mut CMFormatDescriptionRef) -> OSStatus;
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
    fn VTRegisterSupplementalVideoDecoderIfAvailable(codec: u32);
    fn VTIsHardwareDecodeSupported(codec: u32) -> u8;
}

/// '420v': NV12, video range.
const NV12: u32 = 0x3432_3076;
/// 'x420': 10-bit 4:2:0, two planes, video range (P010's layout).
const X420: u32 = 0x7834_3230;
/// Let the decoder hold pictures back and hand them out in display order.
const TEMPORAL_PROCESSING: u32 = 1 << 3;
/// kVTDecodeInfo_FrameDropped.
const FRAME_DROPPED: u32 = 1 << 1;

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

fn codec_type(c: Codec) -> u32 {
    match c {
        Codec::H264 => fourcc(b"avc1"),
        Codec::Hevc => fourcc(b"hvc1"),
        Codec::Vp9 => fourcc(b"vp09"),
        Codec::Av1 => fourcc(b"av01"),
    }
}

/// The codecs this Mac decodes in hardware ([`Codec::bit`]s). VP9 and
/// AV1 decoders are "supplemental" and must be asked for first.
pub fn caps() -> u32 {
    let mut bits = 0;
    for c in Codec::ALL {
        // SAFETY: plain calls with a codec type.
        let ok = unsafe {
            if matches!(c, Codec::Vp9 | Codec::Av1) {
                VTRegisterSupplementalVideoDecoderIfAvailable(codec_type(c));
            }
            VTIsHardwareDecodeSupported(codec_type(c)) != 0
        };
        if ok {
            bits |= c.bit();
        }
    }
    bits
}

/// What a session decodes: H.264 / HEVC parameter sets, or a VP9 / AV1
/// stream's [`Config`] (with the AV1 sequence header OBU).
#[derive(Debug, Clone, Copy)]
pub enum Desc<'a> {
    Nal(Codec, &'a [&'a [u8]], u32),
    Vp9(&'a Config),
    Av1(&'a Config, &'a [u8]),
}

/// A decoded picture: VideoToolbox's pixel buffer, held until it is
/// written into a guest buffer.
pub struct Image(CVImageBufferRef);

// SAFETY: a CVPixelBuffer may be used from any thread; we only read it.
unsafe impl Send for Image {}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: we own one reference (retained in the callback).
        unsafe { CFRelease(self.0) };
    }
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Image")
    }
}

impl Image {
    /// Write the picture into `dst` (`len` bytes) as `layout` in `fmt`:
    /// top left, the rest black. False: it doesn't fit or the pixel
    /// buffer isn't one we know.
    ///
    /// # Safety
    /// `dst..dst + len` must be writable memory no Rust reference points
    /// into (the guest's buffer window).
    pub unsafe fn write(&self, dst: *mut u8, len: usize, layout: Layout, fmt: PixFmt) -> bool {
        let pb = self.0;
        let need = layout.size(fmt) as usize;
        if len < need {
            return false;
        }
        // SAFETY: pb is a valid pixel buffer (we hold a reference); planes
        // are read under the base address lock, within the sizes CoreVideo
        // reports; every write is checked against `need` <= `len`.
        unsafe {
            let src_fmt = CVPixelBufferGetPixelFormatType(pb);
            let src_bytes = match src_fmt {
                NV12 => 1,
                X420 => 2,
                _ => return false,
            };
            if CVPixelBufferGetPlaneCount(pb) != 2 || CVPixelBufferLockBaseAddress(pb, 1) != 0 {
                return false;
            }
            let db = fmt.bytes() as usize;
            let (lw, lh) = (layout.width as usize, layout.height as usize);
            let stride = lw * db;
            for plane in 0..2usize {
                let base = CVPixelBufferGetBaseAddressOfPlane(pb, plane);
                let src_stride = CVPixelBufferGetBytesPerRowOfPlane(pb, plane);
                // Samples per row (chroma: Cb and Cr each), rows.
                let samples = (CVPixelBufferGetWidthOfPlane(pb, plane) * (plane + 1)).min(lw).min(src_stride / src_bytes);
                let rows = CVPixelBufferGetHeightOfPlane(pb, plane).min(if plane == 0 { lh } else { lh / 2 });
                let dst0 = if plane == 0 { 0 } else { stride * lh };
                let all_rows = if plane == 0 { lh } else { lh / 2 };
                let fill: u8 = if plane == 0 { 0 } else { 128 };
                for y in 0..all_rows {
                    let row = dst.add(dst0 + y * stride);
                    let mut done = 0;
                    if y < rows && !base.is_null() {
                        let src = base.add(y * src_stride);
                        copy_row(src, src_bytes, row, db, samples);
                        done = samples;
                    }
                    fill_row(row, db, done, lw, fill);
                }
            }
            CVPixelBufferUnlockBaseAddress(pb, 1);
        }
        true
    }
}

/// Copy `n` samples, converting between 8 and 16 bits a sample.
///
/// # Safety
/// `src` readable for `n * sb` bytes, `dst` writable for `n * db`.
unsafe fn copy_row(src: *const u8, sb: usize, dst: *mut u8, db: usize, n: usize) {
    // SAFETY: the caller's ranges.
    unsafe {
        match (sb, db) {
            (1, 1) | (2, 2) => ptr::copy_nonoverlapping(src, dst, n * sb),
            // 10 bits (in the top of 16) to 8: the high byte.
            (2, 1) => {
                for x in 0..n {
                    *dst.add(x) = *src.add(2 * x + 1);
                }
            }
            _ => {
                for x in 0..n {
                    *dst.add(2 * x) = 0;
                    *dst.add(2 * x + 1) = *src.add(x);
                }
            }
        }
    }
}

/// Fill samples `from..to` of a row with `v` (in the high byte for 16 bits).
///
/// # Safety
/// `dst` writable for `to * db` bytes.
unsafe fn fill_row(dst: *mut u8, db: usize, from: usize, to: usize, v: u8) {
    if from >= to {
        return;
    }
    // SAFETY: the caller's range.
    unsafe {
        if db == 1 {
            ptr::write_bytes(dst.add(from), v, to - from);
        } else {
            for x in from..to {
                *dst.add(2 * x) = 0;
                *dst.add(2 * x + 1) = v;
            }
        }
    }
}

/// A decoded picture (or a failed one), in VideoToolbox's output order.
#[derive(Debug)]
pub struct Decoded {
    /// The `tag` given to [`Session::decode`].
    pub tag: u64,
    /// `None`: the picture failed (or was dropped).
    pub image: Option<Image>,
}

/// What the output callback writes to.
struct Sink {
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

/// One VideoToolbox decompression session for one format. A format the
/// session can't take needs a new one.
pub struct Session {
    vt: VTDecompressionSessionRef,
    fd: Cf,
    sink: Box<Sink>,
    pub layout: Layout,
    pub visible: (u32, u32),
    /// Bits per sample of the pictures (8 or 10).
    pub depth: u32,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("layout", &self.layout).finish()
    }
}

// SAFETY: VideoToolbox sessions may be used from any thread; we use each
// from one thread at a time (the helper's).
unsafe impl Send for Session {}

fn cfstr(s: &str) -> Cf {
    // SAFETY: bytes of a &str, UTF-8 (0x08000100).
    Cf(unsafe { CFStringCreateWithBytes(ptr::null(), s.as_ptr(), s.len() as isize, 0x0800_0100, 0) })
}

/// A dictionary of `pairs` (each retained by it).
fn dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Cf {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
    let vals: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
    // SAFETY: keys and values are live CF objects for the call.
    Cf(unsafe { CFDictionaryCreate(ptr::null(), keys.as_ptr(), vals.as_ptr(), pairs.len() as isize, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks) })
}

/// A format description for a stream (already checked by `vroom_video`).
fn format(desc: Desc) -> Option<Cf> {
    let mut fd: CMFormatDescriptionRef = ptr::null();
    // SAFETY: pointers and sizes describe live data for each call; fd is
    // written by CoreMedia (owned reference) on success.
    let st = unsafe {
        match desc {
            Desc::Nal(codec, sets, _) => {
                if sets.is_empty() || sets.len() > 64 {
                    return None;
                }
                let ptrs: Vec<*const u8> = sets.iter().map(|s| s.as_ptr()).collect();
                let sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
                if codec == Codec::H264 {
                    CMVideoFormatDescriptionCreateFromH264ParameterSets(ptr::null(), sets.len(), ptrs.as_ptr(), sizes.as_ptr(), 4, &mut fd)
                } else {
                    CMVideoFormatDescriptionCreateFromHEVCParameterSets(ptr::null(), sets.len(), ptrs.as_ptr(), sizes.as_ptr(), 4, ptr::null(), &mut fd)
                }
            }
            Desc::Vp9(c) | Desc::Av1(c, _) => {
                let (codec, name, record) = match desc {
                    Desc::Av1(_, seq) => (Codec::Av1, "av1C", vpx::av1c(c, seq)),
                    _ => (Codec::Vp9, "vpcC", vpx::vpcc(c)),
                };
                let data = Cf(CFDataCreate(ptr::null(), record.as_ptr(), record.len() as isize));
                let key = cfstr(name);
                let atoms = dict(&[(key.0, data.0)]);
                let ext = dict(&[(kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, atoms.0)]);
                CMVideoFormatDescriptionCreate(ptr::null(), codec_type(codec), c.width as i32, c.height as i32, ext.0, &mut fd)
            }
        }
    };
    if st != 0 || fd.is_null() {
        log::info!("video: stream format refused ({st})");
        return None;
    }
    Some(Cf(fd))
}

unsafe extern "C" fn on_output(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    flags: u32,
    image: CVImageBufferRef,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: refcon is the session's Sink, alive until the session is
    // invalidated (Session::drop waits for every frame first).
    let sink = unsafe { &*(refcon as *const Sink) };
    let image = if status == 0 && !image.is_null() && flags & FRAME_DROPPED == 0 {
        // SAFETY: a pixel buffer VideoToolbox hands us for this call; we
        // keep a reference of our own.
        Some(Image(unsafe { CFRetain(image) }))
    } else {
        None
    };
    if image.is_none() {
        log::debug!("video: a picture failed to decode ({status}, flags {flags:#x})");
    }
    sink.out.lock().unwrap_or_else(|e| e.into_inner()).push_back(Decoded { tag: frame_refcon as u64, image });
}

impl Session {
    /// A session for this stream, or None if VideoToolbox won't take it.
    pub fn new(desc: Desc) -> Option<Session> {
        if let Desc::Vp9(_) = desc {
            // SAFETY: a plain call with a codec type.
            unsafe { VTRegisterSupplementalVideoDecoderIfAvailable(codec_type(Codec::Vp9)) };
        }
        let fd = format(desc)?;
        let depth = match desc {
            Desc::Vp9(c) | Desc::Av1(c, _) => c.depth as u32,
            Desc::Nal(_, _, d) => d,
        };
        if depth > 10 {
            return None;
        }
        // SAFETY: fd is a valid format description.
        let dim = unsafe { CMVideoFormatDescriptionGetDimensions(fd.0) };
        let (w, h) = (u32::try_from(dim.width).ok()?, u32::try_from(dim.height).ok()?);
        let layout = Layout::for_picture(w, h)?;
        let sink = Box::new(Sink { out: Mutex::new(VecDeque::new()) });
        let pixfmt = if depth > 8 { X420 } else { NV12 } as i32;
        // SAFETY: CF objects created and released here; the callback record
        // points at our boxed sink, which outlives the session.
        unsafe {
            let num = Cf(CFNumberCreate(ptr::null(), 3, &pixfmt as *const i32 as *const c_void));
            let attrs = dict(&[(kCVPixelBufferPixelFormatTypeKey, num.0)]);
            let rec = OutputCallbackRecord { callback: on_output, refcon: &*sink as *const Sink as *mut c_void };
            let mut vt: VTDecompressionSessionRef = ptr::null();
            let st = VTDecompressionSessionCreate(ptr::null(), fd.0, ptr::null(), attrs.0, &rec, &mut vt);
            if st != 0 || vt.is_null() {
                log::info!("video: no decompression session ({st})");
                return None;
            }
            Some(Session { vt, fd, sink, layout, visible: (w, h), depth })
        }
    }

    /// Go on with an H.264 / HEVC stream using these parameter sets, if
    /// the session can: later samples carry them (a PPS whose content
    /// changed must reach the decoder). False: it needs a new session.
    pub fn accept(&mut self, codec: Codec, sets: &[&[u8]]) -> Option<bool> {
        let fd = format(Desc::Nal(codec, sets, 8))?;
        // SAFETY: both valid.
        let dim = unsafe { CMVideoFormatDescriptionGetDimensions(fd.0) };
        if (dim.width as u32, dim.height as u32) != self.visible {
            return Some(false);
        }
        // SAFETY: both valid.
        let ok = unsafe { VTDecompressionSessionCanAcceptFormatDescription(self.vt, fd.0) } != 0;
        if ok {
            self.fd = fd;
        }
        Some(ok)
    }

    /// Decode one sample (H.264 / HEVC: length-prefixed NAL units of one
    /// access unit; VP9: a frame; AV1: a temporal unit). `tag` comes back
    /// with its picture; `reorder`: VideoToolbox may hold pictures back
    /// for display order. False: VideoToolbox refused it outright.
    pub fn decode(&mut self, sample: &[u8], tag: u64, order: i64, reorder: bool) -> bool {
        if sample.is_empty() {
            return false;
        }
        // SAFETY: the block buffer gets its own copy of `sample`; the
        // sample buffer and block buffer are released before returning.
        unsafe {
            let mut bb: CMBlockBufferRef = ptr::null();
            if CMBlockBufferCreateWithMemoryBlock(ptr::null(), ptr::null_mut(), sample.len(), ptr::null(), ptr::null(), 0, sample.len(), 1, &mut bb) != 0 || bb.is_null() {
                return false;
            }
            let bb = Cf(bb);
            if CMBlockBufferReplaceDataBytes(sample.as_ptr() as *const c_void, bb.0, 0, sample.len()) != 0 {
                return false;
            }
            let t = CMTime { value: order, timescale: 1000, flags: CM_TIME_VALID, epoch: 0 };
            let timing = CMSampleTimingInfo { duration: CM_TIME_INVALID, pts: t, dts: CM_TIME_INVALID };
            let size = sample.len();
            let mut sb: CMSampleBufferRef = ptr::null();
            if CMSampleBufferCreateReady(ptr::null(), bb.0, self.fd.0, 1, 1, &timing, 1, &size, &mut sb) != 0 || sb.is_null() {
                return false;
            }
            let sb = Cf(sb);
            let mut info = 0u32;
            let flags = if reorder { TEMPORAL_PROCESSING } else { 0 };
            let st = VTDecompressionSessionDecodeFrame(self.vt, sb.0, flags, tag as *mut c_void, &mut info);
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

    /// Pictures done so far, in VideoToolbox's output order.
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
