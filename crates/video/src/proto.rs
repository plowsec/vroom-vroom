//! Messages between the VMM and `vroom video-helper`, over a stream
//! socket. Each is a 12-byte header (kind, 3 zero bytes, session, body
//! length, little endian) and a body. The reader checks every length
//! against the kind's limit before reading the body, and the body's
//! shape after: a malformed message ends the connection.
//!
//! The VMM decides when the helper may hand back pictures: it grants a
//! credit for each guest buffer free to take one, under an epoch it
//! bumps whenever those buffers go away (the guest stopped its picture
//! queue, or a seek dropped what was decoding). Pictures come back with
//! the epoch they were granted under; the VMM drops stale ones. So the
//! VMM never holds more pictures than the guest gave it buffers for, and
//! the helper never more than [`MAX_HELD`] per session.

use std::io::{self, Read, Write};

use crate::{Codec, Layout, MAX_BITSTREAM};

/// Sessions (guest decoders open at once).
pub const MAX_SESSIONS: u32 = 8;
/// Pictures the helper keeps per session while it has no credit.
pub const MAX_HELD: usize = 8;

const HDR: usize = 12;

/// VMM -> helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToHelper {
    Open { session: u32, codec: Codec },
    /// One guest buffer of bitstream; `cookie` names it in the answers.
    Decode { session: u32, cookie: u32, data: Vec<u8> },
    /// `n` more pictures may come (same epoch), or exactly `n` (new one).
    Credit { session: u32, epoch: u32, n: u32 },
    /// Decode everything sent, hand back every picture, then `DrainDone`.
    Drain { session: u32 },
    /// Drop everything queued, decoding or decoded (a seek). Credits go
    /// to zero until the next `Credit`.
    Flush { session: u32 },
    Close { session: u32 },
}

