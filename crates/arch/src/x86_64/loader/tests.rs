use super::*;

fn image() -> Vec<u8> {
    let mut b = vec![0; 5 * 512 + 0x4000];
    b[0x1f1] = 4;
    put32(&mut b, 0x1f4, 0x4000 / 16);
    b[0x1fe..0x200].copy_from_slice(&0xaa55u16.to_le_bytes());
    b[0x200] = 0xeb;
    b[0x201] = (HEADER_MIN_END - 0x202) as u8;
    b[0x202..0x206].copy_from_slice(b"HdrS");
    b[0x206..0x208].copy_from_slice(&0x020cu16.to_le_bytes());
    b[0x211] = 1;
    put32(&mut b, 0x22c, 0x7fff_ffff);
    put32(&mut b, 0x230, LARGE_PAGE_SIZE as u32);
    b[0x234] = 1;
    b[0x235] = 21;
    b[0x236] = 1;
    put32(&mut b, 0x238, 2047);
    put32(&mut b, 0x248, 0x300);
    put32(&mut b, 0x24c, 0x1000);
    put64(&mut b, 0x258, KERNEL_MIN);
    put32(&mut b, 0x260, LARGE_PAGE_SIZE as u32);
    b[5 * 512..].fill(0xa5);
    b
}

fn prepare(b: Vec<u8>) -> Result<PreparedBoot> {
    prepare_boot(b, None, "console=ttyS0", BootConfig::default())
}

#[test]
fn loads_linux64_payload_and_zero_page() {
    let mut b = image();
    // Loader-owned pointers/flags in the image cannot inject boot data.
    for off in [0x250, 0x218, 0x220] { put64(&mut b, off, u64::MAX); }
    b[0x211] = 0xe3;
    let initrd = b"070701-test-initramfs".to_vec();
    let boot = prepare_boot(b.clone(), Some(initrd.clone()), "console=ttyS0", BootConfig::default()).unwrap();
    let plan = boot.plan();
    assert_eq!(plan.entry, BootInfo { rip: KERNEL_MIN + 0x200, rsi: BOOT_PARAMS, cr3: PAGE_TABLES, gdt: GDT });
    let ram = GuestMemory::new(&[(GuestAddress(0), RAM_SIZE)]).unwrap();
    ram.fill(GuestAddress(KERNEL_MIN), LARGE_PAGE_SIZE, 0xcc).unwrap();
    assert_eq!(boot.load(&ram).unwrap(), plan.entry);
    let mut payload = vec![0; 0x4000];
    ram.read(GuestAddress(KERNEL_MIN), &mut payload).unwrap();
    assert_eq!(payload, b[5 * 512..]);
    assert_eq!(ram.read_obj::<u64>(GuestAddress(KERNEL_MIN + 0x4000)).unwrap(), 0);
    let mut params = vec![0; PAGE_SIZE as usize];
    ram.read(GuestAddress(BOOT_PARAMS), &mut params).unwrap();
    assert_eq!(&params[0x202..0x206], b"HdrS");
    assert_eq!(params[0x210], 0xff);
    assert_eq!(params[0x211], 0x21);
    assert_eq!(u64_at(&params, 0x250), 0);
    assert_eq!(u64_at(&params, 0x220), 0);
    assert_eq!(u32_at(&params, 0x214), KERNEL_MIN as u32);
    assert_eq!(u32_at(&params, 0x228), COMMAND_LINE as u32);
    assert_eq!(params[0x1ef], 0);
    assert_eq!(u64_at(&params, 0x70), 0);
    assert!(params[0x1c0..0x1e0].iter().all(|b| *b == 0));
    assert_eq!(params[0x1e8], 3);
    let e820: Vec<_> = (0..3).map(|i| {
        let off = E820_TABLE + i * 20;
        (u64_at(&params, off), u64_at(&params, off + 8), u32_at(&params, off + 16))
    }).collect();
    assert_eq!(e820, vec![(0, LOW_RAM_END, 1), (LOW_RAM_END, HIGH_RAM_START - LOW_RAM_END, 2), (HIGH_RAM_START, RAM_SIZE - HIGH_RAM_START, 1)]);
    let r = plan.initrd.unwrap();
    assert!(r.address >= plan.kernel.end().unwrap());
    assert_eq!(u32_at(&params, 0x218) as u64, r.address);
    assert_eq!(u32_at(&params, 0x21c) as usize, initrd.len());
    let mut loaded = vec![0; initrd.len()];
    ram.read(GuestAddress(r.address), &mut loaded).unwrap();
    assert_eq!(loaded, initrd);
    let mut command = [0; 14];
    ram.read(GuestAddress(COMMAND_LINE), &mut command).unwrap();
    assert_eq!(&command, b"console=ttyS0\0");
    for (offset, value) in [(0, 0), (8, 0), (16, 0x00af_9b00_0000_ffff), (24, 0x00cf_9300_0000_ffff)] {
        assert_eq!(ram.read_obj::<u64>(GuestAddress(GDT + offset)).unwrap(), value);
    }
}

