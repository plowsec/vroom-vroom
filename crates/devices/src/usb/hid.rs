//! HID devices: boot keyboard and absolute-pointer tablet.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::keymap::{hid_usage, modifier_bit};
use super::{ControlResult, Descriptors, Setup, StdState, UsbDevice};

const MAX_QUEUED: usize = 64;
/// pid.codes test vendor; fine for virtual hardware.
const VENDOR: u16 = 0x1209;

const KBD_REPORT: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
    0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01, 0x05, 0x08, 0x19, 0x01,
    0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x26, 0xff,
    0x00, 0x05, 0x07, 0x19, 0x00, 0x2a, 0xff, 0x00, 0x81, 0x00, 0xc0,
];

const TABLET_REPORT: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00,
    0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30,
    0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x7f, 0x35, 0x00, 0x46, 0xff, 0x7f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02,
    0x05, 0x01, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06,
    0xc0, 0xc0,
];

/// Input event for HID devices (evdev codes, like virtio-input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidEvent {
    Key { code: u16, down: bool },
    Pointer { x: u16, y: u16 },
    Button { code: u16, down: bool },
    Wheel { dy: i8 },
}

fn descriptors(product: u16, name: &'static str, protocol: u8, report: &[u8], report_len: usize) -> Descriptors {
    let mut device = vec![18, 1, 0x00, 0x02, 0, 0, 0, 64];
    device.extend_from_slice(&VENDOR.to_le_bytes());
    device.extend_from_slice(&product.to_le_bytes());
    device.extend_from_slice(&[0x00, 0x01, 1, 2, 3, 1]);
    let subclass = if protocol == 0 { 0 } else { 1 };
    let mut config = vec![9, 2, 34, 0, 1, 1, 0, 0xa0, 50];
    config.extend_from_slice(&[9, 4, 0, 0, 1, 3, subclass, protocol, 0]);
    config.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22]);
    config.extend_from_slice(&(report_len as u16).to_le_bytes());
    config.extend_from_slice(&[7, 5, 0x81, 3, 8, 0, 10]);
    Descriptors { device, config, strings: vec!["Vroom", name, "1"], hid_report: report.to_vec() }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct KbdState {
    std: StdState,
    modifiers: u8,
    keys: Vec<u8>,
}

#[derive(Debug)]
pub struct Keyboard {
    desc: Descriptors,
    st: KbdState,
    queue: VecDeque<Vec<u8>>,
}

impl Default for Keyboard {
    fn default() -> Self {
        Keyboard { desc: descriptors(0x0001, "Vroom USB Keyboard", 1, KBD_REPORT, KBD_REPORT.len()), st: KbdState::default(), queue: VecDeque::new() }
    }
}

impl Keyboard {
    fn report(&self) -> Vec<u8> {
        let mut r = vec![self.st.modifiers, 0, 0, 0, 0, 0, 0, 0];
        if self.st.keys.len() > 6 {
            r[2..8].fill(0x01); // ErrorRollOver
        } else {
            for (i, k) in self.st.keys.iter().enumerate() {
                r[2 + i] = *k;
            }
        }
        r
    }

    pub fn event(&mut self, code: u16, down: bool) {
        if let Some(bit) = modifier_bit(code) {
            if down {
                self.st.modifiers |= bit;
            } else {
                self.st.modifiers &= !bit;
            }
        } else if let Some(u) = hid_usage(code) {
            if down && !self.st.keys.contains(&u) {
                if self.st.keys.len() < 16 {
                    self.st.keys.push(u);
                }
            } else if !down {
                self.st.keys.retain(|&k| k != u);
            }
        } else {
            return;
        }
        if self.queue.len() < MAX_QUEUED {
            self.queue.push_back(self.report());
        }
    }
}

impl UsbDevice for Keyboard {
    fn control(&mut self, s: Setup, out: &[u8]) -> ControlResult {
        if let Some(r) = self.desc.handle(&mut self.st.std, s, out) {
            return r;
        }
        match s.request {
            0x01 => Ok(self.report()), // GET_REPORT
            0x09 => Ok(vec![]),        // SET_REPORT (LEDs): accepted, ignored
            _ => Err(super::Stall),
        }
    }

    fn poll_in(&mut self, ep: u8) -> Option<Vec<u8>> {
        (ep == 1).then(|| self.queue.pop_front()).flatten()
    }

    fn reset(&mut self) {
        self.st = KbdState::default();
        self.queue.clear();
    }

    fn save(&self) -> Vec<u8> {
        serde_json::to_vec(&self.st).unwrap_or_default()
    }

