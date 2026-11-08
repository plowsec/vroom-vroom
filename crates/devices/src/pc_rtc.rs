//! Checked PC RTC/CMOS for the selected OVMF first-EFI profile.
//!
//! Only BCD/24-hour calendar, SET transactions and disabled alarm storage are
//! supported. Active interrupts and alternate clock modes return errors; this
//! is not a complete MC146818 or wakeup implementation. The caller supplies
//! one cold UTC epoch and monotonic elapsed time, with no host clock access in
//! the device. No infallible BusDevice adapter, snapshots or timer worker.
//!
//! Pinned PcRtcInit/GetTime/SetTime and QEMU UIP semantics are documented in
//! `crates/linux-harness/PC_RTC_BOOT.md`.

const SECOND: u64 = 1_000_000_000;
const UIP_HOLD: u64 = 8 * SECOND / 32768;
const MIN_SECONDS: u64 = 883_612_800; // 1998-01-01 UTC
const END_SECONDS: u64 = 4_039_372_800; // 2098-01-01 UTC, exclusive
const CALENDAR_REGS: [usize; 6] = [0, 2, 4, 7, 8, 9];
const CMOS_MEMORY_REGS: [u8; 5] = [0x34, 0x35, 0x5b, 0x5c, 0x5d];

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RtcError {
    #[error("RTC requires one byte wholly inside port 70 or 71")]
    Access,
    #[error("RTC clock regressed or exceeded the selected 1998..2097 calendar range")]
    Clock,
    #[error("unsupported RTC control mode or active interrupt enable")]
    Control,
    #[error("calendar writes require SET; weekday writes are unsupported")]
    CalendarWrite,
    #[error("SET calendar transaction is incomplete")]
    IncompleteCalendar,
    #[error("SET calendar contains invalid BCD or date fields")]
    Calendar,
    #[error("RTC CMOS memory seed must name 34,35,5b,5c,5d in order")]
    MemorySeed,
}

type Result<T> = std::result::Result<T, RtcError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Calendar { year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8 }

fn leap(year: u16) -> bool { year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) }
fn month_days(year: u16, month: u8) -> u8 {
    match month { 2 if leap(year) => 29, 2 => 28, 4 | 6 | 9 | 11 => 30, _ => 31 }
}
fn bcd(value: u8) -> u8 { (value / 10) << 4 | value % 10 }
fn unbcd(value: u8) -> Result<u8> {
    if value >> 4 > 9 || value & 15 > 9 { return Err(RtcError::Calendar); }
    Ok((value >> 4) * 10 + (value & 15))
}

impl Calendar {
    fn from_seconds(seconds: u64) -> Result<Self> {
        if !(MIN_SECONDS..END_SECONDS).contains(&seconds) { return Err(RtcError::Clock); }
        let mut days = seconds / 86400;
        let mut year = 1970;
        // Bounded by the checked 1998..2097 profile, not by elapsed ticks.
        loop {
            let n = if leap(year) { 366 } else { 365 };
            if days < n { break; }
            days -= n; year += 1;
        }
        let mut month = 1;
        while days >= u64::from(month_days(year, month)) {
            days -= u64::from(month_days(year, month)); month += 1;
        }
        Ok(Self { year, month, day: days as u8 + 1, hour: (seconds / 3600 % 24) as u8,
            minute: (seconds / 60 % 60) as u8, second: (seconds % 60) as u8 })
    }

    fn seconds(self) -> Result<u64> {
        if !(1998..=2097).contains(&self.year) || !(1..=12).contains(&self.month)
            || self.day == 0 || self.day > month_days(self.year, self.month)
            || self.hour > 23 || self.minute > 59 || self.second > 59 {
            return Err(RtcError::Calendar);
        }
        let mut days = 0u64;
        for year in 1970..self.year { days += if leap(year) { 366 } else { 365 }; }
        for month in 1..self.month { days += u64::from(month_days(self.year, month)); }
        days += u64::from(self.day - 1);
        Ok(days * 86400 + u64::from(self.hour) * 3600 + u64::from(self.minute) * 60 + u64::from(self.second))
    }

    fn read_cmos(cmos: &[u8; 128]) -> Result<Self> {
        let year = unbcd(cmos[9])?;
        // Same century pivot as selected PcdMinimalValidYear=1998.
        Ok(Self { year: if year >= 98 { 1900 } else { 2000 } + u16::from(year),
            month: unbcd(cmos[8])?, day: unbcd(cmos[7])?, hour: unbcd(cmos[4])?,
            minute: unbcd(cmos[2])?, second: unbcd(cmos[0])? })
    }

