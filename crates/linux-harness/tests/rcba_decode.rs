//! Actual CF8/ECAM/MMIO decoder acceptance without a VM or firmware run.
//! Layout inventories are real; this does not populate the complete board.

use std::sync::{Arc, Mutex};
use vroom_arch::x86_64::Q35FirmwareLayout;
use vroom_devices::{Bus, BusBuilder, BusDevice, BusRange};
use vroom_devices::pci::{LpcError, LpcIoContext, LpcPioLayout, LpcPioWriteOutcome,
    MchError, MchPci, PciConfigError};
use vroom_memory::{GuestAddress, GuestMemory, page_size};

const BASE: u64 = 0xfed1_c000;
const LEN: u64 = 0x4000;
const ECAM: u64 = 0xe000_0000;
const LPC: u64 = ECAM + (31 << 15);
fn range((base, len): (u64, u64)) -> BusRange { BusRange { base, len } }
fn layout() -> Q35FirmwareLayout { Q35FirmwareLayout::for_images(3_653_632, 540_672).unwrap() }
fn pio() -> LpcPioLayout {
    LpcPioLayout::new(vec![range((0x600, 0x80)), range((0x800, 0x80))], vec![]).unwrap()
}
fn reserved() -> Vec<BusRange> { layout().reserved_ranges().into_iter().map(range).collect() }
fn make(fixed: Bus, ram: &GuestMemory, ecam: BusRange, windows: Vec<BusRange>,
        reserved: Vec<BusRange>) -> Result<MchPci, MchError> {
    MchPci::with_q35_lpc_rcrb(vec![], fixed, ram, ecam, windows, reserved, pio())
}
fn router() -> MchPci {
    make(Bus::default(), &GuestMemory::default(), range(layout().ecam()),
        layout().bar_windows().into_iter().map(range).collect(), reserved()).unwrap()
}
fn select(r: &mut MchPci, slot: u8, off: u8) {
    r.pio_write(0xcf8, &(0x8000_0000 | (u32::from(slot) << 11) | u32::from(off & !3)).to_le_bytes()).unwrap();
}
fn cf8_write(r: &mut MchPci, slot: u8, off: u8, bytes: &[u8]) -> Result<(), MchError> {
    select(r, slot, off); r.pio_write(0xcfc + u16::from(off & 3), bytes)
}
fn rcba(r: &mut MchPci) -> u32 {
    select(r, 31, 0xf0); let mut bytes = [0; 4]; r.pio_read(0xcfc, &mut bytes).unwrap();
    u32::from_le_bytes(bytes)
}
fn enable_ecam(r: &mut MchPci) {
    for (off, value) in [(0x60, 0), (0x64, 0), (0x60, ECAM as u32 | 1)] {
        cf8_write(r, 0, off, &value.to_le_bytes()).unwrap();
    }
}
fn unsupported() -> MchError { MchError::Lpc(LpcError::RcrbRegister) }

#[test]
fn pinned_cf8_setup_and_ecam_share_one_actual_enabled_decoder() {
    let mut r = router();
    assert_eq!(rcba(&mut r), 0);
    let mut bytes = [0xa5; 4];
    assert_eq!(r.mmio_read(BASE, &mut bytes), Ok(false)); assert_eq!(bytes, [0xa5; 4]);
    assert_eq!(r.mmio_write(BASE, &[1; 4]), Ok(false));
    // PlatformMiscInitialization writes F0 before enabling PCIEXBAR.
    cf8_write(&mut r, 31, 0xf0, &(BASE as u32 | 1).to_le_bytes()).unwrap();
    assert_eq!(r.mmio_read(BASE, &mut bytes), Err(unsupported()));
    enable_ecam(&mut r);
    r.mmio_read(LPC + 0xf0, &mut bytes).unwrap(); assert_eq!(u32::from_le_bytes(bytes), BASE as u32 | 1);
    for width in [1, 2, 4, 8] {
        let mut data = vec![0xa5; width];
        for address in [BASE, BASE + LEN - width as u64, BASE + 0x3100] {
            assert_eq!(r.mmio_read(address, &mut data), Err(unsupported()));
            assert_eq!(r.mmio_write(address, &data), Err(unsupported()));
            assert_eq!(data, vec![0xa5; width]);
        }
    }
    // RCBA is independent of PM decode and the PCI command bits.
    r.mmio_write(LPC + 4, &3u16.to_le_bytes()).unwrap();
    r.mmio_write(LPC + 0x40, &0x600u32.to_le_bytes()).unwrap();
    r.mmio_write(LPC + 0x44, &[0x80]).unwrap();
    r.mmio_write(LPC + 0x44, &[0]).unwrap();
    r.mmio_write(LPC + 4, &[0; 2]).unwrap();
    assert_eq!(r.mmio_read(BASE, &mut bytes), Err(unsupported()));
    for value in [BASE as u32, 0] {
        r.mmio_write(LPC + 0xf0, &value.to_le_bytes()).unwrap();
        assert_eq!(rcba(&mut r), value);
        bytes.fill(0xa5);
        assert_eq!(r.mmio_read(BASE, &mut bytes), Ok(false)); assert_eq!(bytes, [0xa5; 4]);
        assert_eq!(r.mmio_write(BASE + LEN - 4, &[0; 4]), Ok(false));
    }
}

