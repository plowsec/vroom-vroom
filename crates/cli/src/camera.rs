//! `vroom camera-helper`: the Mac's camera for a VM's USB camera.
//!
//! Started by the VMM in a sandbox of its own (the camera and nothing
//! else); stdin is a socket to the VMM. The VMM says when to run and at
//! what size; we send frames as YUY2 at exactly that size (the camera's
//! picture cropped to that shape, then scaled).
//!
//! VMM -> us: `[1, w u16, h u16, fps u32]` run, `[0, ...]` stop (9 bytes).
//! Us -> VMM: `[1, w u16, h u16, len u32, pixels]` a frame,
//! `[2, len u16, text]` news for the window ("on", "off", "denied"...).
//!
//! `VROOM_CAMERA_TEST=1`: moving color bars instead of the camera (tests,
//! and no camera permission needed).

use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A camera running until dropped.
trait Capture {}

/// Where frames and news go, and what the VMM wants now.
struct Link {
    out: Mutex<UnixStream>,
    want: Mutex<Option<(u16, u16, u32)>>,
    last: Mutex<Option<Instant>>,
}

impl Link {
    fn say(&self, text: &str) {
        let mut m = vec![2];
        m.extend_from_slice(&(text.len() as u16).to_le_bytes());
        m.extend_from_slice(text.as_bytes());
        let _ = self.out.lock().unwrap_or_else(|e| e.into_inner()).write_all(&m);
    }

    /// Send a frame if one is due at the wanted rate.
    fn frame(&self, w: u16, h: u16, px: &[u8]) {
        let Some((ww, wh, fps)) = *self.want.lock().unwrap_or_else(|e| e.into_inner()) else { return };
        if (ww, wh) != (w, h) {
            return;
        }
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let every = Duration::from_micros(1_000_000 / fps.max(1) as u64);
        // A little slack: cameras run at 30 fps, not exactly.
        if last.is_some_and(|l| l.elapsed() + every / 4 < every) {
            return;
        }
        *last = Some(Instant::now());
        drop(last);
        let mut m = Vec::with_capacity(9 + px.len());
        m.push(1);
        m.extend_from_slice(&w.to_le_bytes());
        m.extend_from_slice(&h.to_le_bytes());
        m.extend_from_slice(&(px.len() as u32).to_le_bytes());
        m.extend_from_slice(px);
        let _ = self.out.lock().unwrap_or_else(|e| e.into_inner()).write_all(&m);
    }

