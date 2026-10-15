//! ACPI table plumbing shared by boards: table headers, checksums, and the
//! fw_cfg "linker/loader" script EDK2 uses to place and patch tables.

pub mod aml;

/// The names in every table header (from the VM's `[identity]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpiIds {
    pub oem_id: [u8; 6],
    pub table_id: [u8; 8],
    pub creator_id: [u8; 4],
    /// FADT hypervisor vendor; zeros: don't say.
    pub hypervisor_id: [u8; 8],
}

impl AcpiIds {
    /// Space padded, as ACPI wants (hypervisor id: zero padded).
    pub fn new(oem: &str, table: &str, creator: &str, hypervisor: Option<&str>) -> Self {
        fn pad<const N: usize>(s: &str, fill: u8) -> [u8; N] {
            let mut o = [fill; N];
            for (d, b) in o.iter_mut().zip(s.bytes()) {
                *d = b;
            }
            o
        }
        AcpiIds {
            oem_id: pad(oem, b' '),
            table_id: pad(table, b' '),
            creator_id: pad(creator, b' '),
            hypervisor_id: hypervisor.map_or([0; 8], |h| pad(h, 0)),
        }
    }

    /// Stamp the header of a finished table.
    fn stamp(&self, t: &mut [u8]) {
        if t.len() >= 36 {
            t[10..16].copy_from_slice(&self.oem_id);
            t[16..24].copy_from_slice(&self.table_id);
            t[28..32].copy_from_slice(&self.creator_id);
        }
    }
}

impl Default for AcpiIds {
    fn default() -> Self {
        AcpiIds::new("VROOM", "VROOMVM", "VRM", Some("VROOM"))
    }
}

pub const RSDP_FILE: &str = "etc/acpi/rsdp";
pub const TABLES_FILE: &str = "etc/acpi/tables";
pub const LOADER_FILE: &str = "etc/table-loader";

/// An ACPI table under construction: header + body.
#[derive(Debug, Clone)]
pub struct Table {
    pub bytes: Vec<u8>,
}

impl Table {
    pub fn new(sig: &[u8; 4], revision: u8) -> Self {
        let mut b = Vec::with_capacity(256);
        b.extend_from_slice(sig);
        b.extend_from_slice(&[0; 4]); // length, fixed in finish()
        b.push(revision);
        b.push(0); // checksum, filled by the loader
        // OEM, table and creator IDs: stamped by `assemble`.
        b.extend_from_slice(&[b' '; 14]);
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&[b' '; 4]);
        b.extend_from_slice(&1u32.to_le_bytes());
        Table { bytes: b }
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.bytes.push(v);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.bytes.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.bytes.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.bytes.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn raw(&mut self, v: &[u8]) -> &mut Self {
        self.bytes.extend_from_slice(v);
        self
    }
    pub fn zeros(&mut self, n: usize) -> &mut Self {
        self.bytes.resize(self.bytes.len() + n, 0);
        self
    }
    /// Generic Address Structure for 32-bit MMIO.
    pub fn gas_mmio32(&mut self, addr: u64) -> &mut Self {
        self.u8(0).u8(32).u8(0).u8(3).u64(addr)
    }
    pub fn len(&self) -> usize {
        self.bytes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn finish(mut self) -> Vec<u8> {
        let l = self.bytes.len() as u32;
        self.bytes[4..8].copy_from_slice(&l.to_le_bytes());
        self.bytes
    }
}

/// Compute a checksum byte so the region sums to zero.
pub fn checksum(b: &[u8]) -> u8 {
    0u8.wrapping_sub(b.iter().fold(0u8, |a, x| a.wrapping_add(*x)))
}

fn name56(n: &str) -> [u8; 56] {
    let mut a = [0u8; 56];
    a[..n.len()].copy_from_slice(n.as_bytes());
    a
}

/// QEMU BIOS linker/loader commands (128 bytes each).
#[derive(Debug, Default)]
pub struct Loader {
    pub bytes: Vec<u8>,
}

impl Loader {
    fn cmd(&mut self, c: u32, body: &[u8]) {
        let mut e = [0u8; 128];
        e[..4].copy_from_slice(&c.to_le_bytes());
        e[4..4 + body.len()].copy_from_slice(body);
        self.bytes.extend_from_slice(&e);
    }

