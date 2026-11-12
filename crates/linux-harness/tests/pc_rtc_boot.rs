//! Replay selected OVMF RTC port transactions against the actual device.
//! No firmware, guest, IRQ worker, host wall-clock read or VM is executed.

use vroom_arch::x86_64::Q35FirmwareLayout;
use vroom_devices::pc_rtc::{PcRtc, RtcError};

const SECOND: u64 = 1_000_000_000;
const EPOCH: u64 = 1_794_096_000; // 2026-11-08T00:00:00Z
const FIELDS: [u8; 6] = [0, 2, 4, 7, 8, 9];

fn rtc() -> PcRtc {
    let layout = Q35FirmwareLayout::for_images(3_653_632, 540_672).unwrap();
    PcRtc::new(EPOCH, layout.cmos_memory()).unwrap()
}

// PcRtc.c IoRtcRead/IoRtcWrite read70 to preserve bit7 before selecting a
// register. Pin deliberately returnsff at70; no readable-index assumption.
fn select(rtc: &mut PcRtc, reg: u8, now: u64) {
    let mut index = [0]; assert!(rtc.pio_read(0x70, &mut index, now).unwrap());
    assert_eq!(index, [0xff]);
    assert!(rtc.pio_write(0x70, &[reg | (index[0] & 0x80)], now).unwrap());
}
fn read(rtc: &mut PcRtc, reg: u8, now: u64) -> u8 {
    select(rtc, reg, now); let mut value = [0];
    assert!(rtc.pio_read(0x71, &mut value, now).unwrap()); value[0]
}
fn write(rtc: &mut PcRtc, reg: u8, value: u8, now: u64) {
    select(rtc, reg, now); assert!(rtc.pio_write(0x71, &[value], now).unwrap());
}

fn wait_ready(rtc: &mut PcRtc, start: u64) -> u64 {
    // RtcWaitToUpdate's bounded100ms wait, using explicit deterministic time
    // instead of host sleeps. Normal UIP is only the last244us of a second.
    for tick in 0..=10_000 {
        let now = start + tick * 10_000;
        assert_eq!(read(rtc, 0x0d, now) & 0x80, 0x80);
        if read(rtc, 0x0a, now) & 0x80 == 0 { return now; }
    }
    panic!("RTC did not become ready within selected firmware timeout");
}

fn calendar(rtc: &mut PcRtc, now: u64) -> [u8; 6] {
    assert_eq!(read(rtc, 0x0b, now), 2);
    FIELDS.map(|reg| read(rtc, reg, now))
}
fn set_time(rtc: &mut PcRtc, values: [u8; 6], now: u64) {
    let b = read(rtc, 0x0b, now);
    write(rtc, 0x0b, b | 0x80, now);
    for (reg, value) in FIELDS.into_iter().zip(values) { write(rtc, reg, value, now); }
    write(rtc, 0x0b, b & !0x80, now);
}

fn initialize(rtc: &mut PcRtc, now: u64, repair_alarm: bool) {
    // Exact selected PcRtcInit outline: A26; B/C reads; D0; wait; read calendar;
    // reconfigure B02; SetTime using SET. No FADT or century access is needed.
    write(rtc, 0x0a, 0x26, now);
    let old_b = read(rtc, 0x0b, now); let _ = read(rtc, 0x0c, now);
    write(rtc, 0x0d, 0, now);
    assert_eq!(wait_ready(rtc, now), now);
    let time = FIELDS.map(|reg| read(rtc, reg, now));
    write(rtc, 0x0b, 2 | (old_b & 0x20), now);
    set_time(rtc, time, now);
    // GetWakeupTime checks B/C and disabled alarm values. If invalid, init
    // repairs only1/3/5 under SET, without writing any calendar fields.
    assert_eq!(read(rtc, 0x0b, now) & 0x20, 0);
    assert_eq!(read(rtc, 0x0c, now) & 0xe0, 0);
    let alarm = [1, 3, 5].map(|reg| read(rtc, reg, now));
    if repair_alarm {
        assert_eq!(alarm, [0xff; 3]);
        write(rtc, 0x0b, 0x82, now);
        for reg in [1, 3, 5] { write(rtc, reg, 0, now); }
        write(rtc, 0x0b, 2, now);
    } else {
        assert_eq!(alarm, [0; 3]);
    }
}

#[test]
fn selected_init_gettime_settime_and_layout_memory_are_consumed_through_ports() {
    let mut r = rtc();
    let layout = Q35FirmwareLayout::for_images(3_653_632, 540_672).unwrap();
    for (reg, value) in layout.cmos_memory() { assert_eq!(read(&mut r, reg, 0), value); }
    for reg in 0..128 { let _ = read(&mut r, reg, 0); } // PlatformDebugDumpCmos
    initialize(&mut r, 0, false);
    assert_eq!(calendar(&mut r, 0), [0, 0, 0, 8, 0x11, 0x26]);
    assert_eq!(calendar(&mut r, 61 * SECOND), [1, 1, 0, 8, 0x11, 0x26]);
    assert_eq!(read(&mut r, 6, 61 * SECOND), 1); // Sunday
    set_time(&mut r, [0x59, 0x59, 0x23, 0x28, 2, 0], 61 * SECOND);
    assert_eq!(calendar(&mut r, 62 * SECOND), [0, 0, 0, 0x29, 2, 0]);
    assert_eq!(read(&mut r, 0x0b, 62 * SECOND), 2);
    assert_eq!(read(&mut r, 0x0c, 62 * SECOND) & 0xe0, 0); // never IRQ/alarm/periodic
}

#[test]
fn disabled_alarm_repair_and_uip_wait_do_not_require_active_irq_delivery() {
    let mut r = rtc();
    for reg in [1, 3, 5] { write(&mut r, reg, 0xff, 0); }
    initialize(&mut r, 0, true);
    assert_eq!([1, 3, 5].map(|reg| read(&mut r, reg, 0)), [0; 3]);
    assert_eq!(calendar(&mut r, 0), [0, 0, 0, 8, 0x11, 0x26]);
    let ready = wait_ready(&mut r, SECOND - 200_000);
    assert_eq!(ready, SECOND);
    assert_eq!(calendar(&mut r, ready)[0], 1);
    select(&mut r, 0x0b, ready); let old = r.clone();
    for value in [0x12, 0x22, 0x42] {
        assert_eq!(r.pio_write(0x71, &[value], ready + SECOND), Err(RtcError::Control));
        assert_eq!(r, old);
    }
    // Failed future-time writes did not consume the clock or replace B02.
    assert_eq!(read(&mut r, 0x0b, ready), 2);
}
