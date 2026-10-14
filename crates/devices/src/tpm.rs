//! TPM 2.0 behind the TIS (FIFO) interface, memory mapped, no interrupts.
//!
//! Same device QEMU's arm virt board offers (`tcg,tpm-tis-mmio`): EDK2 finds
//! it through the device tree and measures the boot, Windows through the
//! ACPI TPM2 table. Only locality 0 exists; other localities read as
//! all-ones. Commands are run by a [`TpmBackend`] (swtpm in its own
//! process): this side just frames bytes and tracks the TIS state machine.

use bulwark_snapshot::{decode, encode, Snapshottable};
use serde::{Deserialize, Serialize};

use crate::{get_le, put_le, BusDevice};

pub const MMIO_SIZE: u64 = 0x5000;
/// TPM2 commands/responses we accept (TPM2 max is a few KiB).
pub const MAX_BUFFER: usize = 4096;

const ACCESS: u64 = 0x00;
const INT_ENABLE: u64 = 0x08;
const INT_VECTOR: u64 = 0x0c;
const INT_STATUS: u64 = 0x10;
const INTF_CAPABILITY: u64 = 0x14;
const STS: u64 = 0x18;
const DATA_FIFO: u64 = 0x24;
const INTERFACE_ID: u64 = 0x30;
const XDATA_FIFO: u64 = 0x80;
const DID_VID: u64 = 0xf00;
const RID: u64 = 0xf04;

const ACCESS_VALID: u8 = 0x80;
const ACCESS_ACTIVE: u8 = 0x20;
const ACCESS_REQUEST_USE: u8 = 0x02;
const ACCESS_ESTABLISHMENT: u8 = 0x01;

const STS_VALID: u32 = 1 << 7;
const STS_COMMAND_READY: u32 = 1 << 6;
const STS_GO: u32 = 1 << 5;
const STS_DATA_AVAIL: u32 = 1 << 4;
const STS_EXPECT: u32 = 1 << 3;
const STS_RESPONSE_RETRY: u32 = 1 << 1;
const STS_FAMILY_TPM2: u32 = 1 << 26;
const BURST: u32 = 64;

/// Interface version 1.3 for TPM 2.0, 64-byte transfers, polling only.
const CAPABILITY: u32 = (3 << 28) | (3 << 9);
/// FIFO interface, TIS supported, locality 0 only.
const IFACE_ID: u32 = 1 << 13;

/// Runs TPM commands. The VMM implements it over swtpm.
pub trait TpmBackend: Send {
    fn execute(&mut self, cmd: &[u8]) -> Vec<u8>;
    /// Platform reset (power cycle the TPM).
    fn reset(&mut self);
    /// Make the TPM state durable and return it (snapshots, suspend).
    fn save(&mut self) -> Vec<u8> {
        Vec::new()
    }
    /// Go back to a saved state. False if it can't.
    fn load(&mut self, _state: &[u8]) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum State {
    Idle,
    Ready,
    Receiving,
    Completion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Snap {
    state: State,
    active: bool,
    buf: Vec<u8>,
    pos: usize,
    /// The TPM's own state (NV + volatile), from the backend.
    backend: Vec<u8>,
}

pub struct Tpm {
    base: u64,
    backend: Box<dyn TpmBackend>,
    state: State,
    active: bool,
    /// Command being received, or response being read.
    buf: Vec<u8>,
    pos: usize,
}

impl std::fmt::Debug for Tpm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tpm").field("state", &self.state).finish()
    }
}

/// TPM_RC_FAILURE response, for commands we refuse.
fn failure() -> Vec<u8> {
    let mut r = vec![0x80, 0x01, 0, 0, 0, 10];
    r.extend_from_slice(&0x101u32.to_be_bytes());
    r
}

impl Tpm {
    pub fn new(base: u64, backend: Box<dyn TpmBackend>) -> Self {
        Tpm { base, backend, state: State::Idle, active: false, buf: Vec::new(), pos: 0 }
    }

