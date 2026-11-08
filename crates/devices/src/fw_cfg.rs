//! QEMU fw_cfg, the channel EDK2 uses to get ACPI tables,
//! the ramfb config and boot settings.
//!
//! MMIO layout at `base`: data @0x0 (byte stream), selector @0x8 (BE u16),
//! DMA address @0x10 (BE u64; writing the low half starts the transfer).
//! [`FwCfgIo`] supplies the PC port-I/O transport over the same item/DMA core.
//! Protocol: <https://www.qemu.org/docs/master/specs/fw_cfg.html>.

use std::collections::BTreeMap;

use vroom_memory::{GuestAddress, GuestMemory};
use vroom_snapshot::{decode, encode, Snapshottable};
use serde::{Deserialize, Serialize};

use crate::BusDevice;

pub const SIGNATURE: u16 = 0x0000;
pub const ID: u16 = 0x0001;
pub const RAM_SIZE: u16 = 0x0003;
pub const NB_CPUS: u16 = 0x0005;
pub const FILE_DIR: u16 = 0x0019;
const FILE_FIRST: u16 = 0x0020;

const DMA_ERROR: u32 = 1;
const DMA_READ: u32 = 2;
const DMA_SKIP: u32 = 4;
const DMA_SELECT: u32 = 8;
const DMA_WRITE: u32 = 0x10;

/// Biggest single DMA transfer we do (ACPI blobs are a few KiB).
const MAX_DMA: u32 = 16 << 20;
/// "QEMU" with DMA support, little endian bitmap.
const FEATURES: u32 = 0b11;

/// Called with the new contents after the guest writes a file.
pub type WriteHook = Box<dyn FnMut(&[u8]) + Send>;

struct Item {
    data: Vec<u8>,
    /// Only items with a hook accept guest writes.
    on_write: Option<WriteHook>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct State {
    selected: u16,
    offset: usize,
    dma_hi: u32,
}

pub struct FwCfg {
    base: u64,
    mem: GuestMemory,
    items: BTreeMap<u16, Item>,
    files: Vec<(String, u16)>,
    st: State,
}

impl std::fmt::Debug for FwCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FwCfg").field("files", &self.files).finish()
    }
}

impl FwCfg {
    pub fn new(base: u64, mem: GuestMemory, cpus: u16, ram: u64) -> Self {
        let mut f = FwCfg { base, mem, items: BTreeMap::new(), files: Vec::new(), st: State::default() };
        f.set(SIGNATURE, b"QEMU".to_vec());
        f.set(ID, FEATURES.to_le_bytes().to_vec());
        f.set(NB_CPUS, cpus.to_le_bytes().to_vec());
        f.set(RAM_SIZE, ram.to_le_bytes().to_vec());
        f.rebuild_dir();
        f
    }

    fn set(&mut self, key: u16, data: Vec<u8>) {
        self.items.insert(key, Item { data, on_write: None });
    }

    /// Skip EDK2's optional countdown before booting the OS.
    pub fn skip_boot_menu(&mut self) -> Result<(), String> {
        self.set(0x000e, 1u16.to_le_bytes().to_vec()); // FW_CFG_BOOT_MENU
        self.add_file("etc/boot-menu-wait", 0u16.to_le_bytes().to_vec(), None)
    }

    /// Add a named file. `on_write` makes it guest-writable.
    pub fn add_file(&mut self, name: &str, data: Vec<u8>, on_write: Option<WriteHook>) -> Result<(), String> {
        if name.len() >= 56 || self.files.iter().any(|(n, _)| n == name) {
            return Err(format!("bad fw_cfg file name {name}"));
        }
        let key = FILE_FIRST + self.files.len() as u16;
        self.items.insert(key, Item { data, on_write });
        self.files.push((name.to_string(), key));
        self.rebuild_dir();
        Ok(())
    }

