//! Split virtqueue (virtio 1.x, section 2.7).
//!
//! The guest owns the rings and can scribble on them at any time. Every
//! index read from guest memory is range-checked, chains are bounded, and
//! nothing here panics on bad input: errors drop the request.

use std::num::Wrapping;

use bulwark_memory::{GuestAddress, GuestMemory};
use serde::{Deserialize, Serialize};

use crate::chain::DescriptorChain;
use crate::Error;

pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;
const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Biggest queue we ever offer.
pub const MAX_QUEUE_SIZE: u16 = 1024;

/// Queue configuration + progress. Serializable for snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    pub max_size: u16,
    pub size: u16,
    pub ready: bool,
    pub desc_table: u64,
    pub avail_ring: u64,
    pub used_ring: u64,
    pub next_avail: u16,
    pub next_used: u16,
    /// VIRTIO_RING_F_EVENT_IDX negotiated.
    pub event_idx: bool,
    /// VIRTIO_RING_F_INDIRECT_DESC negotiated.
    pub indirect: bool,
    /// Used idx at the last interrupt (event idx suppression).
    #[serde(default)]
    pub signalled_used: Option<u16>,
}

impl Queue {
    pub fn new(max_size: u16) -> Self {
        Queue { max_size, size: max_size, ..Default::default() }
    }

    /// Back to the state after device reset (config kept: max_size).
    pub fn reset(&mut self) {
        *self = Queue::new(self.max_size);
    }

    /// Everything the driver configured makes sense?
    pub fn is_valid(&self, mem: &GuestMemory) -> bool {
        let n = self.size as u64;
        if !self.ready || self.size == 0 || self.size > self.max_size || !self.size.is_power_of_two() {
            return false;
        }
        let desc_len = 16 * n;
        let avail_len = 6 + 2 * n;
        let used_len = 6 + 8 * n;
        self.desc_table.is_multiple_of(16)
            && self.avail_ring.is_multiple_of(2)
            && self.used_ring.is_multiple_of(4)
            && mem.check_range(GuestAddress(self.desc_table), desc_len)
            && mem.check_range(GuestAddress(self.avail_ring), avail_len)
            && mem.check_range(GuestAddress(self.used_ring), used_len)
    }

    fn avail_idx(&self, mem: &GuestMemory) -> Result<u16, Error> {
        Ok(mem.load_u16_acquire(GuestAddress(self.avail_ring + 2))?)
    }

    /// Number of buffers the driver has made available and we haven't taken.
    pub fn pending(&self, mem: &GuestMemory) -> Result<u16, Error> {
        let idx = self.avail_idx(mem)?;
        let n = (Wrapping(idx) - Wrapping(self.next_avail)).0;
        if n > self.size {
            // Driver claims more buffers than the ring holds: broken or hostile.
            return Err(Error::Queue(format!("avail idx jumped by {n} (size {})", self.size)));
        }
        Ok(n)
    }

    /// Take the next available descriptor chain.
    pub fn pop(&mut self, mem: &GuestMemory) -> Result<Option<DescriptorChain>, Error> {
        if self.pending(mem)? == 0 {
            return Ok(None);
        }
        let slot = self.next_avail % self.size;
        let head: u16 = mem.read_obj(GuestAddress(self.avail_ring + 4 + 2 * slot as u64))?;
        let head = u16::from_le(head);
        self.next_avail = self.next_avail.wrapping_add(1);
        if self.event_idx {
            // Ask to be notified when the driver adds past what we've seen.
            let at = GuestAddress(self.used_ring + 4 + 8 * self.size as u64);
            mem.write_obj(at, &self.next_avail.to_le())?;
        }
        if head >= self.size {
            return Err(Error::Queue(format!("head index {head} >= queue size {}", self.size)));
        }
        Ok(Some(DescriptorChain::new(mem.clone(), self.desc_table, self.size, head, self.indirect)))
    }

    /// Hand a buffer back. `len` = bytes the device wrote into it.
    pub fn add_used(&mut self, mem: &GuestMemory, head: u16, len: u32) -> Result<(), Error> {
        let slot = self.next_used % self.size;
        let at = GuestAddress(self.used_ring + 4 + 8 * slot as u64);
        mem.write_obj(at, &(head as u32).to_le())?;
        mem.write_obj(GuestAddress(at.0 + 4), &len.to_le())?;
        self.next_used = self.next_used.wrapping_add(1);
        // Release: ring entry is visible before the index moves.
        mem.store_u16_release(GuestAddress(self.used_ring + 2), self.next_used)?;
        Ok(())
    }

