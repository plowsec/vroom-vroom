use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

pub const FORMAT_VERSION: u32 = 1;
const MANIFEST: &str = "manifest.json";
/// Refuse absurd blobs from a hostile snapshot dir (RAM goes through streams).
/// Device blobs are read into memory. Old flash snapshots (JSON) get big.
/// Device state can hold a 3D desktop's GPU contents (textures read back).
const MAX_BLOB: u64 = 4 << 30;
/// A manifest is a config and a list of files: small. The app reads them
/// from dirs a (possibly compromised) VMM can write.
const MAX_MANIFEST: u64 = 16 << 20;

/// A RAM range stored in `memory.bin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRange {
    pub gpa: u64,
    pub size: u64,
    pub offset: u64,
}

/// Index of a snapshot. Written last, atomically: no manifest, no snapshot.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    pub created_unix: u64,
    pub description: String,
    pub arch: String,
    pub backend: String,
    /// VM config at snapshot time, opaque here.
    pub config: serde_json::Value,
    pub memory: Vec<MemoryRange>,
    /// blob name -> sha256 hex. Covers memory.bin too.
    pub files: BTreeMap<String, String>,
    /// Arbitrary small metadata (vCPU count, disk overlay names...).
    pub meta: BTreeMap<String, String>,
}

/// A snapshot directory being written or read.
#[derive(Debug)]
pub struct SnapshotDir {
    root: PathBuf,
    pub manifest: Manifest,
}

fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._@-".contains(&b));
    if ok && name != MANIFEST {
        Ok(())
    } else {
        Err(Error::Invalid(format!("bad blob name {name:?}")))
    }
}

/// Writer that hashes what goes through it.
#[derive(Debug)]
pub struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    pub written: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl SnapshotDir {
    /// Start a new snapshot. The directory must not exist yet.
    pub fn create(root: &Path) -> Result<Self> {
        fs::create_dir_all(root.parent().unwrap_or(Path::new(".")))?;
        fs::create_dir(root)?;
        set_private(root)?;
        let created_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(SnapshotDir {
            root: root.to_path_buf(),
            manifest: Manifest { format: FORMAT_VERSION, created_unix, ..Default::default() },
        })
    }

    /// Open an existing, complete snapshot.
    pub fn open(root: &Path) -> Result<Self> {
        // No links, and a FIFO planted there must not block the reader.
        let f = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root.join(MANIFEST))?;
        let md = f.metadata()?;
        if !md.is_file() {
            return Err(Error::Invalid("manifest is not a file".into()));
        }
        if md.len() > MAX_MANIFEST {
            return Err(Error::Invalid("manifest too large".into()));
        }
        // take(): the file can grow after the size check.
        let manifest: Manifest =
            serde_json::from_reader(BufReader::new(f.take(MAX_MANIFEST))).map_err(|e| Error::Decode(e.to_string()))?;
        if manifest.format != FORMAT_VERSION {
            return Err(Error::Invalid(format!("unsupported format {}", manifest.format)));
        }
        for name in manifest.files.keys() {
            check_name(name)?;
        }
        Ok(SnapshotDir { root: root.to_path_buf(), manifest })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn put_blob(&mut self, name: &str, data: &[u8]) -> Result<()> {
        let mut w = self.stream_writer(name)?;
        w.write_all(data)?;
        self.finish_stream(name, w)
    }

    pub fn get_blob(&self, name: &str) -> Result<Vec<u8>> {
        check_name(name)?;
        let want = self
            .manifest
            .files
            .get(name)
            .ok_or_else(|| Error::Invalid(format!("missing blob {name}")))?;
        let f = File::open(self.root.join(name))?;
        if f.metadata()?.len() > MAX_BLOB {
            return Err(Error::Invalid(format!("blob {name} too large")));
        }
        let mut data = Vec::new();
        BufReader::new(f).read_to_end(&mut data)?;
        let got = hex::encode(Sha256::digest(&data));
        if &got != want {
            return Err(Error::Invalid(format!("blob {name} hash mismatch")));
        }
        Ok(data)
    }

    /// For big streams (RAM). Call [`finish_stream`] to record the hash.
    pub fn stream_writer(&self, name: &str) -> Result<HashingWriter<BufWriter<File>>> {
        check_name(name)?;
        let mut o = fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let f = o.open(self.root.join(name))?;
        Ok(HashingWriter { inner: BufWriter::new(f), hasher: Sha256::new(), written: 0 })
    }

    pub fn finish_stream(&mut self, name: &str, mut w: HashingWriter<BufWriter<File>>) -> Result<()> {
        w.flush()?;
        w.inner.get_ref().sync_all()?;
        let digest = hex::encode(w.hasher.finalize());
        self.manifest.files.insert(name.to_string(), digest);
        Ok(())
    }

    /// Verify a streamed blob's hash, then hand back a reader positioned at 0.
    pub fn verified_reader(&self, name: &str) -> Result<BufReader<File>> {
        check_name(name)?;
        let want = self
            .manifest
            .files
            .get(name)
            .ok_or_else(|| Error::Invalid(format!("missing blob {name}")))?;
        let mut r = BufReader::with_capacity(1 << 20, File::open(self.root.join(name))?);
        let mut h = Sha256::new();
        std::io::copy(&mut r, &mut h)?;
        if &hex::encode(h.finalize()) != want {
            return Err(Error::Invalid(format!("blob {name} hash mismatch")));
        }
        Ok(BufReader::with_capacity(1 << 20, File::open(self.root.join(name))?))
    }

    /// Write the manifest atomically. Snapshot is complete after this.
    pub fn commit(&self) -> Result<()> {
        let tmp = self.root.join(".manifest.tmp");
        let mut o = fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = o.open(&tmp)?;
        serde_json::to_writer_pretty(&mut f, &self.manifest)
            .map_err(|e| Error::Encode(e.to_string()))?;
        f.sync_all()?;
        fs::rename(&tmp, self.root.join(MANIFEST))?;
        Ok(())
    }
}