#[test]
fn malformed_config_or_mmio_cannot_change_readback_or_decode() {
    let mut r = router(); enable_ecam(&mut r);
    r.mmio_write(LPC + 0xf0, &(BASE as u32 | 1).to_le_bytes()).unwrap();
    for value in [1, BASE as u32 | 2, BASE as u32 | 3, BASE as u32 + 0x4000, 0xffff_ffff] {
        assert!(cf8_write(&mut r, 31, 0xf0, &value.to_le_bytes()).is_err());
        assert!(r.mmio_write(LPC + 0xf0, &value.to_le_bytes()).is_err());
        assert_eq!(rcba(&mut r), BASE as u32 | 1);
    }
    for (off, width) in [(0xf0, 1), (0xf0, 2), (0xf0, 8), (0xf1, 1), (0xf2, 2), (0xef, 2)] {
        let mut bytes = vec![0xa5; width];
        assert!(r.mmio_read(LPC + off, &mut bytes).is_err()); assert_eq!(bytes, vec![0xa5; width]);
        assert!(r.mmio_write(LPC + off, &bytes).is_err());
        assert_eq!(rcba(&mut r), BASE as u32 | 1);
    }
    for (address, width) in [(BASE - 1, 2), (BASE + LEN - 1, 2), (BASE, 3), (u64::MAX - 7, 8)] {
        let mut bytes = vec![0xa5; width];
        assert!(r.mmio_read(address, &mut bytes).is_err()); assert_eq!(bytes, vec![0xa5; width]);
        assert!(r.mmio_write(address, &bytes).is_err());
    }
    let mut bytes = [0xa5; 4];
    for address in [BASE - 4, BASE + LEN] { assert_eq!(r.mmio_read(address, &mut bytes), Ok(false)); }
    assert_eq!(bytes, [0xa5; 4]);
    assert_eq!(r.mmio_read(BASE, &mut bytes), Err(unsupported()));
    // Non-opted-in LPC retains its exact unsupported-F0 behavior.
    let mut old = MchPci::with_q35_lpc(vec![], Bus::default(), &GuestMemory::default(),
        range(layout().ecam()), vec![], reserved(), pio()).unwrap(); enable_ecam(&mut old);
    assert_eq!(old.mmio_read(LPC + 0xf0, &mut bytes),
        Err(MchError::Config(PciConfigError::Lpc(LpcError::UnsupportedRegister))));
    assert_eq!(old.mmio_read(BASE, &mut bytes), Ok(false));
}

struct Fixed(Arc<Mutex<usize>>);
impl BusDevice for Fixed {
    fn read(&mut self, _: u64, data: &mut [u8]) { *self.0.lock().unwrap() += 1; data.fill(0x55); }
    fn write(&mut self, _: u64, _: &[u8]) { *self.0.lock().unwrap() += 1; }
}
fn fixed(base: u64, len: u64, count: Arc<Mutex<usize>>) -> Bus {
    let mut builder = BusBuilder::new(); builder.insert(base, len, Arc::new(Mutex::new(Fixed(count)))).unwrap(); builder.build()
}

