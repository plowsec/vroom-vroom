//! qcow2 (v2 read, v3 read/write).
//!
//! Supported: standard clusters, zero clusters, compressed clusters (read),
//! backing files, cluster allocation with 16-bit refcounts.
//! Not supported (refused at open): encryption, external data files,
//! extended L2, internal snapshots on writable images, dirty images.
//!
//! The header comes from a file the user may have downloaded: every field
//! is range-checked, and backing files must live in allowed directories.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{check_range, open_file, DiskImage, Error, Format, Result};

pub const MAGIC: u32 = 0x5146_49fb; // "QFI\xfb"
const L1_COPIED: u64 = 1 << 63;
const L2_COPIED: u64 = 1 << 63;
const L2_COMPRESSED: u64 = 1 << 62;
const L2_ZERO: u64 = 1;
const OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;

const INCOMPAT_DIRTY: u64 = 1;
const INCOMPAT_CORRUPT: u64 = 2;
const INCOMPAT_EXTERNAL_DATA: u64 = 4;
const INCOMPAT_COMPRESSION_TYPE: u64 = 8;
const INCOMPAT_EXTENDED_L2: u64 = 16;

const EXT_END: u32 = 0;
const EXT_BACKING_FORMAT: u32 = 0xe279_2aca;

/// Max virtual size we accept (64 TiB).
const MAX_SIZE: u64 = 64 << 40;
/// Max backing chain depth.
const MAX_CHAIN: u32 = 16;
/// L2 tables kept in memory.
const L2_CACHE: usize = 128;

