//! virtio-media as a video encoder: the guest sees a V4L2 stateful
//! encoder (`/dev/videoN`, "Video encoder"): raw NV12 (or P010) frames in
//! on the OUTPUT queue, H.264 or HEVC out on CAPTURE, as the kernel's
//! stateful encoder interface describes. FFmpeg's `h264_v4l2m2m` and
//! GStreamer's `v4l2h264enc` use it as is; our VA-API driver puts
//! Chrome's and FFmpeg's VA-API encoders on top.
//!
//! The encoding happens in an encoder helper (`vroom video-helper
//! --encode`, VideoToolbox). Raw frames don't cross the socket: the
//! helper reads each one from the guest's buffer in the window (it is
//! guest data either way: pixels only) and writes the coded frame into a
//! picture buffer we granted it, as the decoder does the other way. The
//! guest gets its raw buffer back once the helper has read it.
//!
//! Every guest value is checked: sizes, formats, frame rates and control
//! values are kept within what [`EncConfig::valid`] accepts.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use vroom_video::proto::{
    EncConfig, FromHelper, RateControl, ToHelper, ENC_CABAC, ENC_REALTIME, H264_HIGH, H264_MAIN, HEVC_MAIN, HEVC_MAIN10, MAX_BITRATE, MAX_FPS,
};
use vroom_video::{Codec, Layout, PixFmt, MAX_BITSTREAM, MAX_SIDE, MIN_SIDE};

use super::v4l2::*;
use super::{read_payload, Answer, Buf, Dir, Handler, EBUSY, EINVAL, EIO, ENOMEM};
use crate::Reader;

const MAX_RAW_BUFS: u32 = 16;
const MAX_CODED_BUFS: u32 = 16;
/// Smallest coded buffer.
const MIN_CODED: u32 = 64 << 10;
/// Raw frames at the helper at once per session; the rest wait here.
const MAX_FRAMES_IN_FLIGHT: usize = 4;
/// Frames whose coded frame we wait for (timestamps kept for them).
const MAX_STAMPS: usize = 64;
const MAX_EXT_CTRLS: usize = 64;
const DEFAULT_BITRATE: u32 = 2_000_000;

/// An encoder's controls, as V4L2 names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Controls {
    pub(super) bitrate: u32,
    peak: u32,
    /// V4L2_MPEG_VIDEO_BITRATE_MODE: 0 VBR, 1 CBR, 2 constant quality.
    mode: u32,
    quality: u32,
    gop: u32,
    i_period: u32,
    h264_profile: u32,
    h264_level: u32,
    hevc_profile: u32,
    hevc_level: u32,
    /// 0 CAVLC, 1 CABAC.
    entropy: u32,
    frame_rc: u32,
}

impl Default for Controls {
    fn default() -> Self {
        Controls {
            bitrate: DEFAULT_BITRATE,
            peak: 0,
            mode: 0,
            quality: 50,
            gop: 0,
            i_period: 0,
            h264_profile: H264_HIGH as u32,
            h264_level: 11,
            hevc_profile: HEVC_MAIN as u32,
            hevc_level: 8,
            entropy: 1,
            frame_rc: 1,
        }
    }
}

#[derive(Debug)]
pub(super) struct EncSession {
    codec: Codec,
    raw: PixFmt,
    /// The raw frames' layout in their buffers, and the picture coded
    /// (the guest's size, or its crop).
    layout: Layout,
    pub(super) visible: (u32, u32),
    coded_size: u32,
    /// Frames a second: fps.0 / fps.1.
    pub(super) fps: (u32, u32),
    pub(super) c: Controls,
    pub(super) out_bufs: Vec<Buf>,
    pub(super) cap_bufs: Vec<Buf>,
    out_streaming: bool,
    cap_streaming: bool,
    /// The config the helper has; None: not open there.
    open: Option<EncConfig>,
    /// Raw frames queued, not sent yet: (index, key frame).
    pending: VecDeque<(u32, bool)>,
    /// Raw frames at the helper: cookie -> buffer index.
    pub(super) in_flight: BTreeMap<u32, u32>,
    /// Frames sent, their coded frame not back yet: cookie -> timestamp.
    stamps: VecDeque<(u32, [u8; 16])>,
    cap_queue: VecDeque<u32>,
    pub(super) epoch: u32,
    granted: u32,
    /// The next frame is a key frame.
    key_next: bool,
    draining: bool,
    drain_sent: bool,
    last_pending: bool,
    last_is_eos: bool,
    sub_eos: bool,
    sequence: u32,
    pub(super) dead: bool,
    /// Frames lost in a restore: each goes back as an empty coded buffer
    /// with the error flag, with its frame's time.
    failed: VecDeque<[u8; 16]>,
    stat_frames: u64,
    stat_coded: u64,
}

impl EncSession {
    fn new(codec: Codec) -> Self {
        let layout = Layout { width: 1280, height: 720 };
        EncSession {
            codec,
            raw: PixFmt::Nv12,
            layout,
            visible: (1280, 720),
            coded_size: default_coded(layout),
            fps: (30, 1),
            c: Controls::default(),
            out_bufs: Vec::new(),
            cap_bufs: Vec::new(),
            out_streaming: false,
            cap_streaming: false,
            open: None,
            pending: VecDeque::new(),
            in_flight: BTreeMap::new(),
            stamps: VecDeque::new(),
            cap_queue: VecDeque::new(),
            epoch: 0,
            granted: 0,
            key_next: true,
            draining: false,
            drain_sent: false,
            last_pending: false,
            last_is_eos: false,
            sub_eos: false,
            sequence: 0,
            dead: false,
            failed: VecDeque::new(),
            stat_frames: 0,
            stat_coded: 0,
        }
    }

    pub(super) fn bufs(&mut self, d: Dir) -> &mut Vec<Buf> {
        match d {
            Dir::Output => &mut self.out_bufs,
            Dir::Capture => &mut self.cap_bufs,
        }
    }

    /// What the helper is to make, from the formats and controls.
    fn config(&self) -> EncConfig {
        let c = &self.c;
        let rc = match c.mode {
            1 => RateControl::Cbr,
            2 => RateControl::Quality,
            _ => RateControl::Vbr,
        };
        let (profile, level) = match self.codec {
            Codec::Hevc => (if self.raw == PixFmt::P010 { HEVC_MAIN10 } else { c.hevc_profile as u8 }, c.hevc_level as u8),
            _ => (c.h264_profile as u8, c.h264_level as u8),
        };
        let gop = match (c.gop, c.i_period) {
            (0, p) | (p, 0) => p,
            (g, p) => g.min(p),
        };
        let mut flags = 0;
        // Constant bitrate is what calls ask for (Chrome's WebRTC): real
        // time then, each frame out at once.
        if rc == RateControl::Cbr {
            flags |= ENC_REALTIME;
        }
        if self.codec == Codec::H264 && c.entropy == 1 && matches!(profile, H264_MAIN | H264_HIGH) {
            flags |= ENC_CABAC;
        }
        EncConfig {
            codec: self.codec,
            format: self.raw,
            layout: self.layout,
            visible: self.visible,
            fps: self.fps,
            rc,
            bitrate: c.bitrate.clamp(1000, MAX_BITRATE),
            peak: if c.peak >= c.bitrate { c.peak.min(MAX_BITRATE) } else { 0 },
            quality: c.quality.clamp(1, 100),
            gop: gop.min(1 << 20),
            profile,
            level,
            flags,
        }
    }
}

