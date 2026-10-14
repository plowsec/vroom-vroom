//! Frames over a unix datagram socket (one datagram = one Ethernet frame).
//!
//! Used for bridged and host-only networking: the other end is the
//! privileged `bulwark-netd`, which owns the real interfaces.

use std::collections::VecDeque;
use std::io;
use std::os::unix::net::UnixDatagram as StdUnixDatagram;

use mio::net::UnixDatagram;
use mio::{Interest, Registry, Token};

use crate::{NetBackend, RESUME_TOKEN, TOKEN_BASE};

const MAX_FRAME: usize = 65536;
const MAX_QUEUED: usize = 1024;

#[derive(Debug)]
pub struct FrameSocket {
    sock: UnixDatagram,
    rx: VecDeque<Vec<u8>>,
}

impl FrameSocket {
    pub fn new(sock: StdUnixDatagram) -> io::Result<Self> {
        sock.set_nonblocking(true)?;
        Ok(FrameSocket { sock: UnixDatagram::from_std(sock), rx: VecDeque::new() })
    }

    fn drain(&mut self) {
        let mut buf = vec![0u8; MAX_FRAME];
        while self.rx.len() < MAX_QUEUED {
            match self.sock.recv(&mut buf) {
                Ok(n) if n >= 14 => self.rx.push_back(buf[..n].to_vec()),
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
}

impl NetBackend for FrameSocket {
    fn send_frame(&mut self, frame: &[u8]) {
        // Dropping on a full socket is fine: Ethernet is lossy.
        let _ = self.sock.send(frame);
    }

    fn recv_frame(&mut self) -> Option<Vec<u8>> {
        if self.rx.is_empty() {
            self.drain();
        }
        self.rx.pop_front()
    }

    fn register(&mut self, reg: &Registry) -> io::Result<()> {
        reg.register(&mut self.sock, Token(TOKEN_BASE), Interest::READABLE)
    }

    fn deregister(&mut self, reg: &Registry) {
        let _ = reg.deregister(&mut self.sock);
    }

    fn ready(&mut self, token: usize) {
        if token == TOKEN_BASE || token == RESUME_TOKEN {
            self.drain();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_pass_through() {
        let (a, b) = StdUnixDatagram::pair().unwrap();
        let mut fs = FrameSocket::new(a).unwrap();
        b.send(&[1u8; 60]).unwrap();
        b.send(&[2u8; 3]).unwrap(); // runt: dropped
        fs.ready(TOKEN_BASE);
        assert_eq!(fs.recv_frame().unwrap(), vec![1u8; 60]);
        assert!(fs.recv_frame().is_none());
        fs.send_frame(&[3u8; 64]);
        let mut buf = [0u8; 100];
        assert_eq!(b.recv(&mut buf).unwrap(), 64);
    }
}
