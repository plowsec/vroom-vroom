//! VMDK (read-only), to import VMware disks.
//!
//! Supported: hosted sparse extents ("KDMV": monolithicSparse,
//! twoGbMaxExtentSparse), flat extents (monolithicFlat, twoGbMaxExtentFlat,
//! vmfs), ZERO extents, streamOptimized (compressed grains, what OVA exports
//! hold), snapshot deltas and linked clones (parent chain, CIDs checked).
//! Refused at open: ESXi sparse formats (vmfsSparse "COWD", seSparse), raw
//! device mappings, encrypted disks, unknown create types.
//!
//! The files come from the user, maybe from a download: every header field
//! is checked against the file it is in, tables are capped, extent files
//! must sit next to their descriptor, parent chains are bounded.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::{check_range, open_file, DiskImage, Error, Result};

const SECTOR: u64 = 512;
const SPARSE_MAGIC: &[u8; 4] = b"KDMV";
const COWD_MAGIC: &[u8; 4] = b"COWD";
const SESPARSE_MAGIC: u64 = 0xcafe_babe;

const FLAG_NL_TEST: u32 = 1;
const FLAG_ZERO_GTE: u32 = 1 << 2;
const FLAG_COMPRESSED: u32 = 1 << 16;
const FLAG_MARKERS: u32 = 1 << 17;
const COMPRESS_DEFLATE: u16 = 1;
/// streamOptimized: the grain directory is found through the footer.
const GD_AT_END: u64 = u64::MAX;
const MARKER_EOS: u32 = 0;
const MARKER_FOOTER: u32 = 3;
/// A grain table entry meaning "reads as zeros" (with `FLAG_ZERO_GTE`).
const GTE_ZERO: u32 = 1;

/// Max virtual size (64 TiB, as for qcow2).
const MAX_SIZE: u64 = 64 << 40;
/// Largest descriptor we read (a 16 TiB split disk needs ~500 KiB).
const MAX_DESCRIPTOR: u64 = 1 << 20;
const MAX_EXTENTS: usize = 8192;
/// Max parent chain depth (each snapshot adds one).
const MAX_CHAIN: usize = 64;
/// Largest grain (VMware uses 64 KiB).
const MAX_GRAIN_SECTORS: u64 = 2048;
/// Largest grain table (VMware uses 512 entries).
const MAX_GTES: u32 = 4096;
/// Largest grain directory we load per extent.
const MAX_GD_BYTES: u64 = 32 << 20;
/// Grain tables kept in memory (all extents together).
const GT_CACHE: usize = 256;

fn bad(m: impl Into<String>) -> Error {
    Error::Format(m.into())
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn le64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

fn le32s(b: &[u8]) -> Vec<u32> {
    b.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
}

/// Sectors to bytes, refusing overflow.
fn bytes(sectors: u64) -> Result<u64> {
    sectors.checked_mul(SECTOR).ok_or_else(|| bad("sector number out of range"))
}

/// `[off, off+len)` lies inside a file of `file_len` bytes.
fn in_file(off: u64, len: u64, file_len: u64) -> bool {
    off.checked_add(len).is_some_and(|end| end <= file_len)
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

/// The "KDMV" header of a hosted sparse extent (sizes in sectors).
#[derive(Debug, Clone)]
struct SparseHeader {
    flags: u32,
    capacity: u64,
    grain: u64,
    desc_off: u64,
    desc_size: u64,
    gtes: u32,
    gd_off: u64,
    unclean: bool,
}

fn parse_sparse_header(b: &[u8; 512], name: &str) -> Result<SparseHeader> {
    if &b[..4] != SPARSE_MAGIC {
        return Err(bad(format!("{name}: not a VMDK sparse extent (bad magic)")));
    }
    let version = le32(b, 4);
    if !(1..=3).contains(&version) {
        return Err(bad(format!("{name}: sparse extent version {version} not supported")));
    }
    let flags = le32(b, 8);
    // Four bytes of line endings, there to catch a text-mode transfer.
    if flags & FLAG_NL_TEST != 0 && b[73..77] != *b"\n \r\n" {
        return Err(bad(format!("{name}: damaged (line endings converted by a text-mode transfer?)")));
    }
    let h = SparseHeader {
        flags,
        capacity: le64(b, 12),
        grain: le64(b, 20),
        desc_off: le64(b, 28),
        desc_size: le64(b, 36),
        gtes: le32(b, 44),
        gd_off: le64(b, 56),
        unclean: b[72] != 0,
    };
    if h.capacity == 0 || h.capacity > MAX_SIZE / SECTOR {
        return Err(bad(format!("{name}: capacity out of range")));
    }
    if !h.grain.is_power_of_two() || h.grain > MAX_GRAIN_SECTORS {
        return Err(bad(format!("{name}: grain size {} sectors not supported", h.grain)));
    }
    if h.gtes == 0 || h.gtes > MAX_GTES {
        return Err(bad(format!("{name}: grain table size {} not supported", h.gtes)));
    }
    if flags & FLAG_COMPRESSED != 0 && le16(b, 77) != COMPRESS_DEFLATE {
        return Err(bad(format!("{name}: unknown compression {}", le16(b, 77))));
    }
    Ok(h)
}

/// streamOptimized footer: a copy of the header with the real grain
/// directory offset, in the second-to-last sector.
fn read_footer(f: &File, file_len: u64, name: &str) -> Result<SparseHeader> {
    let end = file_len / SECTOR * SECTOR;
    if end < 4 * SECTOR {
        return Err(bad(format!("{name}: stream too short for its footer")));
    }
    let mut b = [0u8; 1536];
    f.read_exact_at(&mut b, end - 1536)?;
    let footer_marker = le32(&b, 8) == 0 && le32(&b, 12) == MARKER_FOOTER;
    let eos = le64(&b, 1024) == 0 && le32(&b, 1032) == 0 && le32(&b, 1036) == MARKER_EOS;
    if !footer_marker || !eos {
        return Err(bad(format!("{name}: stream footer missing (incomplete download?)")));
    }
    let mut hb = [0u8; 512];
    hb.copy_from_slice(&b[512..1024]);
    let h = parse_sparse_header(&hb, name)?;
    if h.gd_off == GD_AT_END {
        return Err(bad(format!("{name}: footer has no grain directory")));
    }
    Ok(h)
}

/// One sparse extent file, its grain directory in memory.
#[derive(Debug)]
struct Sparse {
    file: File,
    file_len: u64,
    /// Bytes.
    grain: u64,
    gtes: u64,
    /// Grain table sectors, 0 = whole table unallocated.
    gd: Vec<u32>,
    zero_gte: bool,
    compressed: bool,
    markers: bool,
}

impl Sparse {
    /// `sectors`: the part of the extent the descriptor uses.
    fn open(file: File, hdr: SparseHeader, sectors: u64, name: &str) -> Result<Sparse> {
        let file_len = file.metadata()?.len();
        let hdr = if hdr.gd_off == GD_AT_END {
            if hdr.flags & FLAG_MARKERS == 0 {
                return Err(bad(format!("{name}: grain directory at the end but no markers")));
            }
            read_footer(&file, file_len, name)?
        } else {
            hdr
        };
        if hdr.unclean {
            log::warn!("{name}: not closed cleanly by VMware; data may be inconsistent");
        }
        if sectors > hdr.capacity {
            return Err(bad(format!("{name}: descriptor says {sectors} sectors, extent holds {}", hdr.capacity)));
        }
        let grains = sectors.div_ceil(hdr.grain);
        let gd_len = grains.div_ceil(hdr.gtes as u64);
        let gd_bytes = gd_len * 4;
        if gd_bytes > MAX_GD_BYTES {
            return Err(bad(format!("{name}: grain directory too large")));
        }
        let gd_off = bytes(hdr.gd_off)?;
        if hdr.gd_off == 0 || !in_file(gd_off, gd_bytes, file_len) {
            return Err(bad(format!("{name}: grain directory outside the file")));
        }
        let mut raw = vec![0u8; gd_bytes as usize];
        file.read_exact_at(&mut raw, gd_off)?;
        let gd = le32s(&raw);
        let gt_bytes = hdr.gtes as u64 * 4;
        for &e in &gd {
            if e != 0 && !in_file(e as u64 * SECTOR, gt_bytes, file_len) {
                return Err(bad(format!("{name}: grain table outside the file")));
            }
        }
        Ok(Sparse {
            file,
            file_len,
            grain: hdr.grain * SECTOR,
            gtes: hdr.gtes as u64,
            gd,
            zero_gte: hdr.flags & FLAG_ZERO_GTE != 0,
            compressed: hdr.flags & FLAG_COMPRESSED != 0,
            markers: hdr.flags & FLAG_MARKERS != 0,
        })
    }
}

#[derive(Debug)]
enum Kind {
    /// Bytes start at `offset` in the file.
    Flat { file: File, offset: u64 },
    Zero,
    Sparse(Sparse),
}

#[derive(Debug)]
struct Extent {
    /// Disk offset and length, bytes.
    start: u64,
    len: u64,
    kind: Kind,
    name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Flat,
    Zero,
    Sparse,
}

#[derive(Debug)]
struct ExtentLine {
    sectors: u64,
    kind: LineKind,
    file: Option<String>,
    /// Sectors (flat extents only).
    offset: u64,
}

#[derive(Debug, Default)]
struct Descriptor {
    cid: Option<u32>,
    /// None: no parent (ffffffff).
    parent_cid: Option<u32>,
    create_type: Option<String>,
    parent_hint: Option<String>,
    extents: Vec<ExtentLine>,
}

/// Split a line into words; a "quoted string" is one word.
fn words(line: &str) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    let mut rest = line.trim_start();
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"')?;
            out.push(&r[..end]);
            rest = r[end + 1..].trim_start();
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            out.push(&rest[..end]);
            rest = rest[end..].trim_start();
        }
    }
    Some(out)
}

