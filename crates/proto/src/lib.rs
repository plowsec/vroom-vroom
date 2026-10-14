//! Host <-> guest agent protocol.
//!
//! Byte stream (virtio-console port), framed:
//! ```text
//! "BW" | version u8 | type u8 | len u32 LE | payload[len]
//! ```
//! Both sides treat the other as hostile: lengths are capped, strings are
//! validated UTF-8, unknown types are errors. On any error the receiver
//! drops its buffer and resyncs on the next magic.
#![forbid(unsafe_code)]

pub const MAGIC: [u8; 2] = *b"BW";
pub const VERSION: u8 = 1;
const HDR: usize = 8;
/// Absolute payload cap. Clipboard policy caps lower.
pub const MAX_PAYLOAD: usize = 64 << 20;
/// Port name the guest agent opens.
pub const AGENT_PORT: &str = "org.bulwark.agent.0";

/// Feature bits for Hello.
pub mod features {
    pub const CLIPBOARD_TEXT: u64 = 1;
    pub const TIME_SYNC: u64 = 2;
    pub const RESOLUTION: u64 = 4;
    pub const SHUTDOWN: u64 = 8;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello { features: u64, agent: String },
    /// Clipboard now holds this text (either direction).
    ClipboardText(String),
    /// Host -> guest: set the clock (after resume from snapshot).
    TimeSync { unix_secs: u64, nanos: u32 },
    /// Host -> guest: window resized; change the display mode.
    SetResolution { width: u32, height: u32 },
    /// Host -> guest: please shut down / reboot cleanly.
    Shutdown { reboot: bool },
    Ping(u64),
    Pong(u64),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("bad magic")]
    Magic,
    #[error("unsupported version {0}")]
    Version(u8),
    #[error("unknown message type {0}")]
    Type(u8),
    #[error("payload too large ({0} bytes)")]
    TooLarge(usize),
    #[error("malformed payload")]
    Malformed,
}

const T_HELLO: u8 = 1;
const T_CLIP: u8 = 2;
const T_TIME: u8 = 3;
const T_RES: u8 = 4;
const T_SHUTDOWN: u8 = 5;
const T_PING: u8 = 6;
const T_PONG: u8 = 7;

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let (ty, payload) = match self {
            Message::Hello { features, agent } => {
                let mut p = features.to_le_bytes().to_vec();
                p.extend_from_slice(agent.as_bytes());
                (T_HELLO, p)
            }
            Message::ClipboardText(s) => (T_CLIP, s.as_bytes().to_vec()),
            Message::TimeSync { unix_secs, nanos } => {
                let mut p = unix_secs.to_le_bytes().to_vec();
                p.extend_from_slice(&nanos.to_le_bytes());
                (T_TIME, p)
            }
            Message::SetResolution { width, height } => {
                let mut p = width.to_le_bytes().to_vec();
                p.extend_from_slice(&height.to_le_bytes());
                (T_RES, p)
            }
            Message::Shutdown { reboot } => (T_SHUTDOWN, vec![*reboot as u8]),
            Message::Ping(v) => (T_PING, v.to_le_bytes().to_vec()),
            Message::Pong(v) => (T_PONG, v.to_le_bytes().to_vec()),
        };
        let mut out = Vec::with_capacity(HDR + payload.len());
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.push(ty);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    fn decode(ty: u8, p: &[u8]) -> Result<Message, Error> {
        let u64_at = |o: usize| -> Result<u64, Error> {
            let b: [u8; 8] = p.get(o..o + 8).ok_or(Error::Malformed)?.try_into().map_err(|_| Error::Malformed)?;
            Ok(u64::from_le_bytes(b))
        };
        let u32_at = |o: usize| -> Result<u32, Error> {
            let b: [u8; 4] = p.get(o..o + 4).ok_or(Error::Malformed)?.try_into().map_err(|_| Error::Malformed)?;
            Ok(u32::from_le_bytes(b))
        };
        let text = |b: &[u8]| -> Result<String, Error> {
            let s = std::str::from_utf8(b).map_err(|_| Error::Malformed)?;
            Ok(s.replace('\0', ""))
        };
        Ok(match ty {
            T_HELLO => Message::Hello { features: u64_at(0)?, agent: text(p.get(8..).ok_or(Error::Malformed)?)? },
            T_CLIP => Message::ClipboardText(text(p)?),
            T_TIME if p.len() == 12 => {
                let nanos = u32_at(8)?;
                if nanos >= 1_000_000_000 {
                    return Err(Error::Malformed);
                }
                Message::TimeSync { unix_secs: u64_at(0)?, nanos }
            }
            T_RES if p.len() == 8 => {
                let (w, h) = (u32_at(0)?, u32_at(4)?);
                if w == 0 || h == 0 || w > 16384 || h > 16384 {
                    return Err(Error::Malformed);
                }
                Message::SetResolution { width: w, height: h }
            }
            T_SHUTDOWN if p.len() == 1 => Message::Shutdown { reboot: p[0] != 0 },
            T_PING if p.len() == 8 => Message::Ping(u64_at(0)?),
            T_PONG if p.len() == 8 => Message::Pong(u64_at(0)?),
            T_TIME | T_RES | T_SHUTDOWN | T_PING | T_PONG => return Err(Error::Malformed),
            t => return Err(Error::Type(t)),
        })
    }
}

