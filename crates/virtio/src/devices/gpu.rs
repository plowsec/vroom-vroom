//! virtio-gpu, 2D only (3D comes via a separate renderer process).
//!
//! The guest creates resources, attaches guest-RAM backing, transfers
//! pixels into our host copy, and flushes rects to the scanout. We push
//! flushed rects to a [`DisplaySink`] (the shared framebuffer).
//!
//! Hostile-guest limits: total resource memory, resource count, backing
//! entries, and every rect/offset are checked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bulwark_memory::{GuestAddress, GuestMemory};

use crate::worker::{QueueCtx, QueueHandler, WorkerSlot};
use crate::{types, ActivateContext, DeviceState, Reader, Result, VirtioDevice, VirtioInterrupt, Writer};

const GET_DISPLAY_INFO: u32 = 0x0100;
const RESOURCE_CREATE_2D: u32 = 0x0101;
const RESOURCE_UNREF: u32 = 0x0102;
const SET_SCANOUT: u32 = 0x0103;
const RESOURCE_FLUSH: u32 = 0x0104;
const TRANSFER_TO_HOST_2D: u32 = 0x0105;
const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const RESOURCE_DETACH_BACKING: u32 = 0x0107;
const UPDATE_CURSOR: u32 = 0x0300;
const MOVE_CURSOR: u32 = 0x0301;

const OK_NODATA: u32 = 0x1100;
const OK_DISPLAY_INFO: u32 = 0x1101;
const ERR_UNSPEC: u32 = 0x1200;
const ERR_OUT_OF_MEMORY: u32 = 0x1201;
const ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const ERR_INVALID_PARAMETER: u32 = 0x1205;

const FLAG_FENCE: u32 = 1;
const EVENT_DISPLAY: u32 = 1;

const MAX_RESOURCE_BYTES: usize = 512 << 20;
const MAX_RESOURCES: usize = 4096;
const MAX_BACKING_ENTRIES: u32 = 16384;
const MAX_DIM: u32 = 16384;

/// Where finished frames go.
pub trait DisplaySink: Send + Sync {
    /// Scanout size changed (or was disabled: 0x0).
    fn set_mode(&self, width: u32, height: u32);
    /// `rows` holds `h` rows of `w` BGRX pixels, `stride` bytes apart.
    fn flush(&self, x: u32, y: u32, w: u32, h: u32, rows: &[u8], stride: usize);
}

/// Display size the host wants; read by GET_DISPLAY_INFO.
struct Shared {
    width: u32,
    height: u32,
    events_read: u32,
    irq: Option<Arc<dyn VirtioInterrupt>>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").field("width", &self.width).field("height", &self.height).finish()
    }
}

/// Handle the frontend uses to resize the virtual monitor.
#[derive(Debug, Clone)]
pub struct GpuControl(Arc<Mutex<Shared>>);

impl GpuControl {
    /// The window changed size: tell the guest the monitor did too.
    pub fn resize(&self, width: u32, height: u32) {
        if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
            return;
        }
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if (s.width, s.height) == (width, height) {
            return;
        }
        s.width = width;
        s.height = height;
        s.events_read |= EVENT_DISPLAY;
        if let Some(irq) = &s.irq {
            irq.signal_config_changed();
        }
    }
}

pub struct Gpu {
    shared: Arc<Mutex<Shared>>,
    sink: Arc<dyn DisplaySink>,
    worker: WorkerSlot<GpuHandler>,
}

impl std::fmt::Debug for Gpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gpu").finish_non_exhaustive()
    }
}

impl Gpu {
    pub fn new(width: u32, height: u32, sink: Arc<dyn DisplaySink>) -> (Self, GpuControl) {
        let shared = Arc::new(Mutex::new(Shared { width, height, events_read: 0, irq: None }));
        (Gpu { shared: shared.clone(), sink, worker: WorkerSlot::default() }, GpuControl(shared))
    }

