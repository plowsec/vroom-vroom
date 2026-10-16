//! USB: device-side model (descriptors, standard requests) and the xHCI
//! host controller. Devices: HID boot keyboard and absolute tablet, a
//! camera (UVC).

pub mod hid;
mod keymap;
pub mod uvc;
pub mod xhci;

use serde::{Deserialize, Serialize};

/// 8-byte SETUP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    pub fn parse(raw: u64) -> Setup {
        let b = raw.to_le_bytes();
        Setup {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        }
    }

    pub fn device_to_host(&self) -> bool {
        self.request_type & 0x80 != 0
    }
}

/// Control request outcome: data for the IN stage, or a STALL.
pub type ControlResult = Result<Vec<u8>, Stall>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall;

/// Port speed as xHCI encodes it in PORTSC.
pub const SPEED_FULL: u8 = 1;
pub const SPEED_HIGH: u8 = 3;

/// A USB function plugged into a root port.
pub trait UsbDevice: Send {
    fn speed(&self) -> u8 {
        SPEED_FULL
    }
    /// EP0 control transfer. `out` holds the OUT data stage, if any.
    fn control(&mut self, setup: Setup, out: &[u8]) -> ControlResult;
    /// Next report for interrupt IN endpoint `ep` (1-based number).
    fn poll_in(&mut self, ep: u8) -> Option<Vec<u8>>;
    /// Next data for IN endpoint `ep`, at most `max` bytes (the transfer
    /// the guest queued). Bulk endpoints use this.
    fn poll_in_max(&mut self, ep: u8, max: usize) -> Option<Vec<u8>> {
        let _ = max;
        self.poll_in(ep)
    }
    fn reset(&mut self);
    fn save(&self) -> Vec<u8>;
    fn restore(&mut self, data: &[u8]);
}

/// Descriptors and the standard-request plumbing shared by our devices.
#[derive(Debug, Clone)]
pub struct Descriptors {
    pub device: Vec<u8>,
    pub config: Vec<u8>,
    pub strings: Vec<String>,
    pub hid_report: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StdState {
    pub configured: u8,
    pub protocol: u8,
    pub idle: u8,
}

const GET_STATUS: u8 = 0;
const CLEAR_FEATURE: u8 = 1;
const SET_FEATURE: u8 = 3;
const GET_DESCRIPTOR: u8 = 6;
const GET_CONFIGURATION: u8 = 8;
const SET_CONFIGURATION: u8 = 9;
const GET_INTERFACE: u8 = 10;
const SET_INTERFACE: u8 = 11;

const HID_GET_IDLE: u8 = 0x02;
const HID_GET_PROTOCOL: u8 = 0x03;
const HID_SET_REPORT: u8 = 0x09;
const HID_SET_IDLE: u8 = 0x0a;
const HID_SET_PROTOCOL: u8 = 0x0b;

fn string_desc(s: &str) -> Vec<u8> {
    let mut v = vec![0, 3];
    for u in s.encode_utf16() {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v[0] = v.len() as u8;
    v
}

impl Descriptors {
    /// Standard + HID class requests. Returns None if the caller must
    /// handle it (e.g. GET_REPORT, SET_REPORT data).
    pub fn handle(&self, st: &mut StdState, s: Setup, out: &[u8]) -> Option<ControlResult> {
        let class = (s.request_type >> 5) & 3 == 1;
        let r = if class {
            match s.request {
                HID_GET_IDLE => Ok(vec![st.idle]),
                HID_GET_PROTOCOL => Ok(vec![st.protocol]),
                HID_SET_IDLE => {
                    st.idle = (s.value >> 8) as u8;
                    Ok(vec![])
                }
                HID_SET_PROTOCOL => {
                    st.protocol = s.value as u8;
                    Ok(vec![])
                }
                HID_SET_REPORT => return None,
                _ => return None,
            }
        } else {
            match s.request {
                GET_STATUS => Ok(vec![0, 0]),
                CLEAR_FEATURE | SET_FEATURE => Ok(vec![]),
                GET_CONFIGURATION => Ok(vec![st.configured]),
                SET_CONFIGURATION => {
                    st.configured = s.value as u8;
                    Ok(vec![])
                }
                GET_INTERFACE => Ok(vec![0]),
                SET_INTERFACE => Ok(vec![]),
                GET_DESCRIPTOR => self.descriptor(s),
                _ => Err(Stall),
            }
        };
        let _ = out;
        Some(r.map(|mut v| {
            v.truncate(s.length as usize);
            v
        }))
    }

    fn descriptor(&self, s: Setup) -> ControlResult {
        let (ty, idx) = ((s.value >> 8) as u8, s.value as u8);
        match ty {
            1 => Ok(self.device.clone()),
            2 => Ok(self.config.clone()),
            3 if idx == 0 => Ok(vec![4, 3, 0x09, 0x04]),
            3 => self.strings.get(idx as usize - 1).map(|t| string_desc(t)).ok_or(Stall),
            // HID descriptor lives inside the config descriptor at 18.
            0x21 => Ok(self.config[18..27].to_vec()),
            0x22 => Ok(self.hid_report.clone()),
            _ => Err(Stall),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_parse_and_strings() {
        let s = Setup::parse(u64::from_le_bytes([0x80, 6, 0, 1, 0, 0, 18, 0]));
        assert_eq!((s.request, s.value, s.length), (6, 0x100, 18));
        assert!(s.device_to_host());
        assert_eq!(string_desc("Hi"), vec![6, 3, b'H', 0, b'i', 0]);
    }
}
