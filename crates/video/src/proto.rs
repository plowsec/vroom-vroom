//! Messages between the VMM and `vroom video-helper`, over a stream
//! socket. Each is a 12-byte header (kind, 3 zero bytes, session, body
//! length, little endian) and a body. The reader checks every length
//! against the kind's limit before reading the body, and the body's
//! shape after: a malformed message ends the connection.
//!
//! Before any message the VMM passes the helper the buffer window (the
//! shared memory the guest's decoder buffers live in, see
//! `vroom_virtio::devices::media`), and the helper answers with
//! [`FromHelper::Caps`]. Pictures don't cross the socket: the helper
//! writes each one straight into a guest buffer in the window.
//!
//! The VMM decides when, and where: it grants each guest picture buffer
//! free to take one ([`ToHelper::Grant`]: where it is, how big, which
//! format), under an epoch it bumps whenever those buffers go away (the
//! guest stopped its picture queue, or a seek dropped what was decoding).
//! The helper fills grants in the order it got them and says which one
//! it filled; the VMM checks that against its own list and drops
//! pictures of an older epoch. So the helper writes only where the VMM
//! said, and holds at most [`MAX_HELD`] pictures per session.
//!
//! An encoder helper (`vroom video-helper --encode`, a process of its
//! own for the guest's encoder device) speaks the same protocol the other
//! way round: [`ToHelper::Encode`] names a raw frame in the window, which
//! the helper reads (and says when it has: [`FromHelper::Consumed`]);
//! grants are guest buffers for the coded frames, which come back as
//! [`FromHelper::Coded`]. Its [`FromHelper::Caps`] are the codecs it
//! encodes.

use std::io::{self, Read, Write};

use crate::{Codec, Layout, PixFmt, MAX_BITSTREAM};

/// Sessions (guest decoders open at once).
pub const MAX_SESSIONS: u32 = 8;
/// Pictures the helper keeps per session while it has no grant.
pub const MAX_HELD: usize = 8;
/// Grants the helper keeps per session.
pub const MAX_GRANTS: usize = 64;
/// Biggest resume state (see [`FromHelper::Resume`]).
pub const MAX_RESUME: usize = 16 << 10;

/// [`ToHelper::Open`] flag: hand pictures out in decoding order, each as
/// soon as it is decoded, and say when one failed. For a VA-API driver on
/// top of the decoder: its player reorders, and it waits for each picture.
pub const OPEN_DECODE_ORDER: u8 = 1;

const HDR: usize = 12;

/// Most bits a second an encoder is asked for.
pub const MAX_BITRATE: u32 = 400_000_000;
/// Most frames a second.
pub const MAX_FPS: u32 = 240;

/// How an encoder spends its bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Around [`EncConfig::bitrate`] on average, up to [`EncConfig::peak`].
    Vbr,
    /// [`EncConfig::bitrate`] as steadily as it can (calls).
    Cbr,
    /// A constant quality ([`EncConfig::quality`]), whatever that costs.
    Quality,
}

impl RateControl {
    pub fn to_u8(self) -> u8 {
        match self {
            RateControl::Vbr => 0,
            RateControl::Cbr => 1,
            RateControl::Quality => 2,
        }
    }

    pub fn from_u8(v: u8) -> Option<RateControl> {
        match v {
            0 => Some(RateControl::Vbr),
            1 => Some(RateControl::Cbr),
            2 => Some(RateControl::Quality),
            _ => None,
        }
    }
}

/// [`EncConfig::flags`]: real time (low latency: each frame out as soon
/// as it is coded, rate control made for calls).
pub const ENC_REALTIME: u8 = 1;
/// [`EncConfig::flags`]: H.264 with CABAC (Main and High); else CAVLC.
pub const ENC_CABAC: u8 = 2;

/// H.264 profiles, as V4L2 numbers them (`V4L2_CID_MPEG_VIDEO_H264_PROFILE`).
pub const H264_BASELINE: u8 = 0;
pub const H264_CONSTRAINED_BASELINE: u8 = 1;
pub const H264_MAIN: u8 = 2;
pub const H264_HIGH: u8 = 4;
/// HEVC profiles (`V4L2_CID_MPEG_VIDEO_HEVC_PROFILE`).
pub const HEVC_MAIN: u8 = 0;
pub const HEVC_MAIN10: u8 = 2;

