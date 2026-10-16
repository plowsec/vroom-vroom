//! USB video class camera (UVC 1.0): what Windows and Linux take as a
//! webcam with their own drivers (usbvideo.sys, uvcvideo).
//!
//! Uncompressed YUY2 at a few sizes, streamed over a bulk endpoint: each
//! frame is one payload (a 2-byte header, then the pixels), handed out in
//! pieces as big as the transfers the guest queues. Frames come from a
//! [`CameraFeed`] the VMM fills from the Mac's camera; the camera runs
//! only while the guest reads frames.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::hid::UsbIds;
use super::{ControlResult, Descriptors, Setup, Stall, StdState, UsbDevice};

/// Sizes offered, the default first.
pub const SIZES: [(u16, u16); 3] = [(640, 480), (1280, 720), (320, 240)];
/// Frame intervals offered, in 100 ns units: 30 and 15 frames a second.
const INTERVALS: [u32; 2] = [333_333, 666_666];
const BULK_IN: u8 = 0x81;
/// The guest stopped reading frames this long ago: camera off.
pub const IDLE: Duration = Duration::from_secs(2);

/// UVC request codes and selectors.
const SET_CUR: u8 = 0x01;
const GET_CUR: u8 = 0x81;
const GET_MIN: u8 = 0x82;
const GET_MAX: u8 = 0x83;
const GET_RES: u8 = 0x84;
const GET_LEN: u8 = 0x85;
const GET_INFO: u8 = 0x86;
const GET_DEF: u8 = 0x87;
const VS_PROBE: u8 = 1;
const VS_COMMIT: u8 = 2;
/// Probe/commit block size in UVC 1.0.
const PROBE_LEN: usize = 26;

/// Frames from the host camera, and what the guest wants of it. Shared
/// by the device (in the USB controller) and whoever runs the camera.
#[derive(Debug, Default)]
pub struct CameraFeed {
    state: Mutex<FeedState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct FeedState {
    /// Size and frames per second the guest streams at, and when it last
    /// asked for a frame.
    want: Option<(u16, u16, u32, Instant)>,
    /// Latest frame (YUY2, `w * h * 2` bytes) and its number.
    frame: Option<(u64, u16, u16, Arc<Vec<u8>>)>,
    seq: u64,
}

impl CameraFeed {
    /// The guest reads frames at this size and rate (called on each
    /// request for one, so it doubles as "still wanted").
    fn demand(&self, w: u16, h: u16, fps: u32) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let new = !s.want.is_some_and(|(a, b, c, _)| (a, b, c) == (w, h, fps));
        s.want = Some((w, h, fps, Instant::now()));
        if new {
            s.frame = None;
            self.changed.notify_all();
        }
    }

    fn stop(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.want.take().is_some() {
            s.frame = None;
            self.changed.notify_all();
        }
    }

    /// What the camera should do now: Some((w, h, fps)) to run. A guest
    /// that stopped asking for frames a while ago counts as stopped.
    pub fn wanted(&self) -> Option<(u16, u16, u32)> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.want.is_some_and(|w| w.3.elapsed() > IDLE) {
            s.want = None;
            s.frame = None;
        }
        s.want.map(|(w, h, f, _)| (w, h, f))
    }

    /// Wait up to `t` for what's wanted to change; returns it.
    pub fn wait(&self, t: Duration) -> Option<(u16, u16, u32)> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let before = s.want.map(|(w, h, f, _)| (w, h, f));
        let _ = self.changed.wait_timeout_while(s, t, |s| s.want.map(|(w, h, f, _)| (w, h, f)) == before);
        self.wanted()
    }

    /// A new frame from the camera. False if nobody wants that size now
    /// (dropped).
    pub fn push(&self, w: u16, h: u16, yuy2: Vec<u8>) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let ok = s.want.is_some_and(|(a, b, _, _)| (a, b) == (w, h)) && yuy2.len() == w as usize * h as usize * 2;
        if ok {
            s.seq += 1;
            s.frame = Some((s.seq, w, h, Arc::new(yuy2)));
        }
        ok
    }

    fn take_newer(&self, seen: u64, w: u16, h: u16) -> Option<(u64, Arc<Vec<u8>>)> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &s.frame {
            Some((n, fw, fh, f)) if *n > seen && (*fw, *fh) == (w, h) => Some((*n, f.clone())),
            _ => None,
        }
    }
}

/// What the guest set with PROBE / COMMIT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Params {
    /// 1-based, into SIZES.
    frame: u8,
    interval: u32,
}

impl Params {
    const DEFAULT: Params = Params { frame: 1, interval: INTERVALS[0] };

    fn size(&self) -> (u16, u16) {
        SIZES[(self.frame as usize).clamp(1, SIZES.len()) - 1]
    }