fn parse_cid(v: &str, name: &str) -> Result<u32> {
    u32::from_str_radix(v, 16).map_err(|_| bad(format!("{name}: bad CID {v:?}")))
}

fn parse_descriptor(text: &str, name: &str) -> Result<Descriptor> {
    let mut d = Descriptor::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let first = line.split_whitespace().next().unwrap_or("");
        if matches!(first, "RW" | "RDONLY" | "NOACCESS") {
            if d.extents.len() >= MAX_EXTENTS {
                return Err(bad(format!("{name}: too many extents")));
            }
            d.extents.push(parse_extent_line(line, name)?);
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(bad(format!("{name}: not a VMDK descriptor (line {line:?})")));
        };
        let k = k.trim();
        let v = v.trim();
        let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v);
        if k.to_ascii_lowercase().starts_with("encryption") || k.starts_with("ddb.encryption") {
            return Err(bad(format!("{name}: encrypted disk; remove the encryption in VMware first")));
        }
        match k {
            "CID" => d.cid = Some(parse_cid(v, name)?),
            "parentCID" => d.parent_cid = Some(parse_cid(v, name)?).filter(|&c| c != u32::MAX),
            "createType" => d.create_type = Some(v.to_string()),
            "parentFileNameHint" => d.parent_hint = Some(v.to_string()),
            _ => {}
        }
    }
    Ok(d)
}

fn parse_extent_line(line: &str, name: &str) -> Result<ExtentLine> {
    let bad_line = || bad(format!("{name}: bad extent line {line:?}"));
    let w = words(line).ok_or_else(bad_line)?;
    if w.len() < 3 || w.len() > 5 {
        return Err(bad_line());
    }
    if w[0] == "NOACCESS" {
        return Err(bad(format!("{name}: has an extent marked NOACCESS")));
    }
    let sectors: u64 = w[1].parse().map_err(|_| bad_line())?;
    let kind = match w[2] {
        "FLAT" | "VMFS" => LineKind::Flat,
        "ZERO" => LineKind::Zero,
        "SPARSE" => LineKind::Sparse,
        "VMFSSPARSE" => return Err(bad(format!("{name}: ESXi snapshot deltas (vmfsSparse) are not supported; consolidate the snapshots on the host first"))),
        "SESPARSE" => return Err(bad(format!("{name}: ESXi seSparse disks are not supported; consolidate or convert on the host first"))),
        "VMFSRDM" | "VMFSRAW" => return Err(bad(format!("{name}: raw device mappings are not supported"))),
        t => return Err(bad(format!("{name}: unknown extent type {t:?}"))),
    };
    let file = w.get(3).map(|s| s.to_string());
    if kind != LineKind::Zero && file.is_none() {
        return Err(bad_line());
    }
    let offset = match w.get(4) {
        Some(o) if kind == LineKind::Flat => o.parse().map_err(|_| bad_line())?,
        Some(_) => return Err(bad_line()),
        None => 0,
    };
    Ok(ExtentLine { sectors, kind, file, offset })
}

