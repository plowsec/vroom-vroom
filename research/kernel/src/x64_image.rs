//! AMD64-only static mapping and analysis PE reconstruction.
use std::path::{Path, PathBuf};
use vroom_kernel::windows::{Image, VirtualMemory};
use vroom_kernel::{Error, Result};
fn bad(s: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("AMD64 image: {s}"))
}
fn u16at(b: &[u8], n: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        b.get(n..n + 2)
            .ok_or_else(|| bad("truncated PE"))?
            .try_into()
            .map_err(|_| bad("invalid 16-bit field"))?,
    ))
}
fn u32at(b: &[u8], n: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(n..n + 4)
            .ok_or_else(|| bad("truncated PE"))?
            .try_into()
            .map_err(|_| bad("invalid 32-bit field"))?,
    ))
}
fn headers(b: &[u8]) -> Result<(usize, usize, usize)> {
    let pe = u32at(b, 60)? as usize;
    if b.get(..2) != Some(b"MZ")
        || !(64..=4096).contains(&pe)
        || b.get(pe..pe + 4) != Some(b"PE\0\0")
        || u16at(b, pe + 4)? != 0x8664
        || u16at(b, pe + 24)? != 0x20b
        || u16at(b, pe + 20)? < 240
    {
        return Err(bad("expected AMD64 PE32+"));
    }
    let count = u16at(b, pe + 6)? as usize;
    if !(1..=96).contains(&count) {
        return Err(bad("invalid section count"));
    }
    Ok((pe, pe + 24 + u16at(b, pe + 20)? as usize, count))
}
pub(super) struct Disk {
    pub image: Image,
    bytes: Vec<u8>,
}
impl VirtualMemory for Disk {
    fn read(&self, a: u64, b: &mut [u8]) -> Result<()> {
        let off = a
            .checked_sub(self.image.base)
            .ok_or_else(|| bad("address below image"))?;
        let end = off
            .checked_add(b.len() as u64)
            .ok_or_else(|| bad("address overflow"))?;
        if end > self.bytes.len() as u64 {
            return Err(bad("address outside image"));
        }
        b.copy_from_slice(&self.bytes[off as usize..end as usize]);
        Ok(())
    }
}
impl Disk {
    pub(super) fn load(path: &Path, expected: &Image) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(bad)?;
        let mut data = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(file, 128 * 1024 * 1024 + 1),
            &mut data,
        )
        .map_err(bad)?;
        if data.len() > 128 * 1024 * 1024 {
            return Err(bad("file exceeds 128 MiB"));
        }
        let (pe, table, count) = headers(&data)?;
        let size = u32at(&data, pe + 80)? as usize;
        let head = u32at(&data, pe + 84)? as usize;
        if !(4096..=64 * 1024 * 1024).contains(&size)
            || head > data.len()
            || head > size
            || head < table + count * 40
        {
            return Err(bad("invalid image/header size"));
        }
        let base = u32at(&data, pe + 48)? as u64 | ((u32at(&data, pe + 52)? as u64) << 32);
        base.checked_add(size as u64)
            .ok_or_else(|| bad("image range overflow"))?;
        let mut bytes = vec![0; size];
        bytes[..head].copy_from_slice(&data[..head]);
        let mut ranges = vec![(0usize, head)];
        for i in 0..count {
            let off = table + i * 40;
            let va = u32at(&data, off + 12)? as usize;
            let raw = u32at(&data, off + 20)? as usize;
            let n = u32at(&data, off + 16)? as usize;
            let vs = u32at(&data, off + 8)? as usize;
            let end = va
                .checked_add(n.max(vs))
                .ok_or_else(|| bad("section overflow"))?;
            if end > size || ranges.iter().any(|&(lo, hi)| va < hi && end > lo) {
                return Err(bad("overlapping/out-of-range section"));
            }
            ranges.push((va, end));
            let source = data
                .get(raw..raw.checked_add(n).ok_or_else(|| bad("raw overflow"))?)
                .ok_or_else(|| bad("truncated section"))?;
            bytes[va..va + n].copy_from_slice(source);
        }
        let mut disk = Self {
            bytes,
            image: expected.clone(),
        };
        disk.image.base = base;
        disk.image = Image::read_amd64(&disk, base)?;
        if disk.image.size != expected.size || disk.image.timestamp != expected.timestamp {
            return Err(bad("timestamp or SizeOfImage mismatch"));
        }
        match (&disk.image.pdb, &expected.pdb) {
            (Some(a), Some(b)) if a.guid == b.guid && a.age == b.age => (),
            _ => {
                return Err(bad(
                    "missing or mismatched CodeView GUID/age; cannot verify exact build",
                ))
            }
        }
        Ok(disk)
    }
}
pub(super) fn fetch(cache: &Path, image: &Image) -> Result<PathBuf> {
    let name = &image.name;
    if name.is_empty()
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err(bad("unsafe image filename"));
    }
    let key = format!("{:08X}{:x}", image.timestamp, image.size);
    let dir = cache.join("images").join(name).join(&key);
    let path = dir.join(name);
    if path.exists() {
        Disk::load(&path, image)?;
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
        let status = std::process::Command::new("curl")
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
        if !status.status.success() {
            return Err(bad(format!(
                "download failed: {}; supply a matching local path",
                String::from_utf8_lossy(&status.stderr)
            )));
        }
        file.sync_all().map_err(bad)?;
        Disk::load(&tmp, image)?;
        match std::fs::hard_link(&tmp, &path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Disk::load(&path, image)?;
            }
            Err(e) => return Err(bad(e)),
        }
        Ok(path)
    })();
    let _ = std::fs::remove_file(tmp);
    result
}

