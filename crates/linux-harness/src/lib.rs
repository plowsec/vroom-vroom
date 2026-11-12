//! Bounded serial evidence shared by the native Linux harness and tests.
#![forbid(unsafe_code)]
pub mod arm64_boot;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

pub const SERIAL_LIMIT: usize = 128 * 1024;

#[derive(Debug)]
pub struct SerialEvidence {
    prefix: String,
    line: Vec<u8>,
    line_overflow: bool,
    pub bytes: Vec<u8>,
    pub heartbeats: u64,
    pub error: Option<String>,
}

impl SerialEvidence {
    pub fn new(token: &str) -> Result<Self, String> {
        if token.len() != 32
            || !token
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err("token must be exactly 32 lowercase hexadecimal characters".into());
        }
        Ok(Self {
            prefix: format!("VROOM_HEARTBEAT:{token}:"),
            line: Vec::new(),
            line_overflow: false,
            bytes: Vec::new(),
            heartbeats: 0,
            error: None,
        })
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > SERIAL_LIMIT {
            self.error = Some("serial output limit exceeded".into());
            return Err(io::Error::other("serial output limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        for &byte in bytes {
            if byte == b'\n' {
                if !self.line_overflow {
                    let line = self.line.strip_suffix(b"\r").unwrap_or(&self.line);
                    if let Some(value) = line.strip_prefix(self.prefix.as_bytes()) {
                        let sequence = std::str::from_utf8(value)
                            .ok()
                            .and_then(|s| s.parse::<u64>().ok());
                        if sequence == Some(self.heartbeats)
                            && value == self.heartbeats.to_string().as_bytes()
                        {
                            self.heartbeats += 1;
                        } else {
                            self.error =
                                Some("heartbeat sequence malformed, repeated or skipped".into());
                        }
                    }
                }
                self.line.clear();
                self.line_overflow = false;
            } else if self.line.len() < 256 {
                self.line.push(byte);
            } else {
                self.line_overflow = true;
            }
        }
        Ok(bytes.len())
    }
}

#[derive(Clone, Debug)]
pub struct SerialSink(pub Arc<Mutex<SerialEvidence>>);
impl Write for SerialSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("serial evidence lock poisoned"))?
            .append(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    #[test]
    fn fragmented_lines_require_exact_token_sequence_and_newline() {
        let mut e = SerialEvidence::new(TOKEN).unwrap();
        e.append(b"kernel boot\nVROOM_HEARTBEAT:another-token:0\n")
            .unwrap();
        let line = format!("VROOM_HEARTBEAT:{TOKEN}:0\r\n");
        for byte in line.as_bytes() {
            e.append(&[*byte]).unwrap();
        }
        assert_eq!(e.heartbeats, 1);
        assert!(e.error.is_none());
        e.append(format!("VROOM_HEARTBEAT:{TOKEN}:1").as_bytes())
            .unwrap();
        assert_eq!(e.heartbeats, 1);
        e.append(b"\n").unwrap();
        assert_eq!(e.heartbeats, 2);
    }
    #[test]
    fn replay_and_noncanonical_sequences_fail() {
        for value in ["1", "00", "+0", "-1", "18446744073709551616"] {
            let mut e = SerialEvidence::new(TOKEN).unwrap();
            e.append(format!("VROOM_HEARTBEAT:{TOKEN}:{value}\n").as_bytes())
                .unwrap();
            assert!(e.error.is_some());
            assert_eq!(e.heartbeats, 0);
        }
        let mut e = SerialEvidence::new(TOKEN).unwrap();
        for _ in 0..2 {
            e.append(format!("VROOM_HEARTBEAT:{TOKEN}:0\n").as_bytes())
                .unwrap();
        }
        assert!(e.error.is_some());
    }
    #[test]
    fn serial_and_token_bounds_fail_closed() {
        assert!(SerialEvidence::new("../bad").is_err());
        let mut e = SerialEvidence::new(TOKEN).unwrap();
        e.append(&vec![b'x'; SERIAL_LIMIT]).unwrap();
        assert!(e.append(b"x").is_err());
        assert!(e.error.is_some());
        assert_eq!(e.bytes.len(), SERIAL_LIMIT);
    }
}
