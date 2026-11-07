//! Disposable Linux firmware/TPM reference fixture, not a production TPM stack.
pub mod eventlog;
pub mod tpm;

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub stage: &'static str,
    pub rc: Option<u32>,
}
pub type Result<T> = std::result::Result<T, Failure>;
pub fn fail(stage: &'static str) -> Failure {
    Failure { stage, rc: None }
}
pub fn require(ok: bool, stage: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(fail(stage))
    }
}
pub fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Cursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    pub fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(count)
            .ok_or_else(|| fail("length_overflow"))?;
        let result = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| fail("truncated_record"))?;
        self.pos = end;
        Ok(result)
    }
    pub fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn be16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn be32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn le16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn le32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn le64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn blob(&mut self, max: usize) -> Result<&'a [u8]> {
        let size = self.be16()? as usize;
        require(size <= max, "oversized_blob")?;
        self.take(size)
    }
    pub fn left(&self) -> usize {
        self.bytes.len() - self.pos
    }
    pub fn finish(self) -> Result<()> {
        require(self.left() == 0, "trailing_record_bytes")
    }
}

pub fn token(bytes: &[u8]) -> Result<[u8; 32]> {
    require(
        bytes.len() == 32
            && bytes
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b)),
        "invalid_token",
    )?;
    Ok(bytes.try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_and_cursor_fail_closed() {
        assert!(token(b"0123456789abcdef0123456789abcdef").is_ok());
        for bad in [
            b"x".as_slice(),
            b"0123456789abcdef0123456789abcdeF",
            b"0123456789abcdef0123456789abcdef\n",
        ] {
            assert!(token(bad).is_err());
        }
        let mut c = Cursor::new(&[0, 4, 1]);
        assert!(c.blob(8).is_err());
        assert!(Cursor::new(&[1]).finish().is_err());
        assert!(Cursor::new(&[]).take(usize::MAX).is_err());
    }
}