/// Incremental decoder for a byte stream.
#[derive(Debug)]
pub struct Decoder {
    buf: Vec<u8>,
    max_payload: usize,
}

impl Decoder {
    pub fn new(max_payload: usize) -> Self {
        Decoder { buf: Vec::new(), max_payload: max_payload.min(MAX_PAYLOAD) }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Next complete message. `Ok(None)` = need more bytes. On `Err`, the
    /// offending bytes are skipped; calling again continues with the rest.
    pub fn next(&mut self) -> Result<Option<Message>, Error> {
        if self.buf.len() < HDR {
            return Ok(None);
        }
        if self.buf[0..2] != MAGIC {
            self.resync();
            return Err(Error::Magic);
        }
        if self.buf[2] != VERSION {
            let v = self.buf[2];
            self.resync();
            return Err(Error::Version(v));
        }
        let ty = self.buf[3];
        let len = u32::from_le_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
        if len > self.max_payload {
            self.resync();
            return Err(Error::TooLarge(len));
        }
        if self.buf.len() < HDR + len {
            return Ok(None);
        }
        let r = Message::decode(ty, &self.buf[HDR..HDR + len]);
        self.buf.drain(..HDR + len);
        r.map(Some)
    }

    /// Drop bytes until the next possible frame start.
    fn resync(&mut self) {
        let skip = self.buf[1..].windows(2).position(|w| w == MAGIC).map(|p| p + 1).unwrap_or(self.buf.len());
        self.buf.drain(..skip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Message> {
        vec![
            Message::Hello { features: 3, agent: "bulwark-agent 0.1".into() },
            Message::ClipboardText("héllo 🦀".into()),
            Message::ClipboardText(String::new()),
            Message::TimeSync { unix_secs: 1_800_000_000, nanos: 5 },
            Message::SetResolution { width: 1920, height: 1080 },
            Message::Shutdown { reboot: true },
            Message::Ping(42),
            Message::Pong(42),
        ]
    }

    #[test]
    fn roundtrip_byte_by_byte() {
        let mut d = Decoder::new(1 << 20);
        let mut got = Vec::new();
        for m in all() {
            for b in m.encode() {
                d.push(&[b]);
                if let Some(x) = d.next().unwrap() {
                    got.push(x);
                }
            }
        }
        assert_eq!(got, all());
    }

    #[test]
    fn limits_and_resync() {
        let mut d = Decoder::new(16);
        d.push(&Message::ClipboardText("x".repeat(17)).encode());
        assert_eq!(d.next(), Err(Error::TooLarge(17)));
        // garbage then a good frame
        let mut d = Decoder::new(1024);
        d.push(b"garbage!!");
        d.push(&Message::Ping(1).encode());
        let mut out = None;
        for _ in 0..10 {
            if let Ok(Some(m)) = d.next() {
                out = Some(m);
                break;
            }
        }
        assert_eq!(out, Some(Message::Ping(1)));
        // invalid utf-8 and NUL stripping
        let mut f = Message::ClipboardText("ab".into()).encode();
        f[8] = 0xff;
        let mut d = Decoder::new(1024);
        d.push(&f);
        assert_eq!(d.next(), Err(Error::Malformed));
        let mut d = Decoder::new(1024);
        d.push(&Message::ClipboardText("a\0b".into()).encode());
        assert_eq!(d.next(), Ok(Some(Message::ClipboardText("ab".into()))));
    }

    #[test]
    fn fuzz_never_panics() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..2000 {
            let mut d = Decoder::new(4096);
            let mut buf = vec![0u8; (x % 64) as usize];
            for b in buf.iter_mut() {
                x ^= x << 7;
                x ^= x >> 9;
                *b = x as u8;
            }
            if buf.len() > 3 && x % 3 == 0 {
                buf[0] = b'B';
                buf[1] = b'W';
                buf[2] = VERSION;
            }
            d.push(&buf);
            for _ in 0..8 {
                let _ = d.next();
            }
        }
    }
}
