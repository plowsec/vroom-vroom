//! virtio-console with named ports (MULTIPORT). Each port is a byte pipe
//! to a host-side unix stream; the guest sees /dev/virtio-ports/<name>.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream as StdUnixStream;

use mio::net::UnixStream;
use mio::{Interest, Registry, Token};
use serde::{Deserialize, Serialize};

use crate::worker::{QueueCtx, QueueHandler, WorkerSlot, BACKEND_TOKEN_BASE, RESUME_TOKEN};
use crate::{types, ActivateContext, DeviceState, Result, VirtioDevice};

const F_MULTIPORT: u64 = 1 << 1;

const DEVICE_READY: u16 = 0;
const DEVICE_ADD: u16 = 1;
const PORT_READY: u16 = 3;
const PORT_OPEN: u16 = 6;
const PORT_NAME: u16 = 7;

const CTRL_RX: usize = 2;
const CTRL_TX: usize = 3;
/// Per-direction buffering cap per port.
const BUF_CAP: usize = 1 << 20;
const MAX_PORTS: usize = 8;

fn port_rx(p: usize) -> usize {
    if p == 0 {
        0
    } else {
        2 + 2 * p
    }
}

fn port_tx(p: usize) -> usize {
    port_rx(p) + 1
}

/// Which queue belongs to which port (None = control).
fn queue_port(q: usize) -> Option<(usize, bool)> {
    match q {
        0 => Some((0, false)),
        1 => Some((0, true)),
        2 | 3 => None,
        q => Some(((q - 2) / 2, q % 2 == 1)),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PortState {
    guest_open: bool,
}

#[derive(Debug)]
struct Port {
    name: String,
    stream: UnixStream,
    state: PortState,
    to_guest: VecDeque<u8>,
    to_host: VecDeque<u8>,
    host_closed: bool,
}

#[derive(Debug)]
pub struct Console {
    /// (name, host end). Moved into the worker while active.
    ports: Vec<(String, StdUnixStream)>,
    saved: Vec<PortState>,
    worker: WorkerSlot<ConsoleHandler>,
}

impl Console {
    /// Each port: guest-visible name + host end of a socketpair.
    pub fn new(ports: Vec<(String, StdUnixStream)>) -> Result<Self> {
        if ports.is_empty() || ports.len() > MAX_PORTS {
            return Err(crate::Error::Device("console: 1..=8 ports".into()));
        }
        Ok(Console { saved: vec![PortState::default(); ports.len()], ports, worker: WorkerSlot::default() })
    }

    fn handler(&self) -> Result<ConsoleHandler> {
        let mut ports = Vec::new();
        for (i, (name, s)) in self.ports.iter().enumerate() {
            let s = s.try_clone()?;
            s.set_nonblocking(true)?;
            ports.push(Port {
                name: name.clone(),
                stream: UnixStream::from_std(s),
                state: self.saved.get(i).cloned().unwrap_or_default(),
                to_guest: VecDeque::new(),
                to_host: VecDeque::new(),
                host_closed: false,
            });
        }
        Ok(ConsoleHandler { ports, ctrl_out: VecDeque::new() })
    }
}

#[derive(Debug)]
pub struct ConsoleHandler {
    ports: Vec<Port>,
    ctrl_out: VecDeque<Vec<u8>>,
}

fn ctrl(id: u32, event: u16, value: u16) -> Vec<u8> {
    let mut v = id.to_le_bytes().to_vec();
    v.extend_from_slice(&event.to_le_bytes());
    v.extend_from_slice(&value.to_le_bytes());
    v
}

impl ConsoleHandler {
    fn control_from_guest(&mut self, ctx: &mut QueueCtx) {
        loop {
            let chain = match ctx.queues[CTRL_TX].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                _ => break,
            };
            let head = chain.head();
            if let Ok((mut r, _)) = chain.into_rw() {
                let id: u32 = r.read_obj().map(u32::from_le).unwrap_or(u32::MAX);
                let event: u16 = r.read_obj().map(u16::from_le).unwrap_or(u16::MAX);
                let value: u16 = r.read_obj().map(u16::from_le).unwrap_or(0);
                let idx = id as usize;
                match event {
                    DEVICE_READY if value == 1 => {
                        for i in 0..self.ports.len() {
                            self.ctrl_out.push_back(ctrl(i as u32, DEVICE_ADD, 0));
                        }
                    }
                    PORT_READY if value == 1 && idx < self.ports.len() => {
                        let mut m = ctrl(id, PORT_NAME, 1);
                        m.extend_from_slice(self.ports[idx].name.as_bytes());
                        self.ctrl_out.push_back(m);
                        self.ctrl_out.push_back(ctrl(id, PORT_OPEN, 1));
                    }
                    PORT_OPEN if idx < self.ports.len() => {
                        let p = &mut self.ports[idx];
                        p.state.guest_open = value == 1;
                        if !p.state.guest_open {
                            p.to_guest.clear();
                        }
                    }
                    _ => {}
                }
            }
            ctx.add_used(CTRL_TX, head, 0);
        }
    }

    fn control_to_guest(&mut self, ctx: &mut QueueCtx) {
        while let Some(m) = self.ctrl_out.front() {
            let chain = match ctx.queues[CTRL_RX].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                _ => break,
            };
            let head = chain.head();
            let mut len = 0;
            if let Ok((_, mut w)) = chain.into_rw() {
                if w.write_all(m).is_ok() {
                    len = m.len() as u32;
                }
            }
            self.ctrl_out.pop_front();
            ctx.add_used(CTRL_RX, head, len);
        }
    }

    fn guest_to_host(&mut self, ctx: &mut QueueCtx, p: usize) {
        let q = port_tx(p);
        let Some(port) = self.ports.get_mut(p) else { return };
        // Backpressure: stop taking guest buffers while the host is slow.
        while port.to_host.len() < BUF_CAP {
            let chain = match ctx.queues.get_mut(q).map(|qq| qq.pop(&ctx.mem)) {
                Some(Ok(Some(c))) => c,
                _ => break,
            };
            let head = chain.head();
            if let Ok((mut r, _)) = chain.into_rw() {
                if let Ok(v) = r.read_to_vec() {
                    if !port.host_closed {
                        port.to_host.extend(v);
                    }
                }
            }
            ctx.add_used(q, head, 0);
        }
        flush_to_host(port);
    }

    fn host_to_guest(&mut self, ctx: &mut QueueCtx, p: usize) {
        let q = port_rx(p);
        let Some(port) = self.ports.get_mut(p) else { return };
        // Read what the host has, within the cap.
        let mut buf = [0u8; 16384];
        while port.to_guest.len() < BUF_CAP && !port.host_closed {
            match port.stream.read(&mut buf) {
                Ok(0) => port.host_closed = true,
                Ok(n) => {
                    if port.state.guest_open {
                        port.to_guest.extend(&buf[..n]);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => port.host_closed = true,
            }
        }
        while !port.to_guest.is_empty() {
            let chain = match ctx.queues.get_mut(q).map(|qq| qq.pop(&ctx.mem)) {
                Some(Ok(Some(c))) => c,
                _ => break,
            };
            let head = chain.head();
            let mut len = 0;
            if let Ok((_, mut w)) = chain.into_rw() {
                let n = w.available().min(port.to_guest.len());
                let data: Vec<u8> = port.to_guest.drain(..n).collect();
                if w.write_all(&data).is_ok() {
                    len = n as u32;
                }
            }
            ctx.add_used(q, head, len);
        }
    }
}

fn flush_to_host(port: &mut Port) {
    while !port.to_host.is_empty() {
        let (a, _) = port.to_host.as_slices();
        match port.stream.write(a) {
            Ok(0) => break,
            Ok(n) => {
                port.to_host.drain(..n);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => {
                port.host_closed = true;
                port.to_host.clear();
            }
        }
    }
}

impl QueueHandler for ConsoleHandler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        match queue_port(q) {
            None => {
                if q == CTRL_TX {
                    self.control_from_guest(ctx);
                }
                self.control_to_guest(ctx);
            }
            Some((p, true)) => self.guest_to_host(ctx, p),
            Some((p, false)) => self.host_to_guest(ctx, p),
        }
    }

    fn backend_ready(&mut self, ctx: &mut QueueCtx, token: usize) {
        let ports: Vec<usize> = if token == RESUME_TOKEN {
            (0..self.ports.len()).collect()
        } else {
            vec![token.wrapping_sub(BACKEND_TOKEN_BASE)]
        };
        for p in ports {
            if p < self.ports.len() {
                self.guest_to_host(ctx, p);
                self.host_to_guest(ctx, p);
            }
        }
    }

    fn register(&mut self, reg: &Registry) -> io::Result<()> {
        for (i, p) in self.ports.iter_mut().enumerate() {
            reg.register(&mut p.stream, Token(BACKEND_TOKEN_BASE + i), Interest::READABLE | Interest::WRITABLE)?;
        }
        Ok(())
    }
}

impl VirtioDevice for Console {
    fn device_type(&self) -> u32 {
        types::CONSOLE
    }
    fn name(&self) -> String {
        "console".into()
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        // port0 rx/tx, control rx/tx, then rx/tx per extra port.
        vec![64; 2 + 2 * self.ports.len()]
    }
    fn device_features(&self) -> u64 {
        F_MULTIPORT | crate::features::RING_EVENT_IDX
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let mut c = [0u8; 12];
        c[4..8].copy_from_slice(&(self.ports.len() as u32).to_le_bytes());
        let o = offset as usize;
        data.fill(0);
        if o < c.len() {
            let n = data.len().min(c.len() - o);
            data[..n].copy_from_slice(&c[o..o + n]);
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        self.saved = vec![PortState::default(); self.ports.len()];
        let h = self.handler()?;
        self.worker.start("console", ctx, h, false)
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
        let ports: Vec<PortState> =
            self.worker.with(|_, h| h.ports.iter().map(|p| p.state.clone()).collect()).unwrap_or_default();
        let data = serde_json::to_vec(&ports).map_err(|e| crate::Error::Device(e.to_string()))?;
        Ok(DeviceState { queues: self.worker.queues(), data })
    }
    fn restore_active(&mut self, ctx: ActivateContext, data: &[u8]) -> Result<()> {
        let ports: Vec<PortState> = serde_json::from_slice(data).map_err(|e| crate::Error::Device(e.to_string()))?;
        if ports.len() != self.ports.len() {
            return Err(crate::Error::Device("console port count mismatch".into()));
        }
        self.saved = ports;
        let h = self.handler()?;
        self.worker.start("console", ctx, h, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_mapping() {
        assert_eq!(queue_port(0), Some((0, false)));
        assert_eq!(queue_port(1), Some((0, true)));
        assert_eq!(queue_port(2), None);
        assert_eq!(queue_port(3), None);
        assert_eq!(queue_port(4), Some((1, false)));
        assert_eq!(queue_port(5), Some((1, true)));
        assert_eq!((port_rx(1), port_tx(1)), (4, 5));
    }
}