/// Extent kinds a create type may use (ZERO goes with any).
fn check_create_type(t: &str, name: &str) -> Result<LineKind> {
    match t {
        "monolithicSparse" | "twoGbMaxExtentSparse" | "streamOptimized" => Ok(LineKind::Sparse),
        "monolithicFlat" | "twoGbMaxExtentFlat" | "vmfs" => Ok(LineKind::Flat),
        "vmfsSparse" => Err(bad(format!("{name}: ESXi snapshot deltas (vmfsSparse) are not supported; consolidate the snapshots on the host first"))),
        "seSparse" => Err(bad(format!("{name}: ESXi seSparse disks are not supported; consolidate or convert on the host first"))),
        "vmfsRDM" | "vmfsRDMP" | "vmfsRaw" | "fullDevice" | "partitionedDevice" => {
            Err(bad(format!("{name}: disks backed by a physical device ({t}) are not supported")))
        }
        _ => Err(bad(format!("{name}: VMDK type {t:?} not supported"))),
    }
}

/// An extent file named in a descriptor: a plain file name, in the
/// descriptor's directory, really there (no symlink out of it).
fn resolve_extent(dir: &Path, name: &str, desc: &str) -> Result<PathBuf> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return Err(bad(format!("{desc}: extent {name:?} must be a plain file name next to the descriptor")));
    }
    let p = dir.join(name);
    let real = p.canonicalize().map_err(|e| bad(format!("{desc}: extent {}: {e}", p.display())))?;
    if real.parent() != Some(dir) || !real.is_file() {
        return Err(bad(format!("{desc}: extent {name:?} is not a regular file next to the descriptor")));
    }
    Ok(real)
}

/// A parent named by `parentFileNameHint`: relative to the child's
/// directory, or absolute (linked clones point into another VM's bundle).
/// Must be an existing `.vmdk` file.
fn resolve_parent(dir: &Path, hint: &str, desc: &str) -> Result<PathBuf> {
    if hint.is_empty() || hint.contains('\0') {
        return Err(bad(format!("{desc}: bad parentFileNameHint")));
    }
    let p = dir.join(hint);
    let real = p.canonicalize().map_err(|e| bad(format!("{desc}: parent disk {}: {e}", p.display())))?;
    let vmdk = real.extension().is_some_and(|e| e.eq_ignore_ascii_case("vmdk"));
    if !vmdk || !real.is_file() {
        return Err(bad(format!("{desc}: parent {} is not a .vmdk file", real.display())));
    }
    Ok(real)
}

/// Read the 512-byte header of a sparse extent; clear errors for the
/// formats we refuse.
fn read_header(f: &File, name: &str) -> Result<Option<[u8; 512]>> {
    let mut b = [0u8; 512];
    let n = read_up_to(f, &mut b, 0)?;
    if n >= 4 && &b[..4] == COWD_MAGIC {
        return Err(bad(format!("{name}: ESXi snapshot deltas (vmfsSparse) are not supported; consolidate the snapshots on the host first")));
    }
    if n >= 8 && le64(&b, 0) == SESPARSE_MAGIC {
        return Err(bad(format!("{name}: ESXi seSparse disks are not supported; consolidate or convert on the host first")));
    }
    if n >= 4 && &b[..4] == SPARSE_MAGIC {
        if n < 512 {
            return Err(bad(format!("{name}: sparse header truncated")));
        }
        return Ok(Some(b));
    }
    Ok(None)
}

#[derive(Debug, Default)]
struct State {
    /// (extent, table sector) -> table.
    gt_cache: HashMap<(usize, u32), Vec<u32>>,
    /// Last decompressed grain: (extent, host offset), data.
    zcache: Option<((usize, u64), Vec<u8>)>,
}

/// A VMware disk, read-only.
pub struct VmdkImage {
    path: PathBuf,
    create_type: String,
    cid: Option<u32>,
    size: u64,
    extents: Vec<Extent>,
    parent: Option<Box<VmdkImage>>,
    files: Vec<PathBuf>,
    state: Mutex<State>,
}

impl std::fmt::Debug for VmdkImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmdkImage")
            .field("path", &self.path)
            .field("type", &self.create_type)
            .field("size", &self.size)
            .field("extents", &self.extents.len())
            .field("parent", &self.parent.as_ref().map(|p| &p.path))
            .finish()
    }
}

impl VmdkImage {
    /// Open a VMDK by its descriptor (or a monolithic sparse file with an
    /// embedded descriptor). Follows the parent chain of snapshot deltas.
    pub fn open(path: &Path) -> Result<VmdkImage> {
        Self::open_chain(path, &mut Vec::new())
    }