    /// zone: 1 = high memory, 2 = "FSEG" (below 1 MiB on x86).
    pub fn allocate(&mut self, file: &str, align: u32, zone: u8) {
        let mut b = name56(file).to_vec();
        b.extend_from_slice(&align.to_le_bytes());
        b.push(zone);
        self.cmd(1, &b);
    }

    /// dest[offset..offset+size] += address of src.
    pub fn add_pointer(&mut self, dest: &str, src: &str, offset: u32, size: u8) {
        let mut b = name56(dest).to_vec();
        b.extend_from_slice(&name56(src));
        b.extend_from_slice(&offset.to_le_bytes());
        b.push(size);
        self.cmd(2, &b);
    }

    pub fn add_checksum(&mut self, file: &str, at: u32, start: u32, len: u32) {
        let mut b = name56(file).to_vec();
        b.extend_from_slice(&at.to_le_bytes());
        b.extend_from_slice(&start.to_le_bytes());
        b.extend_from_slice(&len.to_le_bytes());
        self.cmd(3, &b);
    }
}

/// Final fw_cfg payloads.
#[derive(Debug, Clone)]
pub struct AcpiBlobs {
    pub rsdp: Vec<u8>,
    pub tables: Vec<u8>,
    pub loader: Vec<u8>,
}

/// Lay out tables, build XSDT + RSDP and the loader script.
/// `dsdt` is pointed to by the FADT (`fadt_dsdt_offset` = X_DSDT field).
pub fn assemble(ids: &AcpiIds, fadt: Vec<u8>, fadt_dsdt_offset: usize, dsdt: Vec<u8>, others: Vec<Vec<u8>>) -> AcpiBlobs {
    let mut tables = Vec::new();
    let mut ld = Loader::default();
    ld.allocate(RSDP_FILE, 16, 2);
    ld.allocate(TABLES_FILE, 64, 1);

    let place = |mut t: Vec<u8>, tables: &mut Vec<u8>| -> (u32, u32) {
        ids.stamp(&mut t);
        while !tables.len().is_multiple_of(8) {
            tables.push(0);
        }
        let off = tables.len() as u32;
        let len = t.len() as u32;
        tables.extend(t);
        (off, len)
    };

    let dsdt_at = place(dsdt, &mut tables);
    let mut fadt = fadt;
    fadt[fadt_dsdt_offset..fadt_dsdt_offset + 8].copy_from_slice(&(dsdt_at.0 as u64).to_le_bytes());
    let fadt_at = place(fadt, &mut tables);
    ld.add_pointer(TABLES_FILE, TABLES_FILE, fadt_at.0 + fadt_dsdt_offset as u32, 8);

    let mut placed = vec![fadt_at];
    for t in others {
        placed.push(place(t, &mut tables));
    }

    let mut xsdt = Table::new(b"XSDT", 1);
    for &(off, _) in &placed {
        xsdt.u64(off as u64);
    }
    let xsdt = xsdt.finish();
    let xsdt_at = place(xsdt, &mut tables);
    for i in 0..placed.len() as u32 {
        ld.add_pointer(TABLES_FILE, TABLES_FILE, xsdt_at.0 + 36 + 8 * i, 8);
    }

    for &(off, len) in std::iter::once(&dsdt_at).chain(placed.iter()).chain(std::iter::once(&xsdt_at)) {
        ld.add_checksum(TABLES_FILE, off + 9, off, len);
    }

    let mut rsdp = Vec::with_capacity(36);
    rsdp.extend_from_slice(b"RSD PTR ");
    rsdp.push(0); // checksum (first 20 bytes)
    rsdp.extend_from_slice(&ids.oem_id);
    rsdp.push(2); // revision: ACPI 2.0+
    rsdp.extend_from_slice(&0u32.to_le_bytes()); // RSDT: none
    rsdp.extend_from_slice(&36u32.to_le_bytes());
    rsdp.extend_from_slice(&(xsdt_at.0 as u64).to_le_bytes());
    rsdp.push(0); // extended checksum
    rsdp.extend_from_slice(&[0; 3]);
    ld.add_pointer(RSDP_FILE, TABLES_FILE, 24, 8);
    ld.add_checksum(RSDP_FILE, 8, 0, 20);
    ld.add_checksum(RSDP_FILE, 32, 0, 36);

    AcpiBlobs { rsdp, tables, loader: ld.bytes }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the loader like firmware would, at a fake base address.
    pub(crate) fn link(b: &AcpiBlobs, base: u64, rsdp_base: u64) -> (Vec<u8>, Vec<u8>) {
        let mut files = [(RSDP_FILE, b.rsdp.clone(), rsdp_base), (TABLES_FILE, b.tables.clone(), base)];
        for c in b.loader.chunks(128) {
            let name = |o: usize| String::from_utf8_lossy(&c[o..o + 56]).trim_end_matches('\0').to_string();
            let u32_at = |o: usize| u32::from_le_bytes([c[o], c[o + 1], c[o + 2], c[o + 3]]);
            match u32_at(0) {
                2 => {
                    let (dest, src, off) = (name(4), name(60), u32_at(116) as usize);
                    let src_base = files.iter().find(|f| f.0 == src).unwrap().2;
                    let d = files.iter_mut().find(|f| f.0 == dest).unwrap();
                    let mut v = [0u8; 8];
                    v.copy_from_slice(&d.1[off..off + 8]);
                    let nv = u64::from_le_bytes(v) + src_base;
                    d.1[off..off + 8].copy_from_slice(&nv.to_le_bytes());
                }
                3 => {
                    let (f, at, start, len) = (name(4), u32_at(60) as usize, u32_at(64) as usize, u32_at(68) as usize);
                    let d = files.iter_mut().find(|x| x.0 == f).unwrap();
                    d.1[at] = 0;
                    d.1[at] = checksum(&d.1[start..start + len]);
                }
                _ => {}
            }
        }
        (files[0].1.clone(), files[1].1.clone())
    }

    #[test]
    fn assemble_links_and_checksums() {
        let mut fadt = Table::new(b"FACP", 6);
        fadt.zeros(276 - 36);
        let dsdt = Table::new(b"DSDT", 2).finish();
        let mut madt = Table::new(b"APIC", 4);
        madt.u32(0).u32(0);
        let ids = AcpiIds::new("DELL", "PE_SC3", "INTL", None);
        let b = assemble(&ids, fadt.finish(), 140, dsdt, vec![madt.finish()]);
        // Every header carries the configured names.
        let hdrs = b.tables.windows(10).filter(|w| w[..4] == *b"DELL" && w[4..] == *b"  PE_S").count();
        assert_eq!(hdrs, 4, "dsdt, fadt, madt, xsdt");
        assert_eq!(&b.rsdp[9..15], b"DELL  ");
        let base = 0x7000_0000u64;
        let (rsdp, tables) = link(&b, base, 0x6000_0000);
        assert_eq!(checksum(&rsdp[..20]), 0);
        assert_eq!(checksum(&rsdp), 0);
        let xsdt = u64::from_le_bytes(rsdp[24..32].try_into().unwrap()) - base;
        let x = &tables[xsdt as usize..];
        assert_eq!(&x[..4], b"XSDT");
        let fadt_at = u64::from_le_bytes(x[36..44].try_into().unwrap()) - base;
        let f = &tables[fadt_at as usize..];
        assert_eq!(&f[..4], b"FACP");
        assert_eq!(checksum(&f[..276]), 0);
        let dsdt_at = u64::from_le_bytes(f[140..148].try_into().unwrap()) - base;
        assert_eq!(&tables[dsdt_at as usize..dsdt_at as usize + 4], b"DSDT");
    }
}