    fn rebuild_dir(&mut self) {
        let mut d = (self.files.len() as u32).to_be_bytes().to_vec();
        // Directory is sorted by name, as the spec says.
        let mut sorted = self.files.clone();
        sorted.sort();
        for (name, key) in sorted {
            let size = self.items.get(&key).map(|i| i.data.len()).unwrap_or(0) as u32;
            d.extend_from_slice(&size.to_be_bytes());
            d.extend_from_slice(&key.to_be_bytes());
            d.extend_from_slice(&[0, 0]);
            let mut n = [0u8; 56];
            n[..name.len()].copy_from_slice(name.as_bytes());
            d.extend_from_slice(&n);
        }
        self.set(FILE_DIR, d);
    }

    fn select(&mut self, key: u16) {
        self.st.selected = key;
        self.st.offset = 0;
    }

    fn read_bytes(&mut self, out: &mut [u8]) {
        let data = self.items.get(&self.st.selected).map(|i| &i.data[..]).unwrap_or(&[]);
        for b in out.iter_mut() {
            *b = data.get(self.st.offset).copied().unwrap_or(0);
            self.st.offset = self.st.offset.saturating_add(1);
        }
    }

    fn dma(&mut self, addr: u64) {
        let a = GuestAddress(addr);
        let mut hdr = [0u8; 16];
        if self.mem.read(a, &mut hdr).is_err() {
            return;
        }
        let control = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let len = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let buf = u64::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15]]);
        let ok = self.dma_op(control, len, GuestAddress(buf));
        let status: u32 = if ok { 0 } else { DMA_ERROR };
        let _ = self.mem.write(a, &status.to_be_bytes());
    }

    fn dma_op(&mut self, control: u32, len: u32, buf: GuestAddress) -> bool {
        if control & DMA_SELECT != 0 {
            self.select((control >> 16) as u16);
        }
        if len > MAX_DMA {
            return false;
        }
        if control & DMA_READ != 0 {
            let mut v = vec![0u8; len as usize];
            self.read_bytes(&mut v);
            return self.mem.write(buf, &v).is_ok();
        }
        if control & DMA_WRITE != 0 {
            let mut v = vec![0u8; len as usize];
            if self.mem.read(buf, &mut v).is_err() {
                return false;
            }
            let off = self.st.offset;
            let Some(item) = self.items.get_mut(&self.st.selected) else { return false };
            let Some(hook) = item.on_write.as_mut() else { return false };
            // Writes stay inside the file; no growing.
            if off.checked_add(v.len()).is_none_or(|e| e > item.data.len()) {
                return false;
            }
            item.data[off..off + v.len()].copy_from_slice(&v);
            self.st.offset += v.len();
            hook(&item.data);
            return true;
        }
        // The protocol gives READ priority over WRITE, then SKIP.
        if control & DMA_SKIP != 0 {
            self.st.offset = self.st.offset.saturating_add(len as usize);
        }
        true
    }
}

impl BusDevice for FwCfg {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        match offset {
            0..=7 => self.read_bytes(data),
            // DMA signature "QEMU CFG" when probed.
            0x10..=0x17 => {
                let sig = b"QEMU CFG";
                for (i, b) in data.iter_mut().enumerate() {
                    *b = sig.get(offset as usize - 0x10 + i).copied().unwrap_or(0);
                }
            }
            _ => data.fill(0),
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        match (offset, data.len()) {
            (8, 2) => self.select(u16::from_be_bytes([data[0], data[1]])),
            (0x10, 8) => {
                let mut b = [0u8; 8];
                b.copy_from_slice(data);
                self.dma(u64::from_be_bytes(b));
            }
            (0x10, 4) => self.st.dma_hi = u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
            (0x14, 4) => {
                let lo = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                let addr = ((self.st.dma_hi as u64) << 32) | lo as u64;
                self.st.dma_hi = 0;
                self.dma(addr);
            }
            _ => {}
        }
    }
}

/// PC scalar port-I/O transport. Map [`Self::PORT_SIZE`] bytes at
/// [`Self::PORT_BASE`], with scalar widths 1, 2 and 4 enabled on the bus.
///
/// The overlapping selector/data registers are at offsets 0 (LE u16 write)
/// and 1 (byte read). DMA address halves at offsets 4 and 8 are BE u32;
/// writing the low half starts the transfer and clears the address latch.
/// Unsupported accesses read all ones or ignore writes, without advancing
/// the stream. Modern fw_cfg ignores data-port writes.
///
/// This adapter does not register a board device or enable a firmware profile.
/// Current EDK2 `BaseIoLibIntrinsic` FIFO reads loop over scalar accesses.
/// Other clients' string/multiple-element I/O remains unsupported by the
/// hypervisor interface. No VM snapshot support is implied by this transport.
#[derive(Debug)]
pub struct FwCfgIo {
    inner: FwCfg,
}