    fn frame_bytes(&self) -> u32 {
        let (w, h) = self.size();
        w as u32 * h as u32 * 2
    }

    /// The nearest thing we offer to what the guest asked.
    fn fix(raw: &[u8]) -> Params {
        let frame = raw.get(3).copied().filter(|f| (1..=SIZES.len() as u8).contains(f)).unwrap_or(1);
        let asked = raw.get(4..8).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0);
        let interval = *INTERVALS.iter().min_by_key(|&&i| i.abs_diff(asked)).unwrap_or(&INTERVALS[0]);
        Params { frame, interval }
    }

    fn encode(&self) -> Vec<u8> {
        let mut v = vec![0u8; PROBE_LEN];
        v[0..2].copy_from_slice(&1u16.to_le_bytes()); // bmHint: frame interval fixed
        v[2] = 1; // bFormatIndex
        v[3] = self.frame;
        v[4..8].copy_from_slice(&self.interval.to_le_bytes());
        v[18..22].copy_from_slice(&self.frame_bytes().to_le_bytes()); // dwMaxVideoFrameSize
        // dwMaxPayloadTransferSize: a whole frame and its header.
        v[22..26].copy_from_slice(&(self.frame_bytes() + 2).to_le_bytes());
        v
    }
}

pub struct Camera {
    desc: Descriptors,
    std: StdState,
    probe: Params,
    commit: Option<Params>,
    feed: Arc<CameraFeed>,
    /// The frame being sent: header and pixels, and how far we are.
    payload: Vec<u8>,
    sent: usize,
    fid: bool,
    seen: u64,
}

impl std::fmt::Debug for Camera {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Camera").field("commit", &self.commit).finish()
    }
}

fn le16(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

fn le32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

fn descriptors(ids: &UsbIds) -> Descriptors {
    // Device: USB 2.0, "miscellaneous / IAD" class, as cameras are.
    let mut device = vec![18, 1, 0x00, 0x02, 0xef, 0x02, 0x01, 64];
    device.extend_from_slice(&ids.vendor_id.to_le_bytes());
    device.extend_from_slice(&le16(0x0003));
    device.extend_from_slice(&[0x00, 0x01, 1, 2, 3, 1]);

    // VideoControl: camera input terminal -> streaming output terminal.
    let mut vc = vec![13, 0x24, 0x01];
    vc.extend_from_slice(&le16(0x0100)); // bcdUVC 1.0
    let vc_total_at = vc.len();
    vc.extend_from_slice(&le16(0));
    vc.extend_from_slice(&le32(48_000_000));
    vc.extend_from_slice(&[1, 1]); // one streaming interface: 1
    vc.extend_from_slice(&[18, 0x24, 0x02, 1]);
    vc.extend_from_slice(&le16(0x0201)); // ITT_CAMERA
    vc.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0]);
    vc.extend_from_slice(&[9, 0x24, 0x03, 2]);
    vc.extend_from_slice(&le16(0x0101)); // TT_STREAMING
    vc.extend_from_slice(&[0, 1, 0]);
    let n = vc.len() as u16;
    vc[vc_total_at..vc_total_at + 2].copy_from_slice(&le16(n));

    // VideoStreaming: one format (YUY2), its frames, colors.
    let mut fmt = vec![27, 0x24, 0x04, 1, SIZES.len() as u8];
    // GUID YUY2: 32595559-0000-0010-8000-00AA00389B71
    fmt.extend_from_slice(&[0x59, 0x55, 0x59, 0x32, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);
    fmt.extend_from_slice(&[16, 1, 0, 0, 0, 0]);
    for (i, &(w, h)) in SIZES.iter().enumerate() {
        let bytes = w as u32 * h as u32 * 2;
        fmt.extend_from_slice(&[26 + 4 * INTERVALS.len() as u8, 0x24, 0x05, i as u8 + 1, 0]);
        fmt.extend_from_slice(&le16(w));
        fmt.extend_from_slice(&le16(h));
        fmt.extend_from_slice(&le32(bytes * 8 * 15)); // min bit rate: 15 fps
        fmt.extend_from_slice(&le32(bytes * 8 * 30)); // max: 30 fps
        fmt.extend_from_slice(&le32(bytes));
        fmt.extend_from_slice(&le32(INTERVALS[0]));
        fmt.push(INTERVALS.len() as u8);
        for iv in INTERVALS {
            fmt.extend_from_slice(&le32(iv));
        }
    }
    fmt.extend_from_slice(&[6, 0x24, 0x0d, 1, 1, 4]); // BT.709, BT.709, SMPTE 170M
    let mut vs = vec![14, 0x24, 0x01, 1];
    vs.extend_from_slice(&le16((14 + fmt.len()) as u16));
    vs.extend_from_slice(&[BULK_IN, 0, 2, 0, 0, 0, 1, 0]);
    vs.extend_from_slice(&fmt);

    let mut body = Vec::new();
    // Interface association: interfaces 0 and 1 are one video function.
    body.extend_from_slice(&[8, 0x0b, 0, 2, 0x0e, 0x03, 0, 2]);
    body.extend_from_slice(&[9, 4, 0, 0, 0, 0x0e, 0x01, 0, 2]);
    body.extend_from_slice(&vc);
    body.extend_from_slice(&[9, 4, 1, 0, 1, 0x0e, 0x02, 0, 0]);
    body.extend_from_slice(&vs);
    body.extend_from_slice(&[7, 5, BULK_IN, 0x02]);
    body.extend_from_slice(&le16(512));
    body.push(0);
    let mut config = vec![9, 2];
    config.extend_from_slice(&le16(9 + body.len() as u16));
    config.extend_from_slice(&[2, 1, 0, 0x80, 250]);
    config.extend_from_slice(&body);
    Descriptors { device, config, strings: vec![ids.manufacturer.clone(), "USB Camera".into(), "1".into()], hid_report: Vec::new() }
}

