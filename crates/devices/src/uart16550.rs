//! Byte-wide PC 16550A serial port for the native Linux harness.
//!
//! Transmit drains synchronously to a bounded host sink; there is no serial
//! wire or baud pacing. Receive timeout uses the programmed divisor/framing.
//! This device does not yet participate in VM snapshots.
use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::{BusDevice, IrqLine};

pub struct Uart16550 {
    output: Box<dyn Write + Send>,
    irq: Arc<dyn IrqLine>,
    rx: VecDeque<u8>,
    divisor: u16,
    ier: u8,
    lcr: u8,
    mcr: u8,
    fcr: u8,
    scratch: u8,
    modem_inputs: u8,
    modem_delta: u8,
    overrun: bool,
    thre_pending: bool,
    last_rx: Option<Instant>,
    rx_timeout: bool,
    irq_level: bool,
}

impl std::fmt::Debug for Uart16550 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uart16550")
            .field("divisor", &self.divisor)
            .field("ier", &self.ier)
            .field("lcr", &self.lcr)
            .field("mcr", &self.mcr)
            .field("rx_len", &self.rx.len())
            .finish()
    }
}

impl Uart16550 {
    pub fn new(output: Box<dyn Write + Send>, irq: Arc<dyn IrqLine>) -> Self {
        Self {
            output,
            irq,
            rx: VecDeque::with_capacity(16),
            divisor: 0,
            ier: 0,
            lcr: 0,
            mcr: 0,
            fcr: 0,
            scratch: 0,
            modem_inputs: 0,
            modem_delta: 0,
            overrun: false,
            thre_pending: true,
            last_rx: None,
            rx_timeout: false,
            irq_level: false,
        }
    }

