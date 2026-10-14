//! Descriptor chains and safe byte-stream views over them.

use std::collections::VecDeque;

use bulwark_memory::{ByteValued, GuestAddress, GuestMemory};

use crate::queue::{VIRTQ_DESC_F_INDIRECT, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use crate::Error;

/// Hard cap on a single request's total buffer size (guards host allocations).
pub const MAX_CHAIN_BYTES: u64 = 256 << 20;
/// Hard cap on indirect table entries.
const MAX_INDIRECT: u32 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub writable: bool,
}

/// A chain of descriptors starting at `head`. Iterate to walk it.
#[derive(Debug)]
pub struct DescriptorChain {
    mem: GuestMemory,
    table: u64,
    table_len: u32,
    head: u16,
    next: Option<u16>,
    /// Steps left before we call it a loop.
    ttl: u32,
    indirect_allowed: bool,
    in_indirect: bool,
}

impl DescriptorChain {
    pub(crate) fn new(mem: GuestMemory, table: u64, size: u16, head: u16, indirect: bool) -> Self {
        DescriptorChain {
            mem,
            table,
            table_len: size as u32,
            head,
            next: Some(head),
            ttl: size as u32,
            indirect_allowed: indirect,
            in_indirect: false,
        }
    }

    pub fn head(&self) -> u16 {
        self.head
    }

    pub fn memory(&self) -> &GuestMemory {
        &self.mem
    }

    fn read_desc(&self, i: u16) -> Result<(u64, u32, u16, u16), Error> {
        if i as u32 >= self.table_len {
            return Err(Error::Chain(format!("descriptor index {i} out of range")));
        }
        let at = self.table + 16 * i as u64;
        let addr: u64 = self.mem.read_obj(GuestAddress(at))?;
        let len: u32 = self.mem.read_obj(GuestAddress(at + 8))?;
        let flags: u16 = self.mem.read_obj(GuestAddress(at + 12))?;
        let next: u16 = self.mem.read_obj(GuestAddress(at + 14))?;
        Ok((u64::from_le(addr), u32::from_le(len), u16::from_le(flags), u16::from_le(next)))
    }

    fn step(&mut self) -> Result<Option<Descriptor>, Error> {
        let Some(i) = self.next else { return Ok(None) };
        if self.ttl == 0 {
            return Err(Error::Chain("descriptor chain loops or is too long".into()));
        }
        self.ttl -= 1;
        let (addr, len, flags, next) = self.read_desc(i)?;
        if flags & VIRTQ_DESC_F_INDIRECT != 0 {
            if !self.indirect_allowed || self.in_indirect {
                return Err(Error::Chain("indirect descriptor not allowed here".into()));
            }
            if flags & VIRTQ_DESC_F_NEXT != 0 {
                return Err(Error::Chain("indirect descriptor with NEXT".into()));
            }
            if len == 0 || len % 16 != 0 || len / 16 > MAX_INDIRECT {
                return Err(Error::Chain(format!("bad indirect table length {len}")));
            }
            if !self.mem.check_range(GuestAddress(addr), len as u64) {
                return Err(Error::Chain("indirect table outside RAM".into()));
            }
            // Switch to walking the indirect table from entry 0.
            self.table = addr;
            self.table_len = len / 16;
            self.ttl = self.table_len;
            self.in_indirect = true;
            self.next = Some(0);
            return self.step();
        }
        self.next = (flags & VIRTQ_DESC_F_NEXT != 0).then_some(next);
        Ok(Some(Descriptor { addr, len, writable: flags & VIRTQ_DESC_F_WRITE != 0 }))
    }

    /// Walk the whole chain and split it into device-readable and
    /// device-writable parts. Readable must all come before writable.
    pub fn into_rw(mut self) -> Result<(Reader, Writer), Error> {
        let mut r = VecDeque::new();
        let mut w = VecDeque::new();
        let mut total = 0u64;
        while let Some(d) = self.step()? {
            total += d.len as u64;
            if total > MAX_CHAIN_BYTES {
                return Err(Error::Chain("request too large".into()));
            }
            if d.len == 0 {
                continue;
            }
            if d.writable {
                w.push_back((d.addr, d.len));
            } else {
                if !w.is_empty() {
                    return Err(Error::Chain("readable descriptor after writable".into()));
                }
                r.push_back((d.addr, d.len));
            }
        }
        Ok((
            Reader { mem: self.mem.clone(), bufs: r, consumed: 0 },
            Writer { mem: self.mem, bufs: w, written: 0 },
        ))
    }
}