    fn wanted(&self) -> Option<(u16, u16, u32)> {
        *self.want.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// YUY2 `src` (`sw` x `sh`, `stride` bytes a row) cropped to the shape of
/// `tw` x `th` around its center, then scaled to it (nearest pixel).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn fit_yuy2(src: &[u8], sw: usize, sh: usize, stride: usize, tw: usize, th: usize) -> Vec<u8> {
    let mut out = vec![0u8; tw * th * 2];
    if sw < 2 || sh < 1 || tw < 2 || th < 1 || stride < sw * 2 || src.len() < stride * (sh - 1) + sw * 2 {
        return out;
    }
    // The biggest part of the source with the target's shape.
    let (cw, ch) = if sw * th > sh * tw { (sh * tw / th, sh) } else { (sw, sw * th / tw) };
    let (cx, cy) = ((sw - cw) / 2, (sh - ch) / 2);
    for y in 0..th {
        let row = &src[(cy + y * ch / th) * stride..];
        let o = &mut out[y * tw * 2..(y + 1) * tw * 2];
        for x in (0..tw & !1).step_by(2) {
            let (a, b) = (cx + x * cw / tw, cx + (x + 1) * cw / tw);
            let pair = (a / 2) * 4;
            o[x * 2] = row[a * 2];
            o[x * 2 + 1] = row[pair + 1];
            o[x * 2 + 2] = row[b.min(sw - 1) * 2];
            o[x * 2 + 3] = row[pair + 3];
        }
    }
    out
}

/// Moving color bars (YUY2), for tests.
fn test_pattern(w: u16, h: u16, t: u32) -> Vec<u8> {
    // (Y, U, V) of white, yellow, cyan, green, magenta, red, blue, black.
    const BARS: [(u8, u8, u8); 8] =
        [(235, 128, 128), (210, 16, 146), (170, 166, 16), (145, 54, 34), (106, 202, 222), (81, 90, 240), (41, 240, 110), (16, 128, 128)];
    let (w, h) = (w as usize, h as usize);
    let mut px = vec![0u8; w * h * 2];
    for y in 0..h {
        for x in (0..w).step_by(2) {
            let bar = ((x + t as usize * 4) * 8 / w) % 8;
            let (yy, u, v) = BARS[bar];
            let i = (y * w + x) * 2;
            px[i..i + 4].copy_from_slice(&[yy, u, yy, v]);
        }
    }
    px
}

pub fn helper() -> Result<(), String> {
    // Diagnostics: what the camera sandbox lets us see, without asking
    // for permission (no prompt).
    if std::env::var_os("VROOM_CAMERA_PROBE").is_some() {
        mac::probe();
        return Ok(());
    }
    // SAFETY: fd 0 is the socket the VMM set up for us; nothing else here
    // uses stdin.
    #[allow(unsafe_code)]
    let sock = unsafe { UnixStream::from_raw_fd(0) };
    let link = Arc::new(Link { out: Mutex::new(sock.try_clone().map_err(|e| e.to_string())?), want: Mutex::new(None), last: Mutex::new(None) });
    let test = std::env::var_os("VROOM_CAMERA_TEST").is_some_and(|v| v == "1");
    let mut cam: Option<Box<dyn Capture>> = None;
    let mut sock = sock;
    let mut cmd = [0u8; 9];
    loop {
        if sock.read_exact(&mut cmd).is_err() {
            return Ok(());
        }
        let (w, h, fps) = (u16::from_le_bytes([cmd[1], cmd[2]]), u16::from_le_bytes([cmd[3], cmd[4]]), u32::from_le_bytes([cmd[5], cmd[6], cmd[7], cmd[8]]));
        let run = cmd[0] == 1 && w >= 2 && h >= 2 && w <= 4096 && h <= 4096 && (1..=60).contains(&fps);
        *link.want.lock().unwrap_or_else(|e| e.into_inner()) = run.then_some((w, h, fps));
        if !run {
            if cam.take().is_some() {
                link.say("off");
            }
            continue;
        }
        if cam.is_some() {
            continue;
        }
        cam = if test { Some(Box::new(TestCamera::start(link.clone())) as Box<dyn Capture>) } else { mac::start(link.clone()) };
        if cam.is_some() {
            link.say("on");
        }
    }
}

/// Test mode: frames from a thread until dropped.
struct TestCamera(Arc<std::sync::atomic::AtomicBool>);

impl TestCamera {
    fn start(link: Arc<Link>) -> TestCamera {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = stop.clone();
        std::thread::spawn(move || {
            let mut t = 0;
            while !s.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some((w, h, fps)) = link.wanted() {
                    link.frame(w, h, &test_pattern(w, h, t));
                    t += 1;
                    std::thread::sleep(Duration::from_millis(1000 / fps.max(1) as u64));
                } else {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
        TestCamera(stop)
    }
}

impl Capture for TestCamera {}

impl Drop for TestCamera {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(target_os = "macos")]
mod mac {
    //! AVFoundation: the default camera, YUY2 ("yuvs") frames on a queue
    //! of their own.

    use std::sync::Arc;

    use dispatch2::DispatchQueue;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
    use objc2_av_foundation::{
        AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput, AVCaptureOutput, AVCaptureSession,
        AVCaptureSessionPreset1280x720, AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
    };
    use objc2_core_media::CMSampleBuffer;
    use objc2_core_video::{
        kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_422YpCbCr8_yuvs, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
        CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    use objc2_foundation::{NSDictionary, NSNumber, NSString};

    use super::{fit_yuy2, Link};

    pub struct Ivars {
        link: Arc<Link>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "VroomCameraSink"]
        #[ivars = Ivars]
        pub struct Sink;

        unsafe impl NSObjectProtocol for Sink {}

        unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for Sink {
            #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
            fn did_output(&self, _out: &AVCaptureOutput, sb: &CMSampleBuffer, _conn: &AVCaptureConnection) {
                frame(&self.ivars().link, sb);
            }
        }
    );

    #[allow(unsafe_code)]
    fn frame(link: &Link, sb: &CMSampleBuffer) {
        let Some((tw, th, _)) = link.wanted() else { return };
        // SAFETY: a sample buffer from the capture output, valid for this
        // call; its pixels are read under the base address lock only.
        unsafe {
            let Some(pb) = sb.image_buffer() else { return };
            if CVPixelBufferGetPixelFormatType(&pb) != kCVPixelFormatType_422YpCbCr8_yuvs {
                return;
            }
            if CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly) != 0 {
                return;
            }
            let (w, h, stride) = (CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb), CVPixelBufferGetBytesPerRow(&pb));
            let base = CVPixelBufferGetBaseAddress(&pb) as *const u8;
            if !base.is_null() && h > 0 {
                let src = std::slice::from_raw_parts(base, stride * (h - 1) + w * 2);
                let px = fit_yuy2(src, w, h, stride, tw as usize, th as usize);
                link.frame(tw, th, &px);
            }
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::ReadOnly);
        }
    }

    impl Sink {
        fn new(link: Arc<Link>) -> Retained<Sink> {
            let this = Sink::alloc().set_ivars(Ivars { link });
            // SAFETY: NSObject's init on a fresh allocation.
            #[allow(unsafe_code)]
            unsafe {
                msg_send![super(this), init]
            }
        }
    }

    /// A running capture: stops when dropped.
    pub struct Running {
        session: Retained<AVCaptureSession>,
        _sink: Retained<Sink>,
        _queue: dispatch2::DispatchRetained<DispatchQueue>,
    }

    impl super::Capture for Running {}

    impl Drop for Running {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: plain call on our session.
            unsafe { self.session.stopRunning() };
        }
    }

