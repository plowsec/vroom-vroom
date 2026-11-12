//! Host serial input is a board capability, independent of its output sink.

use std::sync::{Arc, Mutex};

use vroom_devices::pl011::Pl011;

pub(crate) enum SerialInput {
    Pl011(Arc<Mutex<Pl011>>),
    Pc(Arc<Mutex<vroom_devices::serial16550::Serial16550>>),
    #[allow(dead_code)] // Boards without a serviced receive path must reject input.
    Unavailable(&'static str),
}

impl SerialInput {
    pub(crate) fn queue_input(&self, bytes: &[u8]) -> crate::Result<()> {
        match self {
            Self::Pc(uart) => {
                uart.lock().unwrap_or_else(|e| e.into_inner()).queue_input(bytes);
                Ok(())
            }
            Self::Pl011(uart) => {
                uart.lock().unwrap_or_else(|e| e.into_inner()).queue_input(bytes);
                Ok(())
            }
            Self::Unavailable(why) => Err(crate::Error::Unavailable(why)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use vroom_devices::{BusDevice, IrqLine};

    #[derive(Default)]
    struct Irq(AtomicBool);
    impl IrqLine for Irq {
        fn set_level(&self, level: bool) {
            self.0.store(level, Ordering::SeqCst);
        }
    }

    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }

    #[test]
    fn pl011_keeps_fifo_irq_and_output_behavior() {
        let irq = Arc::new(Irq::default());
        let out = Output::default();
        let uart = Arc::new(Mutex::new(Pl011::new(0, Box::new(out.clone()), irq.clone())));
        let input = SerialInput::Pl011(uart.clone());
        // Unmask RX; host input goes to the existing FIFO and IRQ path.
        uart.lock().unwrap().write(0x38, &0x10u32.to_le_bytes());
        input.queue_input(b"ab").unwrap();
        input.queue_input(b"c").unwrap();
        assert!(irq.0.load(Ordering::SeqCst));
        assert!(out.0.lock().unwrap().is_empty());
        for byte in b"abc" {
            let mut data = [0; 4];
            uart.lock().unwrap().read(0, &mut data);
            assert_eq!(u32::from_le_bytes(data), *byte as u32);
        }
        assert!(!irq.0.load(Ordering::SeqCst));
        let mut flags = [0; 4];
        uart.lock().unwrap().read(0x18, &mut flags);
        assert_ne!(u32::from_le_bytes(flags) & 0x10, 0);
        uart.lock().unwrap().write(0, b"Z");
        assert_eq!(*out.0.lock().unwrap(), b"Z");
    }

    #[test]
    fn unavailable_input_reports_the_board_reason() {
        let input = SerialInput::Unavailable("serial receive is unavailable on this board");
        for bytes in [&b"input"[..], &b""[..]] {
            assert!(matches!(input.queue_input(bytes), Err(crate::Error::Unavailable(
                "serial receive is unavailable on this board"
            ))));
        }
    }
}
