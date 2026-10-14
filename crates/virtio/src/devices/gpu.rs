//! virtio-gpu: 2D scanout, plus 3D through a host renderer (Venus).
//!
//! 2D: the guest creates resources, attaches guest-RAM backing, transfers
//! pixels into our host copy, and flushes rects to the scanout. We push
//! flushed rects to a [`DisplaySink`] (the shared framebuffer).
//!
//! 3D (optional, see [`ThreeD`]): contexts, command submission, blob
//! resources and fences go to a [`Renderer3d`]. Host-visible blobs are
//! mapped into a shared memory window the guest maps like VRAM. Simple 2D
//! textures (what Linux uses for dumb buffers once 3D is negotiated) stay
//! on the CPU path here: Venus has no GL to render them with.
//!
//! Hostile-guest limits: total resource memory, resource count, backing
//! entries, command sizes, pending fences, and every rect/offset/window
//! range are checked. The renderer itself still parses guest command
//! streams; that's the cost of 3D (docs/SECURITY.md).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bulwark_memory::{GuestAddress, GuestMemory};
use serde::{Deserialize, Serialize};

use crate::devices::gpu3d::{Blob, Box3d, Create3d, Renderer3d, ShmMapper, Transfer3d};
use crate::worker::{QueueCtx, QueueHandler, WorkerSlot, BACKEND_TOKEN_BASE};
use crate::{types, ActivateContext, DeviceState, Error, Reader, Result, VirtioDevice, VirtioInterrupt, Writer};

const GET_DISPLAY_INFO: u32 = 0x0100;
const RESOURCE_CREATE_2D: u32 = 0x0101;
const RESOURCE_UNREF: u32 = 0x0102;
const SET_SCANOUT: u32 = 0x0103;
const RESOURCE_FLUSH: u32 = 0x0104;
const TRANSFER_TO_HOST_2D: u32 = 0x0105;
const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const RESOURCE_DETACH_BACKING: u32 = 0x0107;
const GET_CAPSET_INFO: u32 = 0x0108;
const GET_CAPSET: u32 = 0x0109;
const RESOURCE_CREATE_BLOB: u32 = 0x010c;
const SET_SCANOUT_BLOB: u32 = 0x010d;
const CTX_CREATE: u32 = 0x0200;
const CTX_DESTROY: u32 = 0x0201;
const CTX_ATTACH_RESOURCE: u32 = 0x0202;
const CTX_DETACH_RESOURCE: u32 = 0x0203;
const RESOURCE_CREATE_3D: u32 = 0x0204;
const TRANSFER_TO_HOST_3D: u32 = 0x0205;
const TRANSFER_FROM_HOST_3D: u32 = 0x0206;
const SUBMIT_3D: u32 = 0x0207;
const RESOURCE_MAP_BLOB: u32 = 0x0208;
const RESOURCE_UNMAP_BLOB: u32 = 0x0209;
const UPDATE_CURSOR: u32 = 0x0300;
const MOVE_CURSOR: u32 = 0x0301;

const OK_NODATA: u32 = 0x1100;
const OK_DISPLAY_INFO: u32 = 0x1101;
const OK_CAPSET_INFO: u32 = 0x1102;
const OK_CAPSET: u32 = 0x1103;
const OK_MAP_INFO: u32 = 0x1106;
const ERR_UNSPEC: u32 = 0x1200;
const ERR_OUT_OF_MEMORY: u32 = 0x1201;
const ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
const ERR_INVALID_PARAMETER: u32 = 0x1205;

const FLAG_FENCE: u32 = 1;
const FLAG_INFO_RING_IDX: u32 = 2;
const EVENT_DISPLAY: u32 = 1;

const F_VIRGL: u64 = 1 << 0;
const F_RESOURCE_BLOB: u64 = 1 << 3;
const F_CONTEXT_INIT: u64 = 1 << 4;

/// VIRTIO_GPU_SHM_ID_HOST_VISIBLE.
pub const SHM_HOST_VISIBLE: u8 = 1;
const BLOB_MEM_GUEST: u32 = 1;
const BLOB_MEM_HOST3D: u32 = 2;
const BLOB_MEM_HOST3D_GUEST: u32 = 3;
const PIPE_TEXTURE_2D: u32 = 2;

const MAX_RESOURCE_BYTES: usize = 512 << 20;
const MAX_RESOURCES: usize = 4096;
const MAX_BACKING_ENTRIES: u32 = 16384;
const MAX_DIM: u32 = 16384;
const MAX_SUBMIT: usize = 16 << 20;
const MAX_PENDING_FENCES: usize = 4096;
const FENCE_POLL: Duration = Duration::from_millis(5);

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

/// The 3D side: renderer, capsets, and the host-visible window.
#[derive(Clone)]
pub struct ThreeD {
    renderer: Arc<Mutex<Box<dyn Renderer3d>>>,
    capsets: Vec<(u32, u32, u32)>,
    mapper: Arc<dyn ShmMapper>,
    shm_size: u64,
    shm_base: Arc<AtomicU64>,
}

impl std::fmt::Debug for ThreeD {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreeD").field("capsets", &self.capsets).field("shm_size", &self.shm_size).finish()
    }
}

impl ThreeD {
    pub fn new(renderer: Box<dyn Renderer3d>, mapper: Arc<dyn ShmMapper>, shm_size: u64) -> Self {
        let capsets = renderer.capsets();
        ThreeD { renderer: Arc::new(Mutex::new(renderer)), capsets, mapper, shm_size, shm_base: Arc::new(AtomicU64::new(0)) }
    }