    fn handler(&self) -> GpuHandler {
        GpuHandler {
            shared: self.shared.clone(),
            sink: self.sink.clone(),
            resources: HashMap::new(),
            used_bytes: 0,
            scanout: None,
            mode: (0, 0),
        }
    }
}

#[derive(Debug)]
struct Resource {
    format: u32,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    backing: Vec<(u64, u32)>,
}

pub struct GpuHandler {
    shared: Arc<Mutex<Shared>>,
    sink: Arc<dyn DisplaySink>,
    resources: HashMap<u32, Resource>,
    used_bytes: usize,
    /// (resource id, visible rect) on scanout 0.
    scanout: Option<(u32, Rect)>,
    mode: (u32, u32),
}

impl std::fmt::Debug for GpuHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuHandler").field("resources", &self.resources.len()).finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl Rect {
    fn read(r: &mut Reader) -> Option<Rect> {
        Some(Rect { x: rd32(r)?, y: rd32(r)?, w: rd32(r)?, h: rd32(r)? })
    }

    /// Inside a w x h surface (no overflow)?
    fn inside(&self, w: u32, h: u32) -> bool {
        self.x.checked_add(self.w).is_some_and(|e| e <= w) && self.y.checked_add(self.h).is_some_and(|e| e <= h)
    }
}

fn rd32(r: &mut Reader) -> Option<u32> {
    r.read_obj::<u32>().ok().map(u32::from_le)
}

fn rd64(r: &mut Reader) -> Option<u64> {
    r.read_obj::<u64>().ok().map(u64::from_le)
}

/// For each output byte (B, G, R, X), which source byte to take.
/// Format names give the in-memory byte order. None = unsupported.
fn swizzle(format: u32) -> Option<[usize; 4]> {
    match format {
        1 | 2 => Some([0, 1, 2, 3]),    // B8G8R8A8 / B8G8R8X8
        3 | 4 => Some([3, 2, 1, 0]),    // A8R8G8B8 / X8R8G8B8
        67 | 134 => Some([2, 1, 0, 3]), // R8G8B8A8 / R8G8B8X8
        68 | 121 => Some([1, 2, 3, 0]), // X8B8G8R8 / A8B8G8R8
        _ => None,
    }
}

struct Hdr {
    ty: u32,
    flags: u32,
    fence: u64,
    ctx: u32,
}

impl GpuHandler {
    fn respond(w: &mut Writer, hdr: &Hdr, ty: u32, body: &[u8]) {
        let mut out = Vec::with_capacity(24 + body.len());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&(hdr.flags & FLAG_FENCE).to_le_bytes());
        out.extend_from_slice(&(if hdr.flags & FLAG_FENCE != 0 { hdr.fence } else { 0 }).to_le_bytes());
        out.extend_from_slice(&hdr.ctx.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(body);
        let _ = w.write_all(&out);
    }