impl FwCfgIo {
    pub const PORT_BASE: u16 = 0x510;
    pub const PORT_SIZE: u32 = 12;

    pub fn new(mem: GuestMemory, cpus: u16, ram: u64) -> Self {
        Self { inner: FwCfg::new(u64::from(Self::PORT_BASE), mem, cpus, ram) }
    }

    /// Add a named file to the shared firmware directory.
    pub fn add_file(&mut self, name: &str, data: Vec<u8>, on_write: Option<WriteHook>) -> Result<(), String> {
        self.inner.add_file(name, data, on_write)
    }

    pub fn skip_boot_menu(&mut self) -> Result<(), String> {
        self.inner.skip_boot_menu()
    }
}

impl BusDevice for FwCfgIo {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        match (offset, data.len()) {
            (1, 1) => self.inner.read_bytes(data),
            // DMA signature bytes are string-preserving, including partial
            // reads. Check the entire span before indexing or touching state.
            (4..=11, 1 | 2 | 4) if data.len() <= 12 - offset as usize => {
                let start = offset as usize - 4;
                data.copy_from_slice(&b"QEMU CFG"[start..start + data.len()]);
            }
            _ => data.fill(0xff),
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        match (offset, data.len()) {
            (0, 2) => self.inner.select(u16::from_le_bytes([data[0], data[1]])),
            // Reuse the MMIO core's BE high/low latch and DMA completion.
            (4, 4) => self.inner.write(0x10, data),
            (8, 4) => self.inner.write(0x14, data),
            _ => {}
        }
    }
}