/// What an encoder session makes, and from what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncConfig {
    /// H.264 or HEVC.
    pub codec: Codec,
    /// The raw frames: NV12, or P010 (HEVC Main 10 only).
    pub format: PixFmt,
    /// How a raw frame lies in its guest buffer ([`Layout::for_frame`]).
    pub layout: Layout,
    /// The picture coded: the top left of each frame, even sizes.
    pub visible: (u32, u32),
    /// Frames a second: `fps.0 / fps.1`.
    pub fps: (u32, u32),
    pub rc: RateControl,
    /// Bits a second.
    pub bitrate: u32,
    /// Most bits a second (VBR); 0: no limit of its own.
    pub peak: u32,
    /// 1 (smallest) to 100 (best), for [`RateControl::Quality`].
    pub quality: u32,
    /// Most frames from one key frame to the next; 0: no limit (key
    /// frames first and when asked for).
    pub gop: u32,
    /// [`H264_MAIN`] and the others, or [`HEVC_MAIN`], [`HEVC_MAIN10`].
    pub profile: u8,
    /// The level as V4L2 numbers it (H.264: 0 is 1, 11 is 4.0, ...). A
    /// hint: VideoToolbox picks one that fits.
    pub level: u8,
    /// [`ENC_REALTIME`], [`ENC_CABAC`].
    pub flags: u8,
}

impl EncConfig {
    const SIZE: usize = 48;

    /// Within limits and consistent (one from the other side).
    pub fn valid(&self) -> bool {
        let (w, h) = self.visible;
        let profile_ok = match self.codec {
            Codec::H264 => matches!(self.profile, H264_BASELINE | H264_CONSTRAINED_BASELINE | H264_MAIN | H264_HIGH) && self.format == PixFmt::Nv12,
            Codec::Hevc => (self.profile == HEVC_MAIN && self.format == PixFmt::Nv12) || self.profile == HEVC_MAIN10,
            Codec::Vp9 | Codec::Av1 => false,
        };
        let (n, d) = (self.fps.0 as u64, self.fps.1 as u64);
        profile_ok
            && self.layout.valid_frame()
            && (crate::MIN_SIDE..=self.layout.width).contains(&w)
            && (crate::MIN_SIDE..=self.layout.height).contains(&h)
            && w % 2 == 0
            && h % 2 == 0
            && n > 0
            && d > 0
            && n <= MAX_FPS as u64 * d
            && d <= 1000 * n
            && (1000..=MAX_BITRATE).contains(&self.bitrate)
            && self.peak <= MAX_BITRATE
            && (1..=100).contains(&self.quality)
            && self.gop <= 1 << 20
            && self.level <= 20
            && self.flags & !(ENC_REALTIME | ENC_CABAC) == 0
    }

    fn encode(&self, v: &mut Vec<u8>) {
        v.extend_from_slice(&[self.codec.to_u8(), self.format.to_u8(), self.rc.to_u8(), self.profile, self.level, self.flags, 0, 0]);
        for x in [self.layout.width, self.layout.height, self.visible.0, self.visible.1, self.fps.0, self.fps.1, self.bitrate, self.peak, self.quality, self.gop] {
            v.extend_from_slice(&x.to_le_bytes());
        }
    }

    fn decode(b: &[u8]) -> Option<EncConfig> {
        if b.len() != Self::SIZE || b[6..8] != [0, 0] {
            return None;
        }
        let u = |i: usize| u32_at(b, 8 + 4 * i);
        let c = EncConfig {
            codec: Codec::from_u8(b[0])?,
            format: PixFmt::from_u8(b[1])?,
            rc: RateControl::from_u8(b[2])?,
            profile: b[3],
            level: b[4],
            flags: b[5],
            layout: Layout { width: u(0), height: u(1) },
            visible: (u(2), u(3)),
            fps: (u(4), u(5)),
            bitrate: u(6),
            peak: u(7),
            quality: u(8),
            gop: u(9),
        };
        c.valid().then_some(c)
    }
}

