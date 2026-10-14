//! MSI-X: capability, vector table and pending bits.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::ConfigSpace;

pub const CAP_ID: u8 = 0x11;
const ENTRY: usize = 16;

/// Sends a message-signalled interrupt (VMM implements it on the irqchip).
pub trait MsiSender: Send + Sync {
    fn send(&self, addr: u64, data: u32);
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    addr: u64,
    data: u32,
    ctrl: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Msix {
    table: Vec<Entry>,
    pending: Vec<bool>,
    cap: usize,
    #[serde(skip)]
    sender: Option<Arc<dyn MsiSender>>,
}

impl std::fmt::Debug for Msix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Msix").field("vectors", &self.table.len()).finish()
    }
}

impl Msix {
    /// Add the capability: table at `bar`+`table_off`, PBA at `bar`+`pba_off`.
    pub fn new(cfg: &mut ConfigSpace, vectors: u16, bar: u8, table_off: u32, pba_off: u32, sender: Arc<dyn MsiSender>) -> Self {
        assert!((1..=2048).contains(&vectors));
        let ctrl = vectors - 1;
        let mut body = Vec::new();
        body.extend_from_slice(&ctrl.to_le_bytes());
        body.extend_from_slice(&(table_off | bar as u32).to_le_bytes());
        body.extend_from_slice(&(pba_off | bar as u32).to_le_bytes());
        // Only enable (bit 15) and function mask (bit 14) are writable.
        let mut wmask = vec![0u8; body.len()];
        wmask[1] = 0xc0;
        let cap = cfg.add_capability(CAP_ID, &body, &wmask);
        Msix {
            table: vec![Entry { ctrl: 1, ..Default::default() }; vectors as usize],
            pending: vec![false; vectors as usize],
            cap,
            sender: Some(sender),
        }
    }

    pub fn set_sender(&mut self, s: Arc<dyn MsiSender>) {
        self.sender = Some(s);
    }

    fn control(cfg: &ConfigSpace, cap: usize) -> u16 {
        u16::from_le_bytes([cfg.raw()[cap + 2], cfg.raw()[cap + 3]])
    }

    pub fn enabled(&self, cfg: &ConfigSpace) -> bool {
        Self::control(cfg, self.cap) & 0x8000 != 0
    }

    fn function_masked(&self, cfg: &ConfigSpace) -> bool {
        Self::control(cfg, self.cap) & 0x4000 != 0
    }

    pub fn vectors(&self) -> usize {
        self.table.len()
    }

    pub fn table_size_bytes(&self) -> u64 {
        (self.table.len() * ENTRY) as u64
    }

    pub fn table_read(&self, off: u64, data: &mut [u8]) {
        let (i, f) = (off as usize / ENTRY, off as usize % ENTRY);
        let Some(e) = self.table.get(i) else {
            data.fill(0);
            return;
        };
        let mut raw = [0u8; ENTRY];
        raw[0..8].copy_from_slice(&e.addr.to_le_bytes());
        raw[8..12].copy_from_slice(&e.data.to_le_bytes());
        raw[12..16].copy_from_slice(&e.ctrl.to_le_bytes());
        for (k, b) in data.iter_mut().enumerate() {
            *b = raw.get(f + k).copied().unwrap_or(0);
        }
    }

    pub fn table_write(&mut self, off: u64, data: &[u8], cfg: &ConfigSpace) {
        let (i, f) = (off as usize / ENTRY, off as usize % ENTRY);
        let Some(e) = self.table.get_mut(i) else { return };
        let mut raw = [0u8; ENTRY];
        raw[0..8].copy_from_slice(&e.addr.to_le_bytes());
        raw[8..12].copy_from_slice(&e.data.to_le_bytes());
        raw[12..16].copy_from_slice(&e.ctrl.to_le_bytes());
        for (k, &b) in data.iter().enumerate() {
            if let Some(r) = raw.get_mut(f + k) {
                *r = b;
            }
        }
        e.addr = u64::from_le_bytes(raw[0..8].try_into().unwrap_or([0; 8]));
        e.data = u32::from_le_bytes(raw[8..12].try_into().unwrap_or([0; 4]));
        e.ctrl = u32::from_le_bytes(raw[12..16].try_into().unwrap_or([0; 4])) & 1;
        // Unmasking delivers a pending message.
        self.flush_pending(cfg);
    }

    pub fn pba_read(&self, off: u64, data: &mut [u8]) {
        for (k, b) in data.iter_mut().enumerate() {
            let byte = off as usize + k;
            let mut v = 0u8;
            for bit in 0..8 {
                if self.pending.get(byte * 8 + bit).copied().unwrap_or(false) {
                    v |= 1 << bit;
                }
            }
            *b = v;
        }
    }

    /// Fire vector `v`, or mark it pending if masked.
    pub fn notify(&mut self, v: u16, cfg: &ConfigSpace) {
        let v = v as usize;
        let Some(e) = self.table.get(v).copied() else { return };
        if e.ctrl & 1 != 0 || self.function_masked(cfg) {
            self.pending[v] = true;
            return;
        }
        if let Some(s) = &self.sender {
            s.send(e.addr, e.data);
        }
    }

    /// Config space changed (enable / function mask): deliver what's due.
    pub fn flush_pending(&mut self, cfg: &ConfigSpace) {
        if !self.enabled(cfg) || self.function_masked(cfg) {
            return;
        }
        for v in 0..self.table.len() {
            if self.pending[v] && self.table[v].ctrl & 1 == 0 {
                self.pending[v] = false;
                if let Some(s) = &self.sender {
                    s.send(self.table[v].addr, self.table[v].data);
                }
            }
        }
    }
}