impl Snapshottable for FwCfg {
    fn snapshot_id(&self) -> String {
        format!("fwcfg@{:x}", self.base)
    }

    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        // Writable items (ramfb config) are part of the state.
        let writable: Vec<(u16, Vec<u8>)> =
            self.items.iter().filter(|(_, i)| i.on_write.is_some()).map(|(k, i)| (*k, i.data.clone())).collect();
        encode(&(self.st.clone(), writable))
    }

    fn restore(&mut self, data: &[u8]) -> vroom_snapshot::Result<()> {
        let (st, writable): (State, Vec<(u16, Vec<u8>)>) = decode(data)?;
        self.st = st;
        for (k, v) in writable {
            if let Some(item) = self.items.get_mut(&k) {
                if let Some(h) = item.on_write.as_mut() {
                    if v.len() == item.data.len() {
                        item.data = v;
                        h(&item.data);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn dev() -> FwCfg {
        let mem = GuestMemory::new(&[(GuestAddress(0), 1 << 20)]).unwrap();
        FwCfg::new(0x9020000, mem, 2, 1 << 30)
    }

    fn dma(f: &mut FwCfg, control: u32, len: u32, buf: u64) -> u32 {
        let mut h = control.to_be_bytes().to_vec();
        h.extend_from_slice(&len.to_be_bytes());
        h.extend_from_slice(&buf.to_be_bytes());
        f.mem.write(GuestAddress(0x1000), &h).unwrap();
        f.write(0x10, &0x1000u64.to_be_bytes());
        u32::from_be_bytes(f.mem.read_obj::<[u8; 4]>(GuestAddress(0x1000)).unwrap())
    }

    #[test]
    fn signature_and_files() {
        let mut f = dev();
        f.add_file("etc/acpi/rsdp", vec![1, 2, 3], None).unwrap();
        f.write(8, &SIGNATURE.to_be_bytes());
        let mut s = [0u8; 4];
        f.read(0, &mut s);
        assert_eq!(&s, b"QEMU");
        // Directory over DMA.
        assert_eq!(dma(&mut f, DMA_SELECT | DMA_READ | ((FILE_DIR as u32) << 16), 68, 0x2000), 0);
        let mut d = [0u8; 68];
        f.mem.read(GuestAddress(0x2000), &mut d).unwrap();
        assert_eq!(u32::from_be_bytes([d[0], d[1], d[2], d[3]]), 1);
        assert_eq!(&d[12..25], b"etc/acpi/rsdp");
    }

    #[test]
    fn writes_only_to_writable_files() {
        let mut f = dev();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        f.add_file("etc/ramfb", vec![0; 28], Some(Box::new(move |d| *s2.lock().unwrap() = d.to_vec()))).unwrap();
        f.add_file("etc/acpi/tables", vec![9; 8], None).unwrap();
        f.mem.write(GuestAddress(0x3000), &[7u8; 28]).unwrap();
        let ramfb = FILE_FIRST as u32;
        assert_eq!(dma(&mut f, DMA_SELECT | DMA_WRITE | (ramfb << 16), 28, 0x3000), 0);
        assert_eq!(*seen.lock().unwrap(), vec![7u8; 28]);
        // Too long: refused.
        assert_eq!(dma(&mut f, DMA_SELECT | DMA_WRITE | (ramfb << 16), 29, 0x3000), DMA_ERROR);
        // Read-only file: refused.
        assert_eq!(dma(&mut f, DMA_SELECT | DMA_WRITE | ((ramfb + 1) << 16), 4, 0x3000), DMA_ERROR);
        // Huge read: refused, no allocation.
        assert_eq!(dma(&mut f, DMA_SELECT | DMA_READ | (ramfb << 16), u32::MAX, 0x3000), DMA_ERROR);
    }

    fn io_dev() -> FwCfgIo {
        let mem = GuestMemory::new(&[
            (GuestAddress(0), 1 << 20),
            (GuestAddress(1 << 32), 1 << 16),
        ]).unwrap();
        FwCfgIo::new(mem, 0x1234, 0x0102_0304_0506_0708)
    }

    fn io_bytes(f: &mut FwCfgIo, count: usize) -> Vec<u8> {
        (0..count).map(|_| {
            let mut byte = [0];
            f.read(1, &mut byte);
            byte[0]
        }).collect()
    }

    fn io_descriptor(f: &FwCfgIo, addr: u64, control: u32, len: u32, buf: u64) {
        let mut h = control.to_be_bytes().to_vec();
        h.extend_from_slice(&len.to_be_bytes());
        h.extend_from_slice(&buf.to_be_bytes());
        f.inner.mem.write(GuestAddress(addr), &h).unwrap();
    }

    fn io_dma(f: &mut FwCfgIo, control: u32, len: u32, buf: u64) -> u32 {
        io_descriptor(f, 0x1000, control, len, buf);
        f.write(8, &0x1000u32.to_be_bytes());
        u32::from_be_bytes(f.inner.mem.read_obj::<[u8; 4]>(GuestAddress(0x1000)).unwrap())
    }

    #[test]
    fn io_selector_is_little_endian_and_data_is_a_byte_stream() {
        let mut f = io_dev();
        assert_eq!(io_bytes(&mut f, 6), b"QEMU\0\0");
        f.write(0, &SIGNATURE.to_le_bytes());
        assert_eq!(io_bytes(&mut f, 2), b"QE");
        f.write(0, &SIGNATURE.to_le_bytes());
        assert_eq!(io_bytes(&mut f, 4), b"QEMU");
        f.write(0, &ID.to_le_bytes());
        assert_eq!(io_bytes(&mut f, 4), FEATURES.to_le_bytes());
        f.write(0, &NB_CPUS.to_le_bytes());
        assert_eq!(io_bytes(&mut f, 2), 0x1234u16.to_le_bytes());
        f.write(0, &RAM_SIZE.to_le_bytes());
        assert_eq!(io_bytes(&mut f, 8), 0x0102_0304_0506_0708u64.to_le_bytes());
        // MMIO's selector encoding must not accidentally work for PC PIO.
        f.write(0, &NB_CPUS.to_be_bytes());
        assert_eq!(io_bytes(&mut f, 2), [0, 0]);
    }

    #[test]
    fn io_directory_preserves_shared_file_and_boot_configuration() {
        let mut f = io_dev();
        f.add_file("etc/test", vec![7, 8, 9], None).unwrap();
        assert!(f.add_file("etc/test", vec![], None).is_err());
        f.skip_boot_menu().unwrap();
        f.write(0, &FILE_DIR.to_le_bytes());
        let dir = io_bytes(&mut f, 4 + 2 * 64);
        assert_eq!(&dir[..4], &2u32.to_be_bytes());
        // The sorted directory retains BE size/key, irrespective of transport.
        assert_eq!(&dir[4..8], &2u32.to_be_bytes());
        assert_eq!(&dir[8..10], &(FILE_FIRST + 1).to_be_bytes());
        assert_eq!(&dir[12..31], b"etc/boot-menu-wait\0");
        assert_eq!(&dir[68..72], &3u32.to_be_bytes());
        assert_eq!(&dir[72..74], &FILE_FIRST.to_be_bytes());
        assert_eq!(&dir[76..85], b"etc/test\0");
        f.write(0, &FILE_FIRST.to_le_bytes());
        assert_eq!(io_bytes(&mut f, 3), [7, 8, 9]);
        f.write(0, &0x000eu16.to_le_bytes());
        assert_eq!(io_bytes(&mut f, 2), [1, 0]);
    }

    #[test]
    fn io_dma_signature_is_string_preserving_for_scalar_reads() {
        let mut f = io_dev();
        for width in [1, 2, 4] {
            for start in 0..=8 - width {
                let mut bytes = vec![0; width];
                f.read(4 + start as u64, &mut bytes);
                assert_eq!(bytes, &b"QEMU CFG"[start..start + width]);
            }
        }
        // Signature probing must not consume the selected data stream.
        assert_eq!(io_bytes(&mut f, 4), b"QEMU");
    }

    #[test]
    fn io_invalid_accesses_have_no_device_side_effects() {
        let mut f = io_dev();
        let initial = f.inner.save().unwrap();
        for (offset, width) in [
            (0, 0), (0, 1), (0, 4), (1, 1), (1, 2), (1, 4),
            (2, 2), (3, 4), (4, 2), (4, 8), (5, 4), (8, 2),
            (8, 8), (9, 4), (12, 4), (u64::MAX, 4),
        ] {
            f.write(offset, &vec![0xa5; width]);
            assert_eq!(f.inner.save().unwrap(), initial, "write {offset:#x}/{width}");
        }
        for (offset, width) in [
            (0, 0), (0, 1), (0, 2), (1, 2), (1, 4), (2, 1),
            (3, 4), (4, 8), (8, 8), (10, 4), (11, 2), (12, 1), (u64::MAX, 1),
        ] {
            let mut bytes = vec![0xa5; width];
            f.read(offset, &mut bytes);
            assert_eq!(bytes, vec![0xff; width], "read {offset:#x}/{width}");
            assert_eq!(f.inner.save().unwrap(), initial);
        }
        assert_eq!(io_bytes(&mut f, 4), b"QEMU");
    }

    #[test]
    fn io_dma_high_then_low_triggers_and_clears_the_address_latch() {
        let mut f = io_dev();
        f.add_file("etc/test", b"abcdef".to_vec(), None).unwrap();
        let high_descriptor = (1 << 32) + 0x1000;
        let control = (u32::from(FILE_FIRST) << 16) | DMA_SELECT | DMA_READ;
        io_descriptor(&f, high_descriptor, control, 3, 0x3000);
        f.write(4, &1u32.to_be_bytes());
        // High-half and invalid-width low-half writes do not transfer.
        f.write(8, &0x1000u16.to_be_bytes());
        assert_eq!(f.inner.mem.read_obj::<[u8; 4]>(GuestAddress(high_descriptor)).unwrap(), control.to_be_bytes());
        assert_eq!(f.inner.mem.read_obj::<[u8; 3]>(GuestAddress(0x3000)).unwrap(), [0; 3]);
        f.write(8, &0x1000u32.to_be_bytes());
        assert_eq!(f.inner.mem.read_obj::<[u8; 4]>(GuestAddress(high_descriptor)).unwrap(), [0; 4]);
        assert_eq!(f.inner.mem.read_obj::<[u8; 3]>(GuestAddress(0x3000)).unwrap(), *b"abc");
        // PIO and DMA consume the same stream offset.
        assert_eq!(io_bytes(&mut f, 1), b"d");
        assert_eq!(io_dma(&mut f, DMA_READ, 2, 0x3000), 0);
        assert_eq!(f.inner.mem.read_obj::<[u8; 2]>(GuestAddress(0x3000)).unwrap(), *b"ef");
        assert_eq!(f.inner.st.dma_hi, 0);
    }

    #[test]
    fn io_dma_writes_hooks_and_reports_bounded_errors() {
        let mut f = io_dev();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        f.add_file("etc/writable", vec![0; 4], Some(Box::new(move |d| *s2.lock().unwrap() = d.to_vec()))).unwrap();
        f.add_file("etc/read-only", vec![9; 4], None).unwrap();
        f.inner.mem.write(GuestAddress(0x3000), b"test").unwrap();
        let select = (u32::from(FILE_FIRST) << 16) | DMA_SELECT;
        assert_eq!(io_dma(&mut f, select | DMA_WRITE | DMA_SKIP, 4, 0x3000), 0);
        assert_eq!(*seen.lock().unwrap(), b"test");
        assert_eq!(io_dma(&mut f, select | DMA_WRITE, 5, 0x3000), DMA_ERROR);
        assert_eq!(io_dma(&mut f, select | DMA_WRITE, 4, u64::MAX), DMA_ERROR);
        assert_eq!(io_dma(&mut f, select | DMA_READ, MAX_DMA + 1, 0x3000), DMA_ERROR);
        assert_eq!(io_dma(&mut f, select | DMA_READ, 4, u64::MAX), DMA_ERROR);
        assert_eq!(io_dma(&mut f, (select + (1 << 16)) | DMA_WRITE, 4, 0x3000), DMA_ERROR);
        assert_eq!(*seen.lock().unwrap(), b"test");
        // An incomplete descriptor is not interpreted or partially executed.
        f.inner.mem.write(GuestAddress((1 << 20) - 8), &[0xa5; 8]).unwrap();
        f.write(8, &((1u32 << 20) - 8).to_be_bytes());
        assert_eq!(f.inner.mem.read_obj::<[u8; 8]>(GuestAddress((1 << 20) - 8)).unwrap(), [0xa5; 8]);
        assert_eq!(io_dma(&mut f, select | DMA_READ, 4, 0x3000), 0);
        assert_eq!(f.inner.mem.read_obj::<[u8; 4]>(GuestAddress(0x3000)).unwrap(), *b"test");
    }

    #[test]
    fn dma_operation_priority_is_read_then_write_then_skip() {
        let mut f = dev();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        f.add_file("etc/test", b"abcd".to_vec(), Some(Box::new(move |d| *s2.lock().unwrap() = d.to_vec()))).unwrap();
        let select = (u32::from(FILE_FIRST) << 16) | DMA_SELECT;
        assert_eq!(dma(&mut f, select | DMA_READ | DMA_WRITE | DMA_SKIP, 4, 0x3000), 0);
        assert_eq!(f.mem.read_obj::<[u8; 4]>(GuestAddress(0x3000)).unwrap(), *b"abcd");
        assert!(seen.lock().unwrap().is_empty());
        f.mem.write(GuestAddress(0x3000), b"wxyz").unwrap();
        assert_eq!(dma(&mut f, select | DMA_WRITE | DMA_SKIP, 4, 0x3000), 0);
        assert_eq!(*seen.lock().unwrap(), b"wxyz");
        assert_eq!(dma(&mut f, select | DMA_SKIP, 2, u64::MAX), 0);
        assert_eq!(dma(&mut f, DMA_READ, 2, 0x3000), 0);
        assert_eq!(f.mem.read_obj::<[u8; 2]>(GuestAddress(0x3000)).unwrap(), *b"yz");
    }
}
