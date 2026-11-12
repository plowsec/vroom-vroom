//! Linux bzImage legacy 64-bit entry protocol (not the EFI entry point).
//! https://docs.kernel.org/arch/x86/boot.html#bit-boot-protocol-1
//! https://docs.kernel.org/arch/x86/zero-page.html

use std::ops::Range;

use vroom_memory::{GuestAddress, GuestMemory};

use super::layout::*;
use crate::{Error, Result};

const HEADER_START: usize = 0x1f1;
const HEADER_MIN_END: usize = 0x268;
const HEADER_MAX_END: usize = 0x290;
const E820_TABLE: usize = 0x2d0;

/// Deliberately narrow until the next board profiles have been validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootConfig {
    pub ram_size: u64,
    pub vcpus: u32,
}

impl Default for BootConfig {
    fn default() -> Self {
        Self { ram_size: RAM_SIZE, vcpus: 1 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootRegion {
    pub address: u64,
    pub size: u64,
}

impl BootRegion {
    fn end(self) -> Result<u64> {
        self.address.checked_add(self.size).ok_or_else(|| Error::Layout("boot region overflow".into()))
    }
}

/// The caller converts these directly to `vroom_hv::BootRegs::X86_64`.
/// The vCPU must be fresh, with interrupts disabled and Linux64 segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootInfo {
    pub rip: u64,
    pub rsi: u64,
    pub cr3: u64,
    pub gdt: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPlan {
    pub ram_size: u64,
    /// Includes the whole decompressor/runtime `init_size`, not just bytes
    /// copied from the file. The initramfs must not overlap this workspace.
    pub kernel: BootRegion,
    pub initrd: Option<BootRegion>,
    pub page_tables: BootRegion,
    pub gdt: BootRegion,
    pub boot_params: BootRegion,
    pub command_line: BootRegion,
    pub entry: BootInfo,
}

impl BootPlan {
    fn regions(&self) -> Vec<BootRegion> {
        let mut ranges = vec![self.kernel, self.page_tables, self.gdt, self.boot_params, self.command_line];
        ranges.extend(self.initrd);
        ranges
    }

    fn validate(&self) -> Result<()> {
        let mut ranges = self.regions();
        ranges.sort_by_key(|r| r.address);
        let mut end = 0;
        for r in ranges {
            if r.size == 0 || r.address < end || r.end()? > self.ram_size {
                return Err(Error::Layout("boot regions overlap or exceed RAM".into()));
            }
            end = r.end()?;
        }
        Ok(())
    }
}

/// Owns its validated inputs, so they cannot change between planning and
/// loading. `load` is for a fresh, stopped VM; it is not a reset operation.
#[derive(Debug)]
pub struct PreparedBoot {
    plan: BootPlan,
    kernel: Vec<u8>,
    payload: Range<usize>,
    initrd: Option<Vec<u8>>,
    blobs: Vec<(u64, Vec<u8>)>,
}

impl PreparedBoot {
    pub fn plan(&self) -> &BootPlan {
        &self.plan
    }

    /// Verify the RAM topology before the first write. No device, firmware
    /// or CPU state is changed. Backing must be one zero-based RAM region.
    pub fn load(&self, mem: &GuestMemory) -> Result<BootInfo> {
        let mut regions = mem.regions();
        let ram = regions.next().ok_or_else(|| Error::Layout("missing x86 RAM".into()))?;
        if ram.start() != GuestAddress(0) || ram.len() != self.plan.ram_size || regions.next().is_some() {
            return Err(Error::Layout("x86 boot requires one 512 MiB RAM region at address zero".into()));
        }
        // All spans (including the kernel's zeroed workspace) are checked
        // before writing anything, even if the memory backend changes later.
        for r in self.plan.regions() {
            if !mem.check_range(GuestAddress(r.address), r.size) {
                return Err(Error::Layout("boot region has no RAM backing".into()));
            }
        }
        mem.fill(GuestAddress(self.plan.kernel.address), self.plan.kernel.size, 0)?;
        mem.write(GuestAddress(self.plan.kernel.address), &self.kernel[self.payload.clone()])?;
        for (address, bytes) in &self.blobs {
            mem.write(GuestAddress(*address), bytes)?;
        }
        if let (Some(region), Some(bytes)) = (self.plan.initrd, &self.initrd) {
            mem.write(GuestAddress(region.address), bytes)?;
        }
        Ok(self.plan.entry)
    }
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}
fn put32(b: &mut [u8], off: usize, value: u32) {
    b[off..off + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(b: &mut [u8], off: usize, value: u64) {
    b[off..off + 8].copy_from_slice(&value.to_le_bytes());
}
fn align_up(value: u64, alignment: u64) -> Result<u64> {
    value.checked_add(alignment - 1).map(|v| v & !(alignment - 1))
        .ok_or_else(|| Error::Layout("kernel alignment overflow".into()))
}

/// Validate and prepare a modern relocatable x64 bzImage without touching
/// guest RAM. The kernel performs its own decompression. Signed PE trailers
/// may remain in the input file, but no signature or Secure Boot claim is made.
pub fn prepare_boot(kernel: Vec<u8>, initrd: Option<Vec<u8>>, cmdline: &str, config: BootConfig) -> Result<PreparedBoot> {
    if config != BootConfig::default() {
        return Err(Error::Layout("only the one-vCPU, 512 MiB x86 boot profile is supported".into()));
    }
    let bad = |s: &str| Error::Kernel(s.into());
    if kernel.len() < HEADER_MIN_END || kernel.len() as u64 > crate::MAX_KERNEL_SIZE {
        return Err(bad("bzImage file size out of bounds"));
    }
    if u16_at(&kernel, 0x1fe) != 0xaa55 || &kernel[0x202..0x206] != b"HdrS" || kernel[0x200] != 0xeb {
        return Err(bad("not a Linux bzImage setup header"));
    }
    let version = u16_at(&kernel, 0x206);
    if !(0x020c..0x0300).contains(&version) || kernel[0x211] & 1 == 0 || u16_at(&kernel, 0x236) & 1 == 0 || kernel[0x234] != 1 {
        return Err(bad("requires a relocatable 64-bit bzImage with boot protocol 2.12 or newer"));
    }
    let header_end = 0x202 + kernel[0x201] as usize;
    if !(HEADER_MIN_END..=HEADER_MAX_END).contains(&header_end) || header_end > kernel.len() {
        return Err(bad("setup header length out of bounds"));
    }
    if u32_at(&kernel, 0x23c) != 0 || u64_at(&kernel, 0x240) != 0 {
        return Err(bad("only the standard x86 PC subarchitecture is supported"));
    }
    let sectors = match kernel[0x1f1] { 0 => 4, n => n as usize };
    if sectors > 63 {
        return Err(bad("setup exceeds the 32 KiB protocol limit"));
    }
    let payload_start = (sectors + 1) * 512;
    let payload_size = u32_at(&kernel, 0x1f4) as u64 * 16;
    let payload_end = (payload_start as u64).checked_add(payload_size).ok_or_else(|| bad("payload overflow"))?;
    if payload_start < header_end || payload_size <= 0x200 || payload_end > kernel.len() as u64 {
        return Err(bad("truncated or empty protected-mode payload"));
    }
    let compressed_end = u32_at(&kernel, 0x248) as u64 + u32_at(&kernel, 0x24c) as u64;
    if u32_at(&kernel, 0x24c) == 0 || compressed_end > payload_size {
        return Err(bad("compressed payload metadata exceeds the protected-mode image"));
    }
    let alignment = u32_at(&kernel, 0x230) as u64;
    if !alignment.is_power_of_two() || !(LARGE_PAGE_SIZE..=RAM_SIZE).contains(&alignment)
        || kernel[0x235] > alignment.trailing_zeros() as u8
    {
        return Err(bad("invalid kernel alignment"));
    }
    let address = align_up(u64_at(&kernel, 0x258).max(KERNEL_MIN), alignment)?;
    let init_size = u32_at(&kernel, 0x260) as u64;
    if init_size < payload_size || init_size > crate::MAX_KERNEL_SIZE {
        return Err(bad("invalid kernel initialization footprint"));
    }
    if cmdline.as_bytes().contains(&0) || cmdline.len() > COMMAND_LINE_MAX || cmdline.len() > u32_at(&kernel, 0x238) as usize {
        return Err(Error::Layout("command line contains NUL or exceeds its kernel/profile bound".into()));
    }
    let initrd_region = match &initrd {
        None => None,
        Some(bytes) => {
            if bytes.is_empty() || bytes.len() > INITRD_MAX {
                return Err(Error::Initrd("initramfs must contain 1 byte to 128 MiB".into()));
            }
            let top = RAM_SIZE.min(u32_at(&kernel, 0x22c) as u64 + 1);
            let address = top.checked_sub(bytes.len() as u64).ok_or_else(|| Error::Initrd("initramfs exceeds its address limit".into()))? & !(PAGE_SIZE - 1);
            if address < HIGH_RAM_START {
                return Err(Error::Initrd("initramfs would occupy reserved low memory".into()));
            }
            Some(BootRegion { address, size: bytes.len() as u64 })
        }
    };
    let entry = BootInfo { rip: address.checked_add(0x200).ok_or_else(|| bad("entry overflow"))?, rsi: BOOT_PARAMS, cr3: PAGE_TABLES, gdt: GDT };
    let plan = BootPlan {
        ram_size: RAM_SIZE,
        kernel: BootRegion { address, size: init_size },
        initrd: initrd_region,
        page_tables: BootRegion { address: PAGE_TABLES, size: PAGE_TABLES_SIZE },
        gdt: BootRegion { address: GDT, size: PAGE_SIZE },
        boot_params: BootRegion { address: BOOT_PARAMS, size: PAGE_SIZE },
        command_line: BootRegion { address: COMMAND_LINE, size: PAGE_SIZE },
        entry,
    };
    plan.validate()?;

    let mut params = vec![0; PAGE_SIZE as usize];
    params[HEADER_START..header_end].copy_from_slice(&kernel[HEADER_START..header_end]);
    params[0x210] = 0xff; // Unassigned bootloader ID, not QEMU or firmware.
    params[0x211] &= 0x21; // LOAD_HIGH/QUIET only; no real-mode heap or KEEP_SEGMENTS.
    for (off, len) in [(0x208, 4), (0x218, 8), (0x220, 8), (0x240, 8), (0x250, 8)] {
        params[off..off + len].fill(0);
    }
    put32(&mut params, 0x214, address as u32);
    put32(&mut params, 0x228, COMMAND_LINE as u32);
    if let Some(r) = initrd_region {
        put32(&mut params, 0x218, r.address as u32);
        put32(&mut params, 0x21c, r.size as u32);
    }
    // No BIOS calls run. Describe usable RAM and the conventional BIOS hole.
    params[0x1e8] = 3;
    for (i, (start, size, kind)) in [(0, LOW_RAM_END, 1u32), (LOW_RAM_END, HIGH_RAM_START - LOW_RAM_END, 2), (HIGH_RAM_START, RAM_SIZE - HIGH_RAM_START, 1)].into_iter().enumerate() {
        let off = E820_TABLE + i * 20;
        put64(&mut params, off, start);
        put64(&mut params, off + 8, size);
        put32(&mut params, off + 16, kind);
    }
    let mut command = vec![0; PAGE_SIZE as usize];
    command[..cmdline.len()].copy_from_slice(cmdline.as_bytes());
    let mut gdt = vec![0; PAGE_SIZE as usize];
    put64(&mut gdt, 16, 0x00af_9b00_0000_ffff); // CS 0x10: long code.
    put64(&mut gdt, 24, 0x00cf_9300_0000_ffff); // DS 0x18: flat data.
    let mut tables = vec![0; PAGE_TABLES_SIZE as usize];
    put64(&mut tables, 0, PAGE_TABLES + PAGE_SIZE | 3);
    put64(&mut tables, PAGE_SIZE as usize, PAGE_TABLES + 2 * PAGE_SIZE | 3);
    for i in 0..RAM_SIZE / LARGE_PAGE_SIZE {
        put64(&mut tables, 2 * PAGE_SIZE as usize + i as usize * 8, i * LARGE_PAGE_SIZE | 0x83);
    }
    Ok(PreparedBoot {
        plan, kernel, payload: payload_start..payload_end as usize, initrd,
        blobs: vec![(BOOT_PARAMS, params), (COMMAND_LINE, command), (GDT, gdt), (PAGE_TABLES, tables)],
    })
}

#[cfg(test)]
mod tests;