    /// Bytes the command header says it has, once we have the header.
    fn expected_len(&self) -> Option<usize> {
        (self.buf.len() >= 6).then(|| u32::from_be_bytes([self.buf[2], self.buf[3], self.buf[4], self.buf[5]]) as usize)
    }

    fn sts(&self) -> u32 {
        let mut s = STS_VALID | STS_FAMILY_TPM2;
        match self.state {
            State::Idle => {}
            State::Ready => s |= STS_COMMAND_READY | (BURST << 8),
            State::Receiving => {
                s |= BURST << 8;
                if self.expected_len().is_none_or(|n| self.buf.len() < n) {
                    s |= STS_EXPECT;
                }
            }
            State::Completion => {
                let left = self.buf.len().saturating_sub(self.pos);
                if left > 0 {
                    s |= STS_DATA_AVAIL | ((left.min(BURST as usize) as u32) << 8);
                }
            }
        }
        s
    }

    fn write_fifo(&mut self, data: &[u8]) {
        if self.state == State::Ready {
            self.state = State::Receiving;
            self.buf.clear();
        }
        if self.state != State::Receiving {
            return;
        }
        for &b in data {
            if self.buf.len() >= MAX_BUFFER || self.expected_len().is_some_and(|n| self.buf.len() >= n) {
                break; // overrun: ignored, the command fails on Go
            }
            self.buf.push(b);
        }
    }

    fn go(&mut self) {
        if self.state != State::Receiving {
            return;
        }
        let ok = self.expected_len().is_some_and(|n| n >= 10 && n == self.buf.len());
        let rsp = if ok { self.backend.execute(&self.buf) } else { failure() };
        self.buf = if rsp.len() >= 10 && rsp.len() <= MAX_BUFFER { rsp } else { failure() };
        self.pos = 0;
        self.state = State::Completion;
    }

    fn write_sts(&mut self, v: u32) {
        if !self.active {
            return;
        }
        if v & STS_COMMAND_READY != 0 {
            // Abort whatever was going on and get ready for a command.
            self.buf.clear();
            self.pos = 0;
            self.state = State::Ready;
        } else if v & STS_GO != 0 {
            self.go();
        } else if v & STS_RESPONSE_RETRY != 0 && self.state == State::Completion {
            self.pos = 0;
        }
    }

    fn read_fifo(&mut self, out: &mut [u8]) {
        for b in out.iter_mut() {
            *b = if self.state == State::Completion && self.pos < self.buf.len() {
                self.pos += 1;
                self.buf[self.pos - 1]
            } else {
                0xff
            };
        }
    }
}

impl BusDevice for Tpm {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= 0x1000 {
            data.fill(0xff); // localities 1..4 don't exist
            return;
        }
        let v: u64 = match offset {
            ACCESS => (ACCESS_VALID | ACCESS_ESTABLISHMENT | if self.active { ACCESS_ACTIVE } else { 0 }) as u64,
            INT_ENABLE | INT_VECTOR | INT_STATUS => 0,
            INTF_CAPABILITY => CAPABILITY as u64,
            o if (STS..STS + 4).contains(&o) => (self.sts() >> (8 * (o - STS))) as u64,
            o if (DATA_FIFO..DATA_FIFO + 4).contains(&o) || (XDATA_FIFO..XDATA_FIFO + 0x40).contains(&o) => {
                self.read_fifo(data);
                return;
            }
            INTERFACE_ID => IFACE_ID as u64,
            DID_VID => 0x0001_1014, // IBM, like swtpm reports
            RID => 1,
            _ => 0,
        };
        put_le(data, v);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset >= 0x1000 {
            return;
        }
        let v = get_le(data);
        match offset {
            ACCESS => {
                let v = v as u8;
                if v & ACCESS_REQUEST_USE != 0 {
                    self.active = true;
                }
                if v & ACCESS_ACTIVE != 0 {
                    // Relinquish.
                    self.active = false;
                    self.state = State::Idle;
                }
            }
            o if (STS..STS + 4).contains(&o) => self.write_sts((v << (8 * (o - STS))) as u32),
            o if ((DATA_FIFO..DATA_FIFO + 4).contains(&o) || (XDATA_FIFO..XDATA_FIFO + 0x40).contains(&o))
                && self.active => {
                    self.write_fifo(data);
                }
            _ => {}
        }
    }
}