    /// May we use the camera? Asks the user the first time (macOS shows
    /// its prompt for the app vroom runs under).
    #[allow(unsafe_code)]
    fn allowed(link: &Link) -> bool {
        // SAFETY: AVFoundation class methods with a valid media type.
        unsafe {
            let Some(video) = AVMediaTypeVideo else { return false };
            match AVCaptureDevice::authorizationStatusForMediaType(video) {
                AVAuthorizationStatus::Authorized => true,
                AVAuthorizationStatus::NotDetermined => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let block = block2::RcBlock::new(move |ok: objc2::runtime::Bool| {
                        let _ = tx.send(ok.as_bool());
                    });
                    AVCaptureDevice::requestAccessForMediaType_completionHandler(video, &block);
                    let ok = rx.recv_timeout(std::time::Duration::from_secs(120)).unwrap_or(false);
                    if !ok {
                        link.say("denied");
                    }
                    ok
                }
                _ => {
                    link.say("denied");
                    false
                }
            }
        }
    }

    /// Print the permission state and the default camera.
    #[allow(unsafe_code)]
    pub fn probe() {
        // SAFETY: AVFoundation class methods with a valid media type.
        unsafe {
            let Some(video) = AVMediaTypeVideo else { return };
            eprintln!("camera permission: {:?}", AVCaptureDevice::authorizationStatusForMediaType(video));
            match AVCaptureDevice::defaultDeviceWithMediaType(video) {
                Some(d) => eprintln!("default camera: {}", d.localizedName()),
                None => eprintln!("default camera: none"),
            }
        }
    }

    #[allow(unsafe_code)]
    pub fn start(link: Arc<Link>) -> Option<Box<dyn super::Capture>> {
        if !allowed(&link) {
            return None;
        }
        // SAFETY: AVFoundation setup on objects we own; the delegate and
        // its queue live as long as the session (in Running).
        unsafe {
            let video = AVMediaTypeVideo?;
            let Some(device) = AVCaptureDevice::defaultDeviceWithMediaType(video) else {
                link.say("no camera");
                return None;
            };
            let input = match AVCaptureDeviceInput::deviceInputWithDevice_error(&device) {
                Ok(i) => i,
                Err(e) => {
                    log::warn!("camera: {}", e.localizedDescription());
                    link.say("no camera");
                    return None;
                }
            };
            let session = AVCaptureSession::new();
            session.beginConfiguration();
            if session.canSetSessionPreset(AVCaptureSessionPreset1280x720) {
                session.setSessionPreset(AVCaptureSessionPreset1280x720);
            }
            if !session.canAddInput(&input) {
                link.say("no camera");
                return None;
            }
            session.addInput(&input);
            let output = AVCaptureVideoDataOutput::new();
            let key: &NSString = &*(kCVPixelBufferPixelFormatTypeKey as *const _ as *const NSString);
            let fmt = NSNumber::new_u32(kCVPixelFormatType_422YpCbCr8_yuvs);
            let settings = NSDictionary::<NSString, AnyObject>::from_slices(&[key], &[&*fmt as &AnyObject]);
            output.setVideoSettings(Some(&settings));
            output.setAlwaysDiscardsLateVideoFrames(true);
            let sink = Sink::new(link.clone());
            let queue = DispatchQueue::new("org.vroom.camera", None);
            output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*sink)), Some(&queue));
            if !session.canAddOutput(&output) {
                link.say("no camera");
                return None;
            }
            session.addOutput(&output);
            session.commitConfiguration();
            session.startRunning();
            log::info!("camera: {} running", device.localizedName());
            Some(Box::new(Running { session, _sink: sink, _queue: queue }))
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod mac {
    use std::sync::Arc;

    pub fn start(link: Arc<super::Link>) -> Option<Box<dyn super::Capture>> {
        link.say("no camera");
        None
    }

    pub fn probe() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_to_the_shape_then_scales() {
        // 8x2 source: Y = column index, U = 100 + pair, V = 200 + pair.
        let (sw, sh) = (8, 2);
        let mut src = vec![0u8; sw * sh * 2];
        for y in 0..sh {
            for x in 0..sw {
                src[(y * sw + x) * 2] = x as u8;
            }
            for p in 0..sw / 2 {
                src[(y * sw + p * 2) * 2 + 1] = 100 + p as u8;
                src[(y * sw + p * 2) * 2 + 3] = 200 + p as u8;
            }
        }
        // 2x1 target (2:1): crops the 4:1 source to its middle 4 columns.
        let out = fit_yuy2(&src, sw, sh, sw * 2, 2, 1);
        assert_eq!(out, vec![2, 101, 4, 201]);
        // Same shape: a straight copy.
        assert_eq!(fit_yuy2(&src, sw, sh, sw * 2, sw, sh), src);
        // Too small a source: black, no panic.
        assert_eq!(fit_yuy2(&src[..4], sw, sh, sw * 2, 2, 1), vec![0; 4]);
    }

    #[test]
    fn test_pattern_is_yuy2_of_the_size() {
        assert_eq!(test_pattern(64, 4, 0).len(), 64 * 4 * 2);
    }
}