/// VMM -> helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToHelper {
    Open { session: u32, codec: Codec, flags: u8 },
    /// One guest buffer of bitstream; `cookie` names it in the answers.
    Decode { session: u32, cookie: u32, data: Vec<u8> },
    /// A guest picture buffer at `offset` in the window, `len` bytes, for
    /// one picture in `format`. A new `epoch` first drops every grant of
    /// the old one; `len` 0 does only that.
    Grant { session: u32, epoch: u32, offset: u32, len: u32, format: PixFmt },
    /// Decode everything sent, hand back every picture, then `DrainDone`.
    Drain { session: u32 },
    /// Drop everything queued, decoding or decoded (a seek). Grants go
    /// too, until the next `Grant`.
    Flush { session: u32 },
    Close { session: u32 },
    /// Open a session again after a snapshot was restored (or a suspended
    /// VM resumed), with the state a [`FromHelper::Resume`] gave: the
    /// stream goes on from its next key frame.
    Resume { session: u32, codec: Codec, flags: u8, state: Vec<u8> },
    /// Encoder: set the session up, or change it. Rates (bitrate, frame
    /// rate, quality, key frame interval) change from the next frame on;
    /// anything else starts the stream again with a key frame.
    Configure { session: u32, config: EncConfig },
    /// Encoder: the raw frame at `offset` in the window, `len` bytes laid
    /// out as the session's config says. `key`: make it a key frame.
    Encode { session: u32, cookie: u32, offset: u32, len: u32, key: bool },
}

/// Helper -> VMM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromHelper {
    /// The first message: the codecs this Mac decodes ([`Codec::bit`]s).
    Caps { codecs: u32 },
    /// The stream's picture size, before its first picture and whenever
    /// it changes. `visible` sits in the top left of `layout`; `depth` is
    /// the stream's bits per sample (8 or 10).
    Format { session: u32, layout: Layout, visible: (u32, u32), min_buffers: u32, depth: u32 },
    /// The grant at `offset` now holds a picture of `bytes` bytes, from
    /// the bitstream buffer `cookie` named. `ok` false (decoding order
    /// only): that picture failed, nothing was written.
    Picture { session: u32, epoch: u32, cookie: u32, offset: u32, bytes: u32, ok: bool },
    /// The helper is done with bitstream buffer `cookie`.
    Consumed { session: u32, cookie: u32, ok: bool },
    /// Every picture before the drain went out.
    DrainDone { session: u32 },
    /// The session can't go on (bad stream, VideoToolbox gave up).
    Error { session: u32, errno: u32 },
    /// What the helper needs to open this stream again in another helper
    /// (after a restore): its parameter sets, VP9 key frame header or AV1
    /// sequence header. Opaque to the VMM, which keeps the latest and
    /// saves it with a snapshot; a new helper parses it like a stream.
    Resume { session: u32, state: Vec<u8> },
    /// Encoder: the grant at `offset` now holds the coded frame (Annex B)
    /// of raw frame `cookie`, `bytes` long. `ok` false: that frame failed
    /// (or didn't fit), nothing was written.
    Coded { session: u32, epoch: u32, cookie: u32, offset: u32, bytes: u32, key: bool, ok: bool },
}

mod kind {
    pub const OPEN: u8 = 1;
    pub const DECODE: u8 = 2;
    pub const GRANT: u8 = 3;
    pub const DRAIN: u8 = 4;
    pub const FLUSH: u8 = 5;
    pub const CLOSE: u8 = 6;
    pub const RESUME: u8 = 7;
    pub const CONFIGURE: u8 = 8;
    pub const ENCODE: u8 = 9;

    pub const FORMAT: u8 = 0x81;
    pub const PICTURE: u8 = 0x82;
    pub const CONSUMED: u8 = 0x83;
    pub const DRAIN_DONE: u8 = 0x84;
    pub const ERROR: u8 = 0x85;
    pub const CAPS: u8 = 0x86;
    pub const RESUMABLE: u8 = 0x87;
    pub const CODED: u8 = 0x88;
}

fn header(kind: u8, session: u32, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(HDR + len);
    v.extend_from_slice(&[kind, 0, 0, 0]);
    v.extend_from_slice(&session.to_le_bytes());
    v.extend_from_slice(&(len as u32).to_le_bytes());
    v
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("video helper protocol: {what}"))
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[i..i + 4]);
    u32::from_le_bytes(a)
}

