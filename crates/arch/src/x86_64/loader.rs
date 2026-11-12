//! Linux bzImage 64-bit boot protocol. Firmware boots use the reset vector.
use super::layout;
use crate::{Error, Result};
use vroom_memory::{GuestAddress, GuestMemory};

pub const BOOT_PARAMS: u64 = 0x7000;
pub const CMDLINE: u64 = 0x20000;
pub const GDT: u64 = 0x500;
pub const PML4: u64 = 0x9000;
pub const KERNEL: u64 = 0x100000;

pub fn load(
    mem: &GuestMemory,
    ram_size: u64,
    kernel: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &str,
) -> Result<u64> {
    let bad = |s: &str| Error::Kernel(s.into());
    if kernel.len() < 0x268
        || &kernel[0x202..0x206] != b"HdrS"
        || kernel[0x1fe..0x200] != [0x55, 0xaa]
    {
        return Err(bad("expected an x86 Linux bzImage"));
    }
    let u16_at = |n| u16::from_le_bytes([kernel[n], kernel[n + 1]]);
    if u16_at(0x206) < 0x20c || u16_at(0x236) & 1 == 0 {
        return Err(bad("bzImage needs the 64-bit boot protocol >= 2.12"));
    }
    let setup = (if kernel[0x1f1] == 0 {
        4
    } else {
        kernel[0x1f1] as usize
    } + 1)
        * 512;
    if setup >= kernel.len() {
        return Err(bad("truncated bzImage setup"));
    }
    let init_size = u32::from_le_bytes(
        kernel[0x260..0x264]
            .try_into()
            .map_err(|_| bad("init_size"))?,
    ) as u64;
    let kernel_end = KERNEL + init_size.max((kernel.len() - setup) as u64);
    let low_end = ram_size.min(layout::LOW_RAM_END);
    if kernel_end > low_end {
        return Err(bad("kernel does not fit low RAM"));
    }
    if cmdline.len() >= 0x10000 || cmdline.as_bytes().contains(&0) {
        return Err(bad("invalid kernel command line"));
    }
    let mut params = vec![0u8; 4096];
    params[0x70..0x78].copy_from_slice(&super::acpi::RSDP_ADDR.to_le_bytes());
    params[0x1f1..0x268].copy_from_slice(&kernel[0x1f1..0x268]);
    params[0x210] = 0xff;
    params[0x211] |= 0x80;
    params[0x224..0x226].copy_from_slice(&0xfe00u16.to_le_bytes());
    params[0x228..0x22c].copy_from_slice(&(CMDLINE as u32).to_le_bytes());
    let e820 = layout::e820(ram_size)?;
    params[0x1e8] = (e820.len() / 20) as u8;
    params[0x2d0..0x2d0 + e820.len()].copy_from_slice(&e820);
    if let Some(data) = initrd {
        let max = u32::from_le_bytes(
            kernel[0x22c..0x230]
                .try_into()
                .map_err(|_| bad("initrd limit"))?,
        ) as u64
            + 1;
        let addr = low_end
            .min(max)
            .checked_sub(data.len() as u64)
            .ok_or_else(|| bad("initrd too large"))?
            & !0xfff;
        if addr < kernel_end {
            return Err(bad("initrd overlaps kernel"));
        }
        mem.write(GuestAddress(addr), data)?;
        params[0x218..0x21c].copy_from_slice(&(addr as u32).to_le_bytes());
        params[0x21c..0x220].copy_from_slice(&(data.len() as u32).to_le_bytes());
    }
    mem.write(GuestAddress(KERNEL), &kernel[setup..])?;
    mem.write(GuestAddress(BOOT_PARAMS), &params)?;
    mem.write(GuestAddress(CMDLINE), &[cmdline.as_bytes(), &[0]].concat())?;
    // Identity map the first 4 GiB with 2 MiB pages.
    let mut pages = vec![0u8; 6 * 4096];
    pages[..8].copy_from_slice(&(PML4 + 4096 + 3).to_le_bytes());
    for i in 0..4usize {
        pages[4096 + i * 8..4104 + i * 8]
            .copy_from_slice(&(PML4 + (2 + i as u64) * 4096 + 3).to_le_bytes());
        for j in 0..512usize {
            let off = (2 + i) * 4096 + j * 8;
            pages[off..off + 8]
                .copy_from_slice(&(((i * 512 + j) as u64 * 0x200000) | 0x83).to_le_bytes());
        }
    }
    mem.write(GuestAddress(PML4), &pages)?;
    let gdt: Vec<u8> = [0u64, 0x00af_9b00_0000_ffff, 0x00cf_9300_0000_ffff]
        .into_iter()
        .flat_map(u64::to_le_bytes)
        .collect();
    mem.write(GuestAddress(GDT), &gdt)?;
    Ok(KERNEL + 0x200)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_bzimage() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 64 << 20)]).unwrap();
        assert!(load(&mem, 64 << 20, &[0; 1024], None, "").is_err());
    }
    #[test]
    fn boot_params_and_identity_map() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 64 << 20)]).unwrap();
        let mut k = vec![0u8; 4096];
        k[0x1fe..0x200].copy_from_slice(&[0x55, 0xaa]);
        k[0x202..0x206].copy_from_slice(b"HdrS");
        k[0x206..0x208].copy_from_slice(&0x20cu16.to_le_bytes());
        k[0x236] = 1;
        k[0x22c..0x230].copy_from_slice(&0x37ff_ffffu32.to_le_bytes());
        assert_eq!(
            load(&mem, 64 << 20, &k, Some(b"initrd"), "console=ttyS0").unwrap(),
            KERNEL + 0x200
        );
        assert_eq!(
            mem.read_obj::<[u8; 14]>(GuestAddress(CMDLINE)).unwrap(),
            *b"console=ttyS0\0"
        );
        assert_eq!(
            u64::from_le_bytes(mem.read_obj(GuestAddress(PML4)).unwrap()),
            PML4 + 4099
        );
    }
}
