//! Direct kernel boot (arm64 `Image` protocol, Documentation/arm64/booting.rst).

use vroom_memory::{GuestAddress, GuestMemory};

use super::fdt::FDT_MAX_SIZE;
use crate::{Error, Result};

const ARM64_MAGIC: u32 = 0x644d_5241; // "ARM\x64"
const HEADER_LEN: usize = 64;
const SZ_2M: u64 = 2 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelInfo {
    pub entry: u64,
    /// First byte past the kernel's memory footprint (incl. BSS).
    pub end: u64,
}

/// Where everything goes in RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootPlan {
    pub fdt_addr: u64,
    pub initrd_addr: Option<u64>,
}

fn le_u64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

/// Validate the header and copy an uncompressed `Image` into RAM.
pub fn load_kernel(mem: &GuestMemory, ram_base: u64, image: &[u8]) -> Result<KernelInfo> {
    if image.len() < HEADER_LEN {
        return Err(Error::Kernel("image too small".into()));
    }
    let magic = u32::from_le_bytes([image[56], image[57], image[58], image[59]]);
    if magic != ARM64_MAGIC {
        return Err(Error::Kernel("not an arm64 Image (bad magic); gzip'd? EFI zboot?".into()));
    }
    let text_offset = le_u64(image, 8);
    let mut image_size = le_u64(image, 16);
    let flags = le_u64(image, 24);
    if image_size == 0 {
        // Pre-3.17 kernels: no size field. Assume text_offset 0x80000 semantics.
        image_size = image.len() as u64;
    }
    if flags & 1 != 0 {
        return Err(Error::Kernel("big-endian kernels not supported".into()));
    }
    if text_offset >= SZ_2M || image_size < image.len() as u64 || image_size > crate::MAX_KERNEL_SIZE {
        return Err(Error::Kernel(format!(
            "insane header: text_offset {text_offset:#x} image_size {image_size:#x}"
        )));
    }
    let base = ram_base.checked_add(SZ_2M - 1).map(|v| v & !(SZ_2M - 1)).ok_or(Error::Layout("ram".into()))?;
    let entry = base + text_offset;
    let end = entry + image_size;
    if !mem.check_range(GuestAddress(entry), image_size) {
        return Err(Error::Kernel("kernel does not fit in RAM".into()));
    }
    mem.write(GuestAddress(entry), image)?;
    Ok(KernelInfo { entry, end })
}

/// Pick FDT and initrd addresses at the top of the first RAM region.
pub fn place_boot_blobs(ram: (u64, u64), kernel_end: u64, initrd_len: Option<u64>) -> Result<BootPlan> {
    let (base, size) = ram;
    let top = base + size;
    let fdt_addr = top.checked_sub(FDT_MAX_SIZE as u64).ok_or(Error::Layout("RAM too small".into()))?
        & !(SZ_2M - 1);
    if fdt_addr < kernel_end {
        return Err(Error::Layout("RAM too small for kernel + FDT".into()));
    }
    let initrd_addr = match initrd_len {
        None => None,
        Some(len) => {
            let a = fdt_addr.checked_sub(len).ok_or(Error::Initrd("too large".into()))? & !0xfff;
            if a < kernel_end {
                return Err(Error::Initrd("initrd does not fit between kernel and FDT".into()));
            }
            Some(a)
        }
    };
    Ok(BootPlan { fdt_addr, initrd_addr })
}

pub fn load_initrd(mem: &GuestMemory, addr: u64, data: &[u8]) -> Result<()> {
    mem.write(GuestAddress(addr), data).map_err(|e| Error::Initrd(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_image(text_offset: u64, image_size: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        v[8..16].copy_from_slice(&text_offset.to_le_bytes());
        v[16..24].copy_from_slice(&image_size.to_le_bytes());
        v[56..60].copy_from_slice(&ARM64_MAGIC.to_le_bytes());
        v
    }

    #[test]
    fn loads_and_places() {
        let base = 0x4000_0000;
        let mem = GuestMemory::new(&[(GuestAddress(base), 64 << 20)]).unwrap();
        let k = load_kernel(&mem, base, &fake_image(0, 0x20_0000, 4096)).unwrap();
        assert_eq!(k, KernelInfo { entry: base, end: base + 0x20_0000 });
        assert_eq!(mem.read_obj::<u32>(GuestAddress(base + 56)).unwrap(), ARM64_MAGIC);
        let p = place_boot_blobs((base, 64 << 20), k.end, Some(0x10_0000)).unwrap();
        assert_eq!(p.fdt_addr, base + (62 << 20));
        assert_eq!(p.initrd_addr, Some(base + (61 << 20)));
    }

    #[test]
    fn rejects_bad_images() {
        let base = 0x4000_0000;
        let mem = GuestMemory::new(&[(GuestAddress(base), 16 << 20)]).unwrap();
        assert!(load_kernel(&mem, base, &[0u8; 10]).is_err());
        assert!(load_kernel(&mem, base, &[0u8; 4096]).is_err());
        assert!(load_kernel(&mem, base, &fake_image(4 << 20, 0x1000, 4096)).is_err());
        assert!(load_kernel(&mem, base, &fake_image(0, 64 << 20, 4096)).is_err());
        assert!(place_boot_blobs((base, 16 << 20), base + (15 << 20), None).is_err());
    }
}