    fn r(&self) -> MutexGuard<'_, Box<dyn Renderer3d>> {
        self.renderer.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct Gpu {
    shared: Arc<Mutex<Shared>>,
    sink: Arc<dyn DisplaySink>,
    three_d: Option<ThreeD>,
    worker: WorkerSlot<GpuHandler>,
}

impl std::fmt::Debug for Gpu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gpu").field("three_d", &self.three_d).finish_non_exhaustive()
    }
}

impl Gpu {
    pub fn new(width: u32, height: u32, sink: Arc<dyn DisplaySink>) -> (Self, GpuControl) {
        let shared = Arc::new(Mutex::new(Shared { width, height, events_read: 0, irq: None }));
        (Gpu { shared: shared.clone(), sink, three_d: None, worker: WorkerSlot::default() }, GpuControl(shared))
    }

    /// Offer 3D (Venus) on top of 2D.
    pub fn with_3d(mut self, t: ThreeD) -> Self {
        self.three_d = Some(t);
        self
    }

    /// Fuzzing: run both queues over guest memory once, synchronously,
    /// the way the worker does. Not for anything else.
    #[doc(hidden)]
    pub fn fuzz_queues(&self, ctx: ActivateContext) {
        let mut h = self.handler();
        let mut c = QueueCtx::new(ctx);
        h.process(&mut c, 0);
        h.process(&mut c, 1);
        let _ = h.save_2d();
    }

    fn handler(&self) -> GpuHandler {
        GpuHandler {
            shared: self.shared.clone(),
            sink: self.sink.clone(),
            resources: HashMap::new(),
            used_bytes: 0,
            scanout: None,
            mode: (0, 0),
            three_d: self.three_d.clone(),
            res3d: HashMap::new(),
            contexts: Vec::new(),
            cpu_contexts: Vec::new(),
            pending: Vec::new(),
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

/// A resource owned by the renderer.
#[derive(Debug, Default)]
struct Res3d {
    /// Guest RAM behind a guest-memory blob (for scanouts).
    guest: Vec<(u64, u32)>,
    /// Where it's mapped in the host-visible window: (gpa, size).
    mapped: Option<(u64, u64)>,
}

#[derive(Debug, Clone, Copy)]
enum Scanout {
    /// A CPU resource; visible rect.
    Cpu { id: u32, view: Rect },
    /// A blob, read straight from its memory.
    Blob { id: u32, view: Rect, format: u32, stride: u32, offset: u32 },
}

/// 2D state for snapshots. Pixels are not saved: they are re-read from
/// the guest backing (restored with RAM), like a full-frame transfer.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Snap2d {
    resources: Vec<SnapRes>,
    /// (resource id, visible rect x, y, w, h) on scanout 0.
    scanout: Option<(u32, u32, u32, u32, u32)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapRes {
    id: u32,
    format: u32,
    width: u32,
    height: u32,
    backing: Vec<(u64, u32)>,
}

/// A fenced command waiting for the GPU before it's returned to the guest.
#[derive(Debug, Clone, Copy)]
struct Pending {
    q: usize,
    head: u16,
    len: u32,
    ctx: u32,
    ring: Option<u8>,
    fence: u64,
}

pub struct GpuHandler {
    shared: Arc<Mutex<Shared>>,
    sink: Arc<dyn DisplaySink>,
    resources: HashMap<u32, Resource>,
    used_bytes: usize,
    scanout: Option<Scanout>,
    mode: (u32, u32),
    three_d: Option<ThreeD>,
    res3d: HashMap<u32, Res3d>,
    contexts: Vec<u32>,
    /// Contexts with no renderer side (capset 0).
    cpu_contexts: Vec<u32>,
    pending: Vec<Pending>,
}

impl std::fmt::Debug for GpuHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuHandler").field("resources", &self.resources.len()).field("res3d", &self.res3d.len()).finish()
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

fn le(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
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

fn to_bgrx(sw: [usize; 4], src: &[u8], dst: &mut [u8]) {
    if sw == [0, 1, 2, 3] {
        dst.copy_from_slice(src);
        return;
    }
    for (d, p) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.chunks_exact(4)) {
        d[0] = p[sw[0]];
        d[1] = p[sw[1]];
        d[2] = p[sw[2]];
        d[3] = 0;
    }
}

struct Hdr {
    ty: u32,
    flags: u32,
    fence: u64,
    ctx: u32,
    ring: u8,
}

impl GpuHandler {
    fn respond(w: &mut Writer, hdr: &Hdr, ty: u32, body: &[u8]) {
        let fenced = hdr.flags & FLAG_FENCE != 0;
        let mut out = Vec::with_capacity(24 + body.len());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&(hdr.flags & (FLAG_FENCE | FLAG_INFO_RING_IDX)).to_le_bytes());
        out.extend_from_slice(&(if fenced { hdr.fence } else { 0 }).to_le_bytes());
        out.extend_from_slice(&hdr.ctx.to_le_bytes());
        out.extend_from_slice(&[hdr.ring, 0, 0, 0]);
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
                (self.unref(id), vec![])
            }
            SET_SCANOUT => (self.set_scanout(r), vec![]),
            RESOURCE_FLUSH => (self.flush(mem, r), vec![]),
            TRANSFER_TO_HOST_2D => {
                let Some(rect) = Rect::read(r) else { return (ERR_INVALID_PARAMETER, vec![]) };
                let (Some(offset), Some(id)) = (rd64(r), rd32(r)) else { return (ERR_INVALID_PARAMETER, vec![]) };
                (self.transfer(mem, id, rect, offset, None), vec![])
            }
            RESOURCE_ATTACH_BACKING => (self.attach(mem, r), vec![]),
            RESOURCE_DETACH_BACKING => {
                let Some(id) = rd32(r) else { return (ERR_INVALID_PARAMETER, vec![]) };
                if let Some(res) = self.resources.get_mut(&id) {
                    res.backing.clear();
                    return (OK_NODATA, vec![]);
                }
                match (self.res3d.contains_key(&id), &self.three_d) {
                    (true, Some(t)) => {
                        t.r().detach_backing(id);
                        (OK_NODATA, vec![])
                    }
                    _ => (ERR_INVALID_RESOURCE_ID, vec![]),
                }
            }
            // Hardware cursor: the host pointer is shown instead (tablet input).
            UPDATE_CURSOR | MOVE_CURSOR => (OK_NODATA, vec![]),
            _ if self.three_d.is_some() => self.command_3d(mem, r, hdr),
            _ => (ERR_UNSPEC, vec![]),
        }
    }

