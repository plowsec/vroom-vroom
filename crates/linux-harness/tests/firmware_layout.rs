//! Metadata acceptance using actual devices, without constructing/running a VM.
//! The sole memory-using test reserves 503.5 MiB demand-zero virtual memory and
//! touches less than 64 KiB. Parser equivalents below follow the pinned EDK2
//! consumers identified in FIRMWARE_LAYOUT.md; profile checks are stricter than
//! those parsers and never interpret a fallback as successful host metadata.

use vroom_arch::x86_64::Q35FirmwareLayout;
use vroom_devices::{BusBuilder, BusDevice, BusRange};
use vroom_devices::fw_cfg::{FwCfgIo, FILE_DIR, NB_CPUS, RAM_SIZE};
use vroom_devices::pci::{MchPci, MchSmramState, MchTsegState};
use vroom_memory::{GuestAddress, GuestMemory};

type Range = (u64, u64); // base, size
type Check<T> = Result<T, &'static str>;
const CODE_LEN: usize = 3_653_632;
const VARS_LEN: usize = 540_672;
const SCRATCH: u64 = 0x1000;

fn layout() -> Q35FirmwareLayout { Q35FirmwareLayout::for_images(CODE_LEN, VARS_LEN).unwrap() }
fn bus_range((base, len): Range) -> BusRange { BusRange { base, len } }
fn read_u64(bytes: &[u8]) -> u64 { u64::from_le_bytes(bytes.try_into().unwrap()) }

fn stream(fw: &mut FwCfgIo, selector: u16, size: usize) -> Vec<u8> {
    fw.write(0, &selector.to_le_bytes());
    let mut bytes = vec![0; size];
    for byte in &mut bytes { fw.read(1, std::slice::from_mut(byte)); }
    bytes
}

fn directory(fw: &mut FwCfgIo) -> Vec<(String, u16, usize)> {
    let count = u32::from_be_bytes(stream(fw, FILE_DIR, 4).try_into().unwrap());
    assert_eq!(count, 2);
    stream(fw, FILE_DIR, 4 + count as usize * 64)[4..].chunks_exact(64).map(|record| {
        let size = u32::from_be_bytes(record[..4].try_into().unwrap()) as usize;
        let selector = u16::from_be_bytes(record[4..6].try_into().unwrap());
        assert_eq!(&record[6..8], &[0, 0]);
        let name = record[8..].split(|b| *b == 0).next().unwrap();
        (String::from_utf8(name.to_vec()).unwrap(), selector, size)
    }).collect()
}

fn dma_read(fw: &mut FwCfgIo, mem: &GuestMemory, selector: u16, target: u64, len: u32) -> u32 {
    let control = (u32::from(selector) << 16) | 8 | 2; // SELECT | READ
    let mut descriptor = Vec::new();
    descriptor.extend(control.to_be_bytes()); descriptor.extend(len.to_be_bytes()); descriptor.extend(target.to_be_bytes());
    mem.write(GuestAddress(SCRATCH), &descriptor).unwrap();
    fw.write(4, &((SCRATCH >> 32) as u32).to_be_bytes());
    fw.write(8, &(SCRATCH as u32).to_be_bytes());
    let mut status = [0; 4]; mem.read(GuestAddress(SCRATCH), &mut status).unwrap();
    u32::from_be_bytes(status)
}

// Independent sequential decoding of the packed EDK2 header/record.
#[derive(Debug)]
struct HostBridge {
    attributes: u64,
    flags: u32,
    buses: [u8; 2],
    padding: [u8; 2],
    // IO, Mem, PMem, MemAbove4G, PMemAbove4G, PcieConfig
    ranges: [Range; 6],
}

