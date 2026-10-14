//! virtio-input: keyboard and absolute-pointer tablet.
//!
//! The display server writes batches of evdev events (8 bytes each:
//! type u16, code u16, value i32, little endian) into the host end of a
//! datagram socket; we forward them into the guest's event queue.

use std::collections::VecDeque;
use std::io;
use std::os::unix::net::UnixDatagram as StdUnixDatagram;

use mio::net::UnixDatagram;
use mio::{Interest, Registry, Token};

use crate::worker::{QueueCtx, QueueHandler, WorkerSlot, BACKEND_TOKEN_BASE};
use crate::{types, ActivateContext, DeviceState, Result, VirtioDevice};

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
pub const EV_ABS: u16 = 3;
pub const EV_REP: u16 = 0x14;
pub const REL_HWHEEL: u16 = 6;
pub const REL_WHEEL: u16 = 8;
pub const ABS_X: u16 = 0;
pub const ABS_Y: u16 = 1;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const ABS_MAX: u32 = 32767;

const CFG_ID_NAME: u8 = 1;
const CFG_ID_SERIAL: u8 = 2;
const CFG_ID_DEVIDS: u8 = 3;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

const MAX_PENDING: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Keyboard,
    Tablet,
}

/// Encode one evdev event.
pub fn event(ty: u16, code: u16, value: i32) -> [u8; 8] {
    let mut e = [0u8; 8];
    e[0..2].copy_from_slice(&ty.to_le_bytes());
    e[2..4].copy_from_slice(&code.to_le_bytes());
    e[4..8].copy_from_slice(&value.to_le_bytes());
    e
}

#[derive(Debug)]
pub struct Input {
    kind: InputKind,
    events: StdUnixDatagram,
    select: u8,
    subsel: u8,
    worker: WorkerSlot<InputHandler>,
}

impl Input {
    pub fn new(kind: InputKind, events: StdUnixDatagram) -> Self {
        Input { kind, events, select: 0, subsel: 0, worker: WorkerSlot::default() }
    }

    fn handler(&self) -> Result<InputHandler> {
        let s = self.events.try_clone()?;
        s.set_nonblocking(true)?;
        Ok(InputHandler { sock: UnixDatagram::from_std(s), pending: VecDeque::new() })
    }

    /// Config payload for the current select/subsel.
    fn payload(&self) -> Vec<u8> {
        let bits = |codes: &[u16]| -> Vec<u8> {
            let max = codes.iter().copied().max().unwrap_or(0) as usize;
            let mut b = vec![0u8; max / 8 + 1];
            for &c in codes {
                b[c as usize / 8] |= 1 << (c % 8);
            }
            b
        };
        match (self.select, self.kind) {
            (CFG_ID_NAME, InputKind::Keyboard) => b"Bulwark Keyboard".to_vec(),
            (CFG_ID_NAME, InputKind::Tablet) => b"Bulwark Tablet".to_vec(),
            (CFG_ID_SERIAL, _) => b"0".to_vec(),
            (CFG_ID_DEVIDS, k) => {
                let product: u16 = if k == InputKind::Keyboard { 1 } else { 2 };
                [0x06u16, 0x627b, product, 1].iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            (CFG_EV_BITS, InputKind::Keyboard) => match self.subsel as u16 {
                EV_KEY => bits(&(1u16..=248).collect::<Vec<_>>()),
                // Non-empty = "kernel, do autorepeat for me".
                EV_REP => vec![1],
                _ => vec![],
            },
            (CFG_EV_BITS, InputKind::Tablet) => match self.subsel as u16 {
                EV_KEY => bits(&[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE]),
                EV_ABS => bits(&[ABS_X, ABS_Y]),
                EV_REL => bits(&[REL_HWHEEL, REL_WHEEL]),
                _ => vec![],
            },
            (CFG_ABS_INFO, InputKind::Tablet) if matches!(self.subsel as u16, ABS_X | ABS_Y) => {
                [0u32, ABS_MAX, 0, 0, 0].iter().flat_map(|v| v.to_le_bytes()).collect()
            }
            _ => vec![],
        }
    }
}

#[derive(Debug)]
pub struct InputHandler {
    sock: UnixDatagram,
    pending: VecDeque<[u8; 8]>,
}

impl InputHandler {
    fn pull(&mut self) {
        let mut buf = [0u8; 4096];
        while let Ok(n) = self.sock.recv(&mut buf) {
            for e in buf[..n - n % 8].as_chunks::<8>().0 {
                if self.pending.len() < MAX_PENDING {
                    let mut a = [0u8; 8];
                    a.copy_from_slice(e);
                    self.pending.push_back(a);
                }
            }
        }
    }