/// Where backing files may come from.
#[derive(Debug, Clone, Default)]
pub struct OpenPolicy {
    /// Extra directories backing files may live in (besides the image's own).
    pub allowed_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct Header {
    version: u32,
    backing_file: Option<String>,
    backing_format: Option<String>,
    cluster_bits: u32,
    size: u64,
    l1_size: u32,
    l1_offset: u64,
    refcount_table_offset: u64,
    refcount_table_clusters: u32,
    nb_snapshots: u32,
    zstd: bool,
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn be64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_be_bytes(a)
}

fn bad(m: impl Into<String>) -> Error {
    Error::Format(m.into())
}

fn parse_header(f: &File, file_len: u64) -> Result<Header> {
    let mut b = vec![0u8; 4096.min(file_len as usize).max(72)];
    f.read_exact_at(&mut b[..72], 0).map_err(|_| bad("file too small for qcow2 header"))?;
    if be32(&b, 0) != MAGIC {
        return Err(bad("not qcow2"));
    }
    let version = be32(&b, 4);
    if version != 2 && version != 3 {
        return Err(bad(format!("qcow2 version {version} not supported")));
    }
    let cluster_bits = be32(&b, 20);
    if !(9..=21).contains(&cluster_bits) {
        return Err(bad("cluster_bits out of range"));
    }
    let cs = 1u64 << cluster_bits;
    if be32(&b, 32) != 0 {
        return Err(bad("encrypted qcow2 not supported"));
    }
    let size = be64(&b, 24);
    if size > MAX_SIZE {
        return Err(bad("virtual size too large"));
    }
    let l1_size = be32(&b, 36);
    let l2_entries = cs / 8;
    let needed_l1 = size.div_ceil(cs * l2_entries);
    if (l1_size as u64) < needed_l1 || l1_size as u64 > needed_l1 + 1024 {
        return Err(bad("l1_size inconsistent with virtual size"));
    }
    let l1_offset = be64(&b, 40);
    let refcount_table_offset = be64(&b, 48);
    let refcount_table_clusters = be32(&b, 56);
    let nb_snapshots = be32(&b, 60);
    for (name, off) in [("l1", l1_offset), ("refcount table", refcount_table_offset)] {
        if off % cs != 0 || off >= file_len {
            return Err(bad(format!("{name} offset invalid")));
        }
    }
    if refcount_table_clusters == 0 || refcount_table_clusters > 1 << 16 {
        return Err(bad("refcount table size invalid"));
    }

    let mut zstd = false;
    let mut header_len = 72u64;
    if version == 3 {
        f.read_exact_at(&mut b[..104], 0).map_err(|_| bad("v3 header truncated"))?;
        let incompat = be64(&b, 72);
        let known = INCOMPAT_DIRTY | INCOMPAT_CORRUPT | INCOMPAT_EXTERNAL_DATA | INCOMPAT_COMPRESSION_TYPE | INCOMPAT_EXTENDED_L2;
        if incompat & !known != 0 {
            return Err(bad(format!("unknown incompatible features {incompat:#x}")));
        }
        if incompat & INCOMPAT_DIRTY != 0 {
            return Err(bad("image is dirty (unclean shutdown); run `qemu-img check -r all`"));
        }
        if incompat & INCOMPAT_CORRUPT != 0 {
            return Err(bad("image is marked corrupt"));
        }
        if incompat & (INCOMPAT_EXTERNAL_DATA | INCOMPAT_EXTENDED_L2) != 0 {
            return Err(bad("external data files / extended L2 not supported"));
        }
        if be32(&b, 96) != 4 {
            return Err(bad("only 16-bit refcounts supported"));
        }
        header_len = be32(&b, 100) as u64;
        if !(104..=4096).contains(&header_len) || header_len % 8 != 0 {
            return Err(bad("header_length invalid"));
        }
        if incompat & INCOMPAT_COMPRESSION_TYPE != 0 {
            let mut t = [0u8; 1];
            f.read_exact_at(&mut t, 104)?;
            zstd = match t[0] {
                0 => false,
                1 => true,
                _ => return Err(bad("unknown compression type")),
            };
        }
    }

    // Header extensions.
    let mut backing_format = None;
    let mut off = header_len;
    for _ in 0..64 {
        let mut e = [0u8; 8];
        if f.read_exact_at(&mut e, off).is_err() {
            break;
        }
        let (ty, len) = (be32(&e, 0), be32(&e, 4) as u64);
        if ty == EXT_END {
            break;
        }
        if len > 4096 || off + 8 + len > cs {
            return Err(bad("header extension too large"));
        }
        if ty == EXT_BACKING_FORMAT {
            let mut v = vec![0u8; len as usize];
            f.read_exact_at(&mut v, off + 8)?;
            backing_format = Some(String::from_utf8(v).map_err(|_| bad("backing format not utf-8"))?);
        }
        off += 8 + len.div_ceil(8) * 8;
    }

    let bf_off = be64(&b, 8);
    let bf_len = be32(&b, 16) as u64;
    let backing_file = if bf_off != 0 && bf_len != 0 {
        if bf_len > 1023 || bf_off + bf_len > cs {
            return Err(bad("backing file name invalid"));
        }
        let mut v = vec![0u8; bf_len as usize];
        f.read_exact_at(&mut v, bf_off)?;
        Some(String::from_utf8(v).map_err(|_| bad("backing file name not utf-8"))?)
    } else {
        None
    };

    Ok(Header {
        version,
        backing_file,
        backing_format,
        cluster_bits,
        size,
        l1_size,
        l1_offset,
        refcount_table_offset,
        refcount_table_clusters,
        nb_snapshots,
        zstd,
    })
}

/// Directory containing `p` ("." for bare file names).
fn dir_of(p: &Path) -> &Path {
    p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."))
}

/// Resolve and vet a backing file reference.
fn resolve_backing(image: &Path, name: &str, policy: &OpenPolicy) -> Result<PathBuf> {
    if name.contains('\0') || name.contains(':') && !name.starts_with('/') && name.split(':').next().is_some_and(|p| !p.contains('/')) {
        // "nbd:...", "http:..." style protocol strings: never.
        return Err(bad(format!("backing file {name:?} uses a protocol prefix")));
    }
    let dir = dir_of(image);
    let p = Path::new(name);
    let p = if p.is_absolute() { p.to_path_buf() } else { dir.join(p) };
    let real = p.canonicalize().map_err(|e| bad(format!("backing file {}: {e}", p.display())))?;
    if !real.is_file() {
        return Err(bad(format!("backing file {} is not a regular file", real.display())));
    }
    let own = dir.canonicalize()?;
    let ok = real.parent().is_some_and(|parent| {
        parent == own || policy.allowed_dirs.iter().any(|a| a.canonicalize().is_ok_and(|a| parent.starts_with(a)))
    });
    if !ok {
        return Err(bad(format!(
            "backing file {} is outside the allowed directories",
            real.display()
        )));
    }
    Ok(real)
}

struct State {
    l1: Vec<u64>,
    l2_cache: HashMap<u64, Vec<u64>>,
    refcount_table: Vec<u64>,
    /// Next free host offset (end of file, cluster aligned).
    next_free: u64,
    /// Last decompressed cluster (host offset, data).
    zcache: Option<(u64, Vec<u8>)>,
}

pub struct Qcow2Image {
    file: File,
    path: PathBuf,
    hdr: Header,
    cluster_size: u64,
    read_only: bool,
    backing: Option<Arc<dyn DiskImage>>,
    state: Mutex<State>,
}

impl std::fmt::Debug for Qcow2Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Qcow2Image")
            .field("path", &self.path)
            .field("size", &self.hdr.size)
            .field("backing", &self.hdr.backing_file)
            .finish()
    }
}