fn host_bridge(file: &[u8]) -> Check<HostBridge> {
    if file.len() != 136 || read_u64(&file[..8]) != 1 || read_u64(&file[8..16]) != 120 {
        return Err("one complete host-bridge record required");
    }
    let record = &file[16..];
    if read_u64(&record[..8]) != 0 { return Err("incompatible version"); }
    let mut ranges = [(0, 0); 6];
    for (range, pair) in ranges.iter_mut().zip(record[24..].chunks_exact(16)) {
        *range = (read_u64(&pair[..8]), read_u64(&pair[8..]));
    }
    Ok(HostBridge { attributes: read_u64(&record[8..16]),
        flags: u32::from_le_bytes(record[16..20].try_into().unwrap()),
        buses: record[20..22].try_into().unwrap(), padding: record[22..24].try_into().unwrap(), ranges })
}

// GetRangeEnd/FillHostBridgeAperture use (MAX,0) for an absent aperture.
// Check arithmetic explicitly here: malformed metadata is never acceptance.
fn aperture((base, size): Range, max: u64) -> Check<Option<(u64, u64)>> {
    if base >= max || size == 0 { return Ok(None); }
    Ok(Some((base, base.checked_add(size - 1).ok_or("aperture overflow")?)))
}

fn merged(a: Option<Range>, b: Option<Range>) -> Option<Range> {
    match (a, b) {
        (Some((a, z)), Some((b, y))) => Some((a.min(b), z.max(y))),
        (one, None) | (None, one) => one,
    }
}

#[derive(Debug)]
struct Apertures { memory: [Option<Range>; 4], io: Option<Range>, stale_warning: bool }

fn get_apertures(h: &HostBridge, want_io: bool, want_ecam: bool) -> Check<Apertures> {
    let io = aperture(h.ranges[0], u32::MAX as u64)?;
    let ecam = aperture(h.ranges[5], u64::MAX)?;
    let mut memory = [aperture(h.ranges[1], u32::MAX as u64)?, aperture(h.ranges[3], u64::MAX)?,
        aperture(h.ranges[2], u32::MAX as u64)?, aperture(h.ranges[4], u64::MAX)?];
    if h.flags & 4 != 0 {
        memory[0] = merged(memory[0], memory[2]); memory[1] = merged(memory[1], memory[3]);
        memory[2] = None; memory[3] = None;
    }
    // Inner NULL-output errors become EFI_WARN_STALE_DATA in GetApertures.
    // Both selected callers check EFI_ERROR, so they continue on this warning.
    Ok(Apertures { memory, io: if want_io { io } else { None },
        stale_warning: (!want_io && io.is_some()) || (!want_ecam && ecam.is_some()) })
}

fn pei_last_mmio(file: &[u8], high: bool) -> Check<(u64, bool)> {
    let h = host_bridge(file)?;
    let parsed = get_apertures(&h, false, false)?;
    let first = usize::from(high);
    Ok(([parsed.memory[first], parsed.memory[first + 2]].into_iter().flatten()
        .map(|(_, limit)| limit).max().unwrap_or(0), parsed.stale_warning))
}

#[derive(Debug)]
struct RootBridge { host: HostBridge, apertures: Apertures, mem64_decode: bool }

fn dxe_root(file: &[u8]) -> Check<RootBridge> {
    let host = host_bridge(file)?;
    if host.buses[0] > host.buses[1] { return Err("invalid bus range"); }
    let apertures = get_apertures(&host, true, false)?;
    let mem64_decode = [apertures.memory[1], apertures.memory[3]].into_iter().flatten().any(|(base, limit)| limit > base);
    Ok(RootBridge { host, apertures, mem64_decode })
}