    fn deliver(&mut self, ctx: &mut QueueCtx) {
        while let Some(e) = self.pending.front() {
            let chain = match ctx.queues[0].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                _ => break,
            };
            let head = chain.head();
            let mut len = 0;
            if let Ok((_, mut w)) = chain.into_rw() {
                if w.write_all(e).is_ok() {
                    len = 8;
                }
            }
            self.pending.pop_front();
            ctx.add_used(0, head, len);
        }
    }
}

impl QueueHandler for InputHandler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        if q == 1 {
            // Status queue (LEDs): consume and ignore.
            while let Ok(Some(c)) = ctx.queues[1].pop(&ctx.mem) {
                let h = c.head();
                ctx.add_used(1, h, 0);
            }
        }
        self.pull();
        self.deliver(ctx);
    }

    fn backend_ready(&mut self, ctx: &mut QueueCtx, _token: usize) {
        self.pull();
        self.deliver(ctx);
    }

    fn register(&mut self, reg: &Registry) -> io::Result<()> {
        reg.register(&mut self.sock, Token(BACKEND_TOKEN_BASE), Interest::READABLE)
    }
}

impl VirtioDevice for Input {
    fn device_type(&self) -> u32 {
        types::INPUT
    }
    fn name(&self) -> String {
        match self.kind {
            InputKind::Keyboard => "kbd".into(),
            InputKind::Tablet => "tablet".into(),
        }
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![64, 64]
    }
    fn device_features(&self) -> u64 {
        crate::features::RING_EVENT_IDX
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let p = self.payload();
        let mut c = [0u8; 136];
        c[0] = self.select;
        c[1] = self.subsel;
        c[2] = p.len().min(128) as u8;
        let n = p.len().min(128);
        c[8..8 + n].copy_from_slice(&p[..n]);
        let o = offset as usize;
        data.fill(0);
        if o < c.len() {
            let n = data.len().min(c.len() - o);
            data[..n].copy_from_slice(&c[o..o + n]);
        }
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            match offset as usize + i {
                0 => self.select = b,
                1 => self.subsel = b,
                _ => {}
            }
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        let h = self.handler()?;
        self.worker.start(&self.name(), ctx, h, false)
    }
    fn queue_notify(&mut self, q: u16) {
        self.worker.notify(q);
    }
    fn reset(&mut self) {
        self.worker.stop();
    }
    fn pause(&mut self) {
        self.worker.pause();
    }
    fn resume(&mut self) {
        self.worker.resume();
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        Ok(DeviceState { queues: self.worker.queues(), data: Vec::new() })
    }
    fn restore_active(&mut self, ctx: ActivateContext, _data: &[u8]) -> Result<()> {
        let h = self.handler()?;
        self.worker.start(&self.name(), ctx, h, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_space() {
        let (a, _b) = StdUnixDatagram::pair().unwrap();
        let mut t = Input::new(InputKind::Tablet, a);
        t.write_config(0, &[CFG_ABS_INFO, ABS_Y as u8]);
        let mut d = [0u8; 16];
        t.read_config(0, &mut d);
        assert_eq!(d[2], 20);
        assert_eq!(u32::from_le_bytes([d[12], d[13], d[14], d[15]]), ABS_MAX);
        t.write_config(0, &[CFG_EV_BITS, EV_KEY as u8]);
        let mut d = [0u8; 136];
        t.read_config(0, &mut d);
        // BTN_LEFT = 0x110 -> byte 34, bit 0
        assert_eq!(d[8 + 34] & 1, 1);
    }
}