pub(super) fn export(
    image: &Image,
    m: &impl VirtualMemory,
    partial: bool,
) -> Result<(Vec<u8>, Vec<u32>)> {
    if !(4096..=64 * 1024 * 1024).contains(&image.size)
        || image.base.checked_add(image.size as u64).is_none()
    {
        return Err(bad("invalid export range (limit 64 MiB)"));
    }
    let mut bytes = vec![0; image.size as usize];
    let mut missing = Vec::new();
    for (i, page) in bytes.chunks_mut(4096).enumerate() {
        if m.read(image.base + i as u64 * 4096, page).is_err() {
            if !partial {
                return Err(bad("unreadable pages; allow_missing must be explicit"));
            }
            page.fill(0);
            missing.push(i as u32 * 4096);
        }
    }
    if missing.contains(&0) {
        return Err(bad("unreadable image header"));
    }
    let (pe, table, count) = headers(&bytes)?;
    let opt = pe + 24;
    if table + count * 40 > bytes.len()
        || missing.iter().any(|&p| (p as usize) < table + count * 40)
        || u32at(&bytes, opt + 56)? != image.size
        || u32at(&bytes, pe + 8)? != image.timestamp
    {
        return Err(bad("incomplete or changed PE headers"));
    }
    bytes[opt + 24..opt + 32].copy_from_slice(&image.base.to_le_bytes());
    let debug = u32at(&bytes, opt + 112 + 6 * 8)? as usize;
    let n = u32at(&bytes, opt + 116 + 6 * 8)? as usize;
    if n > 28 * 128 || n % 28 != 0 || debug.checked_add(n).map_or(true, |end| end > bytes.len()) {
        return Err(bad("invalid debug directory"));
    }
    // Never parse a zero-filled partial directory as genuine metadata.
    if missing
        .iter()
        .any(|&p| (p as usize) < debug + n && p as usize + 4096 > debug)
        && n != 0
    {
        return Err(bad("unreadable debug directory"));
    }
    for at in (debug..debug + n).step_by(28) {
        let r = u32at(&bytes, at + 20)?;
        bytes[at + 24..at + 28].copy_from_slice(&r.to_le_bytes());
    }
    bytes[opt + 112 + 4 * 8..opt + 112 + 6 * 8].fill(0);
    let flags = u16at(&bytes, pe + 22)? | 1;
    bytes[pe + 22..pe + 24].copy_from_slice(&flags.to_le_bytes());
    let dll = u16at(&bytes, opt + 70)? & !0x40;
    bytes[opt + 70..opt + 72].copy_from_slice(&dll.to_le_bytes());
    let alignment = u32at(&bytes, opt + 36)?;
    if !alignment.is_power_of_two() || alignment > 65536 {
        return Err(bad("invalid file alignment"));
    }
    for i in 0..count {
        let at = table + i * 40;
        let va = u32at(&bytes, at + 12)?;
        let size = u32at(&bytes, at + 8)?.max(u32at(&bytes, at + 16)?);
        let raw = size
            .checked_add(alignment - 1)
            .ok_or_else(|| bad("section overflow"))?
            & !(alignment - 1);
        if va as u64 + raw as u64 > image.size as u64 {
            return Err(bad("section exceeds image"));
        }
        bytes[at + 16..at + 20].copy_from_slice(&raw.to_le_bytes());
        bytes[at + 20..at + 24].copy_from_slice(&va.to_le_bytes());
    }
    Ok((bytes, missing))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Memory {
        bytes: Vec<u8>,
        hole: bool,
    }
    impl VirtualMemory for Memory {
        fn read(&self, a: u64, b: &mut [u8]) -> Result<()> {
            if self.hole && a >= 4096 {
                return Err(bad("unmapped"));
            }
            b.copy_from_slice(
                self.bytes
                    .get(a as usize..a as usize + b.len())
                    .ok_or_else(|| bad("unmapped"))?,
            );
            Ok(())
        }
    }
    fn set16(m: &mut Memory, a: usize, v: u16) {
        m.bytes[a..a + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn set32(m: &mut Memory, a: usize, v: u32) {
        m.bytes[a..a + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn setup() -> (Memory, Image) {
        let mut m = Memory {
            bytes: vec![0; 8192],
            hole: false,
        };
        m.bytes[..2].copy_from_slice(b"MZ");
        set32(&mut m, 60, 128);
        m.bytes[128..132].copy_from_slice(b"PE\0\0");
        set16(&mut m, 132, 0x8664);
        set16(&mut m, 134, 1);
        set16(&mut m, 148, 240);
        set16(&mut m, 152, 0x20b);
        set32(&mut m, 152 + 36, 512);
        set32(&mut m, 152 + 56, 8192);
        set32(&mut m, 392 + 8, 512);
        set32(&mut m, 392 + 12, 4096);
        let im = Image {
            base: 0,
            size: 8192,
            timestamp: 0,
            image_base: 0,
            name: "test.exe".into(),
            pdb: None,
            exports: Default::default(),
            relocation_directory: (0, 0),
        };
        (m, im)
    }
    #[test]
    fn export_is_amd64_only_and_reports_holes() {
        let (mut m, im) = setup();
        let (flat, missing) = export(&im, &m, false).unwrap();
        assert!(missing.is_empty());
        assert_eq!(u16at(&flat, 132).unwrap(), 0x8664);
        assert_eq!(u32at(&flat, 392 + 20).unwrap(), 4096);
        m.hole = true;
        assert!(export(&im, &m, false).is_err());
        let (_, missing) = export(&im, &m, true).unwrap();
        assert_eq!(missing, vec![4096]);
        set16(&mut m, 132, 0xaa64);
        assert!(export(&im, &m, true).is_err());
    }
    #[test]
    fn malformed_directories_and_changed_headers_fail_closed() {
        let (mut m, im) = setup();
        set32(&mut m, 152 + 116 + 6 * 8, 29);
        assert!(export(&im, &m, false).is_err());
        set32(&mut m, 152 + 116 + 6 * 8, 0);
        set32(&mut m, 136, 1);
        assert!(export(&im, &m, false).is_err());
    }
    #[test]
    fn static_mapping_checks_architecture_and_exact_codeview_build() {
        let (mut m, _) = setup();
        set32(&mut m, 152 + 60, 1024);
        set32(&mut m, 152 + 108, 16);
        set32(&mut m, 392 + 16, 512);
        set32(&mut m, 392 + 20, 1024);
        set32(&mut m, 152 + 112 + 6 * 8, 448);
        set32(&mut m, 152 + 116 + 6 * 8, 28);
        set32(&mut m, 448 + 12, 2);
        set32(&mut m, 448 + 16, 35);
        set32(&mut m, 448 + 20, 512);
        m.bytes[512..516].copy_from_slice(b"RSDS");
        set32(&mut m, 532, 1);
        m.bytes[536..547].copy_from_slice(b"sample.pdb\0");
        m.bytes[4096] = 0xc3;
        let expected = Image::read_amd64(&m, 0).unwrap();
        let mut file = m.bytes[..1024].to_vec();
        file.extend_from_slice(&m.bytes[4096..4608]);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = PathBuf::from(format!(
            "/tmp/opencode/x64-image-test-{}-{stamp}.exe",
            std::process::id()
        ));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut out, &file).unwrap();
        drop(out);
        let disk = Disk::load(&path, &expected).unwrap();
        assert_eq!(disk.bytes[4096], 0xc3);
        let mut wrong = expected.clone();
        wrong.pdb.as_mut().unwrap().age += 1;
        assert!(Disk::load(&path, &wrong).is_err());
        file[132..134].copy_from_slice(&0xaa64u16.to_le_bytes());
        std::fs::write(&path, &file).unwrap();
        assert!(Disk::load(&path, &expected).is_err());
    }
}
