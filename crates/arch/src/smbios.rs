//! SMBIOS 3.0 tables, handed to UEFI through fw_cfg like QEMU does
//! (`etc/smbios/smbios-tables` + `etc/smbios/smbios-anchor`).
//!
//! Guests use them for hardware identity: Windows reads the system UUID,
//! vendor and memory layout; `dmidecode` shows them on Linux.

pub const TABLES_FILE: &str = "etc/smbios/smbios-tables";
pub const ANCHOR_FILE: &str = "etc/smbios/smbios-anchor";

#[derive(Debug, Clone)]
pub struct SmbiosConfig {
    pub vm_name: String,
    /// Stable per VM (activation, licensing and hardware ids key on it).
    pub uuid: [u8; 16],
    pub cpus: u32,
    pub memory_bytes: u64,
    pub ram_base: u64,
}

#[derive(Debug)]
pub struct Smbios {
    pub tables: Vec<u8>,
    pub anchor: Vec<u8>,
}

/// One structure: formatted area then a string set.
struct Builder {
    out: Vec<u8>,
    handle: u16,
}

impl Builder {
    fn add(&mut self, ty: u8, body: &[u8], strings: &[&str]) {
        self.out.push(ty);
        self.out.push((4 + body.len()) as u8);
        self.out.extend_from_slice(&self.handle.to_le_bytes());
        self.out.extend_from_slice(body);
        for s in strings {
            // No NULs inside strings; they'd end the set early.
            self.out.extend(s.bytes().filter(|&b| b != 0));
            self.out.push(0);
        }
        if strings.is_empty() {
            self.out.push(0);
        }
        self.out.push(0);
        self.handle += 1;
    }
}

/// SMBIOS UUID: first three fields little endian (spec 7.2.1).
fn smbios_uuid(u: &[u8; 16]) -> [u8; 16] {
    let mut o = *u;
    o[0..4].reverse();
    o[4..6].reverse();
    o[6..8].reverse();
    o
}

