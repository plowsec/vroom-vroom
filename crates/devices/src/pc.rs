//! Minimal PC firmware ports: CMOS/RTC, ACPI PM timer, reset and 8042 probe.
//! Input is provided by xHCI; the FADT does not advertise an 8042 keyboard.
use crate::{get_le, put_le, BusDevice};
use serde::{Deserialize, Serialize};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use vroom_snapshot::{decode, encode, Snapshottable};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Reset,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
struct State {
    index: u8,
    cmos: Vec<u8>,
    pm_status: u16,
    pm_enable: u16,
    pm_control: u16,
    ticks: u64,
    ps2_reply: Vec<u8>,
    ps2_command: u8,
    ps2_config: u8,
    port92: u8,
}

pub struct PcPorts {
    state: State,
    epoch: Instant,
    event: Box<dyn Fn(Event) + Send>,
}
impl std::fmt::Debug for PcPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PcPorts").finish_non_exhaustive()
    }
}
impl PcPorts {
    pub fn new(ram: u64, event: Box<dyn Fn(Event) + Send>) -> Self {
        let mut cmos = vec![0u8; 128];
        cmos[0x0a] = 0x26;
        cmos[0x0b] = 2;
        cmos[0x0d] = 0x80;
        cmos[0x14] = 6;
        cmos[0x15] = 0x80;
        cmos[0x16] = 2; // 640 KiB
        let ext = ((ram.min(0xc0000000) >> 20).saturating_sub(1) * 1024).min(65535) as u16;
        cmos[0x17..0x19].copy_from_slice(&ext.to_le_bytes());
        cmos[0x30..0x32].copy_from_slice(&ext.to_le_bytes());
        let above16 = (ram.min(0xc0000000).saturating_sub(16 << 20) >> 16) as u16;
        cmos[0x34..0x36].copy_from_slice(&above16.to_le_bytes());
        let high = (ram.saturating_sub(0xc0000000) >> 16) as u32;
        cmos[0x5b..0x5e].copy_from_slice(&high.to_le_bytes()[..3]);
        cmos[0x3d] = 0x12;
        Self {
            state: State {
                index: 0,
                cmos,
                pm_status: 0,
                pm_enable: 0,
                pm_control: 1,
                ticks: 0,
                ps2_reply: Vec::new(),
                ps2_command: 0,
                ps2_config: 0x30,
                port92: 2,
            },
            epoch: Instant::now(),
            event,
        }
    }
    fn ticks(&self) -> u64 {
        self.state
            .ticks
            .wrapping_add((self.epoch.elapsed().as_nanos() * 3_579_545 / 1_000_000_000) as u64)
    }
    fn rtc(&self, index: usize) -> u8 {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let days = seconds / 86400;
        // Gregorian civil date from the Unix day number (400-year eras).
        let z = days as i64 + 719468;
        let era = z / 146097;
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let mut year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = mp + if mp < 10 { 3 } else { -9 };
        year += i64::from(month <= 2);
        let value = match index {
            0 => (seconds % 60) as u8,
            2 => (seconds / 60 % 60) as u8,
            4 => (seconds / 3600 % 24) as u8,
            6 => ((days + 4) % 7 + 1) as u8,
            7 => day as u8,
            8 => month as u8,
            9 => (year % 100) as u8,
            0x32 => (year / 100) as u8,
            _ => return self.state.cmos[index],
        };
        if self.state.cmos[0xb] & 4 != 0 {
            value
        } else {
            (value / 10) * 16 + value % 10
        }
    }
}
impl BusDevice for PcPorts {
    // Absolute port numbers: mapped in disjoint ranges by the VMM.
    fn read(&mut self, port: u64, data: &mut [u8]) {
        let v = match port {
            0x70 => self.state.index as u64,
            0x71 => {
                let i = self.state.index as usize & 127;
                match i {
                    0x0c => 0,    // no pending RTC interrupt
                    0x0d => 0x80, // valid RAM/time (VRT), hardware-owned
                    _ => self.rtc(i) as u64,
                }
            }
            0xb000 => self.state.pm_status as u64,
            0xb002 => self.state.pm_enable as u64,
            0xb004 => self.state.pm_control as u64,
            0xb008 => self.ticks() as u32 as u64,
            0x60 => {
                if self.state.ps2_reply.is_empty() {
                    0
                } else {
                    self.state.ps2_reply.remove(0) as u64
                }
            }
            0x64 => 4 | u64::from(!self.state.ps2_reply.is_empty()),
            0x92 => self.state.port92 as u64,
            _ => 0,
        };
        put_le(data, v);
    }
    fn write(&mut self, port: u64, data: &[u8]) {
        let v = get_le(data);
        match port {
            0x70 => self.state.index = v as u8,
            0x71 => {
                let index = self.state.index as usize & 127;
                // MC146818 status C/D are read-only. OVMF writes zero
                // to D during initialization; accepting that write
                // clears VRT and makes its GetTime return DEVICE_ERROR.
                if !matches!(index, 0x0c | 0x0d) {
                    self.state.cmos[index] = v as u8;
                }
            }
            0xb000 => self.state.pm_status &= !(v as u16),
            0xb002 => self.state.pm_enable = v as u16,
            0xb004 => {
                self.state.pm_control = v as u16 | 1;
                if v & (1 << 13) != 0 && (v >> 10) & 7 == 0 {
                    (self.event)(Event::Shutdown);
                }
            }
            0xcf9 if v & 4 != 0 => (self.event)(Event::Reset),
            0x92 => {
                self.state.port92 = v as u8 & !1;
                if v & 1 != 0 {
                    (self.event)(Event::Reset);
                }
            }
            0x64 => {
                self.state.ps2_command = v as u8;
                self.state.ps2_reply = match v {
                    0x20 => vec![self.state.ps2_config],
                    0xaa => vec![0x55],
                    0xab | 0xa9 => vec![0],
                    0xd0 => vec![3],
                    0xfe => {
                        (self.event)(Event::Reset);
                        vec![]
                    }
                    _ => vec![],
                };
            }
            0x60 => match self.state.ps2_command {
                0x60 => {
                    self.state.ps2_config = v as u8;
                    self.state.ps2_command = 0;
                }
                0xd1 => {
                    if v & 1 == 0 {
                        (self.event)(Event::Reset);
                    }
                    self.state.ps2_command = 0;
                }
                // No attached PS/2 keyboard: do not pretend input is available.
                _ => {}
            },
            _ => {}
        }
    }
}
impl Snapshottable for PcPorts {
    fn snapshot_id(&self) -> String {
        "pc-ports".into()
    }
    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        self.state.ticks = self.ticks();
        self.epoch = Instant::now();
        encode(&self.state)
    }
    fn restore(&mut self, data: &[u8]) -> vroom_snapshot::Result<()> {
        let s: State = decode(data)?;
        if s.cmos.len() != 128 || s.ps2_reply.len() > 16 {
            return Err(vroom_snapshot::Error::Invalid("PC port state".into()));
        }
        self.state = s;
        self.epoch = Instant::now();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn ovmf_rtc_initialization_preserves_read_only_status() {
        let mut p = PcPorts::new(512 << 20, Box::new(|_| {}));
        let mut byte = [0];
        // PcRtcInit writes zero to register D. The battery-valid bit is
        // hardware-owned: clearing it makes EFI GetTime fail, which
        // Windows reports as BlInitializeLibrary failed 0xc0000185.
        for nmi in [0, 0x80] {
            p.write(0x70, &[nmi | 0x0d]);
            for value in [0, 0x7f, 0xff] {
                p.write(0x71, &[value]);
                p.read(0x71, &mut byte);
                assert_eq!(byte[0], 0x80);
            }
            p.write(0x70, &[nmi | 0x0c]);
            p.write(0x71, &[0xff]);
            p.read(0x71, &mut byte);
            assert_eq!(byte[0], 0);
        }
        let saved = p.save().unwrap();
        p.restore(&saved).unwrap();
        p.write(0x70, &[0x0d]);
        p.read(0x71, &mut byte);
        assert_eq!(byte[0], 0x80);
    }

    #[test]
    fn cmos_memory_timer_and_power_events() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut p = PcPorts::new(4 << 30, Box::new(move |e| sink.lock().unwrap().push(e)));
        let mut byte = [0];
        p.write(0x70, &[0x34]);
        p.read(0x71, &mut byte);
        assert_eq!(byte[0], 0);
        p.write(0x70, &[0x35]);
        p.read(0x71, &mut byte);
        assert_eq!(byte[0], 0xbf);
        p.write(0x70, &[0x5d]);
        p.read(0x71, &mut byte);
        assert_eq!(byte[0], 0);
        p.write(0x70, &[0x5c]);
        p.read(0x71, &mut byte);
        assert_eq!(byte[0], 0x40);
        let snapshot = p.save().unwrap();
        p.write(0x70, &[0x10]);
        p.write(0x71, &[0xab]);
        p.restore(&snapshot).unwrap();
        p.write(0x70, &[0x10]);
        p.read(0x71, &mut byte);
        assert_eq!(byte[0], 0);
        let mut timer = [0; 4];
        p.read(0xb008, &mut timer);
        let before = u32::from_le_bytes(timer);
        for _ in 0..1000 {
            p.read(0xb008, &mut timer);
        }
        assert!(u32::from_le_bytes(timer) > before);
        p.write(0xb004, &0x2000u16.to_le_bytes());
        p.write(0x64, &[0xfe]);
        assert_eq!(*events.lock().unwrap(), vec![Event::Shutdown, Event::Reset]);
    }
}