/// A coded buffer big enough for any frame of this size: half a raw frame
/// (an intra frame at a high bitrate comes close to a third), within caps.
fn default_coded(l: Layout) -> u32 {
    (l.size(PixFmt::Nv12) / 2).clamp(512 << 10, MAX_BITSTREAM)
}

fn coded_pixfmt(c: Codec) -> u32 {
    match c {
        Codec::Hevc => PIX_HEVC,
        _ => PIX_H264,
    }
}

/// A raw format the guest asked for: what we take, or None.
fn raw_of(pix: u32, codec: Codec) -> Option<PixFmt> {
    match pix {
        PIX_NV12 => Some(PixFmt::Nv12),
        // 10 bits: HEVC Main 10 only.
        PIX_P010 if codec == Codec::Hevc => Some(PixFmt::P010),
        _ => None,
    }
}

/// A picture size within limits: even, at least the smallest side.
fn even_side(v: u32) -> u32 {
    (v.clamp(MIN_SIDE, MAX_SIDE) + 1) & !1
}

/// A control of the encoder.
#[derive(Debug)]
struct Ctrl {
    id: u32,
    name: &'static str,
    kind: u32,
    min: i64,
    max: i64,
    default: i64,
    /// Menu entries by value; "" for values we don't take.
    menu: &'static [&'static str],
    /// Changes while the queues run (rates, key frames).
    live: bool,
}

const fn ctrl(id: u32, name: &'static str, kind: u32, min: i64, max: i64, default: i64, live: bool) -> Ctrl {
    Ctrl { id, name, kind, min, max, default, menu: &[], live }
}

const fn menu(id: u32, name: &'static str, min: i64, max: i64, default: i64, menu: &'static [&'static str], live: bool) -> Ctrl {
    Ctrl { id, name, kind: CTRL_TYPE_MENU, min, max, default, menu, live }
}

const H264_LEVELS: &[&str] = &["1", "1b", "1.1", "1.2", "1.3", "2", "2.1", "2.2", "3", "3.1", "3.2", "4", "4.1", "4.2", "5", "5.1", "5.2"];
const HEVC_LEVELS: &[&str] = &["1", "2", "2.1", "3", "3.1", "4", "4.1", "5", "5.1", "5.2", "6", "6.1", "6.2"];

/// Sorted by id: QUERYCTRL walks them in order.
const CONTROLS: &[Ctrl] = &[
    ctrl(CID_MIN_BUFFERS_FOR_OUTPUT, "Min Number of Output Buffers", CTRL_TYPE_INTEGER, 1, 32, 2, false),
    ctrl(CID_B_FRAMES, "Number of B-Frames", CTRL_TYPE_INTEGER, 0, 0, 0, false),
    ctrl(CID_GOP_SIZE, "GOP Size", CTRL_TYPE_INTEGER, 0, 1 << 20, 0, true),
    menu(CID_BITRATE_MODE, "Video Bitrate Mode", 0, 2, 0, &["Variable Bitrate", "Constant Bitrate", "Constant Quality"], true),
    ctrl(CID_BITRATE, "Video Bitrate", CTRL_TYPE_INTEGER, 1000, MAX_BITRATE as i64, DEFAULT_BITRATE as i64, true),
    ctrl(CID_BITRATE_PEAK, "Video Peak Bitrate", CTRL_TYPE_INTEGER, 0, MAX_BITRATE as i64, 0, true),
    ctrl(CID_FRAME_RC_ENABLE, "Frame Level Rate Control Enable", CTRL_TYPE_BOOLEAN, 0, 1, 1, true),
    // Parameter sets come with each key frame, in its buffer.
    menu(CID_HEADER_MODE, "Sequence Header Mode", 1, 1, 1, &["", "Joined With 1st Frame"], false),
    ctrl(CID_REPEAT_SEQ_HEADER, "Repeat Sequence Header", CTRL_TYPE_BOOLEAN, 1, 1, 1, false),
    ctrl(CID_FORCE_KEY_FRAME, "Force an I-Frame", CTRL_TYPE_BUTTON, 0, 0, 0, true),
    menu(CID_H264_ENTROPY_MODE, "H264 Entropy Mode", 0, 1, 1, &["CAVLC", "CABAC"], false),
    ctrl(CID_H264_I_PERIOD, "H264 I-Frame Period", CTRL_TYPE_INTEGER, 0, 1 << 20, 0, true),
    menu(CID_H264_LEVEL, "H264 Level", 0, 16, 11, H264_LEVELS, false),
    // VideoToolbox makes no Extended profile.
    menu(CID_H264_PROFILE, "H264 Profile", 0, 4, 4, &["Baseline", "Constrained Baseline", "Main", "", "High"], false),
    menu(CID_HEVC_PROFILE, "HEVC Profile", 0, 2, 0, &["Main", "", "Main 10"], false),
    menu(CID_HEVC_LEVEL, "HEVC Level", 0, 12, 8, HEVC_LEVELS, false),
    ctrl(CID_PREPEND_SPSPPS_TO_IDR, "Prepend SPS and PPS to IDR", CTRL_TYPE_BOOLEAN, 1, 1, 1, false),
    ctrl(CID_CONSTANT_QUALITY, "Constant Quality", CTRL_TYPE_INTEGER, 0, 100, 50, true),
];

fn control(id: u32) -> Option<&'static Ctrl> {
    CONTROLS.iter().find(|c| c.id == id)
}

impl Ctrl {
    /// A value the guest may set this control to.
    fn takes(&self, v: i64) -> bool {
        (self.min..=self.max).contains(&v) && (self.menu.is_empty() || self.menu.get(v as usize).is_some_and(|n| !n.is_empty()))
    }
}

impl Handler {
    // ---- the encoder's ioctls ----

    pub(super) fn enc_open(&mut self, id: u32) {
        let codec = self.codecs.first().copied().unwrap_or(Codec::H264);
        self.enc.insert(id, EncSession::new(codec));
    }

    /// The codecs the encoder helper said it makes (H.264, HEVC).
    pub(super) fn enc_codecs(bits: u32) -> Vec<Codec> {
        [Codec::H264, Codec::Hevc].into_iter().filter(|c| bits & c.bit() != 0).collect()
    }