impl Snapshottable for Tpm {
    fn snapshot_id(&self) -> String {
        format!("tpm@{:x}", self.base)
    }

    fn save(&mut self) -> bulwark_snapshot::Result<Vec<u8>> {
        let backend = self.backend.save();
        encode(&Snap { state: self.state, active: self.active, buf: self.buf.clone(), pos: self.pos, backend })
    }

    fn restore(&mut self, d: &[u8]) -> bulwark_snapshot::Result<()> {
        let s: Snap = decode(d)?;
        if s.buf.len() > MAX_BUFFER || s.pos > s.buf.len() {
            return Err(bulwark_snapshot::Error::Invalid("tpm snapshot".into()));
        }
        if !s.backend.is_empty() && !self.backend.load(&s.backend) {
            return Err(bulwark_snapshot::Error::Invalid("tpm state could not be restored".into()));
        }
        self.state = s.state;
        self.active = s.active;
        self.buf = s.buf;
        self.pos = s.pos;
        Ok(())
    }

    fn reset(&mut self, _pristine: &[u8]) -> bulwark_snapshot::Result<()> {
        // Platform reset: the TPM restarts too (its NV state stays).
        self.backend.reset();
        self.state = State::Idle;
        self.active = false;
        self.buf.clear();
        self.pos = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl TpmBackend for Echo {
        fn execute(&mut self, cmd: &[u8]) -> Vec<u8> {
            let mut r = cmd.to_vec();
            r[0] = 0x80;
            r
        }
        fn reset(&mut self) {}
    }

    fn rd32(t: &mut Tpm, off: u64) -> u32 {
        let mut b = [0u8; 4];
        t.read(off, &mut b);
        u32::from_le_bytes(b)
    }

    #[test]
    fn command_round_trip() {
        let mut t = Tpm::new(0, Box::new(Echo));
        t.write(ACCESS, &[ACCESS_REQUEST_USE]);
        t.write(STS, &STS_COMMAND_READY.to_le_bytes());
        assert!(rd32(&mut t, STS) & STS_COMMAND_READY != 0);
        let cmd = [0x80, 0x01, 0, 0, 0, 12, 0, 0, 1, 0x44, 0, 0];
        t.write(DATA_FIFO, &cmd[..4]);
        assert!(rd32(&mut t, STS) & STS_EXPECT != 0);
        for &b in &cmd[4..] {
            t.write(DATA_FIFO, &[b]);
        }
        assert_eq!(rd32(&mut t, STS) & STS_EXPECT, 0);
        t.write(STS, &STS_GO.to_le_bytes());
        assert!(rd32(&mut t, STS) & STS_DATA_AVAIL != 0);
        let mut out = [0u8; 12];
        for b in out.iter_mut() {
            let mut x = [0u8];
            t.read(DATA_FIFO, &mut x);
            *b = x[0];
        }
        assert_eq!(&out[2..], &cmd[2..]);
        assert_eq!(rd32(&mut t, STS) & STS_DATA_AVAIL, 0);
    }

    #[test]
    fn hostile_lengths_fail_cleanly() {
        let mut t = Tpm::new(0, Box::new(Echo));
        t.write(ACCESS, &[ACCESS_REQUEST_USE]);
        t.write(STS, &STS_COMMAND_READY.to_le_bytes());
        // Header claims 1 MiB; we stop at MAX_BUFFER and fail on Go.
        t.write(DATA_FIFO, &[0x80, 1, 0, 0x10, 0, 0]);
        for _ in 0..(MAX_BUFFER + 100) {
            t.write(DATA_FIFO, &[0]);
        }
        t.write(STS, &STS_GO.to_le_bytes());
        let mut rc = [0u8; 10];
        t.read(DATA_FIFO, &mut rc[..4]);
        t.read(DATA_FIFO, &mut rc[4..8]);
        t.read(DATA_FIFO, &mut rc[8..10]);
        assert_eq!(&rc[6..10], &0x101u32.to_be_bytes());
    }
}
