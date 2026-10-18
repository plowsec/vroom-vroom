//! USB descriptors the helper reads from the device (device-supplied, so
//! parsed defensively: every length checked, counts capped).

use crate::proto::XferType;

/// One endpoint of an interface alternate setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    /// Address: number, bit 7 set for IN.
    pub addr: u8,
    /// None: not a valid type for a data endpoint (control).
    pub kind: Option<XferType>,
    pub max_packet: u16,
    /// Most bytes per service interval: high-bandwidth packets (USB 2)
    /// or the SuperSpeed companion's bytes per interval.
    pub per_interval: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltSetting {
    pub iface: u8,
    pub alt: u8,
    pub class: u8,
    pub endpoints: Vec<Endpoint>,
}

/// A configuration: its value and every interface alternate setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub value: u8,
    pub alts: Vec<AltSetting>,
}

const MAX_ALTS: usize = 256;
const MAX_EPS: usize = 32;

impl Config {
    /// Parse a configuration descriptor with everything after it.
    pub fn parse(b: &[u8]) -> Option<Config> {
        if b.len() < 9 || b[0] < 9 || b[1] != 2 {
            return None;
        }
        let total = (u16::from_le_bytes([b[2], b[3]]) as usize).min(b.len());
        if total < 9 {
            return None;
        }
        let b = &b[..total];
        let mut c = Config { value: b[5], alts: Vec::new() };
        let mut at = b[0] as usize;
        while at + 2 <= b.len() {
            let len = b[at] as usize;
            if len < 2 || at + len > b.len() {
                break;
            }
            let d = &b[at..at + len];
            match d[1] {
                4 if len >= 9 => {
                    if c.alts.len() >= MAX_ALTS {
                        return None;
                    }
                    c.alts.push(AltSetting { iface: d[2], alt: d[3], class: d[5], endpoints: Vec::new() });
                }
                5 if len >= 7 => {
                    let a = c.alts.last_mut()?;
                    if a.endpoints.len() >= MAX_EPS || d[2] & 0x70 != 0 || d[2] & 0x0f == 0 {
                        return None;
                    }
                    let kind = match d[3] & 3 {
                        1 => Some(XferType::Iso),
                        2 => Some(XferType::Bulk),
                        3 => Some(XferType::Interrupt),
                        _ => None,
                    };
                    let mp = u16::from_le_bytes([d[4], d[5]]);
                    let max_packet = mp & 0x7ff;
                    // USB 2 high bandwidth: up to 3 packets per microframe.
                    let per_interval = max_packet as u32 * (1 + ((mp >> 11) & 3).min(2) as u32);
                    a.endpoints.push(Endpoint { addr: d[2], kind, max_packet, per_interval });
                }
                // SuperSpeed endpoint companion: wBytesPerInterval (periodic
                // endpoints); SuperSpeedPlus isochronous: dwBytesPerInterval.
                0x30 if len >= 6 => {
                    if let Some(e) = c.alts.last_mut().and_then(|a| a.endpoints.last_mut()).filter(|e| e.kind != Some(XferType::Bulk)) {
                        e.per_interval = u16::from_le_bytes([d[4], d[5]]) as u32;
                    }
                }
                0x31 if len >= 8 => {
                    if let Some(e) = c.alts.last_mut().and_then(|a| a.endpoints.last_mut()).filter(|e| e.kind == Some(XferType::Iso)) {
                        e.per_interval = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
                    }
                }
                _ => {}
            }
            at += len;
        }
        Some(c)
    }

    /// Interface numbers, each once.
    pub fn interfaces(&self) -> Vec<u8> {
        let mut v: Vec<u8> = self.alts.iter().map(|a| a.iface).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    pub fn alt(&self, iface: u8, alt: u8) -> Option<&AltSetting> {
        self.alts.iter().find(|a| a.iface == iface && a.alt == alt)
    }
}

/// The 18-byte device descriptor fields we use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceDesc {
    pub class: u8,
    pub vendor: u16,
    pub product: u16,
    pub configs: u8,
}

impl DeviceDesc {
    pub fn parse(b: &[u8]) -> Option<DeviceDesc> {
        if b.len() < 18 || b[0] < 18 || b[1] != 1 {
            return None;
        }
        Some(DeviceDesc {
            class: b[4],
            vendor: u16::from_le_bytes([b[8], b[9]]),
            product: u16::from_le_bytes([b[10], b[11]]),
            configs: b[17],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_bounds() {
        let cfg = crate::fake::Acm::config_descriptor(64);
        let c = Config::parse(&cfg).unwrap();
        assert_eq!(c.value, 1);
        assert_eq!(c.interfaces(), vec![0, 1]);
        let data = c.alt(1, 0).unwrap();
        assert_eq!(data.endpoints.len(), 2);
        assert!(data.endpoints.iter().all(|e| e.kind == Some(XferType::Bulk)));
        // Truncated anywhere: never panics, never reads past the end.
        for n in 0..cfg.len() {
            let _ = Config::parse(&cfg[..n]);
        }
        // wTotalLength shorter than the header (found by fuzzing).
        assert!(Config::parse(&[9, 2, 3, 0, 1, 1, 0, 0x80, 50]).is_none());
        // Endpoint before any interface, endpoint 0 in a list.
        assert!(Config::parse(&[9, 2, 16, 0, 1, 1, 0, 0x80, 50, 7, 5, 0x81, 3, 8, 0, 10]).is_none());
        assert!(Config::parse(&[9, 2, 25, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 1, 3, 0, 0, 0, 7, 5, 0x80, 3, 8, 0, 10]).is_none());
    }

    #[test]
    fn isochronous_endpoints() {
        let c = Config::parse(&crate::fake::Audio::config_descriptor()).unwrap();
        assert_eq!(c.interfaces(), vec![0, 1, 2]);
        assert!(c.alt(1, 0).unwrap().endpoints.is_empty());
        let out = c.alt(1, 1).unwrap().endpoints[0];
        assert_eq!((out.addr, out.kind, out.per_interval), (0x01, Some(XferType::Iso), 200));
        let mic = c.alt(2, 1).unwrap().endpoints[0];
        assert_eq!((mic.addr, mic.kind), (0x82, Some(XferType::Iso)));
        // High bandwidth (3 x 1024), then a SuperSpeed companion saying 3000.
        let mut b = vec![9, 2, 0, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 1, 1, 0xff, 0, 0, 0];
        b.extend_from_slice(&[7, 5, 0x81, 1, 0x00, 0x14, 1]);
        b.extend_from_slice(&[7, 5, 0x82, 1, 0x00, 0x04, 1, 6, 0x30, 2, 0, 0xb8, 0x0b]);
        b[2] = b.len() as u8;
        let c = Config::parse(&b).unwrap();
        let eps = &c.alt(0, 1).unwrap().endpoints;
        assert_eq!((eps[0].per_interval, eps[1].per_interval), (3072, 3000));
    }
}