#[test]
fn page_tables_identity_map_exactly_the_reference_ram() {
    let boot = prepare(image()).unwrap();
    let ram = GuestMemory::new(&[(GuestAddress(0), RAM_SIZE)]).unwrap();
    let entry = boot.load(&ram).unwrap();
    let translate = |va: u64| -> Option<u64> {
        let pml4e = ram.read_obj::<u64>(GuestAddress(entry.cr3 + ((va >> 39) & 511) * 8)).unwrap();
        if pml4e & 1 == 0 { return None; }
        let pdpte = ram.read_obj::<u64>(GuestAddress((pml4e & !0xfff) + ((va >> 30) & 511) * 8)).unwrap();
        if pdpte & 1 == 0 { return None; }
        let pde = ram.read_obj::<u64>(GuestAddress((pdpte & !0xfff) + ((va >> 21) & 511) * 8)).unwrap();
        if pde & 0x81 != 0x81 { return None; }
        Some((pde & !0x1f_ffff) | (va & 0x1f_ffff))
    };
    for address in [0, entry.rsi, entry.gdt, COMMAND_LINE, entry.rip, boot.plan().kernel.end().unwrap() - 1, RAM_SIZE - 1] {
        assert_eq!(translate(address), Some(address));
    }
    for address in [RAM_SIZE, 1 << 30, 1 << 39] { assert_eq!(translate(address), None); }
}

#[test]
fn rejects_incompatible_and_truncated_headers() {
    for len in [0, 1, 0x200, HEADER_MIN_END - 1] { assert!(prepare(vec![0; len]).is_err()); }
    let changes: Vec<(usize, Vec<u8>)> = vec![
        (0x1fe, vec![0, 0]), (0x200, vec![0]), (0x202, b"NOPE".to_vec()),
        (0x206, 0x020bu16.to_le_bytes().to_vec()), (0x206, 0x0300u16.to_le_bytes().to_vec()),
        (0x211, vec![0]), (0x234, vec![0]), (0x236, vec![0, 0]),
        (0x201, vec![0]), (0x201, vec![255]), (0x1f1, vec![64]),
        (0x1f4, 0u32.to_le_bytes().to_vec()), (0x1f4, u32::MAX.to_le_bytes().to_vec()),
        (0x24c, 0u32.to_le_bytes().to_vec()), (0x248, u32::MAX.to_le_bytes().to_vec()),
        (0x23c, 1u32.to_le_bytes().to_vec()), (0x240, 1u64.to_le_bytes().to_vec()),
        (0x230, 0u32.to_le_bytes().to_vec()), (0x230, 3u32.to_le_bytes().to_vec()),
        (0x230, 4096u32.to_le_bytes().to_vec()), (0x235, vec![63]),
        (0x260, 1u32.to_le_bytes().to_vec()), (0x260, u32::MAX.to_le_bytes().to_vec()),
        (0x258, u64::MAX.to_le_bytes().to_vec()), (0x258, RAM_SIZE.to_le_bytes().to_vec()),
    ];
    for (offset, value) in changes {
        let mut b = image();
        b[offset..offset + value.len()].copy_from_slice(&value);
        assert!(prepare(b).is_err(), "accepted malformed field at {offset:#x}");
    }
    let mut b = image();
    b.pop();
    assert!(prepare(b).is_err());
}

