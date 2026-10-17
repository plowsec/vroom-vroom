//! VideoToolbox encoding for the video helper: one compression session
//! per encoder, raw frames copied from the guest's buffer into one of
//! the session's pixel buffers, coded frames turned into Annex B (with
//! the parameter sets in front of each key frame) as they come out.
//!
//! What reaches VideoToolbox is pixels and settings `EncConfig::valid`
//! accepted; what comes back is VideoToolbox's, parsed by
//! `vroom_video::nal::annexb` all the same.
#![allow(unsafe_code)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::Mutex;

use vroom_video::proto::{EncConfig, RateControl, ENC_CABAC, ENC_REALTIME, H264_BASELINE, H264_CONSTRAINED_BASELINE, H264_MAIN, HEVC_MAIN10};
use vroom_video::{nal, Codec, PixFmt, MAX_BITSTREAM};

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFArrayRef = *const c_void;
type CFBooleanRef = *const c_void;
type CMSampleBufferRef = *const c_void;
type CMBlockBufferRef = *const c_void;
type CMFormatDescriptionRef = *const c_void;
type CVPixelBufferRef = *const c_void;
type CVPixelBufferPoolRef = *const c_void;
type VTCompressionSessionRef = *const c_void;
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

type OutputCallback = unsafe extern "C" fn(refcon: *mut c_void, frame_refcon: *mut c_void, status: OSStatus, flags: u32, sample: CMSampleBufferRef);