    pub(super) fn enc_ioctl(&mut self, id: u32, code: u32, r: &mut Reader) -> Answer {
        if !self.enc.contains_key(&id) {
            return Err(EINVAL);
        }
        match code {
            ioc::ENUM_FMT => {
                let mut b = read_payload(r, FMTDESC_SIZE)?;
                let (index, t) = (get_u32(&b, 0), get_u32(&b, 4));
                let hevc = self.codecs.contains(&Codec::Hevc);
                let (fourcc, flags, name) = match (dir(t).ok_or(EINVAL)?, index) {
                    (Dir::Capture, i) => {
                        let c = *self.codecs.get(i as usize).ok_or(EINVAL)?;
                        (coded_pixfmt(c), FMT_FLAG_COMPRESSED, if c == Codec::Hevc { "HEVC" } else { "H.264" })
                    }
                    (Dir::Output, 0) => (PIX_NV12, 0, "Y/UV 4:2:0"),
                    (Dir::Output, 1) if hevc => (PIX_P010, 0, "10-bit Y/UV 4:2:0"),
                    _ => return Err(EINVAL),
                };
                b[8..].fill(0);
                put_u32(&mut b, 8, flags);
                put_str(&mut b, 12, 32, name);
                put_u32(&mut b, 44, fourcc);
                Ok(b)
            }
            ioc::ENUM_FRAMESIZES => {
                let mut b = read_payload(r, FRMSIZEENUM_SIZE)?;
                let f = get_u32(&b, 4);
                let known = self.codecs.iter().any(|&c| coded_pixfmt(c) == f) || f == PIX_NV12 || f == PIX_P010;
                if get_u32(&b, 0) != 0 || !known {
                    return Err(EINVAL);
                }
                b[8..].fill(0);
                put_u32(&mut b, 8, 3); // stepwise
                for (i, v) in [MIN_SIDE, MAX_SIDE, 2, MIN_SIDE, MAX_SIDE, 2].into_iter().enumerate() {
                    put_u32(&mut b, 12 + 4 * i, v);
                }
                Ok(b)
            }
            ioc::ENUM_FRAMEINTERVALS => {
                let mut b = read_payload(r, FRMIVALENUM_SIZE)?;
                if get_u32(&b, 0) != 0 {
                    return Err(EINVAL);
                }
                b[16..].fill(0);
                put_u32(&mut b, 16, 2); // continuous
                // From 1/240 s to 1000 s a frame.
                for (i, v) in [1, MAX_FPS, 1000, 1, 1, 1].into_iter().enumerate() {
                    put_u32(&mut b, 20 + 4 * i, v);
                }
                Ok(b)
            }
            ioc::G_FMT | ioc::S_FMT | ioc::TRY_FMT => self.enc_fmt(id, code, r),
            ioc::G_PARM | ioc::S_PARM => {
                let mut b = read_payload(r, STREAMPARM_SIZE)?;
                let t = get_u32(&b, 0);
                if dir(t).is_none() && t != OUTPUT && t != CAPTURE {
                    return Err(EINVAL);
                }
                let s = self.enc.get_mut(&id).ok_or(EINVAL)?;
                if code == ioc::S_PARM {
                    // Seconds per frame: numerator / denominator.
                    let (num, den) = (get_u32(&b, 12), get_u32(&b, 16));
                    s.fps = if num == 0 || den == 0 {
                        (30, 1)
                    } else {
                        let (n, d) = (den as u64, num as u64);
                        if n > MAX_FPS as u64 * d {
                            (MAX_FPS, 1)
                        } else if d > 1000 * n {
                            (1, 1000)
                        } else {
                            // Keep it in u32 without losing the ratio much.
                            let g = gcd(n, d);
                            let (mut n, mut d) = (n / g, d / g);
                            while n > u32::MAX as u64 || d > u32::MAX as u64 {
                                n /= 2;
                                d = (d / 2).max(1);
                            }
                            (n.max(1) as u32, d as u32)
                        }
                    };
                }
                let fps = s.fps;
                b[4..].fill(0);
                put_u32(&mut b, 4, CAP_TIMEPERFRAME);
                put_u32(&mut b, 12, fps.1);
                put_u32(&mut b, 16, fps.0);
                if code == ioc::S_PARM {
                    self.enc_reconfigure(id);
                }
                Ok(b)
            }
            ioc::REQBUFS => {
                let mut b = read_payload(r, REQBUFS_SIZE)?;
                let (count, t, mem) = (get_u32(&b, 0), get_u32(&b, 4), get_u32(&b, 8));
                let d = dir(t).ok_or(EINVAL)?;
                if mem != MEMORY_MMAP {
                    return Err(EINVAL);
                }
                self.enc_streamoff(id, d);
                let s = self.enc.get_mut(&id).ok_or(EINVAL)?;
                let (max, size) = match d {
                    Dir::Output => (MAX_RAW_BUFS, s.layout.size(s.raw)),
                    Dir::Capture => (MAX_CODED_BUFS, s.coded_size),
                };
                let old = std::mem::take(s.bufs(d));
                for x in &old {
                    self.alloc.free(x.off);
                }
                let mut new = Vec::new();
                for _ in 0..count.min(max) {
                    match self.alloc.alloc(size as u64) {
                        Some(off) => new.push(Buf { off, len: size, queued: false, bytesused: 0, ts: [0; 16] }),
                        None => break,
                    }
                }
                if count > 0 && new.is_empty() {
                    return Err(ENOMEM);
                }
                let n = new.len() as u32;
                *self.enc.get_mut(&id).ok_or(EINVAL)?.bufs(d) = new;
                put_u32(&mut b, 0, n);
                put_u32(&mut b, 12, BUF_CAP_SUPPORTS_MMAP);
                Ok(b)
            }
            ioc::QUERYBUF | ioc::QBUF => {
                let mut b = read_payload(r, BUFFER_SIZE)?;
                let (index, t) = (get_u32(&b, buf::INDEX), get_u32(&b, buf::TYPE));
                let d = dir(t).ok_or(EINVAL)?;
                let nplanes = get_u32(&b, buf::LENGTH) as usize;
                if !(1..=MAX_PLANES).contains(&nplanes) {
                    return Err(EINVAL);
                }
                let mut planes = read_payload(r, PLANE_SIZE * nplanes)?;
                if code == ioc::QBUF {
                    self.enc_qbuf(id, d, index, &b, &planes)?;
                }
                let s = self.enc.get_mut(&id).ok_or(EINVAL)?;
                let x = s.bufs(d).get(index as usize).ok_or(EINVAL)?.clone();
                put_u32(&mut b, buf::FLAGS, (if x.queued { BUF_FLAG_QUEUED } else { 0 }) | BUF_FLAG_TIMESTAMP_COPY);
                put_u32(&mut b, buf::MEMORY, MEMORY_MMAP);
                put_u32(&mut b, buf::FIELD, FIELD_NONE);
                put_u32(&mut b, buf::LENGTH, 1);
                b[buf::M..buf::M + 8].fill(0);
                planes[..PLANE_SIZE].fill(0);
                put_u32(&mut planes, plane::BYTESUSED, x.bytesused);
                put_u32(&mut planes, plane::LENGTH, x.len);
                put_u32(&mut planes, plane::MEM_OFFSET, x.off as u32);
                b.extend_from_slice(&planes);
                Ok(b)
            }
            ioc::STREAMON | ioc::STREAMOFF => {
                let b = read_payload(r, 4)?;
                let d = dir(get_u32(&b, 0)).ok_or(EINVAL)?;
                if code == ioc::STREAMOFF {
                    self.enc_streamoff(id, d);
                    return Ok(Vec::new());
                }
                if self.helper_lost {
                    return Err(EIO);
                }
                let s = self.enc.get_mut(&id).ok_or(EINVAL)?;
                if s.bufs(d).is_empty() {
                    return Err(EINVAL);
                }
                match d {
                    Dir::Output => s.out_streaming = true,
                    Dir::Capture => s.cap_streaming = true,
                }
                if s.open.is_none() {
                    let config = s.config();
                    s.open = Some(config);
                    s.epoch = 0;
                    s.granted = 0;
                    self.send(&ToHelper::Configure { session: id, config });
                }
                self.enc_deliver(id);
                self.enc_pump(id);
                self.enc_grant(id);
                Ok(Vec::new())
            }
            ioc::SUBSCRIBE_EVENT | ioc::UNSUBSCRIBE_EVENT => {
                let b = read_payload(r, SUBSCRIPTION_SIZE)?;
                let s = self.enc.get_mut(&id).ok_or(EINVAL)?;
                let on = code == ioc::SUBSCRIBE_EVENT;
                match get_u32(&b, 0) {
                    EVENT_EOS => s.sub_eos = on,
                    EVENT_ALL if !on => s.sub_eos = false,
                    _ => return Err(EINVAL),
                }
                Ok(Vec::new())
            }
            ioc::G_SELECTION | ioc::S_SELECTION => {
                let mut b = read_payload(r, SELECTION_SIZE)?;
                let (t, target) = (get_u32(&b, 0), get_u32(&b, 4));
                if t != OUTPUT && t != OUTPUT_MPLANE {
                    return Err(EINVAL);
                }
                let s = self.enc.get_mut(&id).ok_or(EINVAL)?;
                if code == ioc::S_SELECTION {
                    if target != SEL_TGT_CROP {
                        return Err(EINVAL);
                    }
                    if s.out_streaming {
                        return Err(EBUSY);
                    }
                    // The top left of the frame, even sizes, inside it.
                    let (w, h) = (get_u32(&b, 20), get_u32(&b, 24));
                    s.visible = (even_side(w).min(s.layout.width), even_side(h).min(s.layout.height));
                }
                let (w, h) = match target {
                    SEL_TGT_CROP | SEL_TGT_CROP_DEFAULT => s.visible,
                    SEL_TGT_CROP_BOUNDS | SEL_TGT_NATIVE_SIZE => (s.layout.width, s.layout.height),
                    _ => return Err(EINVAL),
                };
                put_i32(&mut b, 12, 0);
                put_i32(&mut b, 16, 0);
                put_u32(&mut b, 20, w);
                put_u32(&mut b, 24, h);
                Ok(b)
            }
            ioc::G_CTRL | ioc::S_CTRL => {
                let mut b = read_payload(r, CONTROL_SIZE)?;
                let (cid, v) = (get_u32(&b, 0), get_u32(&b, 4) as i32 as i64);
                if code == ioc::S_CTRL {
                    self.enc_check_ctrl(id, cid, v)?;
                    self.enc_set_ctrls(id, &[(cid, v)]);
                } else {
                    let v = self.enc_get_ctrl(id, cid, false)?;
                    put_u32(&mut b, 4, v as u32);
                }
                Ok(b)
            }
            ioc::G_EXT_CTRLS | ioc::S_EXT_CTRLS | ioc::TRY_EXT_CTRLS => {
                let mut b = read_payload(r, EXT_CONTROLS_SIZE)?;
                let (which, count) = (get_u32(&b, 0), get_u32(&b, 4) as usize);
                // The current values (or a class of them), the defaults
                // (to read); no requests.
                let defaults = which == CTRL_WHICH_DEF_VAL;
                if (which != CTRL_WHICH_CUR_VAL && !defaults && which != CTRL_CLASS_CODEC && which != CTRL_CLASS_USER) || count > MAX_EXT_CTRLS {
                    return Err(EINVAL);
                }
                let mut list = read_payload(r, count * EXT_CONTROL_SIZE)?;
                let item = |list: &[u8], i: usize| (get_u32(list, i * EXT_CONTROL_SIZE), get_u32(list, i * EXT_CONTROL_SIZE + 12) as i32 as i64);
                put_u32(&mut b, 8, count as u32); // error_idx: none
                if code == ioc::G_EXT_CTRLS {
                    for i in 0..count {
                        let (cid, _) = item(&list, i);
                        let Ok(v) = self.enc_get_ctrl(id, cid, defaults) else {
                            put_u32(&mut b, 8, i as u32);
                            return Err(EINVAL);
                        };
                        put_u32(&mut list, i * EXT_CONTROL_SIZE + 12, v as u32);
                        put_u32(&mut list, i * EXT_CONTROL_SIZE + 16, if v < 0 { u32::MAX } else { 0 });
                    }
                } else {
                    if defaults {
                        return Err(EINVAL);
                    }
                    let mut set = Vec::with_capacity(count);
                    for i in 0..count {
                        let (cid, v) = item(&list, i);
                        self.enc_check_ctrl(id, cid, v)?;
                        set.push((cid, v));
                    }
                    if code == ioc::S_EXT_CTRLS {
                        self.enc_set_ctrls(id, &set);
                    }
                }
                b.extend_from_slice(&list);
                Ok(b)
            }
            ioc::QUERYCTRL | ioc::QUERY_EXT_CTRL => {
                let size = if code == ioc::QUERYCTRL { QUERYCTRL_SIZE } else { QUERY_EXT_CTRL_SIZE };
                let mut b = read_payload(r, size)?;
                let want = get_u32(&b, 0);
                let c = if want & CTRL_FLAG_NEXT_CTRL != 0 {
                    let after = want & !(CTRL_FLAG_NEXT_CTRL | CTRL_FLAG_NEXT_COMPOUND);
                    CONTROLS.iter().find(|c| c.id > after)
                } else {
                    control(want)
                }
                .ok_or(EINVAL)?;
                b.fill(0);
                put_u32(&mut b, 0, c.id);
                put_u32(&mut b, 4, c.kind);
                put_str(&mut b, 8, 32, c.name);
                let mut flags = if c.id == CID_MIN_BUFFERS_FOR_OUTPUT { CTRL_FLAG_READ_ONLY } else { 0 };
                if c.kind == CTRL_TYPE_BUTTON {
                    flags |= CTRL_FLAG_WRITE_ONLY;
                }
                let step = if c.kind == CTRL_TYPE_BUTTON { 0 } else { 1 };
                if code == ioc::QUERYCTRL {
                    for (i, v) in [c.min as i32, c.max as i32, step, c.default as i32, flags as i32].into_iter().enumerate() {
                        put_i32(&mut b, 40 + 4 * i, v);
                    }
                } else {
                    for (i, v) in [c.min, c.max, step as i64, c.default].into_iter().enumerate() {
                        put_u64(&mut b, 40 + 8 * i, v as u64);
                    }
                    put_u32(&mut b, 72, flags);
                    put_u32(&mut b, 76, 4); // elem_size
                    put_u32(&mut b, 80, 1); // elems
                }
                Ok(b)
            }
            ioc::QUERYMENU => {
                let mut b = read_payload(r, QUERYMENU_SIZE)?;
                let c = control(get_u32(&b, 0)).filter(|c| !c.menu.is_empty()).ok_or(EINVAL)?;
                let index = get_u32(&b, 4) as i64;
                if !c.takes(index) {
                    return Err(EINVAL);
                }
                put_str(&mut b, 8, 32, c.menu[index as usize]);
                put_u32(&mut b, 40, 0);
                Ok(b)
            }
            ioc::ENCODER_CMD | ioc::TRY_ENCODER_CMD => {
                let mut b = read_payload(r, ENCODER_CMD_SIZE)?;
                let cmd = get_u32(&b, 0);
                if cmd != ENC_CMD_START && cmd != ENC_CMD_STOP {
                    return Err(EINVAL);
                }
                b[4..].fill(0);
                if code == ioc::ENCODER_CMD {
                    self.enc_cmd(id, cmd);
                }
                Ok(b)
            }
            _ => Err(super::ENOTTY),
        }
    }