    /// Every file the image reads (descriptors, extents, parents), for the
    /// caller to show/check.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// `seen`: descriptors of the children below us (loop check).
    fn open_chain(path: &Path, seen: &mut Vec<PathBuf>) -> Result<VmdkImage> {
        let name = path.display().to_string();
        let real = path.canonicalize().map_err(|e| bad(format!("{name}: {e}")))?;
        if seen.contains(&real) {
            return Err(bad(format!("{name}: snapshot chain loops back to itself")));
        }
        if seen.len() >= MAX_CHAIN {
            return Err(bad(format!("{name}: snapshot chain longer than {MAX_CHAIN}")));
        }
        seen.push(real.clone());
        let dir = real.parent().ok_or_else(|| bad(format!("{name}: no directory")))?.to_path_buf();

        let file = open_file(&real, true)?;
        let file_len = file.metadata()?.len();
        let (desc, embedded) = match read_header(&file, &name)? {
            Some(hb) => {
                let hdr = parse_sparse_header(&hb, &name)?;
                if hdr.desc_off == 0 || hdr.desc_size == 0 {
                    return Err(bad(format!(
                        "{name}: this is one piece of a split disk; open the .vmdk that describes it (without -sNNN / -flat)"
                    )));
                }
                let (off, len) = (bytes(hdr.desc_off)?, bytes(hdr.desc_size)?);
                if len > MAX_DESCRIPTOR || !in_file(off, len, file_len) {
                    return Err(bad(format!("{name}: embedded descriptor out of range")));
                }
                let mut b = vec![0u8; len as usize];
                file.read_exact_at(&mut b, off)?;
                let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                let d = parse_descriptor(&String::from_utf8_lossy(&b[..end]), &name)?;
                (d, Some((file, hdr)))
            }
            None => {
                if file_len > MAX_DESCRIPTOR {
                    return Err(bad(format!(
                        "{name}: not a VMDK descriptor; for a flat disk open the small .vmdk next to it"
                    )));
                }
                let mut b = Vec::new();
                (&file).take(MAX_DESCRIPTOR).read_to_end(&mut b)?;
                if b.contains(&0) {
                    return Err(bad(format!("{name}: not a VMDK file")));
                }
                (parse_descriptor(&String::from_utf8_lossy(&b), &name)?, None)
            }
        };
        let create_type = desc.create_type.clone().ok_or_else(|| bad(format!("{name}: not a VMDK descriptor (no createType)")))?;
        let family = check_create_type(&create_type, &name)?;

        let mut files = vec![real.clone()];
        let mut extents = Vec::new();
        let mut size = 0u64;
        if let Some((file, hdr)) = embedded {
            // monolithicSparse / streamOptimized: the extent is this file,
            // whatever name the descriptor gives it (users rename files).
            if family != LineKind::Sparse {
                return Err(bad(format!("{name}: {create_type} disk in a sparse extent")));
            }
            let sectors = match desc.extents.as_slice() {
                [] => hdr.capacity,
                [e] if e.kind == LineKind::Sparse => e.sectors,
                _ => return Err(bad(format!("{name}: embedded descriptor must list just this file"))),
            };
            let sp = Sparse::open(file, hdr, sectors, &name)?;
            size = bytes(sectors)?;
            extents.push(Extent { start: 0, len: size, kind: Kind::Sparse(sp), name: name.clone() });
        } else {
            if desc.extents.is_empty() {
                return Err(bad(format!("{name}: descriptor lists no extents")));
            }
            for e in &desc.extents {
                if e.kind != LineKind::Zero && e.kind != family {
                    return Err(bad(format!("{name}: extent type doesn't fit a {create_type} disk")));
                }
                let len = bytes(e.sectors)?;
                let end = size.checked_add(len).filter(|&s| s <= MAX_SIZE).ok_or_else(|| bad(format!("{name}: disk too large")))?;
                if len == 0 {
                    continue;
                }
                let (kind, ename) = match (e.kind, &e.file) {
                    (LineKind::Zero, _) => (Kind::Zero, "ZERO".to_string()),
                    (_, None) => return Err(bad(format!("{name}: extent without a file"))),
                    (k, Some(f)) => {
                        let p = resolve_extent(&dir, f, &name)?;
                        let ename = p.display().to_string();
                        let file = open_file(&p, true)?;
                        if !files.contains(&p) {
                            files.push(p);
                        }
                        let kind = if k == LineKind::Flat {
                            let offset = bytes(e.offset)?;
                            if !in_file(offset, len, file.metadata()?.len()) {
                                return Err(bad(format!("{ename}: shorter than the descriptor says")));
                            }
                            Kind::Flat { file, offset }
                        } else {
                            let hb = read_header(&file, &ename)?.ok_or_else(|| bad(format!("{ename}: not a VMDK sparse extent (bad magic)")))?;
                            let hdr = parse_sparse_header(&hb, &ename)?;
                            Kind::Sparse(Sparse::open(file, hdr, e.sectors, &ename)?)
                        };
                        (kind, ename)
                    }
                };
                extents.push(Extent { start: size, len, kind, name: ename });
                size = end;
            }
        }
        if size == 0 {
            return Err(bad(format!("{name}: empty disk")));
        }

        let parent = match (desc.parent_cid, &desc.parent_hint) {
            (None, None) => None,
            (None, Some(_)) => return Err(bad(format!("{name}: names a parent disk but has no parentCID"))),
            (Some(_), None) => return Err(bad(format!("{name}: snapshot delta without parentFileNameHint"))),
            (Some(want), Some(hint)) => {
                let pp = resolve_parent(&dir, hint, &name)?;
                let p = Self::open_chain(&pp, seen)?;
                if p.cid != Some(want) {
                    let have = p.cid.map_or("none".into(), |c| format!("{c:08x}"));
                    return Err(bad(format!(
                        "{name}: parent {} changed after this snapshot was taken (CID {have}, expected {want:08x}); the chain is broken",
                        pp.display()
                    )));
                }
                for f in p.files() {
                    if !files.contains(f) {
                        files.push(f.clone());
                    }
                }
                Some(Box::new(p))
            }
        };

        Ok(VmdkImage { path: real, create_type, cid: desc.cid, size, extents, parent, files, state: Mutex::new(State::default()) })
    }

    fn read_parent(&self, out: &mut [u8], guest: u64) -> Result<()> {
        match &self.parent {
            None => out.fill(0),
            Some(p) => {
                let psize = p.size();
                if guest >= psize {
                    out.fill(0);
                } else {
                    let n = ((psize - guest) as usize).min(out.len());
                    p.read_at(&mut out[..n], guest)?;
                    out[n..].fill(0);
                }
            }
        }
        Ok(())
    }

    /// Read within one grain of a sparse extent. `off`: bytes into the
    /// extent; `guest`: the same place on the disk (for the parent).
    fn read_sparse(&self, st: &mut State, ei: usize, sp: &Sparse, out: &mut [u8], off: u64, guest: u64) -> Result<()> {
        let name = &self.extents[ei].name;
        let gi = off / sp.grain;
        let in_grain = off % sp.grain;
        let gde = *sp.gd.get((gi / sp.gtes) as usize).ok_or(Error::OutOfRange)?;
        if gde == 0 {
            return self.read_parent(out, guest);
        }
        if !st.gt_cache.contains_key(&(ei, gde)) {
            if st.gt_cache.len() >= GT_CACHE {
                if let Some(&k) = st.gt_cache.keys().next() {
                    st.gt_cache.remove(&k);
                }
            }
            let mut raw = vec![0u8; sp.gtes as usize * 4];
            sp.file.read_exact_at(&mut raw, gde as u64 * SECTOR).map_err(|_| bad(format!("{name}: grain table truncated")))?;
            st.gt_cache.insert((ei, gde), le32s(&raw));
        }
        let gt = st.gt_cache.get(&(ei, gde)).ok_or_else(|| bad("gt cache"))?;
        let gte = *gt.get((gi % sp.gtes) as usize).ok_or(Error::OutOfRange)?;
        if gte == 0 {
            return self.read_parent(out, guest);
        }
        if gte == GTE_ZERO && sp.zero_gte {
            out.fill(0);
            return Ok(());
        }
        let host = gte as u64 * SECTOR;
        if sp.compressed {
            return self.read_compressed(st, ei, sp, host, out, in_grain as usize);
        }
        if !in_file(host + in_grain, out.len() as u64, sp.file_len) {
            return Err(bad(format!("{name}: grain past the end of the file")));
        }
        sp.file.read_exact_at(out, host + in_grain)?;
        Ok(())
    }

