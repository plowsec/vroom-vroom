//! Exact-build disk images for static analysis. Never substituted for live RAM.
use crate::windows::{Image, VirtualMemory};
use crate::{Error, Result};
use std::path::{Path, PathBuf};

fn bad(e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("binary: {e}"))
}
fn word(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset + 4)
            .ok_or_else(|| bad("truncated PE"))?
            .try_into()
            .expect("four-byte slice"),
    ))
}
#[derive(Debug)]
pub struct DiskImage {
    pub bytes: Vec<u8>,
    pub image: Image,
}
impl VirtualMemory for DiskImage {
    fn read(&self, address: u64, data: &mut [u8]) -> Result<()> {
        let offset = address
            .checked_sub(self.image.base)
            .ok_or_else(|| bad("address below image"))? as usize;
        let end = offset
            .checked_add(data.len())
            .ok_or_else(|| bad("address overflow"))?;
        data.copy_from_slice(
            self.bytes
                .get(offset..end)
                .ok_or_else(|| bad("address outside image"))?,
        );
        Ok(())
    }
}
impl DiskImage {
    pub fn load(path: &Path, expected: &Image) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(bad)?;
        let mut data = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(file, 128 * 1024 * 1024 + 1),
            &mut data,
        )
        .map_err(bad)?;
        if data.len() > 128 * 1024 * 1024 || data.get(..2) != Some(b"MZ") {
            return Err(bad("invalid file size/header"));
        }
        let pe = word(&data, 0x3c)? as usize;
        if !(64..=4096).contains(&pe) || data.get(pe..pe + 4) != Some(b"PE\0\0") {
            return Err(bad("invalid PE header"));
        }
        let header = word(&data, pe + 4)?;
        let count = header >> 16;
        let size = word(&data, pe + 80)?;
        let timestamp = word(&data, pe + 8)?;
        if header & 65535 != 0xaa64
            || !(1..=96).contains(&count)
            || size != expected.size
            || timestamp != expected.timestamp
            || !(4096..=64 * 1024 * 1024).contains(&size)
        {
            return Err(bad("architecture, timestamp or SizeOfImage mismatch"));
        }
        let optional = (word(&data, pe + 20)? & 65535) as usize;
        if optional < 240 || word(&data, pe + 24)? & 65535 != 0x20b {
            return Err(bad("expected PE32+"));
        }
        let base = word(&data, pe + 48)? as u64 | ((word(&data, pe + 52)? as u64) << 32);
        let headers = word(&data, pe + 84)? as usize;
        if headers > size as usize
            || headers < pe + 24 + optional + count as usize * 40
            || headers > data.len()
        {
            return Err(bad("invalid header size"));
        }
        let mut bytes = vec![0; size as usize];
        bytes[..headers].copy_from_slice(&data[..headers]);
        let mut ranges = vec![(0usize, headers)];
        for i in 0..count as usize {
            let entry = pe + 24 + optional + i * 40;
            let va = word(&data, entry + 12)? as usize;
            let raw_size = word(&data, entry + 16)? as usize;
            let raw = word(&data, entry + 20)? as usize;
            let virtual_size = word(&data, entry + 8)? as usize;
            let end = va
                .checked_add(raw_size.max(virtual_size))
                .ok_or_else(|| bad("section overflow"))?;
            if end > bytes.len() || ranges.iter().any(|&(lo, hi)| va < hi && end > lo) {
                return Err(bad("overlapping/out-of-range section"));
            }
            ranges.push((va, end));
            let source = data
                .get(
                    raw..raw
                        .checked_add(raw_size)
                        .ok_or_else(|| bad("raw overflow"))?,
                )
                .ok_or_else(|| bad("truncated section"))?;
            bytes[va..va + raw_size].copy_from_slice(source);
        }
        let mut loaded = Self {
            bytes,
            image: Image {
                base,
                ..expected.clone()
            },
        };
        let actual = Image::read(&loaded, base)?;
        match (&actual.pdb, &expected.pdb) {
            (Some(a), Some(b)) if a.guid == b.guid && a.age == b.age => (),
            _ => {
                return Err(bad(
                    "CodeView GUID/age mismatch or absent; cannot verify exact build",
                ))
            }
        }
        loaded.image = actual;
        Ok(loaded)
    }
}

pub fn fetch(cache: &Path, image: &Image) -> Result<PathBuf> {
    let name = &image.name;
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(bad("unsafe image filename"));
    }
    // Same index convention as Microsoft's PerfView SymbolReader.
    let key = format!("{:08X}{:x}", image.timestamp, image.size);
    let dir = cache.join("images").join(name).join(&key);
    let path = dir.join(name);
    if path.exists() {
        DiskImage::load(&path, image)?;
        return Ok(path);
    }
    std::fs::create_dir_all(&dir).map_err(bad)?;
    let tmp = dir.join(format!("download-{}.part", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(bad)?;
    let result = (|| {
        let response = std::process::Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--max-redirs",
                "3",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--connect-timeout",
                "10",
                "--max-time",
                "120",
                "--max-filesize",
                "134217728",
            ])
            .arg(format!(
                "https://msdl.microsoft.com/download/symbols/{name}/{key}/{name}"
            ))
            .stdout(file.try_clone().map_err(bad)?)
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(bad)?;
        if !response.status.success() {
            return Err(bad(format!(
                "image unavailable: {}; supply a matching local file",
                String::from_utf8_lossy(&response.stderr).trim()
            )));
        }
        file.sync_all().map_err(bad)?;
        DiskImage::load(&tmp, image)?;
        match std::fs::hard_link(&tmp, &path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                DiskImage::load(&path, image)?;
            }
            Err(e) => return Err(bad(e)),
        }
        Ok(path)
    })();
    let _ = std::fs::remove_file(tmp);
    result
}