    fn restore(&mut self, data: &[u8]) {
        if let Ok(mut s) = serde_json::from_slice::<KbdState>(data) {
            s.keys.truncate(16);
            self.st = s;
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TabletState {
    std: StdState,
    buttons: u8,
    x: u16,
    y: u16,
}

#[derive(Debug)]
pub struct Tablet {
    desc: Descriptors,
    st: TabletState,
    queue: VecDeque<Vec<u8>>,
}

impl Default for Tablet {
    fn default() -> Self {
        Tablet {
            desc: descriptors(0x0002, "Vroom USB Tablet", 0, TABLET_REPORT, TABLET_REPORT.len()),
            st: TabletState::default(),
            queue: VecDeque::new(),
        }
    }
}

impl Tablet {
    fn report(&self, wheel: i8) -> Vec<u8> {
        let mut r = vec![self.st.buttons];
        r.extend_from_slice(&self.st.x.to_le_bytes());
        r.extend_from_slice(&self.st.y.to_le_bytes());
        r.push(wheel as u8);
        r
    }

    pub fn event(&mut self, e: HidEvent) {
        let mut wheel = 0i8;
        match e {
            HidEvent::Pointer { x, y } => {
                self.st.x = x.min(0x7fff);
                self.st.y = y.min(0x7fff);
                // Coalesce plain moves: replace a queued move with the newest.
                let fresh = self.report(0);
                if let Some(last) = self.queue.back_mut() {
                    if last[0] == self.st.buttons && last[5] == 0 {
                        *last = fresh;
                        return;
                    }
                }
            }
            HidEvent::Button { code, down } => {
                let bit = match code {
                    0x110 => 1,
                    0x111 => 2,
                    0x112 => 4,
                    _ => return,
                };
                if down {
                    self.st.buttons |= bit;
                } else {
                    self.st.buttons &= !bit;
                }
            }
            HidEvent::Wheel { dy } => wheel = dy,
            HidEvent::Key { .. } => return,
        }
        if self.queue.len() < MAX_QUEUED {
            self.queue.push_back(self.report(wheel));
        }
    }
}

impl UsbDevice for Tablet {
    fn control(&mut self, s: Setup, out: &[u8]) -> ControlResult {
        if let Some(r) = self.desc.handle(&mut self.st.std, s, out) {
            return r;
        }
        match s.request {
            0x01 => Ok(self.report(0)),
            0x09 => Ok(vec![]),
            _ => Err(super::Stall),
        }
    }

    fn poll_in(&mut self, ep: u8) -> Option<Vec<u8>> {
        (ep == 1).then(|| self.queue.pop_front()).flatten()
    }

    fn reset(&mut self) {
        self.st = TabletState::default();
        self.queue.clear();
    }

    fn save(&self) -> Vec<u8> {
        serde_json::to_vec(&self.st).unwrap_or_default()
    }

    fn restore(&mut self, data: &[u8]) {
        if let Ok(s) = serde_json::from_slice::<TabletState>(data) {
            self.st = s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_desc(d: &mut dyn UsbDevice, ty: u8, len: u16) -> Vec<u8> {
        d.control(Setup { request_type: 0x80, request: 6, value: (ty as u16) << 8, index: 0, length: len }, &[]).unwrap()
    }

    #[test]
    fn descriptors_are_consistent() {
        let mut k = Keyboard::default();
        let dev = get_desc(&mut k, 1, 64);
        assert_eq!((dev.len(), dev[0], dev[7]), (18, 18, 64));
        let cfg = get_desc(&mut k, 2, 255);
        assert_eq!(cfg.len(), u16::from_le_bytes([cfg[2], cfg[3]]) as usize);
        let hid_len = u16::from_le_bytes([cfg[25], cfg[26]]) as usize;
        let rd = k.control(Setup { request_type: 0x81, request: 6, value: 0x2200, index: 0, length: 255 }, &[]).unwrap();
        assert_eq!(rd.len(), hid_len);
        assert_eq!(get_desc(&mut k, 2, 9).len(), 9); // truncated to wLength
    }

    #[test]
    fn keyboard_reports() {
        let mut k = Keyboard::default();
        k.event(42, true); // LShift
        k.event(30, true); // a
        k.poll_in(1);
        assert_eq!(k.poll_in(1).unwrap(), vec![0x02, 0, 0x04, 0, 0, 0, 0, 0]);
        k.event(30, false);
        assert_eq!(k.poll_in(1).unwrap()[2], 0);
        assert!(k.poll_in(2).is_none());
    }

    #[test]
    fn tablet_coalesces_moves() {
        let mut t = Tablet::default();
        t.event(HidEvent::Pointer { x: 1, y: 1 });
        t.event(HidEvent::Pointer { x: 100, y: 200 });
        t.event(HidEvent::Button { code: 0x110, down: true });
        assert_eq!(t.poll_in(1).unwrap(), vec![0, 100, 0, 200, 0, 0]);
        assert_eq!(t.poll_in(1).unwrap()[0], 1);
        assert!(t.poll_in(1).is_none());
    }
}