impl Iterator for DescriptorChain {
    type Item = Result<Descriptor, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        self.step().transpose()
    }
}

/// Reads the driver -> device part of a request as a byte stream.
#[derive(Debug)]
pub struct Reader {
    mem: GuestMemory,
    bufs: VecDeque<(u64, u32)>,
    consumed: usize,
}

impl Reader {
    pub fn available(&self) -> usize {
        self.bufs.iter().map(|&(_, l)| l as usize).sum()
    }

    pub fn consumed(&self) -> usize {
        self.consumed
    }

    /// Fill `out` completely or fail.
    pub fn read_exact(&mut self, out: &mut [u8]) -> Result<(), Error> {
        if out.len() > self.available() {
            return Err(Error::Chain("request shorter than expected".into()));
        }
        let mut done = 0;
        while done < out.len() {
            let Some((addr, len)) = self.bufs.front_mut() else { break };
            let n = (*len as usize).min(out.len() - done);
            self.mem.read(GuestAddress(*addr), &mut out[done..done + n])?;
            *addr += n as u64;
            *len -= n as u32;
            if *len == 0 {
                self.bufs.pop_front();
            }
            done += n;
        }
        self.consumed += done;
        Ok(())
    }

    pub fn read_obj<T: ByteValued>(&mut self) -> Result<T, Error> {
        let mut v = T::default();
        self.read_exact(v.as_bytes_mut())?;
        Ok(v)
    }

    /// Read everything left (bounded by MAX_CHAIN_BYTES via into_rw).
    pub fn read_to_vec(&mut self) -> Result<Vec<u8>, Error> {
        let mut v = vec![0u8; self.available()];
        self.read_exact(&mut v)?;
        Ok(v)
    }

    /// Hand out the remaining (addr, len) segments, e.g. for direct disk I/O.
    pub fn take_segments(&mut self) -> Vec<(GuestAddress, usize)> {
        let s = self.bufs.drain(..).map(|(a, l)| (GuestAddress(a), l as usize)).collect::<Vec<_>>();
        self.consumed += s.iter().map(|&(_, l)| l).sum::<usize>();
        s
    }
}

/// Writes the device -> driver part of a request.
#[derive(Debug)]
pub struct Writer {
    mem: GuestMemory,
    bufs: VecDeque<(u64, u32)>,
    written: usize,
}

impl Writer {
    pub fn available(&self) -> usize {
        self.bufs.iter().map(|&(_, l)| l as usize).sum()
    }

    /// Bytes written so far (goes in the used ring `len`).
    pub fn written(&self) -> usize {
        self.written
    }

    pub fn write_all(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.len() > self.available() {
            return Err(Error::Chain("response buffer too small".into()));
        }
        let mut done = 0;
        while done < data.len() {
            let Some((addr, len)) = self.bufs.front_mut() else { break };
            let n = (*len as usize).min(data.len() - done);
            self.mem.write(GuestAddress(*addr), &data[done..done + n])?;
            *addr += n as u64;
            *len -= n as u32;
            if *len == 0 {
                self.bufs.pop_front();
            }
            done += n;
        }
        self.written += done;
        Ok(())
    }

    pub fn write_obj<T: ByteValued>(&mut self, v: &T) -> Result<(), Error> {
        self.write_all(v.as_bytes())
    }

    /// Split off the last `n` bytes as a separate writer (e.g. virtio-blk
    /// status byte lives at the very end).
    pub fn split_tail(&mut self, n: usize) -> Result<Writer, Error> {
        let total = self.available();
        if n > total {
            return Err(Error::Chain("buffer too small for trailer".into()));
        }
        let mut keep = total - n;
        let mut head = VecDeque::new();
        let mut tail = VecDeque::new();
        for &(a, l) in &self.bufs {
            let l = l as usize;
            if keep >= l {
                head.push_back((a, l as u32));
                keep -= l;
            } else {
                if keep > 0 {
                    head.push_back((a, keep as u32));
                }
                tail.push_back((a + keep as u64, (l - keep) as u32));
                keep = 0;
            }
        }
        self.bufs = head;
        Ok(Writer { mem: self.mem.clone(), bufs: tail, written: 0 })
    }