#[test]
fn accepts_legacy_sector_count_and_ignores_file_trailers() {
    let mut b = image();
    b[0x1f1] = 0; // The specified historical meaning is four setup sectors.
    b.extend_from_slice(b"not part of the protected payload");
    let boot = prepare(b).unwrap();
    assert_eq!(boot.payload, 5 * 512..5 * 512 + 0x4000);
}

#[test]
fn obeys_kernel_and_profile_command_line_limits() {
    let config = BootConfig::default();
    let mut b = image();
    put32(&mut b, 0x238, 3);
    assert!(prepare_boot(b.clone(), None, "abc", config).is_ok());
    assert!(prepare_boot(b, None, "abcd", config).is_err());
    assert!(prepare_boot(image(), None, "a\0b", config).is_err());
    let mut b = image();
    put32(&mut b, 0x238, u32::MAX);
    assert!(prepare_boot(b.clone(), None, &"x".repeat(COMMAND_LINE_MAX), config).is_ok());
    assert!(prepare_boot(b, None, &"x".repeat(COMMAND_LINE_MAX + 1), config).is_err());
}

#[test]
fn initramfs_respects_address_limit_and_kernel_workspace() {
    let config = BootConfig::default();
    assert!(prepare_boot(image(), Some(vec![]), "", config).is_err());
    let mut b = image();
    put32(&mut b, 0x22c, (32 << 20) - 1);
    let boot = prepare_boot(b.clone(), Some(vec![1; 4097]), "", config).unwrap();
    let r = boot.plan().initrd.unwrap();
    assert_eq!(r.address % PAGE_SIZE, 0);
    assert!(r.end().unwrap() <= 32 << 20);
    // Fits RAM, but overlaps the decompressor workspace rather than file bytes.
    put32(&mut b, 0x260, 32 << 20);
    assert!(prepare_boot(b, Some(vec![1; 4097]), "", config).is_err());
    for limit in [4095, HIGH_RAM_START as u32 - 1] {
        let mut b = image();
        put32(&mut b, 0x22c, limit);
        assert!(prepare_boot(b, Some(vec![1; 4097]), "", config).is_err());
    }
}

#[test]
fn aligns_runtime_above_the_preferred_address() {
    let mut b = image();
    put64(&mut b, 0x258, (32 << 20) + 1);
    put32(&mut b, 0x230, 4 << 20);
    let boot = prepare(b).unwrap();
    assert_eq!(boot.plan().kernel.address, 36 << 20);
    assert_eq!(boot.plan().entry.rip, (36 << 20) + 0x200);
}

#[test]
fn wrong_profile_or_backing_is_rejected_without_writes() {
    for config in [BootConfig { vcpus: 2, ..Default::default() }, BootConfig { ram_size: 256 << 20, ..Default::default() }, BootConfig { ram_size: u64::MAX, ..Default::default() }] {
        assert!(prepare_boot(image(), None, "", config).is_err());
    }
    let boot = prepare(image()).unwrap();
    for regions in [vec![(GuestAddress(0), 64 << 20)], vec![(GuestAddress(0x10000), RAM_SIZE)], vec![(GuestAddress(0), RAM_SIZE / 2), (GuestAddress(RAM_SIZE / 2), RAM_SIZE / 2)]] {
        let mem = GuestMemory::new(&regions).unwrap();
        mem.write(GuestAddress(BOOT_PARAMS), b"untouched").unwrap();
        assert!(boot.load(&mem).is_err());
        let mut sentinel = [0; 9];
        mem.read(GuestAddress(BOOT_PARAMS), &mut sentinel).unwrap();
        assert_eq!(&sentinel, b"untouched");
    }
}