impl Qcow2Image {
    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        Self::open_with(path, read_only, &OpenPolicy::default(), 0)
    }

    pub fn open_with(path: &Path, read_only: bool, policy: &OpenPolicy, depth: u32) -> Result<Self> {
        if depth > MAX_CHAIN {
            return Err(bad("backing chain too deep"));
        }
        let file = open_file(path, read_only)?;
        let file_len = file.metadata()?.len();
        let hdr = parse_header(&file, file_len)?;
        if !read_only && hdr.nb_snapshots != 0 {
            return Err(bad("writable images with internal snapshots not supported"));
        }
        if !read_only && hdr.version != 3 {
            return Err(bad("writable qcow2 must be version 3"));
        }
        let cs = 1u64 << hdr.cluster_bits;

        let mut l1 = vec![0u64; hdr.l1_size as usize];
        read_be64s(&file, hdr.l1_offset, &mut l1)?;
        let rt_entries = (hdr.refcount_table_clusters as u64 * cs / 8) as usize;
        let mut refcount_table = vec![0u64; rt_entries];
        read_be64s(&file, hdr.refcount_table_offset, &mut refcount_table)?;
        for e in l1.iter().chain(refcount_table.iter()) {
            let off = e & OFFSET_MASK;
            if off % cs != 0 {
                return Err(bad("metadata offset not cluster aligned"));
            }
        }

        let backing = match &hdr.backing_file {
            None => None,
            Some(name) => {
                let bp = resolve_backing(path, name, policy)?;
                let fmt = match hdr.backing_format.as_deref() {
                    Some("qcow2") => Format::Qcow2,
                    Some("raw") => Format::Raw,
                    Some(other) => return Err(bad(format!("backing format {other} not supported"))),
                    None => crate::detect(&bp)?,
                };
                let b: Arc<dyn DiskImage> = match fmt {
                    Format::Raw => Arc::new(crate::raw::RawImage::open(&bp, true)?),
                    Format::Qcow2 => Arc::new(Qcow2Image::open_with(&bp, true, policy, depth + 1)?),
                };
                Some(b)
            }
        };

        Ok(Qcow2Image {
            file,
            path: path.to_path_buf(),
            cluster_size: cs,
            read_only,
            backing,
            state: Mutex::new(State {
                l1,
                l2_cache: HashMap::new(),
                refcount_table,
                next_free: file_len.div_ceil(cs) * cs,
                zcache: None,
            }),
            hdr,
        })
    }

    /// Create a new empty v3 image, optionally on top of a backing file.
    /// `backing` is stored relative if it sits in the same directory.
    pub fn create(path: &Path, size: u64, backing: Option<(&Path, Format)>) -> Result<()> {
        if size == 0 || size > MAX_SIZE {
            return Err(bad("bad virtual size"));
        }
        let cb = 16u32;
        let cs = 1u64 << cb;
        let l2_entries = cs / 8;
        let l1_size = size.div_ceil(cs * l2_entries);
        let l1_clusters = (l1_size * 8).div_ceil(cs).max(1);
        // cluster 0 header, 1 refcount table, 2 refcount block 0, 3.. L1
        let rt_off = cs;
        let rb_off = 2 * cs;
        let l1_off = 3 * cs;
        let used = 3 + l1_clusters;

        let mut hdr = vec![0u8; cs as usize];
        let put32 = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_be_bytes());
        let put64 = |b: &mut Vec<u8>, o: usize, v: u64| b[o..o + 8].copy_from_slice(&v.to_be_bytes());
        put32(&mut hdr, 0, MAGIC);
        put32(&mut hdr, 4, 3);
        put32(&mut hdr, 20, cb);
        put64(&mut hdr, 24, size);
        put32(&mut hdr, 36, l1_size as u32);
        put64(&mut hdr, 40, l1_off);
        put64(&mut hdr, 48, rt_off);
        put32(&mut hdr, 56, 1);
        put32(&mut hdr, 96, 4);
        put32(&mut hdr, 100, 104);
        let mut off = 104usize;
        if let Some((bp, fmt)) = backing {
            let fmt_name: &[u8] = if fmt == Format::Qcow2 { b"qcow2" } else { b"raw" };
            put32(&mut hdr, off, EXT_BACKING_FORMAT);
            put32(&mut hdr, off + 4, fmt_name.len() as u32);
            hdr[off + 8..off + 8 + fmt_name.len()].copy_from_slice(fmt_name);
            off += 8 + fmt_name.len().div_ceil(8) * 8;
            put32(&mut hdr, off, EXT_END);
            put32(&mut hdr, off + 4, 0);
            off += 8;
            let same_dir = dir_of(bp).canonicalize().ok() == dir_of(path).canonicalize().ok();
            let name = if same_dir {
                bp.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            } else {
                bp.to_string_lossy().into_owned()
            };
            if name.len() > 1023 || off + name.len() > cs as usize {
                return Err(bad("backing path too long"));
            }
            hdr[off..off + name.len()].copy_from_slice(name.as_bytes());
            put64(&mut hdr, 8, off as u64);
            put32(&mut hdr, 16, name.len() as u32);
        }

        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let f = o.open(path)?;
        f.write_all_at(&hdr, 0)?;
        let mut rt = vec![0u8; cs as usize];
        rt[..8].copy_from_slice(&rb_off.to_be_bytes());
        f.write_all_at(&rt, rt_off)?;
        let mut rb = vec![0u8; cs as usize];
        for i in 0..used as usize {
            rb[2 * i..2 * i + 2].copy_from_slice(&1u16.to_be_bytes());
        }
        f.write_all_at(&rb, rb_off)?;
        f.set_len((used) * cs)?;
        f.sync_all()?;
        Ok(())
    }

    pub fn backing(&self) -> Option<&Arc<dyn DiskImage>> {
        self.backing.as_ref()
    }

    fn l2_entries(&self) -> u64 {
        self.cluster_size / 8
    }

    fn load_l2<'a>(&self, st: &'a mut State, l2_off: u64) -> Result<&'a mut Vec<u64>> {
        if !st.l2_cache.contains_key(&l2_off) {
            if st.l2_cache.len() >= L2_CACHE {
                // Cheap eviction: drop an arbitrary entry. Tables are write-through.
                if let Some(&k) = st.l2_cache.keys().next() {
                    st.l2_cache.remove(&k);
                }
            }
            let mut t = vec![0u64; self.l2_entries() as usize];
            read_be64s(&self.file, l2_off, &mut t)?;
            st.l2_cache.insert(l2_off, t);
        }
        st.l2_cache.get_mut(&l2_off).ok_or_else(|| bad("l2 cache"))
    }

    /// L2 entry for a guest offset (0 if unallocated).
    fn l2_entry(&self, st: &mut State, guest: u64) -> Result<u64> {
        let cs = self.cluster_size;
        let l1_idx = (guest / cs / self.l2_entries()) as usize;
        let l2_idx = (guest / cs % self.l2_entries()) as usize;
        let l1e = *st.l1.get(l1_idx).ok_or(Error::OutOfRange)?;
        let l2_off = l1e & OFFSET_MASK;
        if l2_off == 0 {
            return Ok(0);
        }
        Ok(self.load_l2(st, l2_off)?[l2_idx])
    }

    fn read_compressed(&self, st: &mut State, entry: u64, out: &mut [u8], in_cluster: usize) -> Result<()> {
        let cb = self.hdr.cluster_bits;
        let x = 62 - (cb - 8);
        let host = entry & ((1u64 << x) - 1);
        let sectors = ((entry >> x) & ((1u64 << (62 - x)) - 1)) + 1;
        let len = sectors * 512 - (host & 511);
        if st.zcache.as_ref().map(|(h, _)| *h) != Some(host) {
            if len > 2 * self.cluster_size + 512 {
                return Err(bad("compressed cluster too large"));
            }
            let mut comp = vec![0u8; len as usize];
            // The last compressed cluster may end before `len`.
            let n = read_up_to(&self.file, &mut comp, host)?;
            comp.truncate(n);
            let mut data = Vec::with_capacity(self.cluster_size as usize);
            let lim = self.cluster_size + 1;
            if self.hdr.zstd {
                let d = ruzstd_decoder(&comp)?;
                d.take(lim).read_to_end(&mut data)?;
            } else {
                flate2::read::DeflateDecoder::new(&comp[..]).take(lim).read_to_end(&mut data)?;
            }
            if data.len() as u64 != self.cluster_size {
                return Err(bad("compressed cluster has wrong size"));
            }
            st.zcache = Some((host, data));
        }
        let (_, data) = st.zcache.as_ref().ok_or_else(|| bad("zcache"))?;
        out.copy_from_slice(&data[in_cluster..in_cluster + out.len()]);
        Ok(())
    }

    fn read_from_backing(&self, out: &mut [u8], guest: u64) -> Result<()> {
        match &self.backing {
            None => out.fill(0),
            Some(b) => {
                let bsize = b.size();
                if guest >= bsize {
                    out.fill(0);
                } else {
                    let n = ((bsize - guest) as usize).min(out.len());
                    b.read_at(&mut out[..n], guest)?;
                    out[n..].fill(0);
                }
            }
        }
        Ok(())
    }

    fn read_chunk(&self, st: &mut State, out: &mut [u8], guest: u64) -> Result<()> {
        let in_cluster = (guest % self.cluster_size) as usize;
        let e = self.l2_entry(st, guest)?;
        if e & L2_COMPRESSED != 0 {
            return self.read_compressed(st, e, out, in_cluster);
        }
        if e & L2_ZERO != 0 {
            out.fill(0);
            return Ok(());
        }
        let host = e & OFFSET_MASK;
        if host == 0 {
            return self.read_from_backing(out, guest);
        }
        self.file.read_exact_at(out, host + in_cluster as u64)?;
        Ok(())
    }

    fn alloc_cluster(&self, st: &mut State) -> Result<u64> {
        let off = st.next_free;
        st.next_free += self.cluster_size;
        self.set_refcount(st, off, 1)?;
        Ok(off)
    }

    fn set_refcount(&self, st: &mut State, host: u64, val: u16) -> Result<()> {
        let cs = self.cluster_size;
        let per_block = cs / 2;
        let idx = host / cs;
        let blk = (idx / per_block) as usize;
        let i = idx % per_block;
        if blk >= st.refcount_table.len() {
            return Err(bad("image needs a bigger refcount table (not supported)"));
        }
        if st.refcount_table[blk] & OFFSET_MASK == 0 {
            let new = st.next_free;
            st.next_free += cs;
            self.file.write_all_at(&vec![0u8; cs as usize], new)?;
            st.refcount_table[blk] = new;
            self.file.write_all_at(&new.to_be_bytes(), self.hdr.refcount_table_offset + 8 * blk as u64)?;
            self.set_refcount(st, new, 1)?;
        }
        let rb = st.refcount_table[blk] & OFFSET_MASK;
        self.file.write_all_at(&val.to_be_bytes(), rb + 2 * i)?;
        Ok(())
    }

    /// Host offset of the cluster for `guest`, allocating (with COW from the
    /// backing image / old cluster) if needed.
    fn cluster_for_write(&self, st: &mut State, guest: u64) -> Result<u64> {
        let cs = self.cluster_size;
        let l1_idx = (guest / cs / self.l2_entries()) as usize;
        let l2_idx = (guest / cs % self.l2_entries()) as usize;
        let l1e = *st.l1.get(l1_idx).ok_or(Error::OutOfRange)?;
        let mut l2_off = l1e & OFFSET_MASK;
        if l2_off == 0 || l1e & L1_COPIED == 0 {
            let new = self.alloc_cluster(st)?;
            let table = if l2_off == 0 {
                vec![0u64; self.l2_entries() as usize]
            } else {
                self.load_l2(st, l2_off)?.clone()
            };
            write_be64s(&self.file, new, &table)?;
            st.l2_cache.insert(new, table);
            st.l1[l1_idx] = new | L1_COPIED;
            self.file.write_all_at(&(new | L1_COPIED).to_be_bytes(), self.hdr.l1_offset + 8 * l1_idx as u64)?;
            l2_off = new;
        }
        let e = self.load_l2(st, l2_off)?[l2_idx];
        if e & L2_COPIED != 0 && e & L2_COMPRESSED == 0 && e & OFFSET_MASK != 0 {
            return Ok(e & OFFSET_MASK);
        }
        // Copy-on-write: fill a fresh cluster with the current contents.
        let base = guest - guest % cs;
        let mut buf = vec![0u8; cs as usize];
        if e & L2_ZERO == 0 {
            self.read_chunk(st, &mut buf, base)?;
        }
        let new = self.alloc_cluster(st)?;
        self.file.write_all_at(&buf, new)?;
        let ne = new | L2_COPIED;
        self.load_l2(st, l2_off)?[l2_idx] = ne;
        self.file.write_all_at(&ne.to_be_bytes(), l2_off + 8 * l2_idx as u64)?;
        Ok(new)
    }
}