    fn fifo(&self) -> bool {
        self.fcr & 1 != 0
    }
    fn trigger(&self) -> usize {
        if self.fifo() {
            [1, 4, 8, 14][(self.fcr >> 6) as usize]
        } else {
            1
        }
    }
    fn modem(&self) -> u8 {
        if self.mcr & 0x10 == 0 {
            return self.modem_inputs;
        }
        ((self.mcr & 2) << 3)
            | ((self.mcr & 1) << 5)
            | ((self.mcr & 4) << 4)
            | ((self.mcr & 8) << 4)
    }
    fn changed_modem(&mut self, previous: u8) {
        let current = self.modem();
        let changed = previous ^ current;
        self.modem_delta |= ((changed & 0x30) >> 4) | ((changed & 0x80) >> 4);
        if previous & 0x40 != 0 && current & 0x40 == 0 {
            self.modem_delta |= 4;
        }
    }
    /// External CTS/DSR/RI/DCD levels occupy the upper four bits, as in MSR.
    pub fn set_modem_inputs(&mut self, levels: u8) {
        let previous = self.modem();
        self.modem_inputs = levels & 0xf0;
        self.changed_modem(previous);
        self.update_irq();
    }
    fn cause(&self) -> u8 {
        if self.ier & 4 != 0 && self.overrun {
            6
        } else if self.ier & 1 != 0 && self.rx.len() >= self.trigger() {
            4
        } else if self.ier & 1 != 0 && self.rx_timeout && !self.rx.is_empty() {
            12
        } else if self.ier & 2 != 0 && self.thre_pending {
            2
        } else if self.ier & 8 != 0 && self.modem_delta != 0 {
            0
        } else {
            1
        }
    }
    fn update_irq(&mut self) {
        let level = self.mcr & 8 != 0 && self.cause() != 1;
        if level != self.irq_level {
            self.irq_level = level;
            self.irq.set_level(level);
        }
    }
    fn receive(&mut self, byte: u8, now: Instant) {
        if self.rx.len() == if self.fifo() { 16 } else { 1 } {
            self.overrun = true;
        } else {
            self.rx.push_back(byte);
        }
        self.last_rx = Some(now);
        self.rx_timeout = false;
        self.update_irq();
    }
    pub fn queue_input(&mut self, bytes: &[u8]) {
        if self.mcr & 0x10 == 0 {
            for &byte in bytes {
                self.receive(byte, Instant::now());
            }
        }
    }
    /// Service the FIFO's four-character receive timeout. Call periodically
    /// even while the guest is halted. A zero divisor has no valid baud rate.
    pub fn tick(&mut self, now: Instant) {
        if self.fifo() && self.divisor != 0 && !self.rx.is_empty() {
            let data = 5 + (self.lcr & 3) as u64;
            // Half-bit units preserve 1.5 stop bits for five-bit words.
            let stops = if self.lcr & 4 == 0 {
                2
            } else if data == 5 {
                3
            } else {
                4
            };
            let half_bits = 2 + data * 2 + if self.lcr & 8 != 0 { 2 } else { 0 } + stops;
            let ns = (half_bits * 4 * self.divisor as u64 * 1_000_000_000).div_ceil(230_400);
            if self
                .last_rx
                .is_some_and(|then| now.saturating_duration_since(then) >= Duration::from_nanos(ns))
            {
                self.rx_timeout = true;
            }
        }
        self.update_irq();
    }
    fn read_byte(&mut self, offset: u64) -> u8 {
        let value = match offset {
            0 if self.lcr & 0x80 != 0 => self.divisor as u8,
            1 if self.lcr & 0x80 != 0 => (self.divisor >> 8) as u8,
            0 => {
                let value = self.rx.pop_front().unwrap_or(0);
                self.last_rx = if self.rx.is_empty() {
                    None
                } else {
                    Some(Instant::now())
                };
                self.rx_timeout = false;
                value
            }
            1 => self.ier,
            2 => {
                let cause = self.cause();
                if cause == 2 {
                    self.thre_pending = false;
                }
                cause | if self.fifo() { 0xc0 } else { 0 }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => {
                let value = 0x60 | u8::from(!self.rx.is_empty()) | (u8::from(self.overrun) << 1);
                self.overrun = false;
                value
            }
            6 => {
                let value = self.modem() | self.modem_delta;
                self.modem_delta = 0;
                value
            }
            7 => self.scratch,
            _ => 0,
        };
        self.update_irq();
        value
    }
    fn write_byte(&mut self, offset: u64, value: u8) {
        match offset {
            0 if self.lcr & 0x80 != 0 => self.divisor = (self.divisor & 0xff00) | value as u16,
            1 if self.lcr & 0x80 != 0 => {
                self.divisor = (self.divisor & 0xff) | ((value as u16) << 8)
            }
            0 => {
                self.thre_pending = false;
                self.update_irq(); // empty -> busy -> empty must create a new edge
                if self.mcr & 0x10 != 0 {
                    self.receive(value, Instant::now());
                } else {
                    let _ = self.output.write_all(&[value]);
                    let _ = self.output.flush();
                }
                self.thre_pending = true;
            }
            1 => {
                if value & 2 != 0 && self.ier & 2 == 0 {
                    self.thre_pending = true;
                }
                self.ier = value & 0x0f;
            }
            2 => {
                if (self.fcr ^ value) & 1 != 0 || value & 2 != 0 {
                    self.rx.clear();
                    self.overrun = false;
                    self.last_rx = None;
                    self.rx_timeout = false;
                }
                // TX FIFO is always empty because the sink drains immediately.
                self.fcr = value & 0xc9;
            }
            3 => self.lcr = value,
            4 => {
                let previous = self.modem();
                self.mcr = value & 0x1f;
                self.changed_modem(previous);
            }
            7 => self.scratch = value,
            _ => (),
        }
        self.update_irq();
    }
}

impl BusDevice for Uart16550 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if data.len() == 1 && offset < 8 {
            data[0] = self.read_byte(offset);
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        if data.len() == 1 && offset < 8 {
            self.write_byte(offset, data[0]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    #[derive(Default)]
    struct Line(Mutex<Vec<bool>>);
    impl IrqLine for Line {
        fn set_level(&self, value: bool) {
            self.0.lock().unwrap().push(value);
        }
    }
    fn uart() -> (Uart16550, Arc<Line>) {
        let line = Arc::new(Line::default());
        (Uart16550::new(Box::new(Vec::new()), line.clone()), line)
    }
    #[test]
    fn linux_probe_divisor_scratch_fifo_and_loopback() {
        let (mut u, _) = uart();
        u.write_byte(3, 0x80);
        u.write_byte(0, 12);
        u.write_byte(1, 1);
        assert_eq!(u.read_byte(0), 12);
        assert_eq!(u.read_byte(1), 1);
        u.write_byte(3, 3);
        u.write_byte(7, 0xa5);
        assert_eq!(u.read_byte(7), 0xa5);
        u.write_byte(2, 1);
        assert_eq!(u.read_byte(2), 0xc1);
        u.write_byte(4, 0x1a);
        assert_eq!(u.read_byte(6) & 0xf0, 0x90);
        u.write_byte(0, 0x5a);
        assert_eq!(u.read_byte(5) & 1, 1);
        assert_eq!(u.read_byte(0), 0x5a);
        assert_eq!(u.read_byte(5), 0x60);
    }
    #[test]
    fn thre_ack_reenable_and_write_reassert_edges() {
        let (mut u, line) = uart();
        u.write_byte(4, 8);
        u.write_byte(1, 2);
        assert_eq!(u.read_byte(2), 2);
        assert_eq!(u.read_byte(2), 1);
        u.write_byte(1, 0);
        u.write_byte(1, 2);
        assert_eq!(u.read_byte(2), 2);
        u.write_byte(0, b'x');
        assert_eq!(u.read_byte(2), 2);
        assert_eq!(
            *line.0.lock().unwrap(),
            [true, false, true, false, true, false]
        );
    }
    #[test]
    fn receive_priority_overrun_and_fifo_reset() {
        let (mut u, _) = uart();
        u.write_byte(4, 8);
        u.write_byte(1, 7);
        u.queue_input(b"ab");
        assert_eq!(u.read_byte(2), 6);
        assert_eq!(u.read_byte(5), 0x63);
        assert_eq!(u.read_byte(2), 4);
        assert_eq!(u.read_byte(0), b'a');
        assert_eq!(u.read_byte(2), 2);
        u.write_byte(2, 0x41);
        u.queue_input(b"123");
        assert_eq!(u.read_byte(2), 0xc1);
        u.queue_input(b"4");
        assert_eq!(u.read_byte(2), 0xc4);
        u.write_byte(2, 0x43);
        assert_eq!(u.read_byte(5), 0x60);
    }
    #[test]
    fn receive_timeout_and_modem_delta_ack() {
        let (mut u, _) = uart();
        u.write_byte(3, 0x83);
        u.write_byte(0, 1);
        u.write_byte(3, 3);
        u.write_byte(4, 8);
        u.write_byte(2, 0x41);
        u.write_byte(1, 9);
        u.queue_input(b"a");
        u.tick(Instant::now() + Duration::from_millis(1));
        assert_eq!(u.read_byte(2), 0xcc);
        u.read_byte(0);
        u.set_modem_inputs(0x40);
        assert_eq!(u.read_byte(2), 0xc1);
        u.set_modem_inputs(0x90);
        assert_eq!(u.read_byte(2), 0xc0);
        assert_eq!(u.read_byte(6), 0x9d);
        assert_eq!(u.read_byte(6), 0x90);
        assert_eq!(u.read_byte(2), 0xc1);
    }
    #[test]
    fn malformed_accesses_have_no_register_side_effects() {
        let (mut u, _) = uart();
        u.write(7, &[1, 2]);
        assert_eq!(u.read_byte(7), 0);
        let mut bytes = [0xff; 4];
        u.read(2, &mut bytes);
        assert_eq!(bytes, [0; 4]);
        u.write(u64::MAX, &[1]);
        u.read(u64::MAX, &mut bytes[..1]);
    }
}
