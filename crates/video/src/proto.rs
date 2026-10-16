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

use std::io::{self, Read, Write};

use crate::{Codec, Layout, PixFmt, MAX_BITSTREAM};

/// Sessions (guest decoders open at once).
pub const MAX_SESSIONS: u32 = 8;
/// Pictures the helper keeps per session while it has no grant.
pub const MAX_HELD: usize = 8;
/// Grants the helper keeps per session.
pub const MAX_GRANTS: usize = 64;

/// [`ToHelper::Open`] flag: hand pictures out in decoding order, each as
/// soon as it is decoded, and say when one failed. For a VA-API driver on
/// top of the decoder: its player reorders, and it waits for each picture.
pub const OPEN_DECODE_ORDER: u8 = 1;

const HDR: usize = 12;

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
}

mod kind {
    pub const OPEN: u8 = 1;
    pub const DECODE: u8 = 2;
    pub const GRANT: u8 = 3;
    pub const DRAIN: u8 = 4;
    pub const FLUSH: u8 = 5;
    pub const CLOSE: u8 = 6;

    pub const FORMAT: u8 = 0x81;
    pub const PICTURE: u8 = 0x82;
    pub const CONSUMED: u8 = 0x83;
    pub const DRAIN_DONE: u8 = 0x84;
    pub const ERROR: u8 = 0x85;
    pub const CAPS: u8 = 0x86;
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
            _ => None,
        })?;
        let exact = |n: usize| if b.len() == n { Ok(()) } else { Err(bad("body size")) };
        Ok(match k {
            kind::OPEN => {
                exact(2)?;
                if b[1] & !OPEN_DECODE_ORDER != 0 {
                    return Err(bad("open flags"));
                }
                ToHelper::Open { session, codec: Codec::from_u8(b[0]).ok_or_else(|| bad("codec"))?, flags: b[1] }
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
            | FromHelper::Error { session, .. } => *session,
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
        ];
        for m in from {
            assert_eq!(FromHelper::read_from(&mut &m.encode()[..]).unwrap(), m);
        }
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