    fn write_cmos(self, cmos: &mut [u8; 128], seconds: u64) {
        let values = [self.second, self.minute, self.hour, self.day, self.month, (self.year % 100) as u8];
        for (reg, value) in CALENDAR_REGS.into_iter().zip(values) { cmos[reg] = bcd(value); }
        cmos[6] = ((seconds / 86400 + 4) % 7 + 1) as u8; // Sunday=1
    }
}

/// Cold-only RTC. Private CMOS bytes do not expose host NVRAM and cannot
/// remap guest RAM. Mutating them does not provide warm-reset/persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcRtc {
    cmos: [u8; 128],
    index: u8,
    nmi_mask: bool,
    last_ns: u64,
    anchor_ns: u64,
    anchor_seconds: u64,
    current_seconds: u64,
    set: bool,
    calendar_dirty: u8,
    update_pending: bool,
}

impl PcRtc {
    /// `elapsed_ns=0` corresponds to this UTC epoch. The board supplies
    /// Q35FirmwareLayout::cmos_memory(); the device has no arch dependency.
    pub fn new(unix_seconds: u64, cmos_memory: [(u8, u8); 5]) -> Result<Self> {
        let calendar = Calendar::from_seconds(unix_seconds)?;
        if cmos_memory.map(|(reg, _)| reg) != CMOS_MEMORY_REGS { return Err(RtcError::MemorySeed); }
        let mut cmos = [0; 128];
        calendar.write_cmos(&mut cmos, unix_seconds);
        for (reg, value) in cmos_memory { cmos[usize::from(reg)] = value; }
        Ok(Self { cmos, index: 0, nmi_mask: false, last_ns: 0, anchor_ns: 0,
            anchor_seconds: unix_seconds, current_seconds: unix_seconds,
            set: false, calendar_dirty: 0, update_pending: false })
    }

    fn access(port: u16, len: usize) -> Result<bool> {
        let end = usize::from(port).checked_add(len).ok_or(RtcError::Access)?;
        if end > 65536 { return Err(RtcError::Access); }
        if port == 0x70 || port == 0x71 {
            return if len == 1 { Ok(true) } else { Err(RtcError::Access) };
        }
        if usize::from(port) < 0x70 && end > 0x70 { return Err(RtcError::Access); }
        Ok(false)
    }

    fn advance(&mut self, now: u64) -> Result<()> {
        if now < self.last_ns { return Err(RtcError::Clock); }
        if !self.set {
            let elapsed = now.checked_sub(self.anchor_ns).ok_or(RtcError::Clock)?;
            let seconds = self.anchor_seconds.checked_add(elapsed / SECOND).ok_or(RtcError::Clock)?;
            let calendar = Calendar::from_seconds(seconds)?;
            self.update_pending |= seconds != self.current_seconds;
            self.current_seconds = seconds;
            calendar.write_cmos(&mut self.cmos, seconds);
        }
        self.last_ns = now;
        Ok(())
    }

    /// Returns false outside this device. Malformed/rejected accesses do not
    /// change the data buffer, clock, calendar staging or any other state.
    pub fn pio_read(&mut self, port: u16, data: &mut [u8], elapsed_ns: u64) -> Result<bool> {
        if !Self::access(port, data.len())? { return Ok(false); }
        let mut next = self.clone();
        next.advance(elapsed_ns)?;
        let value = if port == 0x70 { 0xff } else {
            match next.index {
                0x0a => {
                    let uip = !next.set && (elapsed_ns - next.anchor_ns) % SECOND >= SECOND - UIP_HOLD;
                    0x26 | if uip { 0x80 } else { 0 }
                }
                0x0b => 0x02 | if next.set { 0x80 } else { 0 },
                0x0c => {
                    let value = if next.update_pending { 0x10 } else { 0 };
                    next.update_pending = false;
                    value // no AF, PF or IRQF: active event modes are unsupported
                }
                0x0d => 0x80, // VRT; writes to this read-only register are ignored
                reg => next.cmos[usize::from(reg)],
            }
        };
        *self = next;
        data[0] = value;
        Ok(true)
    }

