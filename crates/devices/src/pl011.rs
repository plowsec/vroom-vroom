//! ARM PL011 UART. Enough for Linux `amba-pl011` and EDK2's serial driver.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;

use bulwark_snapshot::{decode, encode, Snapshottable};
use serde::{Deserialize, Serialize};

use crate::{put_le, BusDevice, IrqLine};

const DR: u64 = 0x000;
const RSR: u64 = 0x004;
const FR: u64 = 0x018;
const ILPR: u64 = 0x020;
const IBRD: u64 = 0x024;
const FBRD: u64 = 0x028;
const LCR_H: u64 = 0x02c;
const CR: u64 = 0x030;
const IFLS: u64 = 0x034;
const IMSC: u64 = 0x038;
const RIS: u64 = 0x03c;
const MIS: u64 = 0x040;
const ICR: u64 = 0x044;
const DMACR: u64 = 0x048;

const FR_RXFE: u32 = 0x10;
const FR_TXFE: u32 = 0x80;
const FR_RXFF: u32 = 0x40;

const INT_RX: u32 = 0x10;
const INT_TX: u32 = 0x20;
const INT_RT: u32 = 0x40;

const LCR_FEN: u32 = 0x10;

/// Host-side input buffer cap. Excess input is dropped, not queued forever.
const RX_CAP: usize = 4096;

/// PrimeCell peripheral + cell id bytes (0xfe0..0xfff).
const ID: [u8; 8] = [0x11, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Regs {
    ilpr: u32,
    ibrd: u32,
    fbrd: u32,
    lcr_h: u32,
    cr: u32,
    ifls: u32,
    imsc: u32,
    ris: u32,
    dmacr: u32,
    rx: VecDeque<u8>,
}

pub struct Pl011 {
    base: u64,
    regs: Regs,
    out: Box<dyn Write + Send>,
    irq: Arc<dyn IrqLine>,
}

impl std::fmt::Debug for Pl011 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pl011").field("base", &self.base).finish()
    }
}

impl Pl011 {
    pub fn new(base: u64, out: Box<dyn Write + Send>, irq: Arc<dyn IrqLine>) -> Self {
        Pl011 {
            base,
            regs: Regs { cr: 0x300, ifls: 0x12, ..Default::default() },
            out,
            irq,
        }
    }

    /// Feed host input (keyboard) into the receive FIFO.
    pub fn queue_input(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.regs.rx.len() >= RX_CAP {
                break;
            }
            self.regs.rx.push_back(b);
        }
        if !self.regs.rx.is_empty() {
            self.regs.ris |= INT_RX | INT_RT;
        }
        self.update_irq();
    }

    fn update_irq(&self) {
        self.irq.set_level(self.regs.ris & self.regs.imsc != 0);
    }

    fn fifo_depth(&self) -> usize {
        if self.regs.lcr_h & LCR_FEN != 0 {
            16
        } else {
            1
        }
    }

    fn flags(&self) -> u32 {
        let mut f = FR_TXFE;
        if self.regs.rx.is_empty() {
            f |= FR_RXFE;
        } else if self.regs.rx.len() >= self.fifo_depth() {
            f |= FR_RXFF;
        }
        f
    }
}

