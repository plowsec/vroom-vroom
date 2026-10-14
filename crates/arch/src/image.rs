use std::io::Read;

use crate::{Error, Result};

/// Kernel images bigger than this are refused (decompression bombs).
pub const MAX_KERNEL_SIZE: u64 = 256 << 20;

/// Kernels often ship gzip'd (`vmlinuz`). Inflate with a hard size cap.
pub fn decompress_if_needed(raw: Vec<u8>) -> Result<Vec<u8>> {
    if raw.len() < 2 || raw[0] != 0x1f || raw[1] != 0x8b {
        return Ok(raw);
    }
    let mut out = Vec::new();
    let mut dec = flate2::read::GzDecoder::new(&raw[..]).take(MAX_KERNEL_SIZE + 1);
    dec.read_to_end(&mut out).map_err(|e| Error::Kernel(format!("gunzip: {e}")))?;
    if out.len() as u64 > MAX_KERNEL_SIZE {
        return Err(Error::Kernel("decompressed kernel too large".into()));
    }
    Ok(out)
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