/// Helper -> VMM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromHelper {
    /// The stream's picture size, before its first picture and whenever
    /// it changes. `visible` sits in the top left of `layout`.
    Format { session: u32, layout: Layout, visible: (u32, u32), min_buffers: u32 },
    /// A picture in `layout`'s NV12 form, from the bitstream buffer
    /// `cookie` named.
    Picture { session: u32, epoch: u32, cookie: u32, data: Vec<u8> },
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
    pub const CREDIT: u8 = 3;
    pub const DRAIN: u8 = 4;
    pub const FLUSH: u8 = 5;
    pub const CLOSE: u8 = 6;

    pub const FORMAT: u8 = 0x81;
    pub const PICTURE: u8 = 0x82;
    pub const CONSUMED: u8 = 0x83;
    pub const DRAIN_DONE: u8 = 0x84;
    pub const ERROR: u8 = 0x85;
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
            ToHelper::Open { session, codec } => {
                let mut v = header(kind::OPEN, *session, 1);
                v.push(codec.to_u8());
                v
            }
            ToHelper::Decode { session, cookie, data } => {
                let mut v = header(kind::DECODE, *session, 4 + data.len());
                v.extend_from_slice(&cookie.to_le_bytes());
                v.extend_from_slice(data);
                v
            }
            ToHelper::Credit { session, epoch, n } => {
                let mut v = header(kind::CREDIT, *session, 8);
                v.extend_from_slice(&epoch.to_le_bytes());
                v.extend_from_slice(&n.to_le_bytes());
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
            kind::OPEN => Some(1),
            kind::DECODE => Some(4 + MAX_BITSTREAM as usize),
            kind::CREDIT => Some(8),
            kind::DRAIN | kind::FLUSH | kind::CLOSE => Some(0),
            _ => None,
        })?;
        let exact = |n: usize| if b.len() == n { Ok(()) } else { Err(bad("body size")) };
        Ok(match k {
            kind::OPEN => {
                exact(1)?;
                ToHelper::Open { session, codec: Codec::from_u8(b[0]).ok_or_else(|| bad("codec"))? }
            }
            kind::DECODE => {
                if b.len() < 4 {
                    return Err(bad("body size"));
                }
                ToHelper::Decode { session, cookie: u32_at(&b, 0), data: b[4..].to_vec() }
            }
            kind::CREDIT => {
                exact(8)?;
                ToHelper::Credit { session, epoch: u32_at(&b, 0), n: u32_at(&b, 4) }
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
            FromHelper::Format { session, .. }
            | FromHelper::Picture { session, .. }
            | FromHelper::Consumed { session, .. }
            | FromHelper::DrainDone { session }
            | FromHelper::Error { session, .. } => *session,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        match self {
            FromHelper::Format { session, layout, visible, min_buffers } => {
                let mut v = header(kind::FORMAT, *session, 20);
                for x in [layout.width, layout.height, visible.0, visible.1, *min_buffers] {
                    v.extend_from_slice(&x.to_le_bytes());
                }
                v
            }
            FromHelper::Picture { session, epoch, cookie, data } => {
                let mut v = header(kind::PICTURE, *session, 8 + data.len());
                v.extend_from_slice(&epoch.to_le_bytes());
                v.extend_from_slice(&cookie.to_le_bytes());
                v.extend_from_slice(data);
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
    /// a picture is at most the biggest layout, a format must be one.
    pub fn read_from(r: &mut impl Read) -> io::Result<FromHelper> {
        let max_picture = (crate::MAX_PIXELS + 16 * crate::MAX_SIDE) as usize * 3 / 2;
        let (k, session, b) = read_header(r, |k| match k {
            kind::FORMAT => Some(20),
            kind::PICTURE => Some(8 + max_picture),
            kind::CONSUMED => Some(5),
            kind::DRAIN_DONE => Some(0),
            kind::ERROR => Some(4),
            _ => None,
        })?;
        let exact = |n: usize| if b.len() == n { Ok(()) } else { Err(bad("body size")) };
        Ok(match k {
            kind::FORMAT => {
                exact(20)?;
                let layout = Layout { width: u32_at(&b, 0), height: u32_at(&b, 4) };
                let visible = (u32_at(&b, 8), u32_at(&b, 12));
                let min_buffers = u32_at(&b, 16);
                if !layout.valid()
                    || visible.0 == 0
                    || visible.1 == 0
                    || visible.0 > layout.width
                    || visible.1 > layout.height
                    || !(1..=32).contains(&min_buffers)
                {
                    return Err(bad("format"));
                }
                FromHelper::Format { session, layout, visible, min_buffers }
            }
            kind::PICTURE => {
                if b.len() < 8 {
                    return Err(bad("body size"));
                }
                FromHelper::Picture { session, epoch: u32_at(&b, 0), cookie: u32_at(&b, 4), data: b[8..].to_vec() }
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
            ToHelper::Open { session: 1, codec: Codec::Hevc },
            ToHelper::Decode { session: 2, cookie: 7, data: vec![0, 0, 1, 0x65] },
            ToHelper::Credit { session: 3, epoch: 4, n: 5 },
            ToHelper::Drain { session: 0 },
            ToHelper::Flush { session: 0 },
            ToHelper::Close { session: 7 },
        ];
        for m in to {
            assert_eq!(ToHelper::read_from(&mut &m.encode()[..]).unwrap(), m);
        }
        let from = [
            FromHelper::Format { session: 1, layout: Layout { width: 1920, height: 1088 }, visible: (1920, 1080), min_buffers: 4 },
            FromHelper::Picture { session: 1, epoch: 2, cookie: 3, data: vec![1, 2, 3] },
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
        // Unknown kind, session out of range, too long, bad codec, bad format.
        let mut m = ToHelper::Drain { session: 0 }.encode();
        m[0] = 0x7f;
        assert!(ToHelper::read_from(&mut &m[..]).is_err());
        assert!(ToHelper::read_from(&mut &ToHelper::Drain { session: MAX_SESSIONS }.encode()[..]).is_err());
        let mut m = ToHelper::Credit { session: 0, epoch: 0, n: 0 }.encode();
        m[8] = 9;
        assert!(ToHelper::read_from(&mut &m[..]).is_err());
        let mut m = ToHelper::Open { session: 0, codec: Codec::H264 }.encode();
        m[12] = 9;
        assert!(ToHelper::read_from(&mut &m[..]).is_err());
        let f = FromHelper::Format { session: 0, layout: Layout { width: 1920, height: 1088 }, visible: (1921, 1080), min_buffers: 4 };
        assert!(FromHelper::read_from(&mut &f.encode()[..]).is_err());
        let f = FromHelper::Format { session: 0, layout: Layout { width: 1928, height: 1088 }, visible: (1920, 1080), min_buffers: 4 };
        assert!(FromHelper::read_from(&mut &f.encode()[..]).is_err());
        // A picture claiming more than the biggest layout.
        let mut h = vec![kind::PICTURE, 0, 0, 0, 0, 0, 0, 0];
        h.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(FromHelper::read_from(&mut &h[..]).is_err());
        // Truncated.
        let m = ToHelper::Decode { session: 0, cookie: 1, data: vec![1; 10] }.encode();
        assert!(ToHelper::read_from(&mut &m[..m.len() - 1]).is_err());
    }
}