impl Camera {
    pub fn new(ids: &UsbIds, feed: Arc<CameraFeed>) -> Camera {
        Camera {
            desc: descriptors(ids),
            std: StdState::default(),
            probe: Params::DEFAULT,
            commit: None,
            feed,
            payload: Vec::new(),
            sent: 0,
            fid: false,
            seen: 0,
        }
    }

    fn stop(&mut self) {
        self.commit = None;
        self.payload.clear();
        self.sent = 0;
        self.feed.stop();
    }

    /// Video class requests to the streaming interface (1).
    fn streaming(&mut self, s: Setup, out: &[u8]) -> ControlResult {
        let sel = (s.value >> 8) as u8;
        if sel != VS_PROBE && sel != VS_COMMIT {
            return Err(Stall);
        }
        let cur = if sel == VS_COMMIT { self.commit.unwrap_or(self.probe) } else { self.probe };
        match s.request {
            SET_CUR => {
                let p = Params::fix(out);
                if sel == VS_PROBE {
                    self.probe = p;
                } else {
                    log::info!("usb camera: streaming {}x{} at {} fps", p.size().0, p.size().1, 10_000_000 / p.interval);
                    self.commit = Some(p);
                    self.payload.clear();
                    self.sent = 0;
                }
                Ok(vec![])
            }
            GET_CUR => Ok(cur.encode()),
            GET_MIN | GET_DEF => Ok(Params::DEFAULT.encode()),
            GET_MAX => Ok(Params { frame: SIZES.len() as u8, interval: INTERVALS[INTERVALS.len() - 1] }.encode()),
            GET_RES => Ok(vec![0; PROBE_LEN]),
            GET_LEN => Ok(le16(PROBE_LEN as u16).to_vec()),
            GET_INFO => Ok(vec![0x03]),
            _ => Err(Stall),
        }
    }
}

impl UsbDevice for Camera {
    fn speed(&self) -> u8 {
        super::SPEED_HIGH
    }

    fn control(&mut self, s: Setup, out: &[u8]) -> ControlResult {
        let class = (s.request_type >> 5) & 3 == 1;
        let to_interface = s.request_type & 0x1f == 1;
        if class && to_interface {
            return match s.index & 0xff {
                1 => self.streaming(s, out).map(|mut v| {
                    v.truncate(s.length as usize);
                    v
                }),
                // No controls on the camera itself.
                _ => Err(Stall),
            };
        }
        // The host stops a bulk stream by clearing the endpoint's halt.
        if s.request_type == 0x02 && s.request == 1 && s.index & 0xff == BULK_IN as u16 {
            self.stop();
            return Ok(vec![]);
        }
        if !class {
            if let Some(r) = self.desc.handle(&mut self.std, s, out) {
                return r;
            }
        }
        Err(Stall)
    }

    fn poll_in(&mut self, ep: u8) -> Option<Vec<u8>> {
        self.poll_in_max(ep, 16 << 10)
    }

    fn poll_in_max(&mut self, ep: u8, max: usize) -> Option<Vec<u8>> {
        let p = self.commit.filter(|_| ep == BULK_IN & 0x0f && max > 0)?;
        let (w, h) = p.size();
        self.feed.demand(w, h, 10_000_000 / p.interval);
        if self.sent >= self.payload.len() {
            let (n, f) = self.feed.take_newer(self.seen, w, h)?;
            self.seen = n;
            self.fid = !self.fid;
            // Header: length 2, end of header, end of frame, frame id.
            self.payload.clear();
            self.payload.extend_from_slice(&[2, 0x80 | 0x02 | self.fid as u8]);
            self.payload.extend_from_slice(&f);
            self.sent = 0;
        }
        let end = (self.sent + max).min(self.payload.len());
        let chunk = self.payload[self.sent..end].to_vec();
        self.sent = end;
        Some(chunk)
    }