#[cfg(unix)]
fn set_private(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Snapshots contain guest RAM (keys, passwords). Owner only.
    fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private(_p: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tamper_detection() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("snap1");
        let mut s = SnapshotDir::create(&p).unwrap();
        s.put_blob("dev.pl011@9000000", b"hello").unwrap();
        let mut w = s.stream_writer("memory.bin").unwrap();
        w.write_all(&[7u8; 4096]).unwrap();
        s.finish_stream("memory.bin", w).unwrap();
        s.manifest.description = "test".into();
        s.commit().unwrap();

        let r = SnapshotDir::open(&p).unwrap();
        assert_eq!(r.get_blob("dev.pl011@9000000").unwrap(), b"hello");
        let mut buf = Vec::new();
        r.verified_reader("memory.bin").unwrap().read_to_end(&mut buf).unwrap();
        assert_eq!(buf.len(), 4096);

        fs::write(p.join("dev.pl011@9000000"), b"hellO").unwrap();
        assert!(r.get_blob("dev.pl011@9000000").is_err());
    }

    #[test]
    fn hostile_manifest_files() {
        let td = tempfile::tempdir().unwrap();
        // A FIFO: opening it must not hang.
        let fifo = td.path().join("fifo");
        fs::create_dir(&fifo).unwrap();
        assert!(std::process::Command::new("mkfifo").arg(fifo.join(MANIFEST)).status().unwrap().success());
        assert!(SnapshotDir::open(&fifo).is_err());
        // A link to some other file.
        let link = td.path().join("link");
        fs::create_dir(&link).unwrap();
        fs::write(td.path().join("elsewhere.json"), "{}").unwrap();
        std::os::unix::fs::symlink(td.path().join("elsewhere.json"), link.join(MANIFEST)).unwrap();
        assert!(SnapshotDir::open(&link).is_err());
    }

    #[test]
    fn names_are_sanitized() {
        for bad in ["../x", "a/b", "", ".hidden", "manifest.json", "x\0"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert!(check_name("virtio-blk@a000000.state").is_ok());
    }

    #[test]
    fn incomplete_snapshot_not_openable() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("s");
        let mut s = SnapshotDir::create(&p).unwrap();
        s.put_blob("a", b"1").unwrap();
        assert!(SnapshotDir::open(&p).is_err());
    }
}