    fn enc_fmt(&mut self, id: u32, code: u32, r: &mut Reader) -> Answer {
        let b = read_payload(r, FORMAT_SIZE)?;
        let d = dir(get_u32(&b, fmt::TYPE)).ok_or(EINVAL)?;
        let pix = get_u32(&b, fmt::PIXELFORMAT);
        let codecs = self.codecs.clone();
        let s = self.enc.get_mut(&id).ok_or(EINVAL)?;
        if code != ioc::G_FMT {
            match d {
                Dir::Capture => {
                    // Coded formats we don't make are refused (FFmpeg
                    // probes devices that way).
                    let codec = codecs.iter().copied().find(|&c| coded_pixfmt(c) == pix).ok_or(EINVAL)?;
                    let size = match get_u32(&b, fmt::SIZEIMAGE) {
                        0 => default_coded(s.layout),
                        n => n,
                    }
                    .clamp(MIN_CODED, MAX_BITSTREAM);
                    if code == ioc::TRY_FMT {
                        return Ok(coded_format(b, codec, s.visible, size));
                    }
                    if s.cap_streaming || s.out_streaming || (!s.cap_bufs.is_empty() && size > s.coded_size) {
                        return Err(EBUSY);
                    }
                    s.codec = codec;
                    s.coded_size = size;
                    if s.raw == PixFmt::P010 && codec != Codec::Hevc {
                        s.raw = PixFmt::Nv12;
                    }
                }
                Dir::Output => {
                    let raw = raw_of(pix, s.codec).ok_or(EINVAL)?;
                    let (w, h) = (get_u32(&b, fmt::WIDTH), get_u32(&b, fmt::HEIGHT));
                    let visible = (even_side(w), even_side(h));
                    let layout = Layout::for_frame(visible.0, visible.1).ok_or(EINVAL)?;
                    if code == ioc::TRY_FMT {
                        return Ok(raw_format(b, layout, raw));
                    }
                    if s.out_streaming || (!s.out_bufs.is_empty() && (layout != s.layout || raw != s.raw)) {
                        return Err(EBUSY);
                    }
                    s.raw = raw;
                    s.layout = layout;
                    s.visible = visible;
                    if s.cap_bufs.is_empty() {
                        s.coded_size = s.coded_size.max(default_coded(layout));
                    }
                }
            }
        }
        Ok(match d {
            Dir::Output => raw_format(b, s.layout, s.raw),
            Dir::Capture => coded_format(b, s.codec, s.visible, s.coded_size),
        })
    }