    fn reset(&mut self) {
        self.std = StdState::default();
        self.probe = Params::DEFAULT;
        self.stop();
    }

    // Streaming isn't saved: after a restore the guest starts it again.
    fn save(&self) -> Vec<u8> {
        Vec::new()
    }

    fn restore(&mut self, _data: &[u8]) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(rt: u8, req: u8, value: u16, index: u16, length: u16) -> Setup {
        Setup { request_type: rt, request: req, value, index, length }
    }

    /// Walk the configuration descriptor: every descriptor's length must
    /// add up, and the class-specific totals must be right.
    #[test]
    fn descriptors_are_well_formed() {
        let d = descriptors(&UsbIds::default());
        assert_eq!(d.device.len(), 18);
        let c = &d.config;
        assert_eq!(u16::from_le_bytes([c[2], c[3]]) as usize, c.len());
        let mut i = 0;
        let mut kinds = Vec::new();
        while i < c.len() {
            let n = c[i] as usize;
            assert!(n >= 2 && i + n <= c.len(), "descriptor at {i}");
            kinds.push((c[i + 1], c.get(i + 2).copied().unwrap_or(0)));
            if c[i + 1] == 0x24 && c[i + 2] == 0x01 && kinds.iter().filter(|k| **k == (0x24, 0x01)).count() == 1 {
                // VC header: total of the VC class descriptors.
                let total = u16::from_le_bytes([c[i + 5], c[i + 6]]) as usize;
                assert_eq!(total, 13 + 18 + 9);
            }
            i += n;
        }
        assert!(kinds.contains(&(0x0b, 0)), "interface association");
        assert_eq!(kinds.iter().filter(|k| k.0 == 0x24 && k.1 == 0x05).count(), SIZES.len());
        assert_eq!(kinds.iter().filter(|k| k.0 == 5).count(), 1, "one endpoint");
    }

    #[test]
    fn probe_commit_and_frames() {
        let feed = Arc::new(CameraFeed::default());
        let mut cam = Camera::new(&UsbIds::default(), feed.clone());
        // Nothing streams before a commit.
        assert_eq!(cam.poll_in_max(1, 1024), None);
        assert_eq!(feed.wanted(), None);
        let mut probe = Params::DEFAULT.encode();
        probe[3] = 3; // 320x240
        probe[4..8].copy_from_slice(&700_000u32.to_le_bytes());
        assert_eq!(cam.control(setup(0x21, SET_CUR, 0x0100, 1, 26), &probe), Ok(vec![]));
        let got = cam.control(setup(0xa1, GET_CUR, 0x0100, 1, 26), &[]).unwrap();
        assert_eq!((got[3], u32::from_le_bytes(got[4..8].try_into().unwrap())), (3, 666_666));
        assert_eq!(u32::from_le_bytes(got[22..26].try_into().unwrap()), 320 * 240 * 2 + 2);
        cam.control(setup(0x21, SET_CUR, 0x0200, 1, 26), &got).unwrap();
        // The guest asks: the camera is wanted, no frame yet.
        assert_eq!(cam.poll_in_max(1, 65536), None);
        assert_eq!(feed.wanted(), Some((320, 240, 15)));
        assert!(!feed.push(640, 480, vec![0; 640 * 480 * 2]), "wrong size");
        assert!(feed.push(320, 240, vec![7; 320 * 240 * 2]));
        // One frame in pieces: header first, the rest after.
        let mut frame = Vec::new();
        while let Some(c) = cam.poll_in_max(1, 65536) {
            frame.extend_from_slice(&c);
            if frame.len() >= 320 * 240 * 2 + 2 {
                break;
            }
        }
        assert_eq!(frame.len(), 320 * 240 * 2 + 2);
        assert_eq!(frame[..2], [2, 0x83]);
        assert!(frame[2..].iter().all(|&b| b == 7));
        // No new frame: nothing; the next one flips the frame id.
        assert_eq!(cam.poll_in_max(1, 65536), None);
        feed.push(320, 240, vec![1; 320 * 240 * 2]);
        assert_eq!(cam.poll_in_max(1, 4).unwrap()[..2], [2, 0x82]);
        // Clearing the endpoint's halt stops it.
        cam.control(setup(0x02, 1, 0, 0x81, 0), &[]).unwrap();
        assert_eq!(feed.wanted(), None);
        assert_eq!(cam.poll_in_max(1, 65536), None);
    }
}