    fn command(&mut self, mem: &GuestMemory, r: &mut Reader, hdr: &Hdr) -> (u32, Vec<u8>) {
        match hdr.ty {
            GET_DISPLAY_INFO => {
                let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                let mut b = vec![0u8; 16 * 24];
                b[8..12].copy_from_slice(&s.width.to_le_bytes());
                b[12..16].copy_from_slice(&s.height.to_le_bytes());
                b[16..20].copy_from_slice(&1u32.to_le_bytes()); // enabled
                (OK_DISPLAY_INFO, b)
            }
            RESOURCE_CREATE_2D => (self.create(r), vec![]),
            RESOURCE_UNREF => {
                let Some(id) = rd32(r) else { return (ERR_INVALID_PARAMETER, vec![]) };
                match self.resources.remove(&id) {
                    Some(res) => {
                        self.used_bytes -= res.pixels.len();
                        if self.scanout.map(|s| s.0) == Some(id) {
                            self.scanout = None;
                        }
                        (OK_NODATA, vec![])
                    }
                    None => (ERR_INVALID_RESOURCE_ID, vec![]),
                }
            }
            SET_SCANOUT => (self.set_scanout(r), vec![]),
            RESOURCE_FLUSH => (self.flush(r), vec![]),
            TRANSFER_TO_HOST_2D => (self.transfer(mem, r), vec![]),
            RESOURCE_ATTACH_BACKING => (self.attach(mem, r), vec![]),
            RESOURCE_DETACH_BACKING => {
                let Some(id) = rd32(r) else { return (ERR_INVALID_PARAMETER, vec![]) };
                match self.resources.get_mut(&id) {
                    Some(res) => {
                        res.backing.clear();
                        (OK_NODATA, vec![])
                    }
                    None => (ERR_INVALID_RESOURCE_ID, vec![]),
                }
            }
            // Hardware cursor: the host pointer is shown instead (tablet input).
            UPDATE_CURSOR | MOVE_CURSOR => (OK_NODATA, vec![]),
            _ => (ERR_UNSPEC, vec![]),
        }
    }

