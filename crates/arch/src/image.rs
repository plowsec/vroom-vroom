use std::io::Read;

use crate::{Error, Result};

/// Kernel images bigger than this are refused (decompression bombs).
pub const MAX_KERNEL_SIZE: u64 = 256 << 20;

/// Kernels ship in a few wrappers. Unwrap to a raw image:
/// - gzip'd `vmlinuz`
/// - EFI zboot (`MZ....zimg`, PE app with a compressed payload)
///
/// Decompression has a hard output cap (no decompression bombs).
pub fn decompress_if_needed(raw: Vec<u8>) -> Result<Vec<u8>> {
    if is_gzip(&raw) {
        return gunzip(&raw);
    }
    if raw.len() >= 32 && &raw[0..2] == b"MZ" && &raw[4..8] == b"zimg" {
        let off = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]) as usize;
        let len = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]) as usize;
        let end = off.checked_add(len).filter(|&e| e <= raw.len());
        let Some(end) = end else {
            return Err(Error::Kernel("zboot payload out of bounds".into()));
        };
        let comp = &raw[24..32];
        let payload = &raw[off..end];
        return match comp.split(|&b| b == 0).next().unwrap_or(&[]) {
            b"gzip" => gunzip(payload),
            b"zstd" => unzstd(payload),
            other => Err(Error::Kernel(format!(
                "zboot compression {:?} not supported",
                String::from_utf8_lossy(other)
            ))),
        };
    }
    Ok(raw)
}

fn is_gzip(b: &[u8]) -> bool {
    b.len() >= 2 && b[0] == 0x1f && b[1] == 0x8b
}

fn capped<R: Read>(r: R, what: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    r.take(MAX_KERNEL_SIZE + 1)
        .read_to_end(&mut out)
        .map_err(|e| Error::Kernel(format!("{what}: {e}")))?;
    if out.len() as u64 > MAX_KERNEL_SIZE {
        return Err(Error::Kernel("decompressed kernel too large".into()));
    }
    Ok(out)
}

fn gunzip(b: &[u8]) -> Result<Vec<u8>> {
    capped(flate2::read::GzDecoder::new(b), "gunzip")
}

fn unzstd(b: &[u8]) -> Result<Vec<u8>> {
    let d = ruzstd::decoding::StreamingDecoder::new(b)
        .map_err(|e| Error::Kernel(format!("zstd: {e}")))?;
    capped(d, "zstd")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn gunzip_roundtrip() {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(b"kernel bytes").unwrap();
        let gz = e.finish().unwrap();
        assert_eq!(decompress_if_needed(gz).unwrap(), b"kernel bytes");
        assert_eq!(decompress_if_needed(b"plain".to_vec()).unwrap(), b"plain");
    }
}
