//! Tiny AML encoder: just what our DSDT needs.
//!
//! Every builder returns encoded bytes; containers take children as bytes.
//! Names are single 4-char segments, or absolute paths like "\\_SB_.PCI0.GSI0".

const ZERO: u8 = 0x00;
const ONE: u8 = 0x01;
const ONES: u8 = 0xff;
const NAME_OP: u8 = 0x08;
const BYTE_PREFIX: u8 = 0x0a;
const WORD_PREFIX: u8 = 0x0b;
const DWORD_PREFIX: u8 = 0x0c;
const STRING_PREFIX: u8 = 0x0d;
const QWORD_PREFIX: u8 = 0x0e;
const SCOPE_OP: u8 = 0x10;
const BUFFER_OP: u8 = 0x11;
const PACKAGE_OP: u8 = 0x12;
const METHOD_OP: u8 = 0x14;
const DUAL_NAME: u8 = 0x2e;
const MULTI_NAME: u8 = 0x2f;
const EXT_OP: u8 = 0x5b;
const DEVICE_OP: u8 = 0x82;
const ARG0: u8 = 0x68;
const RETURN_OP: u8 = 0xa4;

/// Encode a PkgLength covering `len` payload bytes (plus itself).
pub fn pkg_length(len: usize) -> Vec<u8> {
    // Try 1..=4 byte encodings; total includes the length bytes.
    for n in 1..=4usize {
        let total = len + n;
        let ok = match n {
            1 => total < 0x40,
            2 => total < 0x1000,
            3 => total < 0x10_0000,
            _ => total < 0x1000_0000,
        };
        if ok {
            if n == 1 {
                return vec![total as u8];
            }
            let mut v = vec![(((n - 1) as u8) << 6) | (total & 0xf) as u8];
            for i in 0..n - 1 {
                v.push((total >> (4 + 8 * i)) as u8);
            }
            return v;
        }
    }
    panic!("AML object too large")
}

fn seg(s: &str) -> [u8; 4] {
    let mut out = [b'_'; 4];
    for (o, b) in out.iter_mut().zip(s.bytes()) {
        *o = b;
    }
    out
}

/// NameString: "FOO", "\\_SB_", "\\_SB_.PCI0", "PCI0.GSI0".
pub fn name_string(path: &str) -> Vec<u8> {
    let mut v = Vec::new();
    let rest = match path.strip_prefix('\\') {
        Some(r) => {
            v.push(b'\\');
            r
        }
        None => path,
    };
    let segs: Vec<&str> = if rest.is_empty() { vec![] } else { rest.split('.').collect() };
    match segs.len() {
        0 => v.push(0),
        1 => {}
        2 => v.push(DUAL_NAME),
        n => {
            v.push(MULTI_NAME);
            v.push(n as u8);
        }
    }
    for s in segs {
        v.extend_from_slice(&seg(s));
    }
    v
}

pub fn int(x: u64) -> Vec<u8> {
    match x {
        0 => vec![ZERO],
        1 => vec![ONE],
        x if x <= 0xff => vec![BYTE_PREFIX, x as u8],
        x if x <= 0xffff => {
            let mut v = vec![WORD_PREFIX];
            v.extend_from_slice(&(x as u16).to_le_bytes());
            v
        }
        x if x <= 0xffff_ffff => {
            let mut v = vec![DWORD_PREFIX];
            v.extend_from_slice(&(x as u32).to_le_bytes());
            v
        }
        x => {
            let mut v = vec![QWORD_PREFIX];
            v.extend_from_slice(&x.to_le_bytes());
            v
        }
    }
}

pub fn ones() -> Vec<u8> {
    vec![ONES]
}