    fn create(&mut self, r: &mut Reader) -> u32 {
        let (Some(id), Some(format), Some(w), Some(h)) = (rd32(r), rd32(r), rd32(r), rd32(r)) else {
            return ERR_INVALID_PARAMETER;
        };
        if id == 0 || self.resources.contains_key(&id) || swizzle(format).is_none() {
            return ERR_INVALID_PARAMETER;
        }
        if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
            return ERR_INVALID_PARAMETER;
        }
        let bytes = w as usize * h as usize * 4;
        if self.resources.len() >= MAX_RESOURCES || self.used_bytes + bytes > MAX_RESOURCE_BYTES {
            return ERR_OUT_OF_MEMORY;
        }
        self.used_bytes += bytes;
        self.resources.insert(id, Resource { format, width: w, height: h, pixels: vec![0u8; bytes], backing: vec![] });
        OK_NODATA
    }

    fn attach(&mut self, mem: &GuestMemory, r: &mut Reader) -> u32 {
        let (Some(id), Some(n)) = (rd32(r), rd32(r)) else { return ERR_INVALID_PARAMETER };
        if n == 0 || n > MAX_BACKING_ENTRIES {
            return ERR_INVALID_PARAMETER;
        }
        let mut entries = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let (Some(addr), Some(len), Some(_pad)) = (rd64(r), rd32(r), rd32(r)) else {
                return ERR_INVALID_PARAMETER;
            };
            if !mem.check_range(GuestAddress(addr), len as u64) {
                return ERR_INVALID_PARAMETER;
            }
            entries.push((addr, len));
        }
        match self.resources.get_mut(&id) {
            Some(res) => {
                res.backing = entries;
                OK_NODATA
            }
            None => ERR_INVALID_RESOURCE_ID,
        }
    }

    fn transfer(&mut self, mem: &GuestMemory, r: &mut Reader) -> u32 {
        let Some(rect) = Rect::read(r) else { return ERR_INVALID_PARAMETER };
        let (Some(offset), Some(id)) = (rd64(r), rd32(r)) else { return ERR_INVALID_PARAMETER };
        let Some(res) = self.resources.get_mut(&id) else { return ERR_INVALID_RESOURCE_ID };
        if !rect.inside(res.width, res.height) || res.backing.is_empty() {
            return ERR_INVALID_PARAMETER;
        }
        let stride = res.width as u64 * 4;
        let row = rect.w as usize * 4;
        for i in 0..rect.h as u64 {
            let Some(src) = offset.checked_add(i * stride) else { return ERR_INVALID_PARAMETER };
            let dst = ((rect.y as u64 + i) * stride + rect.x as u64 * 4) as usize;
            if !read_backing(mem, &res.backing, src, &mut res.pixels[dst..dst + row]) {
                return ERR_INVALID_PARAMETER;
            }
        }
        OK_NODATA
    }

    fn set_scanout(&mut self, r: &mut Reader) -> u32 {
        let Some(rect) = Rect::read(r) else { return ERR_INVALID_PARAMETER };
        let (Some(sid), Some(id)) = (rd32(r), rd32(r)) else { return ERR_INVALID_PARAMETER };
        if sid != 0 {
            return ERR_INVALID_SCANOUT_ID;
        }
        if id == 0 {
            self.scanout = None;
            self.set_mode(0, 0);
            return OK_NODATA;
        }
        let Some(res) = self.resources.get(&id) else { return ERR_INVALID_RESOURCE_ID };
        if !rect.inside(res.width, res.height) || rect.w == 0 || rect.h == 0 {
            return ERR_INVALID_PARAMETER;
        }
        self.scanout = Some((id, rect));
        self.set_mode(rect.w, rect.h);
        OK_NODATA
    }

    fn set_mode(&mut self, w: u32, h: u32) {
        if self.mode != (w, h) {
            self.mode = (w, h);
            self.sink.set_mode(w, h);
        }
    }

    fn flush(&mut self, r: &mut Reader) -> u32 {
        let Some(rect) = Rect::read(r) else { return ERR_INVALID_PARAMETER };
        let Some(id) = rd32(r) else { return ERR_INVALID_PARAMETER };
        let Some((sid, view)) = self.scanout else { return OK_NODATA };
        if sid != id {
            return OK_NODATA;
        }
        let Some(res) = self.resources.get(&id) else { return ERR_INVALID_RESOURCE_ID };
        // Clip to the visible part of the resource.
        let x0 = rect.x.max(view.x);
        let y0 = rect.y.max(view.y);
        let x1 = rect.x.saturating_add(rect.w).min(view.x + view.w);
        let y1 = rect.y.saturating_add(rect.h).min(view.y + view.h);
        if x1 <= x0 || y1 <= y0 {
            return OK_NODATA;
        }
        let (w, h) = (x1 - x0, y1 - y0);
        let Some(sw) = swizzle(res.format) else { return ERR_UNSPEC };
        let stride = res.width as usize * 4;
        let mut out = vec![0u8; w as usize * h as usize * 4];
        for row in 0..h as usize {
            let s = (y0 as usize + row) * stride + x0 as usize * 4;
            let src = &res.pixels[s..s + w as usize * 4];
            let dst = &mut out[row * w as usize * 4..(row + 1) * w as usize * 4];
            if sw == [0, 1, 2, 3] {
                dst.copy_from_slice(src);
            } else {
                for (d, p) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    d[0] = p[sw[0]];
                    d[1] = p[sw[1]];
                    d[2] = p[sw[2]];
                    d[3] = 0;
                }
            }
        }
        self.sink.flush(x0 - view.x, y0 - view.y, w, h, &out, w as usize * 4);
        OK_NODATA
    }

    fn process(&mut self, ctx: &mut QueueCtx, q: usize) {
        loop {
            let chain = match ctx.queues[q].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                _ => break,
            };
            let head = chain.head();
            let mut len = 0;
            if let Ok((mut r, mut w)) = chain.into_rw() {
                let hdr = (|| {
                    Some(Hdr {
                        ty: rd32(&mut r)?,
                        flags: rd32(&mut r)?,
                        fence: rd64(&mut r)?,
                        ctx: {
                            let c = rd32(&mut r)?;
                            rd32(&mut r)?;
                            c
                        },
                    })
                })();
                if let Some(hdr) = hdr {
                    let (ty, body) = self.command(&ctx.mem, &mut r, &hdr);
                    Self::respond(&mut w, &hdr, ty, &body);
                    len = w.written() as u32;
                }
            }
            ctx.add_used(q, head, len);
        }
    }
}