    fn enc_get_ctrl(&self, id: u32, cid: u32, default: bool) -> std::result::Result<i64, u32> {
        let c = control(cid).ok_or(EINVAL)?;
        let s = self.enc.get(&id).ok_or(EINVAL)?;
        if default {
            return Ok(c.default);
        }
        let k = &s.c;
        Ok(match cid {
            CID_BITRATE => k.bitrate as i64,
            CID_BITRATE_PEAK => k.peak as i64,
            CID_BITRATE_MODE => k.mode as i64,
            CID_CONSTANT_QUALITY => k.quality as i64,
            CID_GOP_SIZE => k.gop as i64,
            CID_H264_I_PERIOD => k.i_period as i64,
            CID_H264_PROFILE => k.h264_profile as i64,
            CID_H264_LEVEL => k.h264_level as i64,
            CID_HEVC_PROFILE => k.hevc_profile as i64,
            CID_HEVC_LEVEL => k.hevc_level as i64,
            CID_H264_ENTROPY_MODE => k.entropy as i64,
            CID_FRAME_RC_ENABLE => k.frame_rc as i64,
            CID_FORCE_KEY_FRAME => return Err(EINVAL),
            _ => c.default,
        })
    }

    /// Can the guest set this control to `v` now?
    fn enc_check_ctrl(&self, id: u32, cid: u32, v: i64) -> std::result::Result<(), u32> {
        let c = control(cid).ok_or(EINVAL)?;
        let s = self.enc.get(&id).ok_or(EINVAL)?;
        if cid == CID_MIN_BUFFERS_FOR_OUTPUT || !c.takes(v) {
            return Err(EINVAL);
        }
        if !c.live && (s.out_streaming || s.cap_streaming) && self.enc_get_ctrl(id, cid, false).ok() != Some(v) {
            return Err(EBUSY);
        }
        Ok(())
    }

    /// Set checked controls; a running encoder takes the new rates.
    fn enc_set_ctrls(&mut self, id: u32, set: &[(u32, i64)]) {
        let Some(s) = self.enc.get_mut(&id) else { return };
        for &(cid, v) in set {
            let k = &mut s.c;
            let v32 = v as u32;
            match cid {
                CID_BITRATE => k.bitrate = v32,
                CID_BITRATE_PEAK => k.peak = v32,
                CID_BITRATE_MODE => k.mode = v32,
                CID_CONSTANT_QUALITY => k.quality = v32,
                CID_GOP_SIZE => k.gop = v32,
                CID_H264_I_PERIOD => k.i_period = v32,
                CID_H264_PROFILE => k.h264_profile = v32,
                CID_H264_LEVEL => k.h264_level = v32,
                CID_HEVC_PROFILE => k.hevc_profile = v32,
                CID_HEVC_LEVEL => k.hevc_level = v32,
                CID_H264_ENTROPY_MODE => k.entropy = v32,
                CID_FRAME_RC_ENABLE => k.frame_rc = v32,
                CID_FORCE_KEY_FRAME => s.key_next = true,
                _ => {}
            }
        }
        self.enc_reconfigure(id);
    }

    /// Tell the helper about new settings, if it has the session open.
    fn enc_reconfigure(&mut self, id: u32) {
        let Some(s) = self.enc.get_mut(&id) else { return };
        let Some(old) = s.open else { return };
        let config = s.config();
        if config != old {
            s.open = Some(config);
            self.send(&ToHelper::Configure { session: id, config });
        }
    }