/// Read a header: (kind, session, body length), the length checked
/// against `max(kind)`.
fn read_header(r: &mut impl Read, max: impl Fn(u8) -> Option<usize>) -> io::Result<(u8, u32, Vec<u8>)> {
    let mut h = [0u8; HDR];
    r.read_exact(&mut h)?;
    if h[1..4] != [0, 0, 0] {
        return Err(bad("header"));
    }
    let (kind, session, len) = (h[0], u32_at(&h, 4), u32_at(&h, 8) as usize);
    let limit = max(kind).ok_or_else(|| bad("unknown message"))?;
    if len > limit {
        return Err(bad("message too long"));
    }
    if session >= MAX_SESSIONS {
        return Err(bad("session id"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok((kind, session, body))
}

impl ToHelper {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ToHelper::Open { session, codec, flags } => {
                let mut v = header(kind::OPEN, *session, 2);
                v.push(codec.to_u8());
                v.push(*flags);
                v
            }
            ToHelper::Decode { session, cookie, data } => {
                let mut v = header(kind::DECODE, *session, 4 + data.len());
                v.extend_from_slice(&cookie.to_le_bytes());
                v.extend_from_slice(data);
                v
            }
            ToHelper::Grant { session, epoch, offset, len, format } => {
                let mut v = header(kind::GRANT, *session, 13);
                for x in [epoch, offset, len] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v.push(format.to_u8());
                v
            }
            ToHelper::Drain { session } => header(kind::DRAIN, *session, 0),
            ToHelper::Flush { session } => header(kind::FLUSH, *session, 0),
            ToHelper::Close { session } => header(kind::CLOSE, *session, 0),
            ToHelper::Resume { session, codec, flags, state } => {
                let mut v = header(kind::RESUME, *session, 2 + state.len());
                v.push(codec.to_u8());
                v.push(*flags);
                v.extend_from_slice(state);
                v
            }
            ToHelper::Configure { session, config } => {
                let mut v = header(kind::CONFIGURE, *session, EncConfig::SIZE);
                config.encode(&mut v);
                v
            }
            ToHelper::Encode { session, cookie, offset, len, key } => {
                let mut v = header(kind::ENCODE, *session, 13);
                for x in [cookie, offset, len] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v.push(*key as u8);
                v
            }
        }
    }

    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.encode())
    }

    /// The helper reads the VMM (which a guest may have taken over).
    pub fn read_from(r: &mut impl Read) -> io::Result<ToHelper> {
        let (k, session, b) = read_header(r, |k| match k {
            kind::OPEN => Some(2),
            kind::DECODE => Some(4 + MAX_BITSTREAM as usize),
            kind::GRANT => Some(13),
            kind::DRAIN | kind::FLUSH | kind::CLOSE => Some(0),
            kind::RESUME => Some(2 + MAX_RESUME),
            kind::CONFIGURE => Some(EncConfig::SIZE),
            kind::ENCODE => Some(13),
            _ => None,
        })?;
        let exact = |n: usize| if b.len() == n { Ok(()) } else { Err(bad("body size")) };
        Ok(match k {
            kind::OPEN | kind::RESUME => {
                if b.len() < 2 || (k == kind::OPEN && b.len() != 2) {
                    return Err(bad("body size"));
                }
                if b[1] & !OPEN_DECODE_ORDER != 0 {
                    return Err(bad("open flags"));
                }
                let codec = Codec::from_u8(b[0]).ok_or_else(|| bad("codec"))?;
                if k == kind::RESUME {
                    ToHelper::Resume { session, codec, flags: b[1], state: b[2..].to_vec() }
                } else {
                    ToHelper::Open { session, codec, flags: b[1] }
                }
            }
            kind::DECODE => {
                if b.len() < 4 {
                    return Err(bad("body size"));
                }
                ToHelper::Decode { session, cookie: u32_at(&b, 0), data: b[4..].to_vec() }
            }
            kind::GRANT => {
                exact(13)?;
                let format = PixFmt::from_u8(b[12]).ok_or_else(|| bad("pixel format"))?;
                let len = u32_at(&b, 8);
                if len > crate::MAX_PICTURE {
                    return Err(bad("grant size"));
                }
                ToHelper::Grant { session, epoch: u32_at(&b, 0), offset: u32_at(&b, 4), len, format }
            }
            kind::DRAIN => ToHelper::Drain { session },
            kind::FLUSH => ToHelper::Flush { session },
            kind::CONFIGURE => ToHelper::Configure { session, config: EncConfig::decode(&b).ok_or_else(|| bad("encoder config"))? },
            kind::ENCODE => {
                exact(13)?;
                let len = u32_at(&b, 8);
                if len > crate::MAX_PICTURE || b[12] > 1 {
                    return Err(bad("frame"));
                }
                ToHelper::Encode { session, cookie: u32_at(&b, 0), offset: u32_at(&b, 4), len, key: b[12] != 0 }
            }
            _ => ToHelper::Close { session },
        })
    }
}