impl BusDevice for Pl011 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let r = &mut self.regs;
        let v: u32 = match offset {
            DR => {
                let c = r.rx.pop_front().unwrap_or(0) as u32;
                if r.rx.is_empty() {
                    r.ris &= !(INT_RX | INT_RT);
                }
                self.update_irq();
                c
            }
            RSR => 0,
            FR => self.flags(),
            ILPR => r.ilpr,
            IBRD => r.ibrd,
            FBRD => r.fbrd,
            LCR_H => r.lcr_h,
            CR => r.cr,
            IFLS => r.ifls,
            IMSC => r.imsc,
            RIS => r.ris,
            MIS => r.ris & r.imsc,
            DMACR => r.dmacr,
            0xfe0..=0xffc if offset % 4 == 0 => ID[((offset - 0xfe0) / 4) as usize] as u32,
            _ => 0,
        };
        put_le(data, v as u64);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let v = crate::get_le(data) as u32;
        let r = &mut self.regs;
        match offset {
            DR => {
                // Best effort: a stuck host console must not stall the guest.
                let _ = self.out.write_all(&[v as u8]);
                let _ = self.out.flush();
                // TX FIFO drains instantly, so TX is "ready" right away.
                r.ris |= INT_TX;
            }
            RSR => {}
            ILPR => r.ilpr = v,
            IBRD => r.ibrd = v & 0xffff,
            FBRD => r.fbrd = v & 0x3f,
            LCR_H => r.lcr_h = v & 0xff,
            CR => r.cr = v & 0xffff,
            IFLS => r.ifls = v & 0x3f,
            IMSC => r.imsc = v & 0x7ff,
            ICR => r.ris &= !v,
            DMACR => r.dmacr = v & 7,
            _ => log::debug!("pl011: write to {offset:#x} ignored"),
        }
        self.update_irq();
    }
}

impl Snapshottable for Pl011 {
    fn snapshot_id(&self) -> String {
        format!("pl011@{:x}", self.base)
    }

    fn save(&mut self) -> bulwark_snapshot::Result<Vec<u8>> {
        encode(&self.regs)
    }

    fn restore(&mut self, data: &[u8]) -> bulwark_snapshot::Result<()> {
        let mut regs: Regs = decode(data)?;
        regs.rx.truncate(RX_CAP);
        self.regs = regs;
        self.update_irq();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Line(AtomicBool);
    impl IrqLine for Line {
        fn set_level(&self, l: bool) {
            self.0.store(l, Ordering::SeqCst)
        }
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn rd(u: &mut Pl011, off: u64) -> u32 {
        let mut d = [0u8; 4];
        u.read(off, &mut d);
        u32::from_le_bytes(d)
    }

    #[test]
    fn tx_rx_irq() {
        let line = Arc::new(Line::default());
        let sink = Sink::default();
        let mut u = Pl011::new(0x900_0000, Box::new(sink.clone()), line.clone());
        u.write(DR, &[b'h', 0, 0, 0]);
        u.write(DR, &[b'i', 0, 0, 0]);
        assert_eq!(&*sink.0.lock().unwrap(), b"hi");

        assert_eq!(rd(&mut u, FR) & FR_RXFE, FR_RXFE);
        u.write(IMSC, &INT_RX.to_le_bytes());
        u.queue_input(b"ab");
        assert!(line.0.load(Ordering::SeqCst));
        assert_eq!(rd(&mut u, DR), b'a' as u32);
        assert_eq!(rd(&mut u, DR), b'b' as u32);
        assert!(!line.0.load(Ordering::SeqCst));
        assert_eq!(rd(&mut u, 0xfe0), 0x11);
        assert_eq!(rd(&mut u, 0xffc), 0xb1);
    }

    #[test]
    fn snapshot_roundtrip_and_garbage() {
        let mut u = Pl011::new(0, Box::new(std::io::sink()), Arc::new(crate::NoIrq));
        u.queue_input(b"xyz");
        u.write(IMSC, &[0x10, 0, 0, 0]);
        let s = u.save().unwrap();
        let mut v = Pl011::new(0, Box::new(std::io::sink()), Arc::new(crate::NoIrq));
        v.restore(&s).unwrap();
        assert_eq!(rd(&mut v, DR), b'x' as u32);
        assert!(v.restore(b"{not json").is_err());
    }

    #[test]
    fn rx_cap_enforced() {
        let mut u = Pl011::new(0, Box::new(std::io::sink()), Arc::new(crate::NoIrq));
        u.queue_input(&vec![b'a'; RX_CAP * 2]);
        assert_eq!(u.regs.rx.len(), RX_CAP);
    }
}