    fn enc_qbuf(&mut self, id: u32, d: Dir, index: u32, b: &[u8], planes: &[u8]) -> std::result::Result<(), u32> {
        let s = self.enc.get_mut(&id).ok_or(EINVAL)?;
        if s.dead {
            return Err(EIO);
        }
        if get_u32(b, buf::MEMORY) != MEMORY_MMAP {
            return Err(EINVAL);
        }
        let need = s.layout.size(s.raw);
        let x = s.bufs(d).get_mut(index as usize).ok_or(EINVAL)?;
        if x.queued {
            return Err(EINVAL);
        }
        match d {
            Dir::Output => {
                // A frame starts at the buffer's start and fills it (the
                // buffers were made for this layout).
                let used = get_u32(planes, plane::BYTESUSED).min(x.len);
                if get_u32(planes, plane::DATA_OFFSET) != 0 || x.len < need {
                    return Err(EINVAL);
                }
                x.ts.copy_from_slice(&b[buf::TIMESTAMP..buf::TIMESTAMP + 16]);
                x.bytesused = used;
                x.queued = true;
                if used == 0 {
                    // Nothing in it: straight back.
                    self.enc_dqbuf(id, Dir::Output, index, 0);
                    return Ok(());
                }
                let key = std::mem::take(&mut s.key_next);
                s.pending.push_back((index, key));
                self.enc_pump(id);
            }
            Dir::Capture => {
                x.queued = true;
                x.bytesused = 0;
                s.cap_queue.push_back(index);
                self.enc_deliver(id);
                self.enc_grant(id);
            }
        }
        Ok(())
    }

    /// A buffer goes back to the guest.
    fn enc_dqbuf(&mut self, id: u32, d: Dir, index: u32, flags: u32) {
        let Some(s) = self.enc.get_mut(&id) else { return };
        let seq = if d == Dir::Capture {
            s.sequence = s.sequence.wrapping_add(1);
            s.sequence.wrapping_sub(1)
        } else {
            0
        };
        let Some(b) = s.bufs(d).get_mut(index as usize) else { return };
        b.queued = false;
        let ev = super::buffer_event(id, d, index, b, flags, seq);
        self.event(ev);
    }

    /// Send raw frames to the helper, up to the in-flight cap.
    fn enc_pump(&mut self, id: u32) {
        loop {
            let Some(s) = self.enc.get_mut(&id) else { return };
            if s.dead || s.open.is_none() || !s.out_streaming || s.in_flight.len() >= MAX_FRAMES_IN_FLIGHT {
                break;
            }
            let Some((index, key)) = s.pending.pop_front() else { break };
            let Some(b) = s.out_bufs.get(index as usize) else { continue };
            let (off, ts, len) = (b.off as u32, b.ts, s.layout.size(s.raw));
            let cookie = self.next_cookie;
            self.next_cookie = self.next_cookie.wrapping_add(1).max(1);
            s.in_flight.insert(cookie, index);
            s.stamps.push_back((cookie, ts));
            while s.stamps.len() > MAX_STAMPS {
                s.stamps.pop_front();
            }
            s.stat_frames += 1;
            self.send(&ToHelper::Encode { session: id, cookie, offset: off, len, key });
        }
        let Some(s) = self.enc.get_mut(&id) else { return };
        if s.draining && !s.drain_sent && s.pending.is_empty() {
            s.drain_sent = true;
            self.send(&ToHelper::Drain { session: id });
        }
    }

    /// Grant the helper the queued coded buffers it hasn't got yet.
    fn enc_grant(&mut self, id: u32) {
        let Some(s) = self.enc.get_mut(&id) else { return };
        if s.dead || s.open.is_none() || !s.cap_streaming || s.last_pending || !s.failed.is_empty() {
            return;
        }
        let mut grants = Vec::new();
        while (s.granted as usize) < s.cap_queue.len() {
            let Some(b) = s.cap_queue.get(s.granted as usize).and_then(|&i| s.cap_bufs.get(i as usize)) else { break };
            grants.push(ToHelper::Grant { session: id, epoch: s.epoch, offset: b.off as u32, len: b.len, format: PixFmt::Nv12 });
            s.granted += 1;
        }
        for m in grants {
            self.send(&m);
        }
    }

    fn enc_new_epoch(&mut self, id: u32) {
        let Some(s) = self.enc.get_mut(&id) else { return };
        s.epoch = s.epoch.wrapping_add(1);
        s.granted = 0;
        if s.open.is_some() {
            let m = ToHelper::Grant { session: id, epoch: s.epoch, offset: 0, len: 0, format: PixFmt::Nv12 };
            self.send(&m);
        }
    }

    /// Hand back what is due before coded frames: frames a restore lost
    /// (empty, with the error flag), or the empty LAST buffer of a drain.
    fn enc_deliver(&mut self, id: u32) {
        loop {
            let Some(s) = self.enc.get_mut(&id) else { return };
            if !s.cap_streaming || (s.failed.is_empty() && !s.last_pending) {
                return;
            }
            // Nothing is granted then (lost frames come first after a
            // restore; LAST after the helper has drained, and a new epoch
            // follows it).
            let Some(index) = s.cap_queue.pop_front() else { return };
            if let Some(ts) = s.failed.pop_front() {
                if let Some(b) = s.cap_bufs.get_mut(index as usize) {
                    b.bytesused = 0;
                    b.ts = ts;
                }
                self.enc_dqbuf(id, Dir::Capture, index, BUF_FLAG_ERROR);
                continue;
            }
            s.last_pending = false;
            let eos = std::mem::take(&mut s.last_is_eos) && s.sub_eos;
            if let Some(b) = s.cap_bufs.get_mut(index as usize) {
                b.bytesused = 0;
            }
            self.enc_dqbuf(id, Dir::Capture, index, BUF_FLAG_LAST);
            if eos {
                self.v4l2_event(id, EVENT_EOS, 0);
            }
            self.enc_new_epoch(id);
            return;
        }
    }

    pub(super) fn enc_streamoff(&mut self, id: u32, d: Dir) {
        let Some(s) = self.enc.get_mut(&id) else { return };
        for b in s.bufs(d).iter_mut() {
            b.queued = false;
        }
        match d {
            Dir::Output => {
                let was = s.out_streaming || !s.pending.is_empty() || !s.in_flight.is_empty();
                s.out_streaming = false;
                s.pending.clear();
                s.in_flight.clear();
                s.stamps.clear();
                s.draining = false;
                s.drain_sent = false;
                if was && s.open.is_some() {
                    self.send(&ToHelper::Flush { session: id });
                }
                self.enc_new_epoch(id);
                self.enc_grant(id);
            }
            Dir::Capture => {
                // The stream starts again: from a key frame.
                s.cap_streaming = false;
                s.cap_queue.clear();
                s.last_pending = false;
                s.failed.clear();
                s.key_next = true;
                self.enc_new_epoch(id);
            }
        }
    }