#[test]
fn constructor_binds_exact_unique_reservation_and_rejects_all_collisions() {
    let empty = GuestMemory::default(); let ecam = range(layout().ecam());
    for ranges in [reserved().into_iter().filter(|r| r.base != BASE).collect(),
        { let mut r = reserved(); r.push(range((BASE, LEN))); r },
        { let mut r = reserved(); r.push(range((BASE - 0x1000, 0x2000))); r },
        { let mut r = reserved(); r.retain(|r| r.base != BASE); r.push(range((BASE - 0x4000, 0x8000))); r }] {
        assert!(matches!(make(Bus::default(), &empty, ecam, vec![], ranges), Err(MchError::RcrbLayout)));
    }
    let count = Arc::new(Mutex::new(0));
    assert!(matches!(make(fixed(BASE, LEN, count.clone()), &empty, ecam, vec![], reserved()), Err(MchError::RcrbLayout)));
    let page = page_size() as u64;
    let ram = GuestMemory::new(&[(GuestAddress(BASE / page * page), page)]).unwrap();
    ram.write(GuestAddress(BASE), &[0x44]).unwrap();
    assert!(matches!(make(Bus::default(), &ram, ecam, vec![], reserved()), Err(MchError::RcrbLayout)));
    let mut byte = [0]; ram.read(GuestAddress(BASE), &mut byte).unwrap(); assert_eq!(byte, [0x44]);
    assert!(matches!(make(Bus::default(), &empty, range((0xf000_0000, 0x1000_0000)), vec![], reserved()), Err(MchError::RcrbLayout)));
    assert!(matches!(make(Bus::default(), &empty, ecam, vec![range((BASE, LEN))], reserved()), Err(MchError::RcrbLayout)));
    assert_eq!(*count.lock().unwrap(), 0);
    // Adjacent fixed device stays functional, but RCRB never falls through.
    let mut r = make(fixed(BASE + LEN, 8, count.clone()), &empty, ecam, vec![], reserved()).unwrap();
    let mut bytes = [0xa5; 4];
    assert_eq!(r.mmio_read(BASE, &mut bytes), Ok(false)); assert_eq!(*count.lock().unwrap(), 0);
    cf8_write(&mut r, 31, 0xf0, &(BASE as u32 | 1).to_le_bytes()).unwrap();
    assert_eq!(r.mmio_read(BASE, &mut bytes), Err(unsupported())); assert_eq!(*count.lock().unwrap(), 0);
    assert_eq!(r.mmio_read(BASE + LEN, &mut bytes), Ok(true)); assert_eq!(bytes, [0x55; 4]);
    assert_eq!(*count.lock().unwrap(), 1);
}

#[test]
fn smram_smi_and_s5_gates_precede_every_rcba_route() {
    for failure in 0..4 {
        let mut r = router(); enable_ecam(&mut r);
        r.mmio_write(LPC + 0xf0, &(BASE as u32 | 1).to_le_bytes()).unwrap();
        r.mmio_write(LPC + 0x40, &0x600u32.to_le_bytes()).unwrap(); r.mmio_write(LPC + 0x44, &[0x80]).unwrap();
        select(&mut r, 31, 0xf0);
        let error = match failure {
            0 => {
                assert!(r.mmio_write_with_smram(ECAM + 0x50, &[255; 2], &mut |_| Err(PciConfigError::SmramBackend)).is_err());
                MchError::Config(PciConfigError::SmramPoisoned)
            }
            1 | 2 => {
                r.lpc_pio_write(0x630, &0x21u32.to_le_bytes(), 0).unwrap();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    r.lpc_pio_write_with_smi(0xb2, &[0x42], 0, LpcIoContext::Normal, &mut |_| {
                        if failure == 2 { panic!("injected SMI failure") } Err(LpcError::SmiBackend)
                    })
                }));
                if failure == 2 { assert!(result.is_err()); } else { assert!(result.unwrap().is_err()); }
                MchError::Config(PciConfigError::Lpc(LpcError::SmiPoisoned))
            }
            _ => {
                assert_eq!(r.lpc_pio_write_with_power(0x604, &[0, 0x20], 0, LpcIoContext::Normal), Ok(LpcPioWriteOutcome::AcpiS5));
                MchError::Config(PciConfigError::Lpc(LpcError::PoweredOff))
            }
        };
        let mut bytes = [0xa5; 4];
        for port in [0xcf8, 0xcfc] {
            assert_eq!(r.pio_read(port, &mut bytes), Err(error)); assert_eq!(r.pio_write(port, &[0; 4]), Err(error));
        }
        for address in [BASE, BASE + LEN, LPC + 0xf0] {
            assert_eq!(r.mmio_read(address, &mut bytes), Err(error)); assert_eq!(r.mmio_write(address, &[0; 4]), Err(error));
            assert_eq!(r.mmio_write_with_smram(address, &[0; 4], &mut |_| panic!("unexpected mapping")), Err(error));
        }
        assert_eq!(bytes, [0xa5; 4]);
    }
}