    fn read_compressed(&self, st: &mut State, ei: usize, sp: &Sparse, host: u64, out: &mut [u8], in_grain: usize) -> Result<()> {
        let name = &self.extents[ei].name;
        let corrupt = || bad(format!("{name}: compressed grain is corrupt"));
        if st.zcache.as_ref().map(|(k, _)| *k) != Some((ei, host)) {
            // Compressed data may be a bit larger than the grain.
            let max = 2 * sp.grain + SECTOR;
            let comp = if sp.markers {
                // Grain marker: LBA (u64), compressed length (u32), data.
                let mut m = [0u8; 12];
                sp.file.read_exact_at(&mut m, host).map_err(|_| corrupt())?;
                let len = le32(&m, 8) as u64;
                if len == 0 || len > max || !in_file(host + 12, len, sp.file_len) {
                    return Err(corrupt());
                }
                let mut c = vec![0u8; len as usize];
                sp.file.read_exact_at(&mut c, host + 12)?;
                c
            } else {
                let mut c = vec![0u8; max as usize];
                let n = read_up_to(&sp.file, &mut c, host)?;
                c.truncate(n);
                c
            };
            let mut data = Vec::with_capacity(sp.grain as usize);
            flate2::read::ZlibDecoder::new(&comp[..]).take(sp.grain + 1).read_to_end(&mut data).map_err(|_| corrupt())?;
            if data.len() as u64 > sp.grain {
                return Err(corrupt());
            }
            // The last grain of a disk may be stored short.
            data.resize(sp.grain as usize, 0);
            st.zcache = Some(((ei, host), data));
        }
        let (_, data) = st.zcache.as_ref().ok_or_else(|| bad("zcache"))?;
        out.copy_from_slice(&data[in_grain..in_grain + out.len()]);
        Ok(())
    }
}

impl DiskImage for VmdkImage {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_only(&self) -> bool {
        true
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<()> {
        check_range(self.size, offset, buf.len() as u64)?;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut done = 0usize;
        while done < buf.len() {
            let g = offset + done as u64;
            let ei = self.extents.partition_point(|e| e.start + e.len <= g);
            let e = self.extents.get(ei).ok_or(Error::OutOfRange)?;
            let off = g - e.start;
            let left = ((e.len - off) as usize).min(buf.len() - done);
            let n = match &e.kind {
                Kind::Flat { file, offset } => {
                    file.read_exact_at(&mut buf[done..done + left], offset + off)?;
                    left
                }
                Kind::Zero => {
                    buf[done..done + left].fill(0);
                    left
                }
                Kind::Sparse(sp) => {
                    let n = ((sp.grain - off % sp.grain) as usize).min(left);
                    self.read_sparse(&mut st, ei, sp, &mut buf[done..done + n], off, g)?;
                    n
                }
            };
            done += n;
        }
        Ok(())
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }

    fn discard(&self, _offset: u64, _len: u64) -> Result<()> {
        Err(Error::ReadOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Deterministic filler (xorshift), different per seed.
    fn pattern(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn zlib(d: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(d).unwrap();
        e.finish().unwrap()
    }

    #[derive(Clone)]
    enum G {
        Data(Vec<u8>),
        Zero,
        Packed(Vec<u8>),
    }

    /// A hand-made hosted sparse extent.
    struct Sp {
        sectors: u64,
        grain: u64,
        gtes: u32,
        flags: u32,
        desc: Option<String>,
        /// (grain index, content)
        grains: Vec<(u64, G)>,
        gd_at_end: bool,
    }

    impl Sp {
        fn new(sectors: u64) -> Sp {
            Sp { sectors, grain: 8, gtes: 4, flags: FLAG_NL_TEST | FLAG_ZERO_GTE, desc: None, grains: vec![], gd_at_end: false }
        }

        fn header(&self, gd_off: u64, desc: (u64, u64)) -> Vec<u8> {
            let mut h = vec![0u8; 512];
            h[..4].copy_from_slice(SPARSE_MAGIC);
            h[4..8].copy_from_slice(&1u32.to_le_bytes());
            h[8..12].copy_from_slice(&self.flags.to_le_bytes());
            h[12..20].copy_from_slice(&self.sectors.to_le_bytes());
            h[20..28].copy_from_slice(&self.grain.to_le_bytes());
            h[28..36].copy_from_slice(&desc.0.to_le_bytes());
            h[36..44].copy_from_slice(&desc.1.to_le_bytes());
            h[44..48].copy_from_slice(&self.gtes.to_le_bytes());
            h[56..64].copy_from_slice(&gd_off.to_le_bytes());
            h[73..77].copy_from_slice(b"\n \r\n");
            if self.flags & FLAG_COMPRESSED != 0 {
                h[77..79].copy_from_slice(&COMPRESS_DEFLATE.to_le_bytes());
            }
            h
        }

        fn build(&self) -> Vec<u8> {
            let gs = (self.grain * 512) as usize;
            let pad = |f: &mut Vec<u8>| f.resize(f.len().div_ceil(512) * 512, 0);
            let mut f = vec![0u8; 512];
            let desc = match &self.desc {
                Some(d) => {
                    f.resize(512 + 16 * 512, 0);
                    f[512..512 + d.len()].copy_from_slice(d.as_bytes());
                    (1, 16)
                }
                None => (0, 0),
            };
            let n_gd = self.sectors.div_ceil(self.grain).div_ceil(self.gtes as u64) as usize;
            let gd_off = f.len();
            f.resize(gd_off + n_gd * 4, 0);
            pad(&mut f);
            let mut gd = vec![0u32; n_gd];
            for (gi, _) in &self.grains {
                let d = (*gi / self.gtes as u64) as usize;
                if gd[d] == 0 {
                    gd[d] = (f.len() / 512) as u32;
                    f.resize(f.len() + self.gtes as usize * 4, 0);
                    pad(&mut f);
                }
            }
            for (gi, g) in &self.grains {
                let gte = match g {
                    G::Zero => GTE_ZERO,
                    G::Data(d) => {
                        let s = f.len();
                        f.extend_from_slice(d);
                        f.resize(s + gs, 0);
                        (s / 512) as u32
                    }
                    G::Packed(d) => {
                        let s = f.len();
                        let z = zlib(d);
                        f.extend_from_slice(&(gi * self.grain).to_le_bytes());
                        f.extend_from_slice(&(z.len() as u32).to_le_bytes());
                        f.extend_from_slice(&z);
                        pad(&mut f);
                        (s / 512) as u32
                    }
                };
                let d = (*gi / self.gtes as u64) as usize;
                let at = gd[d] as usize * 512 + (*gi % self.gtes as u64) as usize * 4;
                f[at..at + 4].copy_from_slice(&gte.to_le_bytes());
            }
            for (i, e) in gd.iter().enumerate() {
                f[gd_off + 4 * i..gd_off + 4 * i + 4].copy_from_slice(&e.to_le_bytes());
            }
            let real = self.header((gd_off / 512) as u64, desc);
            if self.gd_at_end {
                f[..512].copy_from_slice(&self.header(GD_AT_END, desc));
                let mut m = vec![0u8; 512];
                m[12..16].copy_from_slice(&MARKER_FOOTER.to_le_bytes());
                f.extend_from_slice(&m);
                f.extend_from_slice(&real);
                f.extend_from_slice(&[0u8; 512]); // end-of-stream marker
            } else {
                f[..512].copy_from_slice(&real);
            }
            f
        }

        /// What the disk should read as (no parent).
        fn expect(&self) -> Vec<u8> {
            let gs = (self.grain * 512) as usize;
            let mut v = vec![0u8; self.sectors as usize * 512];
            for (gi, g) in &self.grains {
                if let G::Data(d) | G::Packed(d) = g {
                    let s = *gi as usize * gs;
                    let n = d.len().min(v.len() - s);
                    v[s..s + n].copy_from_slice(&d[..n]);
                }
            }
            v
        }
    }

    fn descriptor(cid: u32, parent: Option<(u32, &str)>, ty: &str, extents: &str) -> String {
        let (pcid, hint) = match parent {
            Some((c, h)) => (format!("{c:08x}"), format!("parentFileNameHint=\"{h}\"\n")),
            None => ("ffffffff".into(), String::new()),
        };
        format!(
            "# Disk DescriptorFile\nversion=1\nencoding=\"UTF-8\"\nCID={cid:08x}\nparentCID={pcid}\ncreateType=\"{ty}\"\n{hint}\n# Extent description\n{extents}\n\n# The Disk Data Base\n#DDB\n\nddb.adapterType = \"lsilogic\"\n"
        )
    }

    fn read_all(img: &VmdkImage) -> Vec<u8> {
        let mut v = vec![0u8; img.size() as usize];
        img.read_at(&mut v, 0).unwrap();
        v
    }

    fn err(p: &Path) -> String {
        VmdkImage::open(p).unwrap_err().to_string()
    }

    #[test]
    fn monolithic_sparse_allocated_unallocated_zeroed() {
        let td = tempfile::tempdir().unwrap();
        // 130 sectors, 4 KiB grains: the last grain is partial. Grain
        // table 2 (grains 8..12) stays unallocated.
        let mut sp = Sp::new(130);
        sp.desc = Some(descriptor(0x1234, None, "monolithicSparse", "RW 130 SPARSE \"original name.vmdk\""));
        sp.grains = vec![(0, G::Data(pattern(1, 4096))), (1, G::Zero), (5, G::Data(pattern(2, 4096))), (16, G::Data(pattern(3, 1024)))];
        // Users rename monolithic files: the embedded name doesn't matter.
        let p = td.path().join("renamed.vmdk");
        std::fs::write(&p, sp.build()).unwrap();
        let img = VmdkImage::open(&p).unwrap();
        assert_eq!(img.size(), 130 * 512);
        assert!(img.read_only());
        assert_eq!(read_all(&img), sp.expect());
        // Odd offsets across grains.
        let mut b = vec![0u8; 5000];
        img.read_at(&mut b, 3000).unwrap();
        assert_eq!(b, sp.expect()[3000..8000]);
        assert!(img.read_at(&mut b, 130 * 512 - 10).is_err());
        assert!(matches!(img.write_at(&b, 0), Err(Error::ReadOnly)));
        assert_eq!(img.files(), &[p.canonicalize().unwrap()]);
    }

    #[test]
    fn split_sparse() {
        let td = tempfile::tempdir().unwrap();
        let mut a = Sp::new(64);
        a.grains = vec![(0, G::Data(pattern(1, 4096))), (7, G::Data(pattern(2, 4096)))];
        let mut b = Sp::new(40);
        b.grains = vec![(0, G::Data(pattern(3, 4096))), (4, G::Data(pattern(4, 4096)))];
        std::fs::write(td.path().join("My Disk-s001.vmdk"), a.build()).unwrap();
        std::fs::write(td.path().join("My Disk-s002.vmdk"), b.build()).unwrap();
        let ext = "RW 64 SPARSE \"My Disk-s001.vmdk\"\nRW 40 SPARSE \"My Disk-s002.vmdk\"";
        let p = td.path().join("My Disk.vmdk");
        std::fs::write(&p, descriptor(1, None, "twoGbMaxExtentSparse", ext)).unwrap();
        let img = VmdkImage::open(&p).unwrap();
        let mut want = a.expect();
        want.extend(b.expect());
        assert_eq!(read_all(&img), want);
        let mut buf = vec![0u8; 3000];
        img.read_at(&mut buf, 64 * 512 - 1000).unwrap();
        assert_eq!(buf, want[64 * 512 - 1000..64 * 512 + 2000]);
        assert_eq!(img.files().len(), 3);
        // An extent on its own: point at the descriptor.
        let e = err(&td.path().join("My Disk-s001.vmdk"));
        assert!(e.contains("split disk"), "{e}");
    }

    #[test]
    fn flat_with_offset_and_zero_extent() {
        let td = tempfile::tempdir().unwrap();
        let data = pattern(5, 8192);
        std::fs::write(td.path().join("d-flat.vmdk"), &data).unwrap();
        let ext = "RW 8 FLAT \"d-flat.vmdk\" 2\nRW 4 ZERO\nRW 2 VMFS \"d-flat.vmdk\"";
        let p = td.path().join("d.vmdk");
        std::fs::write(&p, descriptor(1, None, "monolithicFlat", ext)).unwrap();
        let img = VmdkImage::open(&p).unwrap();
        let mut want = data[1024..1024 + 4096].to_vec();
        want.extend(vec![0u8; 2048]);
        want.extend(&data[..1024]);
        assert_eq!(read_all(&img), want);
        assert_eq!(img.files().len(), 2);

        // Flat file shorter than the descriptor says.
        std::fs::write(&p, descriptor(1, None, "monolithicFlat", "RW 100 FLAT \"d-flat.vmdk\" 0")).unwrap();
        assert!(err(&p).contains("shorter"));
        // Sparse extent in a flat disk.
        std::fs::write(&p, descriptor(1, None, "monolithicFlat", "RW 8 SPARSE \"d-flat.vmdk\"")).unwrap();
        assert!(err(&p).contains("doesn't fit"));
    }

    #[test]
    fn delta_over_parent() {
        let td = tempfile::tempdir().unwrap();
        let mut base = Sp::new(64);
        base.desc = Some(descriptor(0xabcd_1234, None, "monolithicSparse", "RW 64 SPARSE \"base.vmdk\""));
        base.grains = (0..5).map(|i| (i, G::Data(pattern(10 + i, 4096)))).collect();
        std::fs::write(td.path().join("base.vmdk"), base.build()).unwrap();

        let mut delta = Sp::new(64);
        delta.desc = Some(descriptor(7, Some((0xabcd_1234, "base.vmdk")), "monolithicSparse", "RW 64 SPARSE \"base-000001.vmdk\""));
        delta.grains = vec![(1, G::Data(pattern(20, 4096))), (2, G::Zero)];
        let p = td.path().join("base-000001.vmdk");
        std::fs::write(&p, delta.build()).unwrap();
        let img = VmdkImage::open(&p).unwrap();
        let mut want = base.expect();
        want[4096..8192].copy_from_slice(&pattern(20, 4096));
        want[8192..12288].fill(0);
        assert_eq!(read_all(&img), want);
        assert_eq!(img.files().len(), 2);

        // Linked clone: absolute hint into another directory.
        let other = tempfile::tempdir().unwrap();
        let abs = td.path().join("base.vmdk").canonicalize().unwrap();
        delta.desc = Some(descriptor(7, Some((0xabcd_1234, abs.to_str().unwrap())), "monolithicSparse", "RW 64 SPARSE \"x.vmdk\""));
        let clone = other.path().join("clone.vmdk");
        std::fs::write(&clone, delta.build()).unwrap();
        let img = VmdkImage::open(&clone).unwrap();
        assert_eq!(read_all(&img), want);
        assert_eq!(img.files()[1], abs);

        // The parent must be a .vmdk.
        std::fs::copy(&abs, td.path().join("base.img")).unwrap();
        delta.desc = Some(descriptor(7, Some((0xabcd_1234, "base.img")), "monolithicSparse", "RW 64 SPARSE \"x.vmdk\""));
        std::fs::write(&p, delta.build()).unwrap();
        assert!(err(&p).contains("not a .vmdk"));
    }

    #[test]
    fn stream_optimized_compressed() {
        let td = tempfile::tempdir().unwrap();
        let mut sp = Sp::new(100);
        sp.flags = FLAG_NL_TEST | FLAG_COMPRESSED | FLAG_MARKERS;
        sp.gd_at_end = true;
        sp.desc = Some(descriptor(3, None, "streamOptimized", "RW 100 SPARSE \"export-disk1.vmdk\""));
        // A compressible grain, an incompressible one, the short last one.
        sp.grains = vec![(0, G::Packed(vec![0x5a; 4096])), (3, G::Packed(pattern(6, 4096))), (12, G::Packed(pattern(7, 2048)))];
        let p = td.path().join("export-disk1.vmdk");
        std::fs::write(&p, sp.build()).unwrap();
        let img = VmdkImage::open(&p).unwrap();
        assert_eq!(read_all(&img), sp.expect());
        drop(img);

        // Truncated download: the footer is gone.
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(f.metadata().unwrap().len() - 512).unwrap();
        assert!(err(&p).contains("footer"));
    }

    #[test]
    fn corrupt_compressed_grain_is_an_error() {
        let td = tempfile::tempdir().unwrap();
        let mut sp = Sp::new(16);
        sp.flags = FLAG_NL_TEST | FLAG_COMPRESSED | FLAG_MARKERS;
        sp.desc = Some(descriptor(3, None, "streamOptimized", "RW 16 SPARSE \"s.vmdk\""));
        sp.grains = vec![(0, G::Packed(pattern(6, 4096)))];
        let mut b = sp.build();
        // Overwrite compressed bytes behind the grain marker.
        let at = b.len() - 4096;
        b[at..at + 8].copy_from_slice(&[0xff; 8]);
        let p = td.path().join("s.vmdk");
        std::fs::write(&p, b).unwrap();
        let img = VmdkImage::open(&p).unwrap();
        let mut buf = vec![0u8; 512];
        assert!(img.read_at(&mut buf, 0).is_err());
    }

    #[test]
    fn rejects_bad_files() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        let p = dir.join("d.vmdk");
        let mut sp = Sp::new(64);
        sp.grains = vec![(0, G::Data(pattern(1, 4096)))];

        // Bad magic in an extent.
        std::fs::write(dir.join("junk.vmdk"), pattern(1, 4096)).unwrap();
        std::fs::write(&p, descriptor(1, None, "twoGbMaxExtentSparse", "RW 8 SPARSE \"junk.vmdk\"")).unwrap();
        assert!(err(&p).contains("bad magic"));

        // Grain directory outside the file.
        let mut b = sp.build();
        b[56..64].copy_from_slice(&1_000_000u64.to_le_bytes());
        std::fs::write(dir.join("e.vmdk"), &b).unwrap();
        std::fs::write(&p, descriptor(1, None, "twoGbMaxExtentSparse", "RW 64 SPARSE \"e.vmdk\"")).unwrap();
        assert!(err(&p).contains("grain directory outside"), "{}", err(&p));
        b[56..64].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
        std::fs::write(dir.join("e.vmdk"), &b).unwrap();
        assert!(VmdkImage::open(&p).is_err());
        // A grain table outside the file.
        let mut b = sp.build();
        b[512..516].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(dir.join("e.vmdk"), &b).unwrap();
        assert!(err(&p).contains("grain table outside"));
        // A grain past the end of the file: open works, the read fails.
        let mut b = sp.build();
        b[1024..1028].copy_from_slice(&100_000u32.to_le_bytes());
        std::fs::write(dir.join("e.vmdk"), &b).unwrap();
        let img = VmdkImage::open(&p).unwrap();
        assert!(img.read_at(&mut [0u8; 512], 0).is_err());
        drop(img);
        // Huge grain counts.
        let mut b = sp.build();
        b[12..20].copy_from_slice(&(MAX_SIZE / 512).to_le_bytes());
        b[20..28].copy_from_slice(&1u64.to_le_bytes());
        b[44..48].copy_from_slice(&1u32.to_le_bytes());
        std::fs::write(dir.join("e.vmdk"), &b).unwrap();
        std::fs::write(&p, descriptor(1, None, "twoGbMaxExtentSparse", &format!("RW {} SPARSE \"e.vmdk\"", MAX_SIZE / 512))).unwrap();
        assert!(err(&p).contains("too large"), "{}", err(&p));

        // Extent names that leave the directory.
        std::fs::write(dir.join("e.vmdk"), sp.build()).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.vmdk"), sp.build()).unwrap();
        let abs = outside.path().join("x.vmdk");
        for name in ["../x.vmdk", "sub/e.vmdk", abs.to_str().unwrap(), "..", ""] {
            std::fs::write(&p, descriptor(1, None, "twoGbMaxExtentSparse", &format!("RW 64 SPARSE \"{name}\""))).unwrap();
            let e = err(&p);
            assert!(e.contains("plain file name"), "{name}: {e}");
        }
        // A symlink next to the descriptor that points elsewhere.
        std::os::unix::fs::symlink(&abs, dir.join("link.vmdk")).unwrap();
        std::fs::write(&p, descriptor(1, None, "twoGbMaxExtentSparse", "RW 64 SPARSE \"link.vmdk\"")).unwrap();
        assert!(err(&p).contains("not a regular file next to"));

        // Refused kinds.
        for (ty, ext, msg) in [
            ("seSparse", "RW 64 SESPARSE \"e.vmdk\"", "seSparse"),
            ("vmfsSparse", "RW 64 VMFSSPARSE \"e.vmdk\"", "vmfsSparse"),
            ("vmfsRDM", "RW 64 VMFSRDM \"e.vmdk\"", "raw device"),
            ("vmfsRaw", "", "physical device"),
            ("customThing", "RW 64 SPARSE \"e.vmdk\"", "not supported"),
            ("monolithicSparse", "NOACCESS 64 SPARSE \"e.vmdk\"", "NOACCESS"),
            ("monolithicSparse", "RW 64 SPARSE \"e.vmdk", "bad extent line"),
            ("monolithicSparse", "", "no extents"),
        ] {
            std::fs::write(&p, descriptor(1, None, ty, ext)).unwrap();
            let e = err(&p);
            assert!(e.contains(msg), "{ty}: {e}");
        }
        let enc = descriptor(1, None, "monolithicSparse", "RW 64 SPARSE \"e.vmdk\"") + "encryption.keySafe = \"vmware:key/...\"\n";
        std::fs::write(&p, enc).unwrap();
        assert!(err(&p).contains("encrypted"));
        let mut cowd = vec![0u8; 2048];
        cowd[..4].copy_from_slice(COWD_MAGIC);
        std::fs::write(dir.join("c.vmdk"), &cowd).unwrap();
        assert!(err(&dir.join("c.vmdk")).contains("vmfsSparse"));
        // Not a descriptor at all.
        std::fs::write(&p, pattern(2, 300)).unwrap();
        assert!(VmdkImage::open(&p).is_err());
        std::fs::write(&p, vec![b'a'; 2 << 20]).unwrap();
        assert!(err(&p).contains("not a VMDK descriptor"));
        // Embedded descriptor out of range.
        let mut b = sp.build();
        b[28..36].copy_from_slice(&1u64.to_le_bytes());
        b[36..44].copy_from_slice(&(u64::MAX / 1024).to_le_bytes());
        std::fs::write(&p, &b).unwrap();
        assert!(err(&p).contains("out of range"));
        // Line endings mangled by a text-mode transfer.
        let mut b = sp.build();
        b[75] = b'\n';
        std::fs::write(&p, &b).unwrap();
        assert!(err(&p).contains("text-mode"));
    }

    #[test]
    fn cid_mismatch_and_loops() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        let mut base = Sp::new(64);
        base.desc = Some(descriptor(0x1111, None, "monolithicSparse", "RW 64 SPARSE \"base.vmdk\""));
        std::fs::write(dir.join("base.vmdk"), base.build()).unwrap();
        let mut delta = Sp::new(64);
        delta.desc = Some(descriptor(2, Some((0x2222, "base.vmdk")), "monolithicSparse", "RW 64 SPARSE \"d.vmdk\""));
        std::fs::write(dir.join("d.vmdk"), delta.build()).unwrap();
        let e = err(&dir.join("d.vmdk"));
        assert!(e.contains("changed after this snapshot") && e.contains("00001111"), "{e}");

        // a -> b -> a, and a disk that is its own parent.
        std::fs::write(dir.join("a.vmdk"), descriptor(1, Some((2, "b.vmdk")), "monolithicFlat", "RW 8 ZERO")).unwrap();
        std::fs::write(dir.join("b.vmdk"), descriptor(2, Some((1, "a.vmdk")), "monolithicFlat", "RW 8 ZERO")).unwrap();
        assert!(err(&dir.join("a.vmdk")).contains("loops"));
        std::fs::write(dir.join("s.vmdk"), descriptor(1, Some((1, "./s.vmdk")), "monolithicFlat", "RW 8 ZERO")).unwrap();
        assert!(err(&dir.join("s.vmdk")).contains("loops"));

        // Too deep: c0 <- c1 <- ... <- c70.
        std::fs::write(dir.join("c0.vmdk"), descriptor(100, None, "monolithicFlat", "RW 8 ZERO")).unwrap();
        for i in 1..=70u32 {
            let parent = format!("c{}.vmdk", i - 1);
            std::fs::write(dir.join(format!("c{i}.vmdk")), descriptor(100 + i, Some((99 + i, &parent)), "monolithicFlat", "RW 8 ZERO")).unwrap();
        }
        assert!(VmdkImage::open(&dir.join("c60.vmdk")).is_ok());
        assert!(err(&dir.join("c70.vmdk")).contains("longer than"));

        // Hint and parentCID must come together.
        let half = descriptor(1, None, "monolithicFlat", "RW 8 ZERO") + "parentFileNameHint=\"base.vmdk\"\n";
        std::fs::write(dir.join("h.vmdk"), half).unwrap();
        assert!(err(&dir.join("h.vmdk")).contains("no parentCID"));
    }
}