    /// Errors remain visible to the worker-owned checked board route. This
    /// deliberately implements no infallible BusDevice compatibility adapter.
    pub fn pio_write(&mut self, port: u16, data: &[u8], elapsed_ns: u64) -> Result<bool> {
        if !Self::access(port, data.len())? { return Ok(false); }
        let mut next = self.clone();
        next.advance(elapsed_ns)?;
        let value = data[0];
        if port == 0x70 {
            next.index = value & 0x7f;
            next.nmi_mask = value & 0x80 != 0; // retained only; no NMI injection claim
        } else {
            next.write_register(value, elapsed_ns)?;
        }
        *self = next;
        Ok(true)
    }

    fn write_register(&mut self, value: u8, now: u64) -> Result<()> {
        let reg = usize::from(self.index);
        if let Some(field) = CALENDAR_REGS.iter().position(|r| *r == reg) {
            if !self.set { return Err(RtcError::CalendarWrite); }
            // Validate the complete candidate on SET clear, allowing repair of
            // temporarily inconsistent dates without partial calendar commit.
            self.cmos[reg] = value;
            self.calendar_dirty |= 1 << field;
            return Ok(());
        }
        match reg {
            6 => return Err(RtcError::CalendarWrite),
            0x0a if value & 0x7f != 0x26 => return Err(RtcError::Control),
            0x0a | 0x0c | 0x0d => {} // UIP, C and D are read-only
            0x0b => {
                if !matches!(value, 0x02 | 0x82) { return Err(RtcError::Control); }
                let set = value & 0x80 != 0;
                if set != self.set {
                    if set {
                        self.calendar_dirty = 0; // snapshot already taken by advance
                    } else {
                        if self.calendar_dirty != 0 {
                            if self.calendar_dirty != 0x3f { return Err(RtcError::IncompleteCalendar); }
                            self.current_seconds = Calendar::read_cmos(&self.cmos)?.seconds()?;
                        }
                        // Alarm-only SET cycles preserve the frozen calendar.
                        self.anchor_seconds = self.current_seconds;
                        self.anchor_ns = now;
                        Calendar::from_seconds(self.current_seconds)?.write_cmos(&mut self.cmos, self.current_seconds);
                        self.calendar_dirty = 0;
                    }
                    self.set = set;
                }
                // Repeated SET must preserve staging. Repeated running mode
                // must preserve the epoch and fractional-second progress.
            }
            _ => self.cmos[reg] = value, // includes disabled alarm and private CMOS
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const EPOCH: u64 = 1_794_096_000; // 2026-11-08 UTC
    const MEMORY: [(u8, u8); 5] = [(0x34, 0), (0x35, 0x1f), (0x5b, 0), (0x5c, 0), (0x5d, 0)];
    fn rtc() -> PcRtc { PcRtc::new(EPOCH, MEMORY).unwrap() }
    fn select(rtc: &mut PcRtc, reg: u8, now: u64) { assert!(rtc.pio_write(0x70, &[reg], now).unwrap()); }
    fn read(rtc: &mut PcRtc, reg: u8, now: u64) -> u8 {
        select(rtc, reg, now); let mut byte = [0]; rtc.pio_read(0x71, &mut byte, now).unwrap(); byte[0]
    }
    fn write(rtc: &mut PcRtc, reg: u8, value: u8, now: u64) -> Result<bool> {
        select(rtc, reg, now); rtc.pio_write(0x71, &[value], now)
    }
    fn stage(rtc: &mut PcRtc, fields: [u8; 6], now: u64) {
        for (reg, value) in CALENDAR_REGS.into_iter().zip(fields) { write(rtc, reg as u8, value, now).unwrap(); }
    }

    #[test]
    fn epoch_and_seed_are_checked_without_host_clock() {
        assert_eq!(Calendar::from_seconds(EPOCH).unwrap(), Calendar { year: 2026, month: 11, day: 8, hour: 0, minute: 0, second: 0 });
        for seconds in [0, MIN_SECONDS - 1, END_SECONDS, u64::MAX] { assert!(PcRtc::new(seconds, MEMORY).is_err()); }
        let mut wrong = MEMORY; wrong[4].0 = 0x34;
        assert_eq!(PcRtc::new(EPOCH, wrong), Err(RtcError::MemorySeed));
        for year in 1998..=2097 {
            let c = Calendar { year, month: 12, day: 31, hour: 23, minute: 59, second: 59 };
            assert_eq!(Calendar::from_seconds(c.seconds().unwrap()).unwrap(), c);
            let mut bytes = [0; 128]; c.write_cmos(&mut bytes, c.seconds().unwrap());
            assert_eq!(Calendar::read_cmos(&bytes).unwrap(), c);
        }
    }

    #[test]
    fn elapsed_uip_update_flags_and_readonly_registers_follow_time() {
        let mut r = rtc();
        assert_eq!(read(&mut r, 0x0a, SECOND - UIP_HOLD - 1), 0x26);
        assert_eq!(read(&mut r, 0x0a, SECOND - UIP_HOLD), 0xa6);
        assert_eq!(read(&mut r, 0, SECOND - 1), 0);
        assert_eq!(read(&mut r, 0x0a, SECOND), 0x26);
        assert_eq!(read(&mut r, 0, SECOND), 1);
        assert_eq!(read(&mut r, 0, SECOND), 1); // not a read counter
        assert_eq!(read(&mut r, 0x0c, 10 * SECOND), 0x10); // coalesce, never IRQF
        assert_eq!(read(&mut r, 0x0c, 10 * SECOND), 0);
        write(&mut r, 0x0c, 0xff, 10 * SECOND).unwrap();
        write(&mut r, 0x0d, 0, 10 * SECOND).unwrap();
        write(&mut r, 0x0a, 0xa6, 10 * SECOND).unwrap();
        assert_eq!(read(&mut r, 0x0c, 10 * SECOND), 0);
        assert_eq!(read(&mut r, 0x0d, 10 * SECOND), 0x80);
    }

    #[test]
    fn rollover_is_real_and_elapsed_bounds_are_atomic() {
        for (start, expected) in [
            (Calendar { year: 2000, month: 2, day: 28, hour: 23, minute: 59, second: 59 }, [0x29, 2, 0]),
            (Calendar { year: 2025, month: 2, day: 28, hour: 23, minute: 59, second: 59 }, [1, 3, 0x25]),
            (Calendar { year: 1999, month: 12, day: 31, hour: 23, minute: 59, second: 59 }, [1, 1, 0]),
        ] {
            let mut r = PcRtc::new(start.seconds().unwrap(), MEMORY).unwrap();
            assert_eq!([read(&mut r, 7, SECOND), read(&mut r, 8, SECOND), read(&mut r, 9, SECOND)], expected);
            assert_eq!(read(&mut r, 0, SECOND), 0);
        }
        let mut r = PcRtc::new(END_SECONDS - 1, MEMORY).unwrap();
        let old = r.clone(); let mut output = [0x5a];
        assert_eq!(r.pio_read(0x71, &mut output, SECOND), Err(RtcError::Clock));
        assert_eq!(r, old); assert_eq!(output, [0x5a]);
        assert_eq!(r.pio_write(0x70, &[4], u64::MAX), Err(RtcError::Clock));
        assert_eq!(r, old);
        r.pio_write(0x70, &[4], 1).unwrap(); let old = r.clone();
        assert_eq!(r.pio_read(0x71, &mut output, 0), Err(RtcError::Clock));
        assert_eq!(r, old); assert_eq!(output, [0x5a]);
    }

    #[test]
    fn set_is_idempotent_and_alarm_only_cycle_needs_no_calendar_fields() {
        let mut r = rtc();
        write(&mut r, 0x0b, 0x02, SECOND / 2).unwrap();
        assert_eq!(read(&mut r, 0, SECOND), 1); // repeated02 did not lose half a second
        write(&mut r, 0x0b, 0x82, SECOND).unwrap();
        write(&mut r, 0, 0x12, 2 * SECOND).unwrap();
        write(&mut r, 0x0b, 0x82, 3 * SECOND).unwrap();
        assert_eq!(read(&mut r, 0, 20 * SECOND), 0x12);
        assert_eq!(r.calendar_dirty, 1); // repeated82 retained staging
        assert_eq!(read(&mut r, 0x0a, 20 * SECOND + SECOND - 1), 0x26);
        stage(&mut r, [0x12, 0x34, 0x23, 0x29, 2, 0], 21 * SECOND);
        write(&mut r, 0x0b, 0x02, 21 * SECOND).unwrap();
        assert_eq!(read(&mut r, 0, 22 * SECOND), 0x13);
        write(&mut r, 0x0b, 0x82, 22 * SECOND).unwrap();
        for reg in [1, 3, 5] { write(&mut r, reg, 0, 30 * SECOND).unwrap(); }
        write(&mut r, 0x0b, 0x02, 40 * SECOND).unwrap();
        assert_eq!(read(&mut r, 0, 40 * SECOND), 0x13); // paused calendar preserved
        assert_eq!(read(&mut r, 0, 41 * SECOND), 0x14);
    }

    #[test]
    fn invalid_or_incomplete_set_candidate_can_be_repaired_without_partial_commit() {
        let mut r = rtc(); write(&mut r, 0x0b, 0x82, 0).unwrap();
        write(&mut r, 0, 1, 0).unwrap(); select(&mut r, 0x0b, 0);
        let old = r.clone();
        assert_eq!(r.pio_write(0x71, &[2], 5 * SECOND), Err(RtcError::IncompleteCalendar));
        assert_eq!(r, old); // even last_ns was not consumed
        for fields in [[0xfa, 0, 0, 1, 1, 0x26], [0, 0, 0x24, 1, 1, 0x26],
            [0, 0, 0, 0x30, 2, 0], [0, 0, 0, 0x29, 2, 0x25], [0, 0, 0, 1, 0, 0x26]] {
            stage(&mut r, fields, SECOND); select(&mut r, 0x0b, SECOND); let old = r.clone();
            assert_eq!(r.pio_write(0x71, &[2], 3 * SECOND), Err(RtcError::Calendar));
            assert_eq!(r, old);
        }
        stage(&mut r, [0, 0, 0, 0x29, 2, 0], SECOND);
        write(&mut r, 0x0b, 2, 2 * SECOND).unwrap();
        assert_eq!(read(&mut r, 7, 2 * SECOND), 0x29);
        assert_eq!(read(&mut r, 0, 3 * SECOND), 1);
    }

    #[test]
    fn rejected_accesses_controls_and_active_irqs_leave_everything_unchanged() {
        let mut r = rtc(); let mut bytes = [0x5a; 2];
        for (port, len) in [(0x6f, 2), (0x70, 0), (0x70, 2), (0x71, 2), (0xffff, 2)] {
            let old = r.clone();
            assert_eq!(r.pio_read(port, &mut bytes[..len], 50), Err(RtcError::Access));
            assert_eq!(r, old); assert_eq!(bytes, [0x5a; 2]);
            assert_eq!(r.pio_write(port, &bytes[..len], 50), Err(RtcError::Access)); assert_eq!(r, old);
        }
        assert!(!r.pio_read(0x72, &mut bytes[..1], u64::MAX).unwrap());
        assert!(!r.pio_write(0x80, &[1], u64::MAX).unwrap()); assert_eq!(r, rtc());
        for (reg, values) in [(0x0b, vec![0x12, 0x22, 0x42, 0x72, 0x86, 0x80, 0x03, 0x0a]),
            (0x0a, vec![0, 0x25, 0x36, 0x66])] {
            select(&mut r, reg, 0);
            for value in values {
                let old = r.clone(); assert_eq!(r.pio_write(0x71, &[value], SECOND), Err(RtcError::Control)); assert_eq!(r, old);
            }
        }
        for reg in [0, 2, 4, 6, 7, 8, 9] {
            select(&mut r, reg, 0); let old = r.clone();
            assert_eq!(r.pio_write(0x71, &[1], SECOND), Err(RtcError::CalendarWrite)); assert_eq!(r, old);
        }
    }

    #[test]
    fn private_cmos_and_index_port_match_the_selected_read_preserve_write_sequence() {
        let mut r = rtc(); let mut index = [0];
        r.pio_read(0x70, &mut index, 0).unwrap(); assert_eq!(index, [0xff]);
        r.pio_write(0x70, &[0x35 | (index[0] & 0x80)], 0).unwrap();
        assert_eq!(r.index, 0x35); assert!(r.nmi_mask);
        let mut data = [0]; r.pio_read(0x71, &mut data, 0).unwrap(); assert_eq!(data, [0x1f]);
        for (reg, value) in [(0x0f, 0x0a), (0x35, 0x12), (0x7f, 0xa5), (1, 0xc0), (3, 0x10), (5, 0x20)] {
            write(&mut r, reg, value, 0).unwrap(); assert_eq!(read(&mut r, reg, 0), value);
        }
        assert!(!r.nmi_mask);
        for reg in 0..128 { let _ = read(&mut r, reg, 0); } // firmware CMOS dump
        assert_eq!(read(&mut r, 0x0b, 0), 2); assert_eq!(read(&mut r, 0x0c, 0), 0);
    }
}