fn accept_pci(layout: &Q35FirmwareLayout, file: &[u8]) -> Check<()> {
    let root = dxe_root(file)?;
    // Strict selected-profile acceptance, beyond the permissive OVMF parser.
    if root.host.attributes != 0 || root.host.flags != 4 || root.host.buses != [0, 255]
        || root.host.padding != [0, 0] || !root.mem64_decode || root.apertures.io.is_some()
        || root.apertures.stale_warning || root.apertures.memory[2..] != [None, None]
        || [0, 2, 4, 5].iter().any(|&i| root.host.ranges[i] != (0, 0)) {
        return Err("unexpected host-bridge attributes or aperture");
    }
    for (i, (base, size)) in layout.bar_windows().into_iter().enumerate() {
        let expected = (base, base + size - 1);
        if root.apertures.memory[i] != Some(expected)
            || pei_last_mmio(file, i == 1)? != (expected.1, false) {
            return Err("PEI/DXE resources do not match actual MCH windows");
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Hobs { ordinary: Vec<Range>, reserved: Vec<Range>, low_memory: u64 }

fn replay_hobs(e820: &[u8], tseg_mb: u16, default_smbase: bool) -> Check<Hobs> {
    if e820.is_empty() || e820.len() % 20 != 0 || e820.len() > 20 * 16 { return Err("invalid E820 file"); }
    let mut low_memory = 0;
    for entry in e820.chunks_exact(20) {
        let kind = u32::from_le_bytes(entry[16..].try_into().unwrap());
        let end = read_u64(&entry[..8]).checked_add(read_u64(&entry[8..16])).ok_or("E820 overflow")?;
        if kind == 1 && end < 1 << 32 { low_memory = low_memory.max(end); }
    }
    let size = u64::from(tseg_mb) << 20;
    let tseg = low_memory.checked_sub(size).filter(|base| *base >= 1 << 20).ok_or("TSEG underflow")?;
    let mut ordinary = if default_smbase { vec![(0, 0x30000), (0x50000, 0x50000)] } else { vec![(0, 0xa0000)] };
    ordinary.push((1 << 20, tseg - (1 << 20)));
    let mut reserved = if default_smbase { vec![(0x30000, 0x20000)] } else { vec![] };
    reserved.push((tseg, size));
    Ok(Hobs { ordinary, reserved, low_memory })
}

fn accept_hobs(layout: &Q35FirmwareLayout, e820: &[u8], tseg_mb: u16, default_smbase: bool, cmos: [(u8, u8); 5]) -> Check<()> {
    let hobs = replay_hobs(e820, tseg_mb, default_smbase)?;
    if cmos.map(|(reg, _)| reg) != [0x34, 0x35, 0x5b, 0x5c, 0x5d] { return Err("CMOS register mismatch"); }
    let cmos_low = (16 << 20) + (u64::from(u16::from_le_bytes([cmos[0].1, cmos[1].1])) << 16);
    if hobs.low_memory != layout.nominal_ram_bytes() || cmos_low != hobs.low_memory
        || cmos[2..].iter().any(|(_, value)| *value != 0)
        || hobs.ordinary != layout.ordinary_ram() || hobs.reserved != [layout.default_smbase(), layout.tseg()] {
        return Err("firmware HOB/CMOS/private mapping mismatch");
    }
    Ok(())
}

fn cf8_select(mch: &mut MchPci, reg: u8) {
    mch.pio_write(0xcf8, &(0x8000_0000u32 | u32::from(reg & !3)).to_le_bytes()).unwrap();
}
fn cf8_read(mch: &mut MchPci, reg: u8, bytes: &mut [u8]) {
    cf8_select(mch, reg); mch.pio_read(0xcfc + u16::from(reg & 3), bytes).unwrap();
}
fn config_write(mch: &mut MchPci, reg: u8, bytes: &[u8], state: &mut MchSmramState) {
    cf8_select(mch, reg);
    // This acknowledges a device-state request ONLY. There is no HV/private
    // backing here and this callback is deliberately not a runtime proof.
    mch.pio_write_with_smram(0xcfc + u16::from(reg & 3), bytes, &mut |requested| {
        *state = requested; Ok(requested)
    }).unwrap();
}

fn check_exclusions(l: &Q35FirmwareLayout, mem: &GuestMemory, fw: &mut FwCfgIo, selector: u16) {
    for (base, size) in [l.default_smbase(), l.tseg(), l.code(), l.vars(), l.kvm_private()] {
        assert!(!mem.check_range(GuestAddress(base), size));
        assert!(mem.host_ranges(GuestAddress(base), size).is_none());
        assert!(mem.read(GuestAddress(base), &mut [0; 8]).is_err());
        assert!(mem.write(GuestAddress(base), &[0x5a; 8]).is_err());
        for (target, len) in [(base, 8), (base + size - 4, 8), (base - 4, 8)] {
            assert_eq!(dma_read(fw, mem, selector, target, len), 1);
        }
    }
}

#[test]
fn real_devices_consume_one_layout_without_private_dma_or_firmware_fallback() {
    let l = layout();
    let mem = GuestMemory::new(&l.ordinary_ram().map(|(base, size)| (GuestAddress(base), size))).unwrap();
    assert_eq!(mem.total_size(), 527_958_016);
    assert_ne!(mem.total_size(), l.nominal_ram_bytes());
    let mut fw = FwCfgIo::new(mem.clone(), l.cpu_count(), l.nominal_ram_bytes());
    fw.add_file("etc/e820", l.firmware_e820(), None).unwrap();
    fw.add_file("etc/hardware-info", l.hardware_info(), None).unwrap();
    assert_eq!(read_u64(&stream(&mut fw, RAM_SIZE, 8)), 512 << 20);
    assert_eq!(stream(&mut fw, NB_CPUS, 2), [1, 0]);
    let entries = directory(&mut fw);
    assert_eq!(entries.iter().map(|(name, _, size)| (name.as_str(), *size)).collect::<Vec<_>>(),
        [("etc/e820", 20), ("etc/hardware-info", 136)]);
    let e820 = stream(&mut fw, entries[0].1, entries[0].2);
    let hardware = stream(&mut fw, entries[1].1, entries[1].2);
    for ((_, selector, size), expected) in entries.iter().zip([&e820, &hardware]) {
        assert_eq!(dma_read(&mut fw, &mem, *selector, SCRATCH + 0x100, *size as u32), 0);
        let mut bytes = vec![0; *size]; mem.read(GuestAddress(SCRATCH + 0x100), &mut bytes).unwrap();
        assert_eq!(&bytes, expected);
    }
    accept_pci(&l, &hardware).unwrap();
    let mut mch = MchPci::with_q35_smram(vec![], BusBuilder::new().build(), &mem, bus_range(l.ecam()),
        l.bar_windows().map(bus_range).to_vec(), l.reserved_ranges().map(bus_range).to_vec()).unwrap();
    let mut state = MchSmramState { tseg: MchTsegState::Open, default_smbase_locked: false };
    config_write(&mut mch, 0x50, &[0xff, 0xff], &mut state);
    config_write(&mut mch, 0x9c, &[0xff], &mut state);
    let mut tseg_mb = [0; 2]; cf8_read(&mut mch, 0x50, &mut tseg_mb);
    let mut default = [0]; cf8_read(&mut mch, 0x9c, &mut default);
    assert_eq!(tseg_mb, [8, 0]); assert_eq!(default, [1]);
    accept_hobs(&l, &e820, u16::from_le_bytes(tseg_mb), default == [1], l.cmos_memory()).unwrap();
    // The omitted descriptor ECAM pair does not disable actual decoding.
    for (reg, bytes) in [(0x60, 0u32.to_le_bytes()), (0x64, 0u32.to_le_bytes()),
        (0x60, (l.ecam().0 as u32 | 1).to_le_bytes())] {
        cf8_select(&mut mch, reg); mch.pio_write(0xcfc, &bytes).unwrap();
    }
    let mut via_ecam = [0; 2];
    assert!(mch.mmio_read(l.ecam().0 + 0x50, &mut via_ecam).unwrap());
    assert_eq!(via_ecam, tseg_mb);
    let mut query = [0]; assert!(mch.mmio_read(l.ecam().0 + 0x9c, &mut query).unwrap());
    assert_eq!(query, default);
    // DMA exclusions remain identical through modeled open/closed/locked state.
    check_exclusions(&l, &mem, &mut fw, entries[1].1);
    config_write(&mut mch, 0x9e, &[0x3d], &mut state);
    config_write(&mut mch, 0x9d, &[0x0a], &mut state);
    assert_eq!(state.tseg, MchTsegState::Closed);
    check_exclusions(&l, &mem, &mut fw, entries[1].1);
    config_write(&mut mch, 0x9d, &[0x1a], &mut state);
    config_write(&mut mch, 0x9c, &[2], &mut state);
    assert_eq!(state, MchSmramState { tseg: MchTsegState::Locked, default_smbase_locked: true });
    check_exclusions(&l, &mem, &mut fw, entries[1].1);
}

#[test]
fn firmware_derivation_rejects_double_subtraction_query_and_cmos_mismatches() {
    let l = layout(); let e820 = l.firmware_e820();
    accept_hobs(&l, &e820, 8, true, l.cmos_memory()).unwrap();
    let mut subtracted = e820.clone(); subtracted[8..16].copy_from_slice(&0x1f80_0000u64.to_le_bytes());
    assert_eq!(replay_hobs(&subtracted, 8, true).unwrap().reserved[1].0, 0x1f00_0000);
    assert!(accept_hobs(&l, &subtracted, 8, true, l.cmos_memory()).is_err());
    assert!(accept_hobs(&l, &e820, 4, true, l.cmos_memory()).is_err());
    assert!(accept_hobs(&l, &e820, 8, false, l.cmos_memory()).is_err());
    let mut wrong_cmos = l.cmos_memory(); wrong_cmos[1].1 -= 1;
    assert!(accept_hobs(&l, &e820, 8, true, wrong_cmos).is_err());
    wrong_cmos = l.cmos_memory(); wrong_cmos[2].1 = 1;
    assert!(accept_hobs(&l, &e820, 8, true, wrong_cmos).is_err());
    assert!(replay_hobs(&e820[..19], 8, true).is_err());
    let mut overflow = e820.clone(); overflow[..8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(replay_hobs(&overflow, 8, true).is_err());
}

#[test]
fn pei_and_dxe_take_exact_resources_and_cannot_silently_fall_back() {
    let l = layout(); let bytes = l.hardware_info();
    accept_pci(&l, &bytes).unwrap();
    assert_eq!(pei_last_mmio(&bytes, false).unwrap(), (0xdfff_ffff, false));
    assert_eq!(pei_last_mmio(&bytes, true).unwrap(), (0xf_ffff_ffff, false));
    for size in [0, 15, 16, 135] { assert!(accept_pci(&l, &bytes[..size]).is_err()); }
    let mut oversized = bytes.clone(); oversized.push(0); assert!(accept_pci(&l, &oversized).is_err());
    for (offset, value) in [(0, 2), (8, 119), (16, 1), (24, 1), (32, 5), (36, 1), (37, 0),
        (38, 1), (67, 0), (88, 1), (103, 0x80)] {
        let mut changed = bytes.clone(); changed[offset] = value;
        assert!(accept_pci(&l, &changed).is_err(), "mutation at {offset}");
    }
    // Oversized >64GiB BAR64 or an empty BAR64 must not accept an implicit
    // CPUID-dependent firmware fallback in place of our actual MCH window.
    for (base, size) in [(0x10_0000_0000u64, 0x8_0000_0000u64), (0, 0)] {
        let mut changed = bytes.clone();
        changed[88..96].copy_from_slice(&base.to_le_bytes()); changed[96..104].copy_from_slice(&size.to_le_bytes());
        assert!(accept_pci(&l, &changed).is_err());
    }
}

#[test]
fn unused_apertures_warn_and_callers_continue_but_fixed_profile_rejects_them() {
    let l = layout();
    for offset in [40, 120] { // record IO and ECAM pairs, including 16B header
        let mut bytes = l.hardware_info();
        bytes[offset..offset + 8].copy_from_slice(&0xe000_0000u64.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&0x1000u64.to_le_bytes());
        assert_eq!(pei_last_mmio(&bytes, true).unwrap(), (0xf_ffff_ffff, true));
        let root = dxe_root(&bytes).unwrap(); // EFI_WARN_STALE_DATA is not EFI_ERROR
        assert_eq!(root.apertures.stale_warning, offset == 120);
        assert!(root.mem64_decode);
        assert!(accept_pci(&l, &bytes).is_err()); // our fixed profile omits both
    }
}