impl FromHelper {
    pub fn session(&self) -> u32 {
        match self {
            FromHelper::Caps { .. } => 0,
            FromHelper::Format { session, .. }
            | FromHelper::Picture { session, .. }
            | FromHelper::Consumed { session, .. }
            | FromHelper::DrainDone { session }
            | FromHelper::Error { session, .. }
            | FromHelper::Resume { session, .. }
            | FromHelper::Coded { session, .. } => *session,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        match self {
            FromHelper::Caps { codecs } => {
                let mut v = header(kind::CAPS, 0, 4);
                v.extend_from_slice(&codecs.to_le_bytes());
                v
            }
            FromHelper::Format { session, layout, visible, min_buffers, depth } => {
                let mut v = header(kind::FORMAT, *session, 24);
                for x in [layout.width, layout.height, visible.0, visible.1, *min_buffers, *depth] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v
            }
            FromHelper::Picture { session, epoch, cookie, offset, bytes, ok } => {
                let mut v = header(kind::PICTURE, *session, 17);
                for x in [epoch, cookie, offset, bytes] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v.push(*ok as u8);
                v
            }
            FromHelper::Consumed { session, cookie, ok } => {
                let mut v = header(kind::CONSUMED, *session, 5);
                v.extend_from_slice(&cookie.to_le_bytes());
                v.push(*ok as u8);
                v
            }
            FromHelper::DrainDone { session } => header(kind::DRAIN_DONE, *session, 0),
            FromHelper::Error { session, errno } => {
                let mut v = header(kind::ERROR, *session, 4);
                v.extend_from_slice(&errno.to_le_bytes());
                v
            }
            FromHelper::Resume { session, state } => {
                let mut v = header(kind::RESUMABLE, *session, state.len());
                v.extend_from_slice(state);
                v
            }
            FromHelper::Coded { session, epoch, cookie, offset, bytes, key, ok } => {
                let mut v = header(kind::CODED, *session, 18);
                for x in [epoch, cookie, offset, bytes] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v.push(*key as u8);
                v.push(*ok as u8);
                v
            }
        }
    }

    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.encode())
    }

    /// The VMM reads the helper (which a bad stream may have taken over):
    /// a format must be a valid one. Where a picture went the VMM checks
    /// against its grants.
    pub fn read_from(r: &mut impl Read) -> io::Result<FromHelper> {
        let (k, session, b) = read_header(r, |k| match k {
            kind::CAPS => Some(4),
            kind::FORMAT => Some(24),
            kind::PICTURE => Some(17),
            kind::CONSUMED => Some(5),
            kind::DRAIN_DONE => Some(0),
            kind::ERROR => Some(4),
            kind::RESUMABLE => Some(MAX_RESUME),
            kind::CODED => Some(18),
            _ => None,
        })?;
        let exact = |n: usize| if b.len() == n { Ok(()) } else { Err(bad("body size")) };
        Ok(match k {
            kind::CAPS => {
                exact(4)?;
                FromHelper::Caps { codecs: u32_at(&b, 0) }
            }
            kind::FORMAT => {
                exact(24)?;
                let layout = Layout { width: u32_at(&b, 0), height: u32_at(&b, 4) };
                let visible = (u32_at(&b, 8), u32_at(&b, 12));
                let (min_buffers, depth) = (u32_at(&b, 16), u32_at(&b, 20));
                if !layout.valid()
                    || visible.0 == 0
                    || visible.1 == 0
                    || visible.0 > layout.width
                    || visible.1 > layout.height
                    || !(1..=32).contains(&min_buffers)
                    || (depth != 8 && depth != 10)
                {
                    return Err(bad("format"));
                }
                FromHelper::Format { session, layout, visible, min_buffers, depth }
            }
            kind::PICTURE => {
                exact(17)?;
                FromHelper::Picture { session, epoch: u32_at(&b, 0), cookie: u32_at(&b, 4), offset: u32_at(&b, 8), bytes: u32_at(&b, 12), ok: b[16] != 0 }
            }
            kind::CONSUMED => {
                exact(5)?;
                FromHelper::Consumed { session, cookie: u32_at(&b, 0), ok: b[4] != 0 }
            }
            kind::DRAIN_DONE => FromHelper::DrainDone { session },
            kind::RESUMABLE => FromHelper::Resume { session, state: b },
            kind::CODED => {
                exact(18)?;
                let (epoch, cookie, offset, bytes) = (u32_at(&b, 0), u32_at(&b, 4), u32_at(&b, 8), u32_at(&b, 12));
                FromHelper::Coded { session, epoch, cookie, offset, bytes, key: b[16] != 0, ok: b[17] != 0 }
            }
            _ => {
                exact(4)?;
                FromHelper::Error { session, errno: u32_at(&b, 0) }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let to = [
            ToHelper::Open { session: 1, codec: Codec::Hevc, flags: 0 },
            ToHelper::Open { session: 1, codec: Codec::Av1, flags: OPEN_DECODE_ORDER },
            ToHelper::Decode { session: 2, cookie: 7, data: vec![0, 0, 1, 0x65] },
            ToHelper::Grant { session: 3, epoch: 4, offset: 5 << 16, len: 6, format: PixFmt::P010 },
            ToHelper::Drain { session: 0 },
            ToHelper::Flush { session: 0 },
            ToHelper::Close { session: 7 },
            ToHelper::Resume { session: 2, codec: Codec::Vp9, flags: 0, state: vec![0x82, 0x49] },
            ToHelper::Configure { session: 1, config: config() },
            ToHelper::Configure { session: 1, config: EncConfig { codec: Codec::Hevc, format: PixFmt::P010, profile: HEVC_MAIN10, ..config() } },
            ToHelper::Encode { session: 3, cookie: 9, offset: 1 << 20, len: 1382400, key: true },
        ];
        for m in to {
            assert_eq!(ToHelper::read_from(&mut &m.encode()[..]).unwrap(), m);
        }
        let from = [
            FromHelper::Caps { codecs: 0x1e },
            FromHelper::Format { session: 1, layout: Layout { width: 1920, height: 1088 }, visible: (1920, 1080), min_buffers: 4, depth: 10 },
            FromHelper::Picture { session: 1, epoch: 2, cookie: 3, offset: 4, bytes: 5, ok: true },
            FromHelper::Consumed { session: 1, cookie: 9, ok: true },
            FromHelper::DrainDone { session: 1 },
            FromHelper::Error { session: 1, errno: 22 },
            FromHelper::Resume { session: 3, state: vec![0, 0, 1, 0x67] },
            FromHelper::Coded { session: 2, epoch: 1, cookie: 9, offset: 64 << 10, bytes: 1234, key: true, ok: true },
        ];
        for m in from {
            assert_eq!(FromHelper::read_from(&mut &m.encode()[..]).unwrap(), m);
        }
    }

    fn config() -> EncConfig {
        EncConfig {
            codec: Codec::H264,
            format: PixFmt::Nv12,
            layout: Layout { width: 1280, height: 720 },
            visible: (1280, 720),
            fps: (30, 1),
            rc: RateControl::Cbr,
            bitrate: 2_000_000,
            peak: 0,
            quality: 50,
            gop: 0,
            profile: H264_CONSTRAINED_BASELINE,
            level: 11,
            flags: ENC_REALTIME,
        }
    }

    #[test]
    fn rejects_bad_encoder_configs() {
        let bad = [
            EncConfig { codec: Codec::Vp9, ..config() },
            EncConfig { format: PixFmt::P010, ..config() },
            EncConfig { codec: Codec::Hevc, profile: HEVC_MAIN, format: PixFmt::P010, ..config() },
            EncConfig { profile: 3, ..config() },
            EncConfig { visible: (1296, 720), ..config() },
            EncConfig { visible: (1278, 721), ..config() },
            EncConfig { visible: (0, 720), ..config() },
            EncConfig { layout: Layout { width: 1281, height: 720 }, ..config() },
            EncConfig { layout: Layout { width: 4096, height: 4096 }, visible: (4096, 4096), ..config() },
            EncConfig { fps: (0, 1), ..config() },
            EncConfig { fps: (241, 1), ..config() },
            EncConfig { fps: (1, 1001), ..config() },
            EncConfig { bitrate: 10, ..config() },
            EncConfig { bitrate: MAX_BITRATE + 1, ..config() },
            EncConfig { quality: 0, ..config() },
            EncConfig { flags: 0x80, ..config() },
        ];
        for c in bad {
            assert!(!c.valid(), "{c:?}");
            let m = ToHelper::Configure { session: 0, config: c };
            assert!(ToHelper::read_from(&mut &m.encode()[..]).is_err(), "{c:?}");
        }
        assert!(config().valid());
        // Padding must be zero; a frame's key flag 0 or 1; a frame no
        // bigger than a picture.
        let mut m = ToHelper::Configure { session: 0, config: config() }.encode();
        m[HDR + 6] = 1;
        assert!(ToHelper::read_from(&mut &m[..]).is_err());
        let mut m = ToHelper::Encode { session: 0, cookie: 1, offset: 0, len: 10, key: false }.encode();
        *m.last_mut().unwrap() = 2;
        assert!(ToHelper::read_from(&mut &m[..]).is_err());
        let m = ToHelper::Encode { session: 0, cookie: 1, offset: 0, len: crate::MAX_PICTURE + 1, key: false }.encode();
        assert!(ToHelper::read_from(&mut &m[..]).is_err());
    }

    #[test]
    fn rejects_bad_messages() {
        // Unknown kind, session out of range, too long, bad codec, bad
        // flags, bad pixel format, too big a grant, bad format.
        let mut m = ToHelper::Drain { session: 0 }.encode();
        m[0] = 0x7f;
        assert!(ToHelper::read_from(&mut &m[..]).is_err());
        assert!(ToHelper::read_from(&mut &ToHelper::Drain { session: MAX_SESSIONS }.encode()[..]).is_err());
        let g = ToHelper::Grant { session: 0, epoch: 0, offset: 0, len: 0, format: PixFmt::Nv12 };
        let mut m = g.encode();
        m[8] = 99;
        assert!(ToHelper::read_from(&mut &m[..]).is_err());
        let mut m = g.encode();
        *m.last_mut().unwrap() = 3;
        assert!(ToHelper::read_from(&mut &m[..]).is_err(), "pixel format");
        let big = ToHelper::Grant { session: 0, epoch: 0, offset: 0, len: crate::MAX_PICTURE + 1, format: PixFmt::Nv12 };
        assert!(ToHelper::read_from(&mut &big.encode()[..]).is_err());
        let mut m = ToHelper::Open { session: 0, codec: Codec::H264, flags: 0 }.encode();
        m[12] = 9;
        assert!(ToHelper::read_from(&mut &m[..]).is_err());
        let mut m = ToHelper::Open { session: 0, codec: Codec::H264, flags: 0 }.encode();
        m[13] = 0x80;
        assert!(ToHelper::read_from(&mut &m[..]).is_err(), "open flags");
        let layout = Layout { width: 1920, height: 1088 };
        let f = FromHelper::Format { session: 0, layout, visible: (1921, 1080), min_buffers: 4, depth: 8 };
        assert!(FromHelper::read_from(&mut &f.encode()[..]).is_err());
        let f = FromHelper::Format { session: 0, layout: Layout { width: 1928, height: 1088 }, visible: (1920, 1080), min_buffers: 4, depth: 8 };
        assert!(FromHelper::read_from(&mut &f.encode()[..]).is_err());
        let f = FromHelper::Format { session: 0, layout, visible: (1920, 1080), min_buffers: 4, depth: 12 };
        assert!(FromHelper::read_from(&mut &f.encode()[..]).is_err());
        // A picture message claiming a long body.
        let mut h = vec![kind::PICTURE, 0, 0, 0, 0, 0, 0, 0];
        h.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(FromHelper::read_from(&mut &h[..]).is_err());
        // Truncated.
        let m = ToHelper::Decode { session: 0, cookie: 1, data: vec![1; 10] }.encode();
        assert!(ToHelper::read_from(&mut &m[..m.len() - 1]).is_err());
    }
}