pub fn build(c: &SmbiosConfig) -> Smbios {
    let mut b = Builder { out: Vec::new(), handle: 0 };
    let version = env!("CARGO_PKG_VERSION");

    // Type 0, BIOS. Strings: 1 vendor, 2 version, 3 date.
    let mut t0 = vec![1, 2, 0x00, 0xe8, 3, 0xff];
    t0.extend_from_slice(&(1u64 << 3).to_le_bytes()); // characteristics not supported
    t0.extend_from_slice(&[0x03, 0x18]); // ext1: ACPI, USB legacy; ext2: UEFI (bit 3) + VM (bit 4)
    t0.extend_from_slice(&[0, 0, 0xff, 0xff]); // BIOS / EC release
    t0.extend_from_slice(&16u16.to_le_bytes()); // extended ROM size: 16 MiB (ROM size 0xff)
    b.add(0, &t0, &["vroom", version, "10/14/2026"]);

    // Type 1, system. 1 manufacturer, 2 product, 3 version, 4 serial, 5 sku, 6 family.
    let mut t1 = vec![1, 2, 3, 4];
    t1.extend_from_slice(&smbios_uuid(&c.uuid));
    t1.extend_from_slice(&[6, 5, 6]); // wake-up: power switch; sku; family
    let serial: String = c.uuid.iter().map(|x| format!("{x:02X}")).collect();
    b.add(1, &t1, &["Vroom", "Vroom Virtual Machine", version, &serial, &c.vm_name, "Virtual Machine"]);

    // Type 3, chassis. 1 manufacturer, 2 version, 3 serial, 4 asset tag.
    let t3 = vec![1, 0x01, 2, 3, 4, 0x03, 0x03, 0x03, 0x02, 0, 0, 0, 0, 0, 0, 0, 0];
    b.add(3, &t3, &["Vroom", version, &serial, "none"]);

    // Type 4, processor. 1 socket, 2 manufacturer, 3 version.
    let cores = c.cpus.min(255) as u8;
    let mut t4 = vec![1, 0x03, 0x02, 2]; // socket, CPU, family: unknown, manufacturer
    t4.extend_from_slice(&[0u8; 8]); // processor id
    t4.extend_from_slice(&[3, 0]); // version string, voltage
    t4.extend_from_slice(&0u16.to_le_bytes()); // external clock
    t4.extend_from_slice(&0u16.to_le_bytes()); // max speed
    t4.extend_from_slice(&0u16.to_le_bytes()); // current speed
    t4.extend_from_slice(&[0x41, 0x06]); // populated + enabled; upgrade: none
    t4.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]); // no caches
    t4.extend_from_slice(&[0, 0, 0]); // serial, asset, part (no strings)
    t4.extend_from_slice(&[cores, cores, cores]);
    t4.extend_from_slice(&0x04u16.to_le_bytes()); // characteristics: 64-bit
    t4.extend_from_slice(&0x0101u16.to_le_bytes()); // family 2: ARMv8
    b.add(4, &t4, &["CPU0", "Vroom", "Virtual CPU"]);

    // Type 16, physical memory array (handle referenced by 17 and 19).
    let array_handle = b.handle;
    let kib = c.memory_bytes / 1024;
    let mut t16 = vec![0x03, 0x03, 0x03]; // location: system board; use: system memory; ECC: none
    t16.extend_from_slice(&(if kib >= 0x8000_0000 { 0x8000_0000u32 } else { kib as u32 }).to_le_bytes());
    t16.extend_from_slice(&0xfffeu16.to_le_bytes());
    t16.extend_from_slice(&1u16.to_le_bytes());
    t16.extend_from_slice(&kib.saturating_mul(1024).to_le_bytes()); // extended capacity, bytes
    b.add(16, &t16, &[]);

    // Type 17, memory device. 1 device locator, 2 manufacturer.
    let mib = c.memory_bytes >> 20;
    let mut t17 = Vec::new();
    t17.extend_from_slice(&array_handle.to_le_bytes());
    t17.extend_from_slice(&0xfffeu16.to_le_bytes());
    t17.extend_from_slice(&64u16.to_le_bytes());
    t17.extend_from_slice(&64u16.to_le_bytes());
    t17.extend_from_slice(&(if mib < 0x7fff { mib as u16 } else { 0x7fff }).to_le_bytes());
    t17.extend_from_slice(&[0x09, 0, 1, 0, 0x07]); // form: DIMM; set; locator; bank; type: RAM
    t17.extend_from_slice(&0u16.to_le_bytes()); // type detail
    t17.extend_from_slice(&0u16.to_le_bytes()); // speed
    t17.extend_from_slice(&[2, 0, 0, 0, 0]); // manufacturer, serial, asset, part, attributes
    t17.extend_from_slice(&(if mib >= 0x7fff { mib as u32 } else { 0 }).to_le_bytes()); // extended size
    b.add(17, &t17, &["DIMM 0", "Vroom"]);

    // Type 19, memory array mapped address (extended form, bytes).
    let mut t19 = Vec::new();
    t19.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
    t19.extend_from_slice(&0xffff_ffffu32.to_le_bytes());
    t19.extend_from_slice(&array_handle.to_le_bytes());
    t19.push(1);
    t19.extend_from_slice(&c.ram_base.to_le_bytes());
    t19.extend_from_slice(&(c.ram_base + c.memory_bytes - 1).to_le_bytes());
    b.add(19, &t19, &[]);

    // Type 32, boot information: no errors.
    b.add(32, &[0, 0, 0, 0, 0, 0, 0], &[]);
    b.add(127, &[], &[]);

    let tables = b.out;
    // SMBIOS 3.0 entry point; firmware fills in the table address.
    let mut a = Vec::with_capacity(24);
    a.extend_from_slice(b"_SM3_");
    a.push(0); // checksum, below
    a.push(24);
    a.extend_from_slice(&[3, 0, 0, 1, 0]); // 3.0, docrev 0, EPS rev 1, reserved
    a.extend_from_slice(&(tables.len() as u32).to_le_bytes());
    a.extend_from_slice(&0u64.to_le_bytes());
    let sum = a.iter().fold(0u8, |s, &x| s.wrapping_add(x));
    a[5] = 0u8.wrapping_sub(sum);
    Smbios { tables, anchor: a }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk structures the way a parser would.
    fn structures(t: &[u8]) -> Vec<(u8, usize)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < t.len() {
            let (ty, len) = (t[i], t[i + 1] as usize);
            let mut j = i + len;
            while !(t[j] == 0 && t[j + 1] == 0) {
                j += 1;
            }
            out.push((ty, len));
            i = j + 2;
        }
        out
    }

    #[test]
    fn tables_parse_and_checksum() {
        let s = build(&SmbiosConfig { vm_name: "win11".into(), uuid: [7; 16], cpus: 4, memory_bytes: 6 << 30, ram_base: 0x4000_0000 });
        let types: Vec<u8> = structures(&s.tables).iter().map(|x| x.0).collect();
        assert_eq!(types, [0, 1, 3, 4, 16, 17, 19, 32, 127]);
        // Spec minimum lengths for the 3.0 layouts we emit.
        let lens: Vec<usize> = structures(&s.tables).iter().map(|x| x.1).collect();
        assert_eq!(&lens[..3], &[0x1a, 0x1b, 0x15]);
        assert_eq!(lens[3], 0x2a);
        assert_eq!(s.anchor.len(), 24);
        assert_eq!(s.anchor.iter().fold(0u8, |a, &b| a.wrapping_add(b)), 0);
    }
}