    fn enc_cmd(&mut self, id: u32, cmd: u32) {
        let Some(s) = self.enc.get_mut(&id) else { return };
        if cmd == ENC_CMD_START {
            s.draining = false;
            s.drain_sent = false;
            s.last_pending = false;
            self.enc_grant(id);
            return;
        }
        if s.draining {
            return;
        }
        if s.out_streaming && s.open.is_some() {
            s.draining = true;
            self.enc_pump(id);
        } else {
            s.last_pending = true;
            s.last_is_eos = true;
            self.enc_deliver(id);
        }
    }

    pub(super) fn enc_close(&mut self, id: u32) {
        let Some(s) = self.enc.remove(&id) else { return };
        for b in s.out_bufs.iter().chain(&s.cap_bufs) {
            self.alloc.free(b.off);
        }
        if s.open.is_some() {
            self.send(&ToHelper::Close { session: id });
        }
        self.events.retain(|e| get_u32(e, 4) != id);
    }

    pub(super) fn enc_on_helper(&mut self, m: FromHelper) {
        let id = m.session();
        let Some(s) = self.enc.get_mut(&id) else { return };
        if s.dead {
            return;
        }
        match m {
            FromHelper::Consumed { cookie, ok, .. } => {
                // The helper has read the frame: the guest may fill the
                // buffer again.
                let Some(index) = s.in_flight.remove(&cookie) else { return };
                if !ok {
                    log::debug!("media: encoder {id}: a frame didn't encode");
                }
                self.enc_dqbuf(id, Dir::Output, index, 0);
                self.enc_pump(id);
            }
            FromHelper::Coded { epoch, cookie, offset, bytes, key, ok, .. } => {
                if epoch != s.epoch {
                    log::debug!("media: encoder {id}: stale coded frame dropped");
                    return;
                }
                // The helper fills grants in order: this must be the oldest.
                let Some(&index) = s.cap_queue.front().filter(|_| s.granted > 0) else {
                    log::info!("media: encoder {id}: a coded frame came without a buffer for it");
                    return;
                };
                let Some(b) = s.cap_bufs.get(index as usize) else { return };
                if b.off != offset as u64 {
                    log::warn!("media: encoder {id}: the helper wrote where it wasn't granted");
                    return;
                }
                let good = ok && bytes > 0 && bytes <= b.len;
                if ok && !good {
                    log::warn!("media: encoder {id}: a coded frame of the wrong size from the helper");
                }
                s.cap_queue.pop_front();
                s.granted -= 1;
                let at = s.stamps.iter().position(|(c, _)| *c == cookie);
                let ts = at.and_then(|i| s.stamps.remove(i)).map_or([0; 16], |(_, t)| t);
                s.stat_coded += 1;
                let Some(b) = s.cap_bufs.get_mut(index as usize) else { return };
                b.ts = ts;
                b.bytesused = if good { bytes } else { 0 };
                let flags = match (good, key) {
                    (false, _) => BUF_FLAG_ERROR,
                    (true, true) => BUF_FLAG_KEYFRAME,
                    (true, false) => BUF_FLAG_PFRAME,
                };
                self.enc_dqbuf(id, Dir::Capture, index, flags);
            }
            FromHelper::DrainDone { .. } => {
                if s.draining {
                    s.draining = false;
                    s.drain_sent = false;
                    s.last_pending = true;
                    s.last_is_eos = true;
                    self.enc_deliver(id);
                }
            }
            FromHelper::Error { errno, .. } => {
                log::info!("media: encoder {id}: the helper gave up");
                s.dead = true;
                self.error_event(id, errno.clamp(1, 4095));
            }
            // Decoder messages.
            FromHelper::Caps { .. } | FromHelper::Format { .. } | FromHelper::Picture { .. } | FromHelper::Resume { .. } => {}
        }
    }

    /// The helper is gone: every encoder fails. The ones that hadn't yet.
    pub(super) fn enc_fail_all(&mut self) -> Vec<u32> {
        let mut ids = Vec::new();
        for (&id, s) in self.enc.iter_mut() {
            if !s.dead {
                s.dead = true;
                ids.push(id);
            }
        }
        ids
    }

    pub(super) fn enc_tick(&self) {
        for (id, s) in &self.enc {
            log::debug!(
                "media: encoder {id}: frames {}, coded {}, streaming {}/{}, pending {}, in flight {}, coded queue {:?}, granted {}, draining {}",
                s.stat_frames,
                s.stat_coded,
                s.out_streaming,
                s.cap_streaming,
                s.pending.len(),
                s.in_flight.len(),
                s.cap_queue,
                s.granted,
                s.draining
            );
        }
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a.max(1)
    } else {
        gcd(b, a % b)
    }
}

fn dir(t: u32) -> Option<Dir> {
    super::dir(t)
}

fn raw_format(mut b: Vec<u8>, l: Layout, f: PixFmt) -> Vec<u8> {
    b[8..].fill(0);
    put_u32(&mut b, fmt::WIDTH, l.width);
    put_u32(&mut b, fmt::HEIGHT, l.height);
    put_u32(&mut b, fmt::PIXELFORMAT, if f == PixFmt::P010 { PIX_P010 } else { PIX_NV12 });
    put_u32(&mut b, fmt::FIELD, FIELD_NONE);
    put_u32(&mut b, fmt::COLORSPACE, COLORSPACE_REC709);
    put_u32(&mut b, fmt::SIZEIMAGE, l.size(f));
    put_u32(&mut b, fmt::BYTESPERLINE, l.stride(f));
    put_u8(&mut b, fmt::NUM_PLANES, 1);
    b
}

fn coded_format(mut b: Vec<u8>, c: Codec, visible: (u32, u32), size: u32) -> Vec<u8> {
    b[8..].fill(0);
    put_u32(&mut b, fmt::WIDTH, visible.0);
    put_u32(&mut b, fmt::HEIGHT, visible.1);
    put_u32(&mut b, fmt::PIXELFORMAT, coded_pixfmt(c));
    put_u32(&mut b, fmt::FIELD, FIELD_NONE);
    put_u32(&mut b, fmt::COLORSPACE, COLORSPACE_REC709);
    put_u32(&mut b, fmt::SIZEIMAGE, size);
    put_u8(&mut b, fmt::NUM_PLANES, 1);
    b
}

// ---- snapshots ----

/// An encoder as a snapshot keeps it. All of it is checked again on
/// restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SavedEnc {
    id: u32,
    codec: u8,
    raw: u8,
    layout: (u32, u32),
    pub(super) visible: (u32, u32),
    coded_size: u32,
    pub(super) fps: (u32, u32),
    controls: Controls,
    /// (offset, length, queued) of each buffer.
    out_bufs: Vec<(u64, u32, bool)>,
    pub(super) cap_bufs: Vec<(u64, u32, bool)>,
    streaming: (bool, bool),
    open: bool,
    cap_queue: Vec<u32>,
    pub(super) epoch: u32,
    sub_eos: bool,
    sequence: u32,
    /// last_pending, last_is_eos, draining.
    flags: (bool, bool, bool),
    pub(super) dead: bool,
    /// Timestamps of the frames whose coded frame hadn't come back.
    lost: Vec<[u8; 16]>,
}

