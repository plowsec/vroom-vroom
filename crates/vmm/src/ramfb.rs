//! ramfb: a dumb framebuffer in guest RAM, configured by firmware through
//! fw_cfg ("etc/ramfb"). UEFI's GOP and Windows' basic display driver
//! draw into it; we copy it to the screen a few times per second.

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use vroom_display::SharedFb;
use vroom_memory::{GuestAddress, GuestMemory};

use crate::display::FbSink;

pub const FILE: &str = "etc/ramfb";
pub const CONFIG_LEN: usize = 28;
const XRGB8888: u32 = 0x3432_5258; // DRM fourcc "XR24"
const PERIOD: Duration = Duration::from_millis(33);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamfbConfig {
    pub addr: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Parse and validate the 28-byte big-endian config the guest wrote.
pub fn parse(b: &[u8], mem: &GuestMemory) -> Option<RamfbConfig> {
    if b.len() != CONFIG_LEN {
        return None;
    }
    let be32 = |o: usize| u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let addr = u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
    let (fourcc, width, height, stride) = (be32(8), be32(16), be32(20), be32(24));
    if fourcc != XRGB8888 || !SharedFb::mode_ok(width, height) {
        return None;
    }
    let stride = if stride == 0 { width * 4 } else { stride };
    if stride < width * 4 || stride > width * 16 {
        return None;
    }
    if !mem.check_range(GuestAddress(addr), stride as u64 * height as u64) {
        return None;
    }
    Some(RamfbConfig { addr, width, height, stride })
}

/// Background copier. Exits when the framebuffer goes away.
pub fn spawn(mem: GuestMemory, cfg: Arc<Mutex<Option<RamfbConfig>>>, fb: Weak<FbSink>) -> std::io::Result<()> {
    std::thread::Builder::new().name("ramfb".into()).spawn(move || {
        let mut last: Vec<u8> = Vec::new();
        let mut last_cfg = None;
        loop {
            std::thread::sleep(PERIOD);
            let Some(fb) = fb.upgrade() else { return };
            let Some(c) = *cfg.lock().unwrap_or_else(|e| e.into_inner()) else { continue };
            let len = c.stride as usize * c.height as usize;
            let mut buf = vec![0u8; len];
            if mem.read(GuestAddress(c.addr), &mut buf).is_err() {
                continue;
            }
            // Only push when something changed: idle screens cost nothing.
            if last_cfg == Some(c) && buf == last {
                continue;
            }
            fb.ramfb_frame(c.width, c.height, &buf, c.stride as usize);
            last = buf;
            last_cfg = Some(c);
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(addr: u64, fourcc: u32, w: u32, h: u32, stride: u32) -> Vec<u8> {
        let mut v = addr.to_be_bytes().to_vec();
        for x in [fourcc, 0, w, h, stride] {
            v.extend_from_slice(&x.to_be_bytes());
        }
        v
    }

    #[test]
    fn validation() {
        let mem = GuestMemory::new(&[(GuestAddress(0x4000_0000), 16 << 20)]).unwrap();
        let ok = parse(&cfg(0x4000_0000, XRGB8888, 1024, 768, 4096), &mem).unwrap();
        assert_eq!(ok.stride, 4096);
        assert!(parse(&cfg(0x4000_0000, 0x1234, 1024, 768, 4096), &mem).is_none());
        assert!(parse(&cfg(0x4000_0000, XRGB8888, 100_000, 768, 0), &mem).is_none());
        assert!(parse(&cfg(0x4000_0000, XRGB8888, 1024, 768, 100), &mem).is_none());
        // Frame runs past the end of RAM.
        assert!(parse(&cfg(0x40ff_0000, XRGB8888, 1024, 768, 4096), &mem).is_none());
        assert!(parse(&cfg(u64::MAX - 10, XRGB8888, 16, 16, 64), &mem).is_none());
    }
}