    fn command_3d(&mut self, mem: &GuestMemory, r: &mut Reader, hdr: &Hdr) -> (u32, Vec<u8>) {
        let Some(t) = self.three_d.clone() else { return (ERR_UNSPEC, vec![]) };
        let bad = (ERR_INVALID_PARAMETER, vec![]);
        match hdr.ty {
            GET_CAPSET_INFO => {
                let Some(i) = rd32(r) else { return bad };
                match t.capsets.get(i as usize) {
                    Some(&(id, ver, size)) => (OK_CAPSET_INFO, le(&[id, ver, size, 0])),
                    None => bad,
                }
            }
            GET_CAPSET => {
                let (Some(id), Some(ver)) = (rd32(r), rd32(r)) else { return bad };
                match t.r().capset(id, ver) {
                    Some(caps) => (OK_CAPSET, caps),
                    None => bad,
                }
            }
            CTX_CREATE => {
                let (Some(nlen), Some(init)) = (rd32(r), rd32(r)) else { return bad };
                let mut name = [0u8; 64];
                if r.read_exact(&mut name).is_err() {
                    return bad;
                }
                let capset = init & 0xff;
                if hdr.ctx == 0 || self.contexts.contains(&hdr.ctx) || self.cpu_contexts.contains(&hdr.ctx) {
                    return bad;
                }
                if capset == 0 {
                    // Classic virgl (Linux makes one for its own use). No GL
                    // here: a context that only ever touches CPU resources.
                    self.cpu_contexts.push(hdr.ctx);
                    return (OK_NODATA, vec![]);
                }
                let n = (nlen as usize).min(64);
                if !t.r().context_create(hdr.ctx, capset, &name[..n]) {
                    return (ERR_UNSPEC, vec![]);
                }
                self.contexts.push(hdr.ctx);
                (OK_NODATA, vec![])
            }
            CTX_DESTROY => {
                if let Some(i) = self.cpu_contexts.iter().position(|&c| c == hdr.ctx) {
                    self.cpu_contexts.swap_remove(i);
                    return (OK_NODATA, vec![]);
                }
                match self.contexts.iter().position(|&c| c == hdr.ctx) {
                    Some(i) => {
                        self.contexts.swap_remove(i);
                        t.r().context_destroy(hdr.ctx);
                        (OK_NODATA, vec![])
                    }
                    None => (ERR_INVALID_CONTEXT_ID, vec![]),
                }
            }
            CTX_ATTACH_RESOURCE | CTX_DETACH_RESOURCE => {
                let Some(id) = rd32(r) else { return bad };
                if self.cpu_contexts.contains(&hdr.ctx) && (self.resources.contains_key(&id) || self.res3d.contains_key(&id)) {
                    return (OK_NODATA, vec![]);
                }
                if !self.contexts.contains(&hdr.ctx) {
                    return (ERR_INVALID_CONTEXT_ID, vec![]);
                }
                if self.res3d.contains_key(&id) {
                    if hdr.ty == CTX_ATTACH_RESOURCE {
                        t.r().context_attach(hdr.ctx, id);
                    } else {
                        t.r().context_detach(hdr.ctx, id);
                    }
                    (OK_NODATA, vec![])
                } else if self.resources.contains_key(&id) {
                    (OK_NODATA, vec![]) // CPU resource: nothing to share
                } else {
                    (ERR_INVALID_RESOURCE_ID, vec![])
                }
            }
            RESOURCE_CREATE_3D => self.create_3d(r, &t),
            TRANSFER_TO_HOST_3D | TRANSFER_FROM_HOST_3D => {
                let to_host = hdr.ty == TRANSFER_TO_HOST_3D;
                let mut v = [0u32; 6];
                for x in v.iter_mut() {
                    let Some(n) = rd32(r) else { return bad };
                    *x = n;
                }
                let bx = Box3d { x: v[0], y: v[1], z: v[2], w: v[3], h: v[4], d: v[5] };
                let (Some(offset), Some(id), Some(level), Some(stride), Some(layer_stride)) = (rd64(r), rd32(r), rd32(r), rd32(r), rd32(r))
                else {
                    return bad;
                };
                if self.resources.contains_key(&id) {
                    // CPU texture: the host copy never changes, so a read
                    // back has nothing new to give.
                    if !to_host {
                        return (OK_NODATA, vec![]);
                    }
                    if bx.z != 0 || bx.d > 1 || level != 0 {
                        return bad;
                    }
                    let rect = Rect { x: bx.x, y: bx.y, w: bx.w, h: bx.h };
                    return (self.transfer(mem, id, rect, offset, Some(stride)), vec![]);
                }
                if !self.res3d.contains_key(&id) {
                    return (ERR_INVALID_RESOURCE_ID, vec![]);
                }
                let tr = Transfer3d { resource: id, level, stride, layer_stride, bx, offset };
                if t.r().transfer(to_host, hdr.ctx, &tr) {
                    (OK_NODATA, vec![])
                } else {
                    bad
                }
            }
            SUBMIT_3D => {
                let (Some(size), Some(_pad)) = (rd32(r), rd32(r)) else { return bad };
                let size = size as usize;
                if size > MAX_SUBMIT || !size.is_multiple_of(4) || !self.contexts.contains(&hdr.ctx) {
                    return bad;
                }
                let mut buf = vec![0u8; size];
                if r.read_exact(&mut buf).is_err() {
                    return bad;
                }
                if t.r().submit(hdr.ctx, &mut buf) {
                    (OK_NODATA, vec![])
                } else {
                    (ERR_UNSPEC, vec![])
                }
            }
            RESOURCE_CREATE_BLOB => self.create_blob(mem, r, hdr, &t),
            RESOURCE_MAP_BLOB => {
                let (Some(id), Some(_pad), Some(offset)) = (rd32(r), rd32(r), rd64(r)) else { return bad };
                self.map_blob(&t, id, offset)
            }
            RESOURCE_UNMAP_BLOB => {
                let Some(id) = rd32(r) else { return bad };
                let Some(res) = self.res3d.get_mut(&id) else { return (ERR_INVALID_RESOURCE_ID, vec![]) };
                if let Some((gpa, size)) = res.mapped.take() {
                    t.mapper.unmap(gpa, size);
                    t.r().unmap_blob(id);
                }
                (OK_NODATA, vec![])
            }
            SET_SCANOUT_BLOB => (self.set_scanout_blob(r), vec![]),
            _ => (ERR_UNSPEC, vec![]),
        }
    }

