//! PC COM port, with bounded RX, divisor latch, FIFO and interrupt IDs.
use crate::{BusDevice, IrqLine};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, io::Write, sync::Arc};
use vroom_snapshot::{decode, encode, Snapshottable};

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    fcr: u8,
    tx_irq: bool,
    rx: VecDeque<u8>,
}

pub struct Serial16550 {
    state: State,
    out: Box<dyn Write + Send>,
    irq: Arc<dyn IrqLine>,
}
impl std::fmt::Debug for Serial16550 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Serial16550").finish_non_exhaustive()
    }
}
impl Serial16550 {
    pub fn new(out: Box<dyn Write + Send>, irq: Arc<dyn IrqLine>) -> Self {
        Self {
            state: State::default(),
            out,
            irq,
        }
    }
    pub fn queue_input(&mut self, bytes: &[u8]) {
        let n = bytes
            .len()
            .min(4096usize.saturating_sub(self.state.rx.len()));
        self.state.rx.extend(&bytes[..n]);
        self.update_irq();
    }
    fn iir(&self) -> u8 {
        if self.state.ier & 1 != 0 && !self.state.rx.is_empty() {
            4
        } else if self.state.ier & 2 != 0 && self.state.tx_irq {
            2
        } else {
            1
        }
    }
    fn update_irq(&self) {
        self.irq
            .set_level(self.state.mcr & 8 != 0 && self.iir() & 1 == 0);
    }
}
impl BusDevice for Serial16550 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if data.len() != 1 {
            return;
        }
        let dlab = self.state.lcr & 0x80 != 0;
        data[0] = match offset {
            0 if dlab => self.state.dll,
            1 if dlab => self.state.dlm,
            0 => self.state.rx.pop_front().unwrap_or(0),
            1 => self.state.ier,
            2 => {
                let i = self.iir();
                if i == 2 {
                    self.state.tx_irq = false;
                }
                i | if self.state.fcr & 1 != 0 { 0xc0 } else { 0 }
            }
            3 => self.state.lcr,
            4 => self.state.mcr,
            5 => 0x60 | u8::from(!self.state.rx.is_empty()),
            6 => {
                if self.state.mcr & 0x10 != 0 {
                    ((self.state.mcr & 2) << 3)
                        | ((self.state.mcr & 1) << 5)
                        | ((self.state.mcr & 4) << 4)
                        | ((self.state.mcr & 8) << 4)
                } else {
                    0xb0
                }
            }
            7 => self.state.scr,
            _ => 0,
        };
        self.update_irq();
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        if data.len() != 1 {
            return;
        }
        let v = data[0];
        let dlab = self.state.lcr & 0x80 != 0;
        match offset {
            0 if dlab => self.state.dll = v,
            1 if dlab => self.state.dlm = v,
            0 => {
                if self.state.mcr & 0x10 != 0 {
                    self.queue_input(&[v]);
                } else {
                    let _ = self.out.write_all(&[v]);
                    let _ = self.out.flush();
                }
                self.state.tx_irq = true;
            }
            1 => {
                if v & 2 != 0 && self.state.ier & 2 == 0 {
                    self.state.tx_irq = true;
                }
                self.state.ier = v & 0xf;
            }
            2 => {
                self.state.fcr = v & 0xc9;
                if v & 2 != 0 {
                    self.state.rx.clear();
                }
            }
            3 => self.state.lcr = v,
            4 => self.state.mcr = v & 0x1f,
            7 => self.state.scr = v,
            _ => {}
        }
        self.update_irq();
    }
}
impl Snapshottable for Serial16550 {
    fn snapshot_id(&self) -> String {
        "serial16550@3f8".into()
    }
    fn save(&mut self) -> vroom_snapshot::Result<Vec<u8>> {
        encode(&self.state)
    }
    fn restore(&mut self, data: &[u8]) -> vroom_snapshot::Result<()> {
        let mut s: State = decode(data)?;
        s.rx.truncate(4096);
        self.state = s;
        self.update_irq();
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn divisor_loopback_and_snapshot() {
        let mut s = Serial16550::new(Box::new(std::io::sink()), Arc::new(crate::NoIrq));
        s.write(3, &[0x80]);
        s.write(0, &[12]);
        let mut b = [0];
        s.read(0, &mut b);
        assert_eq!(b, [12]);
        s.write(3, &[3]);
        s.write(4, &[0x18]);
        s.write(1, &[3]);
        s.write(0, b"x");
        let state = s.save().unwrap();
        s.read(0, &mut b);
        assert_eq!(b, *b"x");
        s.restore(&state).unwrap();
        s.read(2, &mut b);
        assert_eq!(b[0] & 0xf, 4);
        s.read(0, &mut b);
        assert_eq!(b, *b"x");
        s.read(2, &mut b);
        assert_eq!(b[0] & 0xf, 2);
        s.read(2, &mut b);
        assert_eq!(b[0] & 0xf, 1);
    }
}