    pub fn take_segments(&mut self) -> Vec<(GuestAddress, usize)> {
        let s = self.bufs.drain(..).map(|(a, l)| (GuestAddress(a), l as usize)).collect::<Vec<_>>();
        self.written += s.iter().map(|&(_, l)| l).sum::<usize>();
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::tests::*;

    #[test]
    fn reader_writer_roundtrip() {
        let mut r = TestRing::new(8);
        r.add_chain(&[(4, false), (6, false), (8, true), (1, true)]);
        r.mem.write(GuestAddress(DATA), b"abcd").unwrap();
        r.mem.write(GuestAddress(DATA + 0x1000), b"efghij").unwrap();
        let mut q = r.q.clone();
        let (mut rd, mut wr) = q.pop(&r.mem).unwrap().unwrap().into_rw().unwrap();
        assert_eq!(rd.available(), 10);
        let mut b = [0u8; 7];
        rd.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"abcdefg");
        assert!(rd.read_exact(&mut [0u8; 4]).is_err());
        let mut status = wr.split_tail(1).unwrap();
        wr.write_all(b"12345678").unwrap();
        assert!(wr.write_all(b"x").is_err());
        status.write_all(&[0]).unwrap();
        let mut out = [0u8; 8];
        r.mem.read(GuestAddress(DATA + 0x2000), &mut out).unwrap();
        assert_eq!(&out, b"12345678");
    }

    #[test]
    fn loop_detected() {
        let mut r = TestRing::new(4);
        // 0 -> 1 -> 0 -> ...
        r.write_desc(0, DATA, 4, VIRTQ_DESC_F_NEXT, 1);
        r.write_desc(1, DATA, 4, VIRTQ_DESC_F_NEXT, 0);
        r.publish(0);
        let c = r.q.clone().pop(&r.mem).unwrap().unwrap();
        assert!(c.into_rw().is_err());
    }

    #[test]
    fn readable_after_writable_rejected() {
        let mut r = TestRing::new(4);
        r.write_desc(0, DATA, 4, VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE, 1);
        r.write_desc(1, DATA, 4, 0, 0);
        r.publish(0);
        assert!(r.q.clone().pop(&r.mem).unwrap().unwrap().into_rw().is_err());
    }

    #[test]
    fn indirect_rules() {
        let mut r = TestRing::new(4);
        // indirect table at DATA+0x8000 with 2 entries
        let t = DATA + 0x8000;
        let mem = r.mem.clone();
        let w = |i: u64, addr: u64, len: u32, flags: u16, next: u16| {
            mem.write_obj(GuestAddress(t + 16 * i), &addr).unwrap();
            mem.write_obj(GuestAddress(t + 16 * i + 8), &len).unwrap();
            mem.write_obj(GuestAddress(t + 16 * i + 12), &flags).unwrap();
            mem.write_obj(GuestAddress(t + 16 * i + 14), &next).unwrap();
        };
        w(0, DATA, 4, VIRTQ_DESC_F_NEXT, 1);
        w(1, DATA + 0x100, 8, VIRTQ_DESC_F_WRITE, 0);
        r.write_desc(0, t, 32, VIRTQ_DESC_F_INDIRECT, 0);
        r.publish(0);
        // Not negotiated -> rejected.
        assert!(r.q.clone().pop(&r.mem).unwrap().unwrap().into_rw().is_err());
        let mut q = r.q.clone();
        q.indirect = true;
        let (rd, wr) = q.pop(&r.mem).unwrap().unwrap().into_rw().unwrap();
        assert_eq!((rd.available(), wr.available()), (4, 8));
        // Nested indirect -> rejected.
        w(1, t, 16, VIRTQ_DESC_F_INDIRECT, 0);
        let mut q = r.q.clone();
        q.indirect = true;
        assert!(q.pop(&r.mem).unwrap().unwrap().into_rw().is_err());
    }

    #[test]
    fn descriptor_outside_ram_fails_on_access() {
        let mut r = TestRing::new(4);
        r.write_desc(0, u64::MAX - 2, 16, 0, 0);
        r.publish(0);
        let (mut rd, _) = r.q.clone().pop(&r.mem).unwrap().unwrap().into_rw().unwrap();
        assert!(rd.read_exact(&mut [0u8; 16]).is_err());
    }
}