    fn id_free(&self, id: u32) -> bool {
        id != 0 && !self.resources.contains_key(&id) && !self.res3d.contains_key(&id)
    }

    fn create(&mut self, r: &mut Reader) -> u32 {
        let (Some(id), Some(format), Some(w), Some(h)) = (rd32(r), rd32(r), rd32(r), rd32(r)) else {
            return ERR_INVALID_PARAMETER;
        };
        self.create_cpu(id, format, w, h)
    }

    fn create_cpu(&mut self, id: u32, format: u32, w: u32, h: u32) -> u32 {
        if !self.id_free(id) || swizzle(format).is_none() {
            return ERR_INVALID_PARAMETER;
        }
        if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
            return ERR_INVALID_PARAMETER;
        }
        let bytes = w as usize * h as usize * 4;
        if self.resource_count() >= MAX_RESOURCES || self.used_bytes + bytes > MAX_RESOURCE_BYTES {
            return ERR_OUT_OF_MEMORY;
        }
        self.used_bytes += bytes;
        self.resources.insert(id, Resource { format, width: w, height: h, pixels: vec![0u8; bytes], backing: vec![] });
        OK_NODATA
    }

    fn resource_count(&self) -> usize {
        self.resources.len() + self.res3d.len()
    }

    fn create_3d(&mut self, r: &mut Reader, t: &ThreeD) -> (u32, Vec<u8>) {
        let mut v = [0u32; 12];
        for x in v.iter_mut() {
            let Some(n) = rd32(r) else { return (ERR_INVALID_PARAMETER, vec![]) };
            *x = n;
        }
        let id = v[0];
        let a = Create3d {
            target: v[1],
            format: v[2],
            bind: v[3],
            width: v[4],
            height: v[5],
            depth: v[6],
            array_size: v[7],
            last_level: v[8],
            nr_samples: v[9],
            flags: v[10],
        };
        // Plain 2D textures (dumb buffers, fbcon) stay on the CPU.
        if a.target == PIPE_TEXTURE_2D && a.depth <= 1 && a.array_size <= 1 && a.last_level == 0 && a.nr_samples <= 1 && swizzle(a.format).is_some() {
            return (self.create_cpu(id, a.format, a.width, a.height), vec![]);
        }
        if !self.id_free(id) || self.resource_count() >= MAX_RESOURCES {
            return (ERR_INVALID_PARAMETER, vec![]);
        }
        if !t.r().create_3d(id, &a) {
            return (ERR_UNSPEC, vec![]);
        }
        self.res3d.insert(id, Res3d::default());
        (OK_NODATA, vec![])
    }

    /// Read `n` (addr, len) entries; each must be guest RAM.
    fn read_entries(mem: &GuestMemory, r: &mut Reader, n: u32) -> Option<Vec<(u64, u32)>> {
        if n > MAX_BACKING_ENTRIES {
            return None;
        }
        let mut entries = Vec::with_capacity(n as usize);
        for i in 0..n {
            let (addr, len, _pad) = (rd64(r)?, rd32(r)?, rd32(r)?);
            if i < 4 || i + 1 == n {
                log::trace!("gpu: backing[{i}] {addr:#x}+{len:#x}");
            }
            if !mem.check_range(GuestAddress(addr), len as u64) {
                return None;
            }
            entries.push((addr, len));
        }
        Some(entries)
    }

    /// Host pointers for guest entries (split at RAM region edges).
    fn host_iov(mem: &GuestMemory, entries: &[(u64, u32)]) -> Option<Vec<(*mut u8, usize)>> {
        let mut out = Vec::new();
        for &(addr, len) in entries {
            out.extend(mem.host_ranges(GuestAddress(addr), len as u64)?);
        }
        Some(out)
    }

    fn create_blob(&mut self, mem: &GuestMemory, r: &mut Reader, hdr: &Hdr, t: &ThreeD) -> (u32, Vec<u8>) {
        let bad = (ERR_INVALID_PARAMETER, vec![]);
        let (Some(id), Some(blob_mem), Some(flags), Some(n), Some(blob_id), Some(size)) =
            (rd32(r), rd32(r), rd32(r), rd32(r), rd64(r), rd64(r))
        else {
            return bad;
        };
        if !self.id_free(id) || self.resource_count() >= MAX_RESOURCES || size == 0 {
            return bad;
        }
        let needs_ctx = matches!(blob_mem, BLOB_MEM_HOST3D | BLOB_MEM_HOST3D_GUEST);
        if !matches!(blob_mem, BLOB_MEM_GUEST | BLOB_MEM_HOST3D | BLOB_MEM_HOST3D_GUEST) || (needs_ctx && !self.contexts.contains(&hdr.ctx)) {
            return bad;
        }
        let Some(entries) = Self::read_entries(mem, r, n) else { return bad };
        let total: u64 = entries.iter().map(|e| e.1 as u64).sum();
        if blob_mem != BLOB_MEM_HOST3D && total < size {
            return bad;
        }
        let Some(iov) = Self::host_iov(mem, &entries) else { return bad };
        let blob = Blob { resource: id, ctx: hdr.ctx, mem: blob_mem, flags, id: blob_id, size };
        if !t.r().create_blob(&blob, iov) {
            return (ERR_UNSPEC, vec![]);
        }
        let guest = if blob_mem == BLOB_MEM_GUEST { entries } else { Vec::new() };
        self.res3d.insert(id, Res3d { guest, mapped: None });
        (OK_NODATA, vec![])
    }

    fn map_blob(&mut self, t: &ThreeD, id: u32, offset: u64) -> (u32, Vec<u8>) {
        let bad = (ERR_INVALID_PARAMETER, vec![]);
        let Some(res) = self.res3d.get_mut(&id) else { return (ERR_INVALID_RESOURCE_ID, vec![]) };
        if res.mapped.is_some() {
            return bad;
        }
        let ps = bulwark_memory::page_size() as u64;
        let base = t.shm_base.load(Ordering::Acquire);
        if base == 0 || !offset.is_multiple_of(ps) {
            return bad;
        }
        let mut rr = t.r();
        let Some(m) = rr.map_blob(id) else {
            log::warn!("gpu: blob {id}: renderer can't map it");
            return (ERR_UNSPEC, vec![]);
        };
        log::debug!("gpu: map blob {id} at +{offset:#x}: {:#x} bytes at {:p}, info {:#x}", m.size, m.ptr, m.map_info);
        // The whole mapping must fit the window, in whole pages: rounding
        // up would hand the guest whatever sits next to it on the host.
        let fits = offset.checked_add(m.size).is_some_and(|e| e <= t.shm_size);
        if !fits || m.size % ps != 0 || !(m.ptr as u64).is_multiple_of(ps) {
            log::warn!("gpu: blob {id} map refused: offset {offset:#x} size {:#x} ptr {:p}", m.size, m.ptr);
            rr.unmap_blob(id);
            return bad;
        }
        let gpa = base + offset;
        if !t.mapper.map(gpa, &m) {
            rr.unmap_blob(id);
            return (ERR_UNSPEC, vec![]);
        }
        res.mapped = Some((gpa, m.size));
        (OK_MAP_INFO, le(&[m.map_info, 0]))
    }

    fn unref(&mut self, id: u32) -> u32 {
        if matches!(self.scanout, Some(Scanout::Cpu { id: s, .. } | Scanout::Blob { id: s, .. }) if s == id) {
            self.scanout = None;
        }
        if let Some(res) = self.resources.remove(&id) {
            self.used_bytes -= res.pixels.len();
            return OK_NODATA;
        }
        match (self.res3d.remove(&id), &self.three_d) {
            (Some(res), Some(t)) => {
                if let Some((gpa, size)) = res.mapped {
                    t.mapper.unmap(gpa, size);
                }
                t.r().unref(id);
                OK_NODATA
            }
            _ => ERR_INVALID_RESOURCE_ID,
        }
    }

    fn attach(&mut self, mem: &GuestMemory, r: &mut Reader) -> u32 {
        let (Some(id), Some(n)) = (rd32(r), rd32(r)) else { return ERR_INVALID_PARAMETER };
        if n == 0 {
            return ERR_INVALID_PARAMETER;
        }
        let Some(entries) = Self::read_entries(mem, r, n) else { return ERR_INVALID_PARAMETER };
        if let Some(res) = self.resources.get_mut(&id) {
            res.backing = entries;
            return OK_NODATA;
        }
        match (self.res3d.contains_key(&id), &self.three_d) {
            (true, Some(t)) => match Self::host_iov(mem, &entries) {
                Some(iov) => {
                    if t.r().attach_backing(id, iov) {
                        OK_NODATA
                    } else {
                        ERR_UNSPEC
                    }
                }
                None => ERR_INVALID_PARAMETER,
            },
            _ => ERR_INVALID_RESOURCE_ID,
        }
    }

    /// Copy a rect from guest backing into the host copy. `stride` is the
    /// guest's row pitch (None: tightly packed, like TRANSFER_TO_HOST_2D).
    fn transfer(&mut self, mem: &GuestMemory, id: u32, rect: Rect, offset: u64, stride: Option<u32>) -> u32 {
        log::trace!("gpu: transfer res {id} {rect:?} offset {offset:#x} stride {stride:?}");
        let Some(res) = self.resources.get_mut(&id) else { return ERR_INVALID_RESOURCE_ID };
        if !rect.inside(res.width, res.height) || res.backing.is_empty() {
            return ERR_INVALID_PARAMETER;
        }
        let host_stride = res.width as u64 * 4;
        let src_stride = match stride {
            Some(s) if s != 0 => s as u64,
            _ => host_stride,
        };
        let row = rect.w as usize * 4;
        for i in 0..rect.h as u64 {
            let Some(src) = i.checked_mul(src_stride).and_then(|o| o.checked_add(offset)) else { return ERR_INVALID_PARAMETER };
            let dst = ((rect.y as u64 + i) * host_stride + rect.x as u64 * 4) as usize;
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
        self.scanout = Some(Scanout::Cpu { id, view: rect });
        self.set_mode(rect.w, rect.h);
        OK_NODATA
    }

    fn set_scanout_blob(&mut self, r: &mut Reader) -> u32 {
        let Some(rect) = Rect::read(r) else { return ERR_INVALID_PARAMETER };
        let mut v = [0u32; 14];
        for x in v.iter_mut() {
            let Some(n) = rd32(r) else { return ERR_INVALID_PARAMETER };
            *x = n;
        }
        let [sid, id, width, height, format, _pad, stride, _, _, _, offset, ..] = v;
        if sid != 0 {
            return ERR_INVALID_SCANOUT_ID;
        }
        if id == 0 {
            self.scanout = None;
            self.set_mode(0, 0);
            return OK_NODATA;
        }
        if !self.res3d.contains_key(&id) {
            return ERR_INVALID_RESOURCE_ID;
        }
        if swizzle(format).is_none() || !rect.inside(width, height) || rect.w == 0 || rect.h == 0 || width > MAX_DIM || height > MAX_DIM {
            return ERR_INVALID_PARAMETER;
        }
        if (stride as u64) < width as u64 * 4 {
            return ERR_INVALID_PARAMETER;
        }
        self.scanout = Some(Scanout::Blob { id, view: rect, format, stride, offset });
        self.set_mode(rect.w, rect.h);
        OK_NODATA
    }

    fn set_mode(&mut self, w: u32, h: u32) {
        if self.mode != (w, h) {
            self.mode = (w, h);
            self.sink.set_mode(w, h);
        }
    }

    fn flush(&mut self, mem: &GuestMemory, r: &mut Reader) -> u32 {
        let Some(rect) = Rect::read(r) else { return ERR_INVALID_PARAMETER };
        let Some(id) = rd32(r) else { return ERR_INVALID_PARAMETER };
        log::trace!("gpu: flush res {id} {rect:?}");
        self.flush_rect(mem, rect, id)
    }

    fn flush_rect(&mut self, mem: &GuestMemory, rect: Rect, id: u32) -> u32 {
        let Some(sc) = self.scanout else { return OK_NODATA };
        let (sid, view) = match sc {
            Scanout::Cpu { id, view } | Scanout::Blob { id, view, .. } => (id, view),
        };
        if sid != id {
            return OK_NODATA;
        }
        // Clip to the visible part of the resource.
        let x0 = rect.x.max(view.x);
        let y0 = rect.y.max(view.y);
        let x1 = rect.x.saturating_add(rect.w).min(view.x + view.w);
        let y1 = rect.y.saturating_add(rect.h).min(view.y + view.h);
        if x1 <= x0 || y1 <= y0 {
            return OK_NODATA;
        }
        let (w, h) = (x1 - x0, y1 - y0);
        let row = w as usize * 4;
        let mut out = vec![0u8; row * h as usize];
        match sc {
            Scanout::Cpu { .. } => {
                let Some(res) = self.resources.get(&id) else { return ERR_INVALID_RESOURCE_ID };
                let Some(sw) = swizzle(res.format) else { return ERR_UNSPEC };
                let stride = res.width as usize * 4;
                for i in 0..h as usize {
                    let s = (y0 as usize + i) * stride + x0 as usize * 4;
                    to_bgrx(sw, &res.pixels[s..s + row], &mut out[i * row..(i + 1) * row]);
                }
            }
            Scanout::Blob { format, stride, offset, .. } => {
                let Some(sw) = swizzle(format) else { return ERR_UNSPEC };
                let Some(res) = self.res3d.get(&id) else { return ERR_INVALID_RESOURCE_ID };
                let mut line = vec![0u8; row];
                for i in 0..h as usize {
                    let off = offset as u64 + (y0 as u64 + i as u64) * stride as u64 + x0 as u64 * 4;
                    let ok = if !res.guest.is_empty() {
                        read_backing(mem, &res.guest, off, &mut line)
                    } else {
                        self.three_d.as_ref().is_some_and(|t| t.r().read_mapped(id, off, &mut line))
                    };
                    if !ok {
                        return ERR_INVALID_PARAMETER;
                    }
                    to_bgrx(sw, &line, &mut out[i * row..(i + 1) * row]);
                }
            }
        }
        self.sink.flush(x0 - view.x, y0 - view.y, w, h, &out, row);
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
            let mut fence = None;
            if let Ok((mut r, mut w)) = chain.into_rw() {
                let hdr = (|| {
                    let (ty, flags, fence, ctx) = (rd32(&mut r)?, rd32(&mut r)?, rd64(&mut r)?, rd32(&mut r)?);
                    let ring = (rd32(&mut r)? & 0xff) as u8;
                    Some(Hdr { ty, flags, fence, ctx, ring })
                })();
                if let Some(hdr) = hdr {
                    let (ty, body) = self.command(&ctx.mem, &mut r, &hdr);
                    if ty >= ERR_UNSPEC {
                        log::debug!("gpu: command {:#x} -> {ty:#x}", hdr.ty);
                    } else {
                        log::trace!("gpu: command {:#x} ok", hdr.ty);
                    }
                    Self::respond(&mut w, &hdr, ty, &body);
                    len = w.written() as u32;
                    if hdr.flags & FLAG_FENCE != 0 {
                        let ring = (hdr.flags & FLAG_INFO_RING_IDX != 0).then_some(hdr.ring);
                        fence = Some((hdr.ctx, ring, hdr.fence));
                    }
                }
            }
            // Fenced commands go back once the GPU has caught up.
            if let (Some((fctx, ring, id)), Some(t)) = (fence, &self.three_d) {
                if self.pending.len() < MAX_PENDING_FENCES && t.r().create_fence(fctx, ring, id) {
                    self.pending.push(Pending { q, head, len, ctx: fctx, ring, fence: id });
                    continue;
                }
            }
            ctx.add_used(q, head, len);
        }
    }

    fn complete_fences(&mut self, ctx: &mut QueueCtx) {
        let Some(t) = &self.three_d else { return };
        let done = t.r().poll();
        for d in done {
            self.pending.retain(|p| {
                let hit = match d.ring {
                    // Global timeline: 32-bit ids in virglrenderer.
                    None => p.ring.is_none() && (p.fence as u32) <= (d.fence as u32),
                    Some(ring) => p.ring == Some(ring) && p.ctx == d.ctx && p.fence <= d.fence,
                };
                if hit {
                    ctx.add_used(p.q, p.head, p.len);
                }
                !hit
            });
        }
    }

    fn save_2d(&self) -> Snap2d {
        let mut resources: Vec<SnapRes> = self
            .resources
            .iter()
            .map(|(&id, r)| SnapRes { id, format: r.format, width: r.width, height: r.height, backing: r.backing.clone() })
            .collect();
        resources.sort_by_key(|r| r.id);
        let scanout = match self.scanout {
            Some(Scanout::Cpu { id, view }) => Some((id, view.x, view.y, view.w, view.h)),
            _ => None,
        };
        Snap2d { resources, scanout }
    }

    /// Rebuild from a snapshot (untrusted input: same checks as commands).
    fn restore_2d(&mut self, mem: &GuestMemory, s: &Snap2d) -> Result<()> {
        let bad = |m: &str| Error::Device(format!("gpu snapshot: {m}"));
        for r in &s.resources {
            if self.create_cpu(r.id, r.format, r.width, r.height) != OK_NODATA {
                return Err(bad("bad resource"));
            }
            if r.backing.len() > MAX_BACKING_ENTRIES as usize || r.backing.iter().any(|&(a, l)| !mem.check_range(GuestAddress(a), l as u64)) {
                return Err(bad("bad backing"));
            }
            if let Some(res) = self.resources.get_mut(&r.id) {
                res.backing = r.backing.clone();
            }
            if !r.backing.is_empty() {
                let full = Rect { x: 0, y: 0, w: r.width, h: r.height };
                // Best effort: a partly unmapped backing just stays black.
                let _ = self.transfer(mem, r.id, full, 0, None);
            }
        }
        if let Some((id, x, y, w, h)) = s.scanout {
            let view = Rect { x, y, w, h };
            let ok = self.resources.get(&id).is_some_and(|r| view.inside(r.width, r.height) && w > 0 && h > 0);
            if !ok {
                return Err(bad("bad scanout"));
            }
            self.scanout = Some(Scanout::Cpu { id, view });
            self.set_mode(w, h);
            self.flush_all(mem);
        }
        Ok(())
    }

    /// Push the whole visible scanout to the sink.
    fn flush_all(&mut self, mem: &GuestMemory) {
        if let Some(Scanout::Cpu { id, view }) = self.scanout {
            let _ = self.flush_rect(mem, view, id);
        }
    }

    /// Anything a snapshot can't capture (host GPU objects)?
    fn has_3d_state(&self) -> bool {
        !self.contexts.is_empty() || !self.res3d.is_empty()
    }
}