#[repr(C)]
struct DictCallbacks {
    _private: [u8; 0],
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeDictionaryKeyCallBacks: DictCallbacks;
    static kCFTypeDictionaryValueCallBacks: DictCallbacks;
    static kCFTypeArrayCallBacks: DictCallbacks;
    static kCFBooleanTrue: CFBooleanRef;
    static kCFBooleanFalse: CFBooleanRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        n: isize,
        kcb: *const DictCallbacks,
        vcb: *const DictCallbacks,
    ) -> CFDictionaryRef;
    fn CFDictionaryGetValue(d: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFArrayCreate(alloc: *const c_void, values: *const *const c_void, n: isize, cb: *const DictCallbacks) -> CFArrayRef;
    fn CFArrayGetCount(a: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CFArrayRef, i: isize) -> *const c_void;
    fn CFNumberCreate(alloc: *const c_void, kind: isize, value: *const c_void) -> CFTypeRef;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferWidthKey: CFStringRef;
    static kCVPixelBufferHeightKey: CFStringRef;
    static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;
    fn CVPixelBufferPoolCreatePixelBuffer(alloc: *const c_void, pool: CVPixelBufferPoolRef, out: *mut CVPixelBufferRef) -> i32;
    fn CVPixelBufferLockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pb: CVPixelBufferRef, flags: u64) -> i32;
    fn CVPixelBufferGetPlaneCount(pb: CVPixelBufferRef) -> usize;
    fn CVPixelBufferGetWidthOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetBytesPerRowOfPlane(pb: CVPixelBufferRef, plane: usize) -> usize;
    fn CVPixelBufferGetBaseAddressOfPlane(pb: CVPixelBufferRef, plane: usize) -> *const u8;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFStringRef;
    fn CMSampleBufferGetDataBuffer(sb: CMSampleBufferRef) -> CMBlockBufferRef;
    fn CMSampleBufferGetFormatDescription(sb: CMSampleBufferRef) -> CMFormatDescriptionRef;
    fn CMSampleBufferGetSampleAttachmentsArray(sb: CMSampleBufferRef, create: u8) -> CFArrayRef;
    fn CMBlockBufferGetDataLength(bb: CMBlockBufferRef) -> usize;
    fn CMBlockBufferCopyDataBytes(bb: CMBlockBufferRef, offset: usize, len: usize, dst: *mut c_void) -> OSStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        fd: CMFormatDescriptionRef,
        index: usize,
        ptr: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_len: *mut i32,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        fd: CMFormatDescriptionRef,
        index: usize,
        ptr: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_len: *mut i32,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFStringRef;
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFStringRef;
    static kVTCompressionPropertyKey_RealTime: CFStringRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    static kVTCompressionPropertyKey_ConstantBitRate: CFStringRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    static kVTCompressionPropertyKey_Quality: CFStringRef;
    static kVTCompressionPropertyKey_H264EntropyMode: CFStringRef;
    static kVTH264EntropyMode_CAVLC: CFStringRef;
    static kVTH264EntropyMode_CABAC: CFStringRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
    static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
    static kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel: CFStringRef;
    static kVTProfileLevel_H264_Main_AutoLevel: CFStringRef;
    static kVTProfileLevel_H264_High_AutoLevel: CFStringRef;
    static kVTProfileLevel_HEVC_Main_AutoLevel: CFStringRef;
    static kVTProfileLevel_HEVC_Main10_AutoLevel: CFStringRef;
    fn VTCompressionSessionCreate(
        alloc: *const c_void,
        width: i32,
        height: i32,
        codec: u32,
        encoder_spec: CFDictionaryRef,
        source_attrs: CFDictionaryRef,
        data_alloc: *const c_void,
        callback: OutputCallback,
        refcon: *mut c_void,
        out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    fn VTSessionSetProperty(s: VTCompressionSessionRef, key: CFStringRef, value: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(s: VTCompressionSessionRef) -> OSStatus;
    fn VTCompressionSessionGetPixelBufferPool(s: VTCompressionSessionRef) -> CVPixelBufferPoolRef;
    fn VTCompressionSessionEncodeFrame(
        s: VTCompressionSessionRef,
        image: CVPixelBufferRef,
        pts: CMTime,
        duration: CMTime,
        props: CFDictionaryRef,
        frame_refcon: *mut c_void,
        info: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(s: VTCompressionSessionRef, until: CMTime) -> OSStatus;
    fn VTCompressionSessionInvalidate(s: VTCompressionSessionRef);
}

/// '420v' and 'x420': NV12 and its 10-bit kin, video range.
const NV12: u32 = 0x3432_3076;
const X420: u32 = 0x7834_3230;
/// kVTEncodeInfo_FrameDropped.
const FRAME_DROPPED: u32 = 1 << 1;
const CF_NUMBER_SINT32: isize = 3;
const CF_NUMBER_SINT64: isize = 4;
const CF_NUMBER_FLOAT64: isize = 13;

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

fn codec_type(c: Codec) -> u32 {
    match c {
        Codec::Hevc => fourcc(b"hvc1"),
        _ => fourcc(b"avc1"),
    }
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

fn int(v: i64) -> Cf {
    // SAFETY: a plain number, read for the call.
    Cf(unsafe { CFNumberCreate(ptr::null(), CF_NUMBER_SINT64, &v as *const i64 as *const c_void) })
}

fn int32(v: i32) -> Cf {
    // SAFETY: as above.
    Cf(unsafe { CFNumberCreate(ptr::null(), CF_NUMBER_SINT32, &v as *const i32 as *const c_void) })
}

fn float(v: f64) -> Cf {
    // SAFETY: as above.
    Cf(unsafe { CFNumberCreate(ptr::null(), CF_NUMBER_FLOAT64, &v as *const f64 as *const c_void) })
}

/// A dictionary of `pairs` (each retained by it).
fn dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Cf {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
    let vals: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
    // SAFETY: keys and values are live CF objects for the call.
    Cf(unsafe { CFDictionaryCreate(ptr::null(), keys.as_ptr(), vals.as_ptr(), pairs.len() as isize, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks) })
}

/// A coded frame, in VideoToolbox's output order (the frames' order: no
/// reordering).
#[derive(Debug)]
pub struct Coded {
    /// The `tag` given to [`Encoder::encode`].
    pub tag: u64,
    /// Annex B; None: the frame failed or was dropped.
    pub data: Option<Vec<u8>>,
    pub key: bool,
}

/// Called (on VideoToolbox's thread) when a coded frame is ready.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// What the output callback writes to.
struct Sink {
    codec: Codec,
    out: Mutex<VecDeque<Coded>>,
    /// Wakes the helper's loop (best effort: it looks anyway when busy).
    wake: Option<Wake>,
}

/// The parameter sets of a coded frame's format, as NAL units.
///
/// # Safety
/// `fd` must be a valid video format description.
unsafe fn param_sets(codec: Codec, fd: CMFormatDescriptionRef) -> Option<(Vec<Vec<u8>>, usize)> {
    let mut sets = Vec::new();
    let mut count = 0usize;
    let mut len_size = 4i32;
    let mut i = 0usize;
    loop {
        let mut p: *const u8 = ptr::null();
        let mut n = 0usize;
        // SAFETY: fd is valid (the caller's); out pointers are ours.
        let st = unsafe {
            if codec == Codec::Hevc {
                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fd, i, &mut p, &mut n, &mut count, &mut len_size)
            } else {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(fd, i, &mut p, &mut n, &mut count, &mut len_size)
            }
        };
        if st != 0 || p.is_null() || n == 0 || n > nal::MAX_PARAM_SET {
            return None;
        }
        // SAFETY: CoreMedia's pointer to `n` bytes, owned by fd.
        sets.push(unsafe { std::slice::from_raw_parts(p, n) }.to_vec());
        i += 1;
        if i >= count || i >= 16 {
            break;
        }
    }
    Some((sets, usize::try_from(len_size).ok()?))
}

unsafe extern "C" fn on_output(refcon: *mut c_void, frame_refcon: *mut c_void, status: OSStatus, flags: u32, sample: CMSampleBufferRef) {
    // SAFETY: refcon is the session's Sink, alive until the session is
    // invalidated (Encoder::drop completes every frame first).
    let sink = unsafe { &*(refcon as *const Sink) };
    let tag = frame_refcon as u64;
    let mut coded = Coded { tag, data: None, key: false };
    if status == 0 && !sample.is_null() && flags & FRAME_DROPPED == 0 {
        // SAFETY: a sample buffer VideoToolbox hands us for this call;
        // what we read of it is copied out before returning.
        unsafe {
            let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
            let not_sync = !attachments.is_null()
                && CFArrayGetCount(attachments) > 0
                && CFDictionaryGetValue(CFArrayGetValueAtIndex(attachments, 0), kCMSampleAttachmentKey_NotSync) == kCFBooleanTrue;
            coded.key = !not_sync;
            let bb = CMSampleBufferGetDataBuffer(sample);
            let len = if bb.is_null() { 0 } else { CMBlockBufferGetDataLength(bb) };
            if len > 0 && len <= MAX_BITSTREAM as usize {
                let mut data = vec![0u8; len];
                if CMBlockBufferCopyDataBytes(bb, 0, len, data.as_mut_ptr() as *mut c_void) == 0 {
                    let fd = CMSampleBufferGetFormatDescription(sample);
                    let params = if fd.is_null() { None } else { param_sets(sink.codec, fd) };
                    if let Some((sets, len_size)) = params {
                        let first: Vec<&[u8]> = if coded.key { sets.iter().map(|s| s.as_slice()).collect() } else { Vec::new() };
                        coded.data = nal::annexb(&first, &data, len_size, MAX_BITSTREAM as usize);
                    }
                }
            }
        }
    }
    if coded.data.is_none() {
        log::debug!("video: a frame didn't encode ({status}, flags {flags:#x})");
    }
    sink.out.lock().unwrap_or_else(|e| e.into_inner()).push_back(coded);
    if let Some(w) = &sink.wake {
        w();
    }
}

/// The codecs this Mac encodes in hardware ([`Codec::bit`]s).
pub fn caps() -> u32 {
    let mut bits = 0;
    for c in [Codec::H264, Codec::Hevc] {
        if session(c, 640, 480, PixFmt::Nv12, false, None).is_some() {
            bits |= c.bit();
        }
    }
    bits
}

/// A hardware compression session, its sink boxed.
fn session(codec: Codec, w: u32, h: u32, format: PixFmt, low_latency: bool, wake: Option<Wake>) -> Option<(VTCompressionSessionRef, Box<Sink>)> {
    let sink = Box::new(Sink { codec, out: Mutex::new(VecDeque::new()), wake });
    let pixfmt = if format == PixFmt::P010 { X420 } else { NV12 } as i32;
    // SAFETY: CF objects made and released here; the callback's refcon
    // is our boxed sink, which the caller keeps as long as the session.
    unsafe {
        let mut spec: Vec<(CFTypeRef, CFTypeRef)> = vec![(kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, kCFBooleanTrue)];
        if low_latency {
            spec.push((kVTVideoEncoderSpecification_EnableLowLatencyRateControl, kCFBooleanTrue));
        }
        let spec = dict(&spec);
        let (pf, wn, hn) = (int32(pixfmt), int32(w as i32), int32(h as i32));
        let surface = dict(&[]);
        let attrs = dict(&[(kCVPixelBufferPixelFormatTypeKey, pf.0), (kCVPixelBufferWidthKey, wn.0), (kCVPixelBufferHeightKey, hn.0), (kCVPixelBufferIOSurfacePropertiesKey, surface.0)]);
        let mut s: VTCompressionSessionRef = ptr::null();
        let refcon = &*sink as *const Sink as *mut c_void;
        let st = VTCompressionSessionCreate(ptr::null(), w as i32, h as i32, codec_type(codec), spec.0, attrs.0, ptr::null(), on_output, refcon, &mut s);
        if st != 0 || s.is_null() {
            log::debug!("video: no compression session ({st}, {codec:?}, low latency {low_latency})");
            return None;
        }
        Some((s, sink))
    }
}

/// One VideoToolbox compression session.
pub struct Encoder {
    vt: VTCompressionSessionRef,
    sink: Box<Sink>,
    pub config: EncConfig,
    /// Presentation time of the next frame, in microseconds.
    pts: i64,
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoder").field("config", &self.config).finish()
    }
}

// SAFETY: VideoToolbox sessions may be used from any thread; we use each
// from one thread at a time (the helper's).
unsafe impl Send for Encoder {}

impl Encoder {
    /// A session for `config` (one `EncConfig::valid` accepted), or None.
    pub fn new(config: &EncConfig, wake: Option<Wake>) -> Option<Encoder> {
        let (w, h) = config.visible;
        // Calls: VideoToolbox's low-latency mode (the one FaceTime uses).
        // Some formats don't have it; then the ordinary one.
        let realtime = config.flags & ENC_REALTIME != 0;
        let (vt, sink) = match realtime {
            true => session(config.codec, w, h, config.format, true, wake.clone()).or_else(|| session(config.codec, w, h, config.format, false, wake))?,
            false => session(config.codec, w, h, config.format, false, wake)?,
        };
        let mut e = Encoder { vt, sink, config: *config, pts: 0 };
        // SAFETY: valid session; constant keys and values.
        unsafe {
            let profile = match (config.codec, config.profile) {
                (Codec::Hevc, HEVC_MAIN10) => kVTProfileLevel_HEVC_Main10_AutoLevel,
                (Codec::Hevc, _) => kVTProfileLevel_HEVC_Main_AutoLevel,
                (_, H264_BASELINE) => kVTProfileLevel_H264_Baseline_AutoLevel,
                (_, H264_CONSTRAINED_BASELINE) => kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
                (_, H264_MAIN) => kVTProfileLevel_H264_Main_AutoLevel,
                _ => kVTProfileLevel_H264_High_AutoLevel,
            };
            e.set("profile", kVTCompressionPropertyKey_ProfileLevel, profile);
            e.set("real time", kVTCompressionPropertyKey_RealTime, if realtime { kCFBooleanTrue } else { kCFBooleanFalse });
            // No B-frames: frames come out in the order they went in.
            e.set("reordering", kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
            if config.codec == Codec::H264 {
                let mode = if config.flags & ENC_CABAC != 0 { kVTH264EntropyMode_CABAC } else { kVTH264EntropyMode_CAVLC };
                e.set("entropy mode", kVTCompressionPropertyKey_H264EntropyMode, mode);
            }
        }
        e.rates();
        // SAFETY: valid session.
        unsafe { VTCompressionSessionPrepareToEncodeFrames(e.vt) };
        Some(e)
    }

    /// Set a property; one VideoToolbox doesn't take is logged and skipped.
    ///
    /// # Safety
    /// `value` must be a live CF object.
    unsafe fn set(&self, what: &str, key: CFStringRef, value: CFTypeRef) -> bool {
        // SAFETY: valid session, key and value (the caller's).
        let st = unsafe { VTSessionSetProperty(self.vt, key, value) };
        if st != 0 {
            log::debug!("video: encoder {what}: not taken ({st})");
        }
        st == 0
    }

    /// Bitrate, frame rate, quality, key frame interval: from the config.
    fn rates(&mut self) {
        let c = self.config;
        // SAFETY: valid session; numbers made here.
        unsafe {
            let fps = float(c.fps.0 as f64 / c.fps.1 as f64);
            self.set("frame rate", kVTCompressionPropertyKey_ExpectedFrameRate, fps.0);
            let gop = int(if c.gop == 0 { i32::MAX as i64 } else { c.gop as i64 });
            self.set("key frame interval", kVTCompressionPropertyKey_MaxKeyFrameInterval, gop.0);
            let rate = int(c.bitrate as i64);
            match c.rc {
                RateControl::Cbr => {
                    if !self.set("constant bitrate", kVTCompressionPropertyKey_ConstantBitRate, rate.0) {
                        self.set("bitrate", kVTCompressionPropertyKey_AverageBitRate, rate.0);
                        self.limit(c.bitrate as u64 * 11 / 10);
                    }
                }
                RateControl::Vbr => {
                    self.set("bitrate", kVTCompressionPropertyKey_AverageBitRate, rate.0);
                    if c.peak > 0 {
                        self.limit(c.peak as u64);
                    }
                }
                RateControl::Quality => {
                    let q = float(c.quality as f64 / 100.0);
                    self.set("quality", kVTCompressionPropertyKey_Quality, q.0);
                }
            }
        }
    }

    /// At most `bits` a second, over each second.
    fn limit(&self, bits: u64) {
        let (bytes, secs) = (int((bits / 8) as i64), float(1.0));
        let vals = [bytes.0, secs.0];
        // SAFETY: an array of two live numbers, retained by it.
        let arr = Cf(unsafe { CFArrayCreate(ptr::null(), vals.as_ptr(), 2, &kCFTypeArrayCallBacks) });
        // SAFETY: valid session and array.
        unsafe { self.set("data rate limits", kVTCompressionPropertyKey_DataRateLimits, arr.0) };
    }

    /// Go on with `config` if only rates changed. False: it needs a new
    /// session.
    pub fn update(&mut self, config: &EncConfig) -> bool {
        let same = |c: &EncConfig| (c.codec, c.format, c.layout, c.visible, c.profile, c.level, c.flags, c.rc);
        if same(config) != same(&self.config) {
            return false;
        }
        if *config != self.config {
            self.config = *config;
            self.rates();
        }
        true
    }

    /// Encode one raw frame laid out as the config says, from `src`
    /// (`len` bytes). `tag` comes back with its coded frame. False:
    /// refused (no pixel buffer, too short).
    ///
    /// # Safety
    /// `src..src + len` must be readable memory no Rust reference points
    /// into (the guest's buffer window; the guest may write it meanwhile,
    /// which spoils only that frame's pixels).
    pub unsafe fn encode(&mut self, src: *const u8, len: usize, tag: u64, key: bool) -> bool {
        let c = self.config;
        let fb = c.format.bytes() as usize;
        let stride = c.layout.stride(c.format) as usize;
        let (w, h) = (c.visible.0 as usize, c.visible.1 as usize);
        let chroma = stride * c.layout.height as usize;
        if len < c.layout.size(c.format) as usize {
            return false;
        }
        // SAFETY: the pool and buffer are VideoToolbox's (we own the
        // buffer we create); planes are written under the base address
        // lock within the sizes CoreVideo reports, read from `src` within
        // `len` (checked above: the layout's size).
        unsafe {
            let pool = VTCompressionSessionGetPixelBufferPool(self.vt);
            if pool.is_null() {
                return false;
            }
            let mut pb: CVPixelBufferRef = ptr::null();
            if CVPixelBufferPoolCreatePixelBuffer(ptr::null(), pool, &mut pb) != 0 || pb.is_null() {
                return false;
            }
            let pb = Cf(pb);
            if CVPixelBufferGetPlaneCount(pb.0) != 2 || CVPixelBufferLockBaseAddress(pb.0, 0) != 0 {
                return false;
            }
            for plane in 0..2usize {
                let base = CVPixelBufferGetBaseAddressOfPlane(pb.0, plane) as *mut u8;
                let dst_stride = CVPixelBufferGetBytesPerRowOfPlane(pb.0, plane);
                let rows = CVPixelBufferGetHeightOfPlane(pb.0, plane).min(if plane == 0 { h } else { h / 2 });
                // Bytes a row: luma samples, or Cb and Cr pairs.
                let row = (CVPixelBufferGetWidthOfPlane(pb.0, plane) * fb * (plane + 1)).min(w * fb).min(dst_stride);
                if base.is_null() {
                    continue;
                }
                let from = if plane == 0 { 0 } else { chroma };
                for y in 0..rows {
                    ptr::copy_nonoverlapping(src.add(from + y * stride), base.add(y * dst_stride), row);
                }
            }
            CVPixelBufferUnlockBaseAddress(pb.0, 0);
            let pts = CMTime { value: self.pts, timescale: 1_000_000, flags: CM_TIME_VALID, epoch: 0 };
            let step = 1_000_000 * c.fps.1 as i64 / c.fps.0 as i64;
            let duration = CMTime { value: step.max(1), timescale: 1_000_000, flags: CM_TIME_VALID, epoch: 0 };
            self.pts += step.max(1);
            let props = key.then(|| dict(&[(kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue)]));
            let mut info = 0u32;
            let st = VTCompressionSessionEncodeFrame(self.vt, pb.0, pts, duration, props.as_ref().map_or(ptr::null(), |p| p.0), tag as *mut c_void, &mut info);
            if st != 0 {
                log::debug!("video: encode refused ({st})");
            }
            st == 0
        }
    }

    /// Hand out every frame sent so far (waits for them).
    pub fn finish(&mut self) {
        // SAFETY: valid session.
        unsafe { VTCompressionSessionCompleteFrames(self.vt, CM_TIME_INVALID) };
    }

    /// Frames coded so far, in order.
    pub fn take(&mut self) -> Vec<Coded> {
        self.sink.out.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect()
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: valid session; after invalidation no callback runs, so
        // the sink may go.
        unsafe {
            VTCompressionSessionCompleteFrames(self.vt, CM_TIME_INVALID);
            VTCompressionSessionInvalidate(self.vt);
            CFRelease(self.vt);
        }
    }
}