fn ruzstd_decoder(b: &[u8]) -> Result<impl Read + '_> {
    ruzstd::decoding::StreamingDecoder::new(b).map_err(|e| bad(format!("zstd: {e}")))
}

fn read_up_to(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match f.read_at(&mut buf[done..], off + done as u64)? {
            0 => break,
            n => done += n,
        }
    }
    Ok(done)
}

fn read_be64s(f: &File, off: u64, out: &mut [u64]) -> Result<()> {
    let mut b = vec![0u8; out.len() * 8];
    f.read_exact_at(&mut b, off).map_err(|_| bad("metadata table truncated"))?;
    for (i, v) in out.iter_mut().enumerate() {
        *v = be64(&b, i * 8);
    }
    Ok(())
}

fn write_be64s(f: &File, off: u64, v: &[u64]) -> Result<()> {
    let mut b = Vec::with_capacity(v.len() * 8);
    for x in v {
        b.extend_from_slice(&x.to_be_bytes());
    }
    f.write_all_at(&b, off)?;
    Ok(())
}

impl DiskImage for Qcow2Image {
    fn size(&self) -> u64 {
        self.hdr.size
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        check_range(self.hdr.size, offset, buf.len() as u64)?;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let cs = self.cluster_size;
        let mut done = 0usize;
        while done < buf.len() {
            let g = offset + done as u64;
            let n = ((cs - g % cs) as usize).min(buf.len() - done);
            self.read_chunk(&mut st, &mut buf[done..done + n], g)?;
            done += n;
        }
        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        check_range(self.hdr.size, offset, buf.len() as u64)?;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let cs = self.cluster_size;
        let mut done = 0usize;
        while done < buf.len() {
            let g = offset + done as u64;
            let n = ((cs - g % cs) as usize).min(buf.len() - done);
            let host = self.cluster_for_write(&mut st, g)?;
            self.file.write_all_at(&buf[done..done + n], host + g % cs)?;
            done += n;
        }
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        if !self.read_only {
            self.file.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawImage;

    #[test]
    fn create_write_read_reopen() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("a.qcow2");
        Qcow2Image::create(&p, 1 << 30, None).unwrap();
        {
            let q = Qcow2Image::open(&p, false).unwrap();
            let mut b = vec![1u8; 100];
            q.read_at(&mut b, 0).unwrap();
            assert!(b.iter().all(|&x| x == 0));
            q.write_at(b"hello qcow2", 65530).unwrap(); // spans 2 clusters
            q.write_at(b"far away", (1 << 30) - 8).unwrap();
            q.flush().unwrap();
        }
        let q = Qcow2Image::open(&p, true).unwrap();
        let mut b = [0u8; 11];
        q.read_at(&mut b, 65530).unwrap();
        assert_eq!(&b, b"hello qcow2");
        let mut b = [0u8; 8];
        q.read_at(&mut b, (1 << 30) - 8).unwrap();
        assert_eq!(&b, b"far away");
        assert!(q.read_at(&mut b, 1 << 30).is_err());
    }

    #[test]
    fn overlay_cow_leaves_base_untouched() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().join("base.raw");
        std::fs::write(&base, vec![7u8; 1 << 20]).unwrap();
        let ov = td.path().join("ov.qcow2");
        Qcow2Image::create(&ov, 1 << 20, Some((&base, Format::Raw))).unwrap();
        let q = Qcow2Image::open(&ov, false).unwrap();
        let mut b = [0u8; 4];
        q.read_at(&mut b, 1000).unwrap();
        assert_eq!(b, [7; 4]);
        q.write_at(&[9, 9], 1001).unwrap();
        q.read_at(&mut b, 1000).unwrap();
        assert_eq!(b, [7, 9, 9, 7]);
        let r = RawImage::open(&base, true).unwrap();
        r.read_at(&mut b, 1000).unwrap();
        assert_eq!(b, [7; 4]);
    }