impl Drop for GpuHandler {
    fn drop(&mut self) {
        // Device reset or VM teardown: take blobs out of the guest window
        // before the renderer frees them.
        if let Some(t) = &self.three_d {
            for res in self.res3d.values() {
                if let Some((gpa, size)) = res.mapped {
                    t.mapper.unmap(gpa, size);
                }
            }
            t.r().reset();
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

    fn register(&mut self, reg: &mio::Registry) -> std::io::Result<()> {
        if let Some(fd) = self.three_d.as_ref().and_then(|t| t.r().poll_fd()) {
            reg.register(&mut mio::unix::SourceFd(&fd), mio::Token(BACKEND_TOKEN_BASE), mio::Interest::READABLE)?;
        }
        Ok(())
    }

    fn backend_ready(&mut self, ctx: &mut QueueCtx, _token: usize) {
        self.complete_fences(ctx);
    }

    fn tick_interval(&mut self) -> Option<Duration> {
        (!self.pending.is_empty()).then_some(FENCE_POLL)
    }

    fn tick(&mut self, ctx: &mut QueueCtx) {
        self.complete_fences(ctx);
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
        let base = crate::features::RING_EVENT_IDX | crate::features::RING_INDIRECT_DESC;
        match &self.three_d {
            // Linux wants VIRGL to expose any 3D at all, even Venus-only.
            Some(_) => base | F_VIRGL | F_RESOURCE_BLOB | F_CONTEXT_INIT,
            None => base,
        }
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let ncaps = self.three_d.as_ref().map(|t| t.capsets.len() as u32).unwrap_or(0);
        let mut c = [0u8; 16];
        c[0..4].copy_from_slice(&s.events_read.to_le_bytes());
        c[8..12].copy_from_slice(&1u32.to_le_bytes()); // num_scanouts
        c[12..16].copy_from_slice(&ncaps.to_le_bytes());
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
    fn shm_regions(&self) -> Vec<(u8, u64)> {
        self.three_d.as_ref().map(|t| vec![(SHM_HOST_VISIBLE, t.shm_size)]).unwrap_or_default()
    }
    fn set_shm_base(&mut self, id: u8, gpa: u64) {
        if let (SHM_HOST_VISIBLE, Some(t)) = (id, &self.three_d) {
            t.shm_base.store(gpa, Ordering::Release);
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
        if self.worker.with(|_, h| h.has_3d_state()).unwrap_or(false) {
            return Err(Error::Device("snapshots of a VM using 3D are not supported (GPU state lives on the host GPU)".into()));
        }
        // Drivers (Linux, viogpudo) never re-create resources on their own:
        // keep the 2D layout; pixels come back from guest RAM.
        let snap = self.worker.with(|_, h| h.save_2d()).unwrap_or_default();
        let data = serde_json::to_vec(&snap).map_err(|e| Error::Device(e.to_string()))?;
        Ok(DeviceState { queues: self.worker.queues(), data })
    }
    fn restore_active(&mut self, ctx: ActivateContext, data: &[u8]) -> Result<()> {
        {
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            s.irq = Some(ctx.irq.clone());
            // Make the driver re-query and re-setup the display.
            s.events_read |= EVENT_DISPLAY;
        }
        let irq = ctx.irq.clone();
        let mut h = self.handler();
        if !data.is_empty() {
            let snap: Snap2d = serde_json::from_slice(data).map_err(|e| Error::Device(e.to_string()))?;
            h.restore_2d(&ctx.mem, &snap)?;
        }
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

    #[derive(Default)]
    struct Rec {
        modes: Mutex<Vec<(u32, u32)>>,
        frames: Mutex<Vec<(u32, u32, Vec<u8>)>>,
    }
    impl DisplaySink for Rec {
        fn set_mode(&self, w: u32, h: u32) {
            self.modes.lock().unwrap().push((w, h));
        }
        fn flush(&self, _x: u32, _y: u32, w: u32, h: u32, rows: &[u8], stride: usize) {
            let px: Vec<u8> = (0..h as usize).flat_map(|i| rows[i * stride..i * stride + w as usize * 4].to_vec()).collect();
            self.frames.lock().unwrap().push((w, h, px));
        }
    }

    /// A 4x2 B8G8R8X8 resource, backing split over two guest pages, shown
    /// on the scanout. Returns the snapshot of it.
    fn snapshot_of_scene(gpu: &Gpu, mem: &GuestMemory) -> Snap2d {
        let px: Vec<u8> = (0..32).collect();
        mem.write(GuestAddress(0x1000), &px[..16]).unwrap();
        mem.write(GuestAddress(0x3000), &px[16..]).unwrap();
        let mut h = gpu.handler();
        assert_eq!(h.create_cpu(7, 2, 4, 2), OK_NODATA);
        h.resources.get_mut(&7).unwrap().backing = vec![(0x1000, 16), (0x3000, 16)];
        let full = Rect { x: 0, y: 0, w: 4, h: 2 };
        assert_eq!(h.transfer(mem, 7, full, 0, None), OK_NODATA);
        h.scanout = Some(Scanout::Cpu { id: 7, view: full });
        // Through JSON, like a real snapshot.
        serde_json::from_slice(&serde_json::to_vec(&h.save_2d()).unwrap()).unwrap()
    }

    #[test]
    fn snapshot_2d_rebuilds_from_guest_ram() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let sink = Arc::new(Rec::default());
        let (gpu, _ctl) = Gpu::new(64, 64, sink.clone());
        let snap = snapshot_of_scene(&gpu, &mem);
        // Fresh handler (new process after resume): same picture, from RAM.
        let mut h = gpu.handler();
        h.restore_2d(&mem, &snap).unwrap();
        assert_eq!(sink.modes.lock().unwrap().last(), Some(&(4, 2)));
        let frames = sink.frames.lock().unwrap();
        let (w, hh, px) = frames.last().unwrap();
        assert_eq!((*w, *hh), (4, 2));
        let bgr: Vec<u8> = px.chunks(4).flat_map(|p| p[..3].to_vec()).collect();
        let want: Vec<u8> = (0..32u8).collect::<Vec<_>>().chunks(4).flat_map(|p| p[..3].to_vec()).collect();
        assert_eq!(bgr, want);
        // And the resource is usable by later commands.
        assert!(h.resources.contains_key(&7));
    }

    #[test]
    fn snapshot_2d_is_validated() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let (gpu, _ctl) = Gpu::new(64, 64, Arc::new(Rec::default()));
        let good = snapshot_of_scene(&gpu, &mem);
        let mut s = serde_json::from_slice::<Snap2d>(&serde_json::to_vec(&good).unwrap()).unwrap();
        s.resources[0].backing.push((0xffff_0000_0000, 4096)); // outside RAM
        assert!(gpu.handler().restore_2d(&mem, &s).is_err());
        let mut s = serde_json::from_slice::<Snap2d>(&serde_json::to_vec(&good).unwrap()).unwrap();
        s.scanout = Some((7, 1, 0, 4, 2)); // runs off the resource
        assert!(gpu.handler().restore_2d(&mem, &s).is_err());
        let mut s = serde_json::from_slice::<Snap2d>(&serde_json::to_vec(&good).unwrap()).unwrap();
        s.resources[0].width = 1 << 30; // over the memory cap
        assert!(gpu.handler().restore_2d(&mem, &s).is_err());
    }

    #[test]
    fn swizzles() {
        assert_eq!(swizzle(2), Some([0, 1, 2, 3]));
        assert_eq!(swizzle(67), Some([2, 1, 0, 3]));
        assert_eq!(swizzle(68), Some([1, 2, 3, 0]));
        assert_eq!(swizzle(999), None);
    }
}