    /// After add_used: should we interrupt the driver?
    pub fn needs_notification(&mut self, mem: &GuestMemory) -> Result<bool, Error> {
        // Make our used idx store visible before reading the driver's flags.
        mem.full_fence();
        let used = self.next_used;
        if !self.event_idx {
            let flags: u16 = u16::from_le(mem.read_obj(GuestAddress(self.avail_ring))?);
            return Ok(flags & VIRTQ_AVAIL_F_NO_INTERRUPT == 0);
        }
        let used_event: u16 =
            u16::from_le(mem.read_obj(GuestAddress(self.avail_ring + 4 + 2 * self.size as u64))?);
        let old = self.signalled_used.replace(used);
        Ok(match old {
            None => true,
            // vring_need_event()
            Some(old) => (Wrapping(used) - Wrapping(used_event) - Wrapping(1)) < (Wrapping(used) - Wrapping(old)),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Minimal driver side, for tests.
    pub struct TestRing {
        pub mem: GuestMemory,
        pub q: Queue,
        avail_idx: u16,
        next_desc: u16,
    }

    pub const DESC: u64 = 0x1000;
    pub const AVAIL: u64 = 0x2000;
    pub const USED: u64 = 0x3000;
    pub const DATA: u64 = 0x10000;

    impl TestRing {
        pub fn new(size: u16) -> Self {
            let mem = GuestMemory::new(&[(GuestAddress(0), 1 << 20)]).unwrap();
            let q = Queue {
                max_size: size,
                size,
                ready: true,
                desc_table: DESC,
                avail_ring: AVAIL,
                used_ring: USED,
                ..Default::default()
            };
            TestRing { mem, q, avail_idx: 0, next_desc: 0 }
        }

        pub fn write_desc(&self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
            let at = DESC + 16 * i as u64;
            self.mem.write_obj(GuestAddress(at), &addr).unwrap();
            self.mem.write_obj(GuestAddress(at + 8), &len).unwrap();
            self.mem.write_obj(GuestAddress(at + 12), &flags).unwrap();
            self.mem.write_obj(GuestAddress(at + 14), &next).unwrap();
        }

        /// Add a chain of (len, writable) buffers; returns head.
        pub fn add_chain(&mut self, bufs: &[(u32, bool)]) -> u16 {
            let head = self.next_desc;
            for (k, &(len, w)) in bufs.iter().enumerate() {
                let i = self.next_desc;
                let last = k + 1 == bufs.len();
                let mut flags = if w { VIRTQ_DESC_F_WRITE } else { 0 };
                if !last {
                    flags |= VIRTQ_DESC_F_NEXT;
                }
                self.write_desc(i, DATA + 0x1000 * i as u64, len, flags, i + 1);
                self.next_desc += 1;
            }
            self.publish(head);
            head
        }

        pub fn publish(&mut self, head: u16) {
            let slot = self.avail_idx % self.q.size;
            self.mem.write_obj(GuestAddress(AVAIL + 4 + 2 * slot as u64), &head).unwrap();
            self.avail_idx = self.avail_idx.wrapping_add(1);
            self.mem.write_obj(GuestAddress(AVAIL + 2), &self.avail_idx).unwrap();
        }

        pub fn used(&self, i: u16) -> (u32, u32) {
            let at = USED + 4 + 8 * i as u64;
            (self.mem.read_obj(GuestAddress(at)).unwrap(), self.mem.read_obj(GuestAddress(at + 4)).unwrap())
        }

        pub fn used_idx(&self) -> u16 {
            self.mem.read_obj(GuestAddress(USED + 2)).unwrap()
        }
    }

    #[test]
    fn pop_and_add_used() {
        let mut r = TestRing::new(8);
        assert!(r.q.is_valid(&r.mem));
        let h = r.add_chain(&[(16, false), (32, true)]);
        let mut q = r.q.clone();
        let chain = q.pop(&r.mem).unwrap().unwrap();
        assert_eq!(chain.head(), h);
        assert!(q.pop(&r.mem).unwrap().is_none());
        q.add_used(&r.mem, h, 32).unwrap();
        assert_eq!(r.used_idx(), 1);
        assert_eq!(r.used(0), (h as u32, 32));
        assert!(q.needs_notification(&r.mem).unwrap());
    }

    #[test]
    fn invalid_configs() {
        let r = TestRing::new(8);
        let mut q = r.q.clone();
        q.size = 6;
        assert!(!q.is_valid(&r.mem));
        q.size = 8;
        q.desc_table = 0x1001;
        assert!(!q.is_valid(&r.mem));
        q.desc_table = (1 << 20) - 16;
        assert!(!q.is_valid(&r.mem));
    }

    #[test]
    fn hostile_avail_idx_and_head() {
        let r = TestRing::new(8);
        // Avail idx says 100 new buffers in an 8-entry ring.
        r.mem.write_obj(GuestAddress(AVAIL + 2), &100u16).unwrap();
        assert!(r.q.clone().pop(&r.mem).is_err());
        // Head index out of range.
        let mut r = TestRing::new(8);
        r.publish(9);
        assert!(r.q.clone().pop(&r.mem).is_err());
    }

    #[test]
    fn event_idx_suppression() {
        let mut r = TestRing::new(8);
        r.q.event_idx = true;
        let mut q = r.q.clone();
        // Driver wants an interrupt only after used idx passes 5.
        r.mem.write_obj(GuestAddress(AVAIL + 4 + 16), &5u16).unwrap();
        for i in 0..7u16 {
            r.add_chain(&[(4, true)]);
            let c = q.pop(&r.mem).unwrap().unwrap();
            q.add_used(&r.mem, c.head(), 0).unwrap();
            let n = q.needs_notification(&r.mem).unwrap();
            // first one always notifies; then only when crossing 5 -> 6
            assert_eq!(n, i == 0 || i == 5, "i={i}");
        }
    }
}