pub fn string(s: &str) -> Vec<u8> {
    let mut v = vec![STRING_PREFIX];
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

/// EISA id like "PNP0A08" as the compressed DWORD.
pub fn eisa_id(id: &str) -> Vec<u8> {
    let b = id.as_bytes();
    let c = |x: u8| ((x - b'@') & 0x1f) as u32;
    let h = |x: u8| (x as char).to_digit(16).unwrap_or(0);
    let v = (c(b[0]) << 26) | (c(b[1]) << 21) | (c(b[2]) << 16) | (h(b[3]) << 12) | (h(b[4]) << 8) | (h(b[5]) << 4) | h(b[6]);
    let mut out = vec![DWORD_PREFIX];
    out.extend_from_slice(&v.swap_bytes().to_le_bytes());
    out
}

pub fn name(n: &str, value: Vec<u8>) -> Vec<u8> {
    let mut v = vec![NAME_OP];
    v.extend(name_string(n));
    v.extend(value);
    v
}

fn with_pkg(op: &[u8], body: Vec<u8>) -> Vec<u8> {
    let mut v = op.to_vec();
    v.extend(pkg_length(body.len()));
    v.extend(body);
    v
}

pub fn scope(path: &str, children: Vec<Vec<u8>>) -> Vec<u8> {
    let mut body = name_string(path);
    body.extend(children.concat());
    with_pkg(&[SCOPE_OP], body)
}

pub fn device(n: &str, children: Vec<Vec<u8>>) -> Vec<u8> {
    let mut body = name_string(n);
    body.extend(children.concat());
    with_pkg(&[EXT_OP, DEVICE_OP], body)
}

/// Method with `args` arguments and a raw AML body.
pub fn method(n: &str, args: u8, serialized: bool, body: Vec<Vec<u8>>) -> Vec<u8> {
    let mut b = name_string(n);
    b.push((args & 7) | if serialized { 8 } else { 0 });
    b.extend(body.concat());
    with_pkg(&[METHOD_OP], b)
}

pub fn ret(value: Vec<u8>) -> Vec<u8> {
    let mut v = vec![RETURN_OP];
    v.extend(value);
    v
}

pub fn arg(n: u8) -> Vec<u8> {
    vec![ARG0 + n]
}

pub fn package(items: Vec<Vec<u8>>) -> Vec<u8> {
    let mut body = vec![items.len() as u8];
    body.extend(items.concat());
    with_pkg(&[PACKAGE_OP], body)
}

pub fn buffer(data: &[u8]) -> Vec<u8> {
    let mut body = int(data.len() as u64);
    body.extend_from_slice(data);
    with_pkg(&[BUFFER_OP], body)
}

/// A reference to a named object (for packages like _PRT).
pub fn path(p: &str) -> Vec<u8> {
    name_string(p)
}

/// Resource template: concatenated descriptors + end tag.
pub fn resource_template(descs: Vec<Vec<u8>>) -> Vec<u8> {
    let mut d = descs.concat();
    d.extend_from_slice(&[0x79, 0]);
    buffer(&d)
}

pub mod res {
    pub fn memory32_fixed(base: u32, len: u32, rw: bool) -> Vec<u8> {
        let mut v = vec![0x86, 9, 0, rw as u8];
        v.extend_from_slice(&base.to_le_bytes());
        v.extend_from_slice(&len.to_le_bytes());
        v
    }

    /// Extended interrupt, consumer. `level`=true -> level triggered.
    pub fn interrupt(gsiv: &[u32], level: bool, active_high: bool, shared: bool) -> Vec<u8> {
        let len = 2 + 4 * gsiv.len() as u16;
        let flags = 1 | (!level as u8) << 1 | (!active_high as u8) << 2 | (shared as u8) << 3;
        let mut v = vec![0x89];
        v.extend_from_slice(&len.to_le_bytes());
        v.push(flags);
        v.push(gsiv.len() as u8);
        for g in gsiv {
            v.extend_from_slice(&g.to_le_bytes());
        }
        v
    }

    /// Bus number range, producer.
    pub fn word_bus(min: u16, max: u16) -> Vec<u8> {
        let mut v = vec![0x88, 13, 0, 2, 0x0c, 0];
        for x in [0u16, min, max, 0, max - min + 1] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v
    }

    fn dword_space(kind: u8, type_flags: u8, min: u32, len: u32, translation: u32) -> Vec<u8> {
        // flags: MinFixed | MaxFixed, producer, positive decode
        let mut v = vec![0x87, 23, 0, kind, 0x0c, type_flags];
        for x in [0u32, min, min + len - 1, translation, len] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v
    }

    /// 32-bit MMIO window (read-write, non-cacheable).
    pub fn dword_memory(min: u32, len: u32) -> Vec<u8> {
        dword_space(0, 0x01, min, len, 0)
    }

    /// I/O window translated to CPU MMIO at `translation`.
    pub fn dword_io(min: u32, len: u32, translation: u32) -> Vec<u8> {
        dword_space(1, 0x03, min, len, translation)
    }

    /// 64-bit MMIO window.
    pub fn qword_memory(min: u64, len: u64) -> Vec<u8> {
        let mut v = vec![0x8a, 43, 0, 0, 0x0c, 0x01];
        for x in [0u64, min, min + len - 1, 0, len] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkg_lengths() {
        assert_eq!(pkg_length(0x3e), vec![0x3f]);
        assert_eq!(pkg_length(0x3f), vec![0x41, 0x04]); // 0x41 total -> 2-byte form
        let l = pkg_length(0x1234);
        assert_eq!(l.len(), 3);
        let total = (l[0] as usize & 0xf) | (l[1] as usize) << 4 | (l[2] as usize) << 12;
        assert_eq!(total, 0x1234 + 3);
    }

    #[test]
    fn names_and_ids() {
        assert_eq!(name_string("PCI0"), b"PCI0".to_vec());
        assert_eq!(name_string("\\_SB"), b"\\_SB_".to_vec());
        assert_eq!(name_string("\\_SB.PCI0"), [&[b'\\', DUAL_NAME][..], b"_SB_PCI0"].concat());
        assert_eq!(name_string("\\_SB.PCI0.GSI0")[..3], [b'\\', MULTI_NAME, 3]);
        // PNP0A08 -> 0x080AD041 in memory order 41 D0 0A 08
        assert_eq!(eisa_id("PNP0A08"), vec![DWORD_PREFIX, 0x41, 0xd0, 0x0a, 0x08]);
        assert_eq!(int(0x12345), vec![DWORD_PREFIX, 0x45, 0x23, 0x01, 0]);
    }
}