/// Copy from a scatter list (logical offset `off`) into `out`.
fn read_backing(mem: &GuestMemory, backing: &[(u64, u32)], mut off: u64, out: &mut [u8]) -> bool {
    let mut done = 0usize;
    for &(addr, len) in backing {
        let len = len as u64;
        if off >= len {
            off -= len;
            continue;
        }
        let n = ((len - off) as usize).min(out.len() - done);
        if mem.read(GuestAddress(addr + off), &mut out[done..done + n]).is_err() {
            return false;
        }
        done += n;
        off = 0;
        if done == out.len() {
            return true;
        }
    }
    done == out.len()
}

impl QueueHandler for GpuHandler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        self.process(ctx, q);
    }
}

impl VirtioDevice for Gpu {
    fn device_type(&self) -> u32 {
        types::GPU
    }
    fn name(&self) -> String {
        "gpu".into()
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![256, 16]
    }
    fn device_features(&self) -> u64 {
        crate::features::RING_EVENT_IDX | crate::features::RING_INDIRECT_DESC
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let mut c = [0u8; 16];
        c[0..4].copy_from_slice(&s.events_read.to_le_bytes());
        c[8..12].copy_from_slice(&1u32.to_le_bytes()); // num_scanouts
        let o = offset as usize;
        data.fill(0);
        if o < c.len() {
            let n = data.len().min(c.len() - o);
            data[..n].copy_from_slice(&c[o..o + n]);
        }
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        if offset == 4 && data.len() == 4 {
            let v = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            self.shared.lock().unwrap_or_else(|e| e.into_inner()).events_read &= !v;
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).irq = Some(ctx.irq.clone());
        let h = self.handler();
        self.worker.start("gpu", ctx, h, false)
    }
    fn queue_notify(&mut self, q: u16) {
        self.worker.notify(q);
    }
    fn reset(&mut self) {
        self.worker.stop();
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).irq = None;
        self.sink.set_mode(0, 0);
    }
    fn pause(&mut self) {
        self.worker.pause();
    }
    fn resume(&mut self) {
        self.worker.resume();
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        // Resources are host-side copies of guest memory; the guest driver
        // re-uploads on the next frame. We only keep the queues.
        Ok(DeviceState { queues: self.worker.queues(), data: Vec::new() })
    }
    fn restore_active(&mut self, ctx: ActivateContext, _data: &[u8]) -> Result<()> {
        {
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            s.irq = Some(ctx.irq.clone());
            // Make the driver re-query and re-setup the display.
            s.events_read |= EVENT_DISPLAY;
        }
        let irq = ctx.irq.clone();
        let h = self.handler();
        self.worker.start("gpu", ctx, h, true)?;
        irq.signal_config_changed();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backing_scatter_read() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 1 << 20)]).unwrap();
        mem.write(GuestAddress(0x1000), &[1, 2, 3]).unwrap();
        mem.write(GuestAddress(0x5000), &[4, 5, 6]).unwrap();
        let b = [(0x1000u64, 3u32), (0x5000, 3)];
        let mut out = [0u8; 4];
        assert!(read_backing(&mem, &b, 1, &mut out));
        assert_eq!(out, [2, 3, 4, 5]);
        assert!(!read_backing(&mem, &b, 4, &mut out)); // runs past the end
    }

    #[test]
    fn rect_bounds() {
        assert!(Rect { x: 0, y: 0, w: 10, h: 10 }.inside(10, 10));
        assert!(!Rect { x: 1, y: 0, w: 10, h: 10 }.inside(10, 10));
        assert!(!Rect { x: u32::MAX, y: 0, w: 2, h: 1 }.inside(10, 10));
    }

    #[test]
    fn swizzles() {
        assert_eq!(swizzle(2), Some([0, 1, 2, 3]));
        assert_eq!(swizzle(67), Some([2, 1, 0, 3]));
        assert_eq!(swizzle(68), Some([1, 2, 3, 0]));
        assert_eq!(swizzle(999), None);
    }
}