impl SavedEnc {
    pub(super) fn of(id: u32, s: &EncSession) -> SavedEnc {
        let bufs = |v: &Vec<Buf>| v.iter().map(|b| (b.off, b.len, b.queued)).collect();
        // Frames still waiting here, then those at the helper or coded
        // but not delivered: their coded frames won't come.
        let mut lost: Vec<[u8; 16]> = s.failed.iter().copied().collect();
        lost.extend(s.stamps.iter().map(|(_, t)| *t));
        lost.extend(s.pending.iter().filter_map(|&(i, _)| s.out_bufs.get(i as usize).map(|b| b.ts)));
        lost.truncate(MAX_STAMPS);
        SavedEnc {
            id,
            codec: s.codec.to_u8(),
            raw: s.raw.to_u8(),
            layout: (s.layout.width, s.layout.height),
            visible: s.visible,
            coded_size: s.coded_size,
            fps: s.fps,
            controls: s.c,
            out_bufs: bufs(&s.out_bufs),
            cap_bufs: bufs(&s.cap_bufs),
            streaming: (s.out_streaming, s.cap_streaming),
            open: s.open.is_some(),
            cap_queue: s.cap_queue.iter().copied().collect(),
            epoch: s.epoch,
            sub_eos: s.sub_eos,
            sequence: s.sequence,
            flags: (s.last_pending, s.last_is_eos, s.draining),
            dead: s.dead,
            lost,
        }
    }
}

impl Handler {
    /// Encoders from a snapshot. One whose state doesn't hold together
    /// fails in the guest.
    pub(super) fn enc_restore(&mut self, data: &[u8]) {
        let saved: Vec<SavedEnc> = serde_json::from_slice(data).unwrap_or_default();
        for sv in saved {
            let id = sv.id;
            if id >= vroom_video::proto::MAX_SESSIONS || self.enc.contains_key(&id) {
                continue;
            }
            match self.enc_rebuild(&sv) {
                Some(s) => {
                    self.enc.insert(id, s);
                    self.enc_resume(id);
                }
                None => {
                    log::warn!("media: encoder {id}: saved state doesn't hold together");
                    let mut s = EncSession::new(Codec::H264);
                    s.dead = true;
                    self.enc.insert(id, s);
                    self.error_event(id, EIO);
                }
            }
        }
    }

    fn enc_rebuild(&mut self, sv: &SavedEnc) -> Option<EncSession> {
        let codec = Codec::from_u8(sv.codec).filter(|c| self.codecs.contains(c))?;
        let mut s = EncSession::new(codec);
        s.raw = PixFmt::from_u8(sv.raw)?;
        s.layout = Layout::for_frame(sv.layout.0, sv.layout.1)?;
        s.visible = sv.visible;
        s.coded_size = sv.coded_size;
        s.fps = sv.fps;
        s.c = sv.controls;
        // Every control and format as the guest could have set them.
        let k = &s.c;
        let ok = [
            (CID_BITRATE, k.bitrate),
            (CID_BITRATE_PEAK, k.peak),
            (CID_BITRATE_MODE, k.mode),
            (CID_CONSTANT_QUALITY, k.quality),
            (CID_GOP_SIZE, k.gop),
            (CID_H264_I_PERIOD, k.i_period),
            (CID_H264_PROFILE, k.h264_profile),
            (CID_H264_LEVEL, k.h264_level),
            (CID_HEVC_PROFILE, k.hevc_profile),
            (CID_HEVC_LEVEL, k.hevc_level),
            (CID_H264_ENTROPY_MODE, k.entropy),
            (CID_FRAME_RC_ENABLE, k.frame_rc),
        ]
        .iter()
        .all(|&(cid, v)| control(cid).is_some_and(|c| c.takes(v as i64)));
        if !ok || raw_of(raw_pix(s.raw), codec).is_none() || !(MIN_CODED..=MAX_BITSTREAM).contains(&s.coded_size) || !s.config().valid() {
            return None;
        }
        if sv.out_bufs.len() > MAX_RAW_BUFS as usize || sv.cap_bufs.len() > MAX_CODED_BUFS as usize || sv.lost.len() > MAX_STAMPS {
            return None;
        }
        let need = s.layout.size(s.raw);
        let mut took = Vec::new();
        let mut good = true;
        for (saved, min, into) in [(&sv.out_bufs, need, &mut s.out_bufs), (&sv.cap_bufs, MIN_CODED, &mut s.cap_bufs)] {
            for &(off, len, queued) in saved {
                if !good || len < min || len > vroom_video::MAX_PICTURE || !self.alloc.claim(off, len as u64) {
                    good = false;
                    break;
                }
                took.push(off);
                into.push(Buf { off, len, queued, bytesused: 0, ts: [0; 16] });
            }
        }
        let queue_ok = good
            && sv.cap_queue.iter().enumerate().all(|(n, &i)| s.cap_bufs.get(i as usize).is_some_and(|b| b.queued) && !sv.cap_queue[..n].contains(&i));
        if !queue_ok {
            for off in took {
                self.alloc.free(off);
            }
            return None;
        }
        s.cap_queue = sv.cap_queue.iter().copied().collect();
        for (i, b) in s.cap_bufs.iter_mut().enumerate() {
            b.queued = s.cap_queue.contains(&(i as u32));
        }
        (s.out_streaming, s.cap_streaming) = sv.streaming;
        s.open = sv.open.then(|| s.config());
        s.epoch = sv.epoch.wrapping_add(1);
        s.sub_eos = sv.sub_eos;
        s.sequence = sv.sequence;
        let (last, eos, draining) = sv.flags;
        // A drain under way: its frames are gone, it ends now.
        s.last_pending = last || draining;
        s.last_is_eos = eos || draining;
        s.dead = sv.dead;
        s.failed = sv.lost.iter().copied().collect();
        s.key_next = true;
        Some(s)
    }

    /// Give the guest back its queued raw frames (their pixels were in the
    /// window, which a snapshot doesn't keep), open the stream in the new
    /// helper, and hand back the frames it lost.
    fn enc_resume(&mut self, id: u32) {
        let Some(s) = self.enc.get_mut(&id) else { return };
        let queued: Vec<u32> = s.out_bufs.iter().enumerate().filter(|(_, b)| b.queued).map(|(i, _)| i as u32).collect();
        let (open, dead) = (s.open, s.dead);
        for i in queued {
            self.enc_dqbuf(id, Dir::Output, i, BUF_FLAG_ERROR);
        }
        if dead {
            self.error_event(id, EIO);
            return;
        }
        if let Some(config) = open {
            self.send(&ToHelper::Configure { session: id, config });
            let epoch = self.enc.get(&id).map_or(0, |s| s.epoch);
            self.send(&ToHelper::Grant { session: id, epoch, offset: 0, len: 0, format: PixFmt::Nv12 });
        }
        self.enc_deliver(id);
        self.enc_grant(id);
    }
}

fn raw_pix(f: PixFmt) -> u32 {
    match f {
        PixFmt::Nv12 => PIX_NV12,
        PixFmt::P010 => PIX_P010,
    }
}