    #[test]
    fn backing_outside_allowed_dir_refused() {
        let td = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let secret = other.path().join("secret");
        std::fs::write(&secret, b"ssh key").unwrap();
        let ov = td.path().join("evil.qcow2");
        Qcow2Image::create(&ov, 1 << 20, Some((&secret, Format::Raw))).unwrap();
        let e = Qcow2Image::open(&ov, true).unwrap_err().to_string();
        assert!(e.contains("outside the allowed"), "{e}");
        let pol = OpenPolicy { allowed_dirs: vec![other.path().to_path_buf()] };
        assert!(Qcow2Image::open_with(&ov, true, &pol, 0).is_ok());
    }

    #[test]
    fn garbage_headers_rejected() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("x.qcow2");
        Qcow2Image::create(&p, 1 << 20, None).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        // cluster_bits = 40
        f.write_all_at(&40u32.to_be_bytes(), 20).unwrap();
        assert!(Qcow2Image::open(&p, true).is_err());
        f.write_all_at(&16u32.to_be_bytes(), 20).unwrap();
        // l1 size way too small for virtual size
        f.write_all_at(&(1u64 << 40).to_be_bytes(), 24).unwrap();
        assert!(Qcow2Image::open(&p, true).is_err());
        f.write_all_at(&(1u64 << 20).to_be_bytes(), 24).unwrap();
        // protocol-style backing name
        assert!(resolve_backing(&p, "nbd:evil:1234", &OpenPolicy::default()).is_err());
        assert!(Qcow2Image::open(&p, true).is_ok());
    }
}
