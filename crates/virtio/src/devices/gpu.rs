//! virtio-gpu: 2D scanout, plus 3D through a host renderer (Venus).
//!
//! 2D: the guest creates resources, attaches guest-RAM backing, transfers
//! pixels into our host copy, and flushes rects to the scanouts. We push
//! flushed rects to a [`DisplaySink`] per scanout (a guest monitor each,
//! up to [`MAX_SCANOUTS`]; a resource may show on several, at different
//! places of it, as with X11's one big framebuffer).
//!
//! 3D (optional, see [`ThreeD`]): contexts, command submission, blob
//! resources and fences go to a [`Renderer3d`]. Host-visible blobs are
//! mapped into a shared memory window the guest maps like VRAM. With GL
//! (virgl), every 3D resource lives in the renderer and a GL scanout is
//! read back from the GPU on flush. Without it (Venus only), simple 2D
//! textures stay on the CPU path here: there is no GL to render them with.
//!
//! Hostile-guest limits: total resource memory, resource count, backing
//! entries, command sizes, pending fences, and every rect/offset/window
//! range are checked. The renderer itself still parses guest command
//! streams; that's the cost of 3D (docs/SECURITY.md).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use vroom_memory::{GuestAddress, GuestMemory};
use serde::{Deserialize, Serialize};

use crate::devices::gpu3d::{Blob, Box3d, Create3d, Renderer3d, ShmMapper, Transfer3d};
use crate::devices::virgl_state::GlState;
use crate::worker::{Poker, QueueCtx, QueueHandler, WorkerSlot, BACKEND_TOKEN_BASE};
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
/// Ours, not in the spec (drivers skip ids they don't know): `with_vram`.
const SHM_VRAM: u8 = 0x7f;
const BLOB_MEM_GUEST: u32 = 1;
const BLOB_MEM_HOST3D: u32 = 2;
const BLOB_MEM_HOST3D_GUEST: u32 = 3;
const PIPE_TEXTURE_2D: u32 = 2;
const BIND_RENDER_TARGET: u32 = 1 << 1;
const RESOURCE_FLAG_Y_0_TOP: u32 = 1;
/// virgl's GL capset: a context made with capset 0 (classic virgl) uses it.
const CAPSET_VIRGL2: u32 = 2;
/// Vulkan (Venus).
const CAPSET_VENUS: u32 = 4;
/// Direct3D (Windows guests).
const CAPSET_NEPTUNE: u32 = 7;
/// VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE.
const BLOB_FLAG_MAPPABLE: u32 = 1;

const MAX_RESOURCE_BYTES: usize = 512 << 20;
const MAX_RESOURCES: usize = 4096;
const MAX_BACKING_ENTRIES: u32 = 16384;
const MAX_DIM: u32 = 16384;
const MAX_SUBMIT: usize = 16 << 20;
const MAX_PENDING_FENCES: usize = 4096;
/// How often to look for finished fences while some are pending. GL
/// fences have no completion callback (no fence thread, see vroom-gpu3d),
/// so this bounds how long a guest waits after the GPU is done.
const FENCE_POLL: Duration = Duration::from_millis(1);

/// Where finished frames go.
pub trait DisplaySink: Send + Sync {
    /// Scanout size changed (or was disabled: 0x0).
    fn set_mode(&self, width: u32, height: u32);
    /// `rows` holds `h` rows of `w` BGRX pixels, `stride` bytes apart.
    fn flush(&self, x: u32, y: u32, w: u32, h: u32, rows: &[u8], stride: usize);
    /// The guest's pointer changed (cursor queue).
    fn cursor(&self, _c: &Cursor) {}
}

/// The pointer a guest shows through the cursor queue, for the host to
/// draw as its own. Guests that draw their pointer into the frame never
/// send one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cursor {
    /// None set (yet, or since a reset).
    Unset,
    /// The guest hid its pointer.
    Hidden,
    /// `w` x `h` RGBA, not premultiplied; the hot spot is inside it.
    Image { w: u32, h: u32, hot_x: u32, hot_y: u32, rgba: Vec<u8> },
}

/// Biggest cursor taken (drivers use 64x64).
const MAX_CURSOR: u32 = 256;
/// Most scanouts (monitors) a device offers. The spec allows 16.
pub const MAX_SCANOUTS: usize = 4;

/// A monitor after the first: the size the host wants, and whether it is
/// plugged in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Head {
    width: u32,
    height: u32,
    on: bool,
}

/// Display size the host wants; read by GET_DISPLAY_INFO. Scanout 0's
/// size is `width` x `height`; the others are in `more`.
struct Shared {
    width: u32,
    height: u32,
    more: Vec<Head>,
    events_read: u32,
    irq: Option<Arc<dyn VirtioInterrupt>>,
    /// The guest has Direct3D contexts (Windows), which can't be saved.
    direct3d: bool,
    /// Windows' 3D display device is off (the VMM turned it off for a
    /// save): its driver is stopped and resets the GPU when it starts
    /// again, so Direct3D contexts it left behind are dropped (by the
    /// worker, woken through `poker`, or at the save).
    driver_stopped: bool,
    poker: Option<Poker>,
    /// Windows' Direct3D driver (viogpu3d) runs the display. It offers a
    /// new size as a mode, but Windows hides modes its monitor didn't list
    /// when it was plugged in, so a resize there unplugs the monitor and
    /// plugs it back in with the new size (see `GpuControl::resize`).
    replug: bool,
    plug: Plug,
    /// Replugs done for the current size, and in all.
    tries: u32,
    replugs: u32,
    /// What the guest shows on scanout 0.
    shown: (u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Plug {
    In,
    /// Unplugged, to come back at this size. `seen`: the guest cleared the
    /// event, so its next read of the display info is the one that finds
    /// the monitor gone.
    Out { size: (u32, u32), seen: bool, since: Instant },
    /// Back in since then: the guest should switch to the new size. A
    /// resize meanwhile waits (`next`): Windows picks its mode on its own
    /// time, and a replug in the middle of that leaves it at 1024x768.
    Back { since: Instant, next: Option<(u32, u32)> },
}

/// Unplugged this long, the guest gets its monitor back anyway.
const REPLUG_TIMEOUT: Duration = Duration::from_secs(3);
/// Early in boot Windows sometimes lets a replug pass without looking at
/// the monitor again. Not at the new size this long after: try again.
const REPLUG_CHECK: Duration = Duration::from_secs(2);
const REPLUG_TRIES: u32 = 3;
/// Time Windows gets to pick its mode after a replug, before the guest
/// agent is asked for the size.
const REPLUG_SETTLE: Duration = Duration::from_millis(500);

impl Shared {
    fn new(sizes: &[(u32, u32)]) -> Self {
        let (width, height) = sizes.first().copied().unwrap_or((1280, 800));
        let more = sizes.iter().skip(1).map(|&(width, height)| Head { width, height, on: true }).collect();
        Shared {
            width,
            height,
            more,
            events_read: 0,
            irq: None,
            direct3d: false,
            driver_stopped: false,
            poker: None,
            replug: false,
            plug: Plug::In,
            tries: 0,
            replugs: 0,
            shown: (0, 0),
        }
    }

    fn display_event(&mut self) {
        self.events_read |= EVENT_DISPLAY;
        if let Some(irq) = &self.irq {
            irq.signal_config_changed();
        }
    }

    fn unplug(&mut self, size: (u32, u32)) {
        log::debug!("gpu: replugging the monitor for {}x{}", size.0, size.1);
        self.tries += 1;
        self.replugs = self.replugs.wrapping_add(1);
        self.plug = Plug::Out { size, seen: false, since: Instant::now() };
        self.display_event();
    }

    /// End a replug: the monitor is back, at the size asked for last.
    fn plug_back(&mut self) {
        if let Plug::Out { size, .. } = self.plug {
            (self.width, self.height) = size;
        }
        self.plug = Plug::Back { since: Instant::now(), next: None };
        self.display_event();
    }
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
        let current = (s.width, s.height);
        match &mut s.plug {
            Plug::Out { size, .. } => {
                // Mid-replug: come back with this size instead.
                *size = (width, height);
                s.tries = 1;
                return;
            }
            Plug::Back { next, .. } => {
                *next = ((width, height) != current).then_some((width, height));
                return;
            }
            Plug::In => {}
        }
        if current == (width, height) {
            return;
        }
        s.tries = 0;
        if s.replug {
            // The guest reads the unplugged monitor, clears the event, then
            // reads it again to see what is connected: that second read
            // brings it back (GET_DISPLAY_INFO).
            s.unplug((width, height));
            return;
        }
        s.width = width;
        s.height = height;
        s.display_event();
    }

    /// Monitor `head`'s window changed size: tell the guest. Head 0 is
    /// [`resize`]; 0x0 unplugs a monitor after the first.
    pub fn resize_head(&self, head: usize, width: u32, height: u32) {
        if head == 0 {
            return self.resize(width, height);
        }
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let want = match (width, height) {
            (0, 0) => None,
            (w, h) if w <= MAX_DIM && h <= MAX_DIM && w > 0 && h > 0 => Some((w, h)),
            _ => return,
        };
        let Some(m) = s.more.get_mut(head - 1) else { return };
        let before = *m;
        match want {
            Some((w, h)) => *m = Head { width: w, height: h, on: true },
            None => m.on = false,
        }
        if *m != before {
            s.display_event();
        }
    }

    /// The size scanout `head` is offered at; None while unplugged.
    pub fn head_size(&self, head: usize) -> Option<(u32, u32)> {
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match head {
            0 => Some((s.width, s.height)),
            h => s.more.get(h - 1).filter(|m| m.on).map(|m| (m.width, m.height)),
        }
    }

    /// Scanouts the device has.
    pub fn heads(&self) -> usize {
        1 + self.0.lock().unwrap_or_else(|e| e.into_inner()).more.len()
    }

    /// Replugs so far, once the last one has had time to take: the guest's
    /// agent should set the mode after each (a mode Windows' monitor
    /// doesn't list yet can't be set). None while one runs.
    pub fn replugs_done(&self) -> Option<u32> {
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match s.plug {
            Plug::In => Some(s.replugs),
            Plug::Back { since, .. } if s.shown == (s.width, s.height) || since.elapsed() >= REPLUG_SETTLE => Some(s.replugs),
            _ => None,
        }
    }

    /// Call now and then: follows up on replugs.
    pub fn tick(&self) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match s.plug {
            Plug::Out { since, .. } if since.elapsed() > REPLUG_TIMEOUT => {
                log::warn!("gpu: the guest didn't take its monitor's unplug in; plugging it back");
                s.plug_back();
            }
            Plug::Back { since, next } if s.shown == (s.width, s.height) || since.elapsed() > REPLUG_CHECK => {
                if let Some(size) = next {
                    s.tries = 0;
                    s.unplug(size);
                } else if s.shown == (s.width, s.height) {
                    s.plug = Plug::In;
                } else if s.tries < REPLUG_TRIES {
                    let size = (s.width, s.height);
                    s.unplug(size);
                } else {
                    log::info!("gpu: the guest stays at {}x{}, not {}x{}", s.shown.0, s.shown.1, s.width, s.height);
                    s.plug = Plug::In;
                }
            }
            _ => {}
        }
    }

    /// Whether the guest draws with Direct3D now (see [`Shared`]).
    pub fn direct3d(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).direct3d
    }

    /// The guest's 3D driver is stopped (Windows turned the device off for
    /// a save) or running again. Stopped, a save drops the Direct3D
    /// contexts it left behind instead of refusing.
    pub fn set_driver_stopped(&self, stopped: bool) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.driver_stopped = stopped;
        let poker = s.poker.clone().filter(|_| stopped);
        drop(s);
        if let Some(p) = poker {
            p.poke();
        }
    }

    /// Unplug the monitor and plug it back in at the same size (Windows'
    /// 3D driver; others get a display event): the guest sets a mode, and
    /// a scanout, again.
    pub fn replug(&self) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if s.replug && s.plug == Plug::In {
            let size = (s.width, s.height);
            s.tries = 0;
            s.unplug(size);
        } else {
            s.display_event();
        }
    }

    /// The guest shows something on scanout 0.
    pub fn showing(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).shown != (0, 0)
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
    /// The renderer does GL (virgl), not just Venus.
    gl: bool,
}

impl std::fmt::Debug for ThreeD {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreeD").field("capsets", &self.capsets).field("shm_size", &self.shm_size).finish()
    }
}

impl ThreeD {
    pub fn new(renderer: Box<dyn Renderer3d>, mapper: Arc<dyn ShmMapper>, shm_size: u64) -> Self {
        let capsets = renderer.capsets();
        let gl = capsets.iter().any(|c| c.0 == CAPSET_VIRGL2);
        ThreeD { renderer: Arc::new(Mutex::new(renderer)), capsets, mapper, shm_size, shm_base: Arc::new(AtomicU64::new(0)), gl }
    }

    fn r(&self) -> MutexGuard<'_, Box<dyn Renderer3d>> {
        self.renderer.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct Gpu {
    /// Which GPU of the VM (Windows guests get one per monitor).
    index: u16,
    /// Plain memory in BAR 0 (see `with_vram`), and what places it.
    vram: Option<(Arc<dyn ShmMapper>, u64)>,
    shared: Arc<Mutex<Shared>>,
    /// One per scanout.
    sinks: Vec<Arc<dyn DisplaySink>>,
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
        Self::with_heads(&[(width, height)], vec![sink])
    }

    /// A scanout per sink (1..=MAX_SCANOUTS), each starting at its size.
    pub fn with_heads(sizes: &[(u32, u32)], sinks: Vec<Arc<dyn DisplaySink>>) -> (Self, GpuControl) {
        let n = sinks.len().clamp(1, MAX_SCANOUTS);
        let mut sizes = sizes.to_vec();
        sizes.resize(n, sizes.first().copied().unwrap_or((1280, 800)));
        let sinks: Vec<_> = sinks.into_iter().take(n).collect();
        let shared = Arc::new(Mutex::new(Shared::new(&sizes)));
        (Gpu { index: 0, vram: None, shared: shared.clone(), sinks, three_d: None, worker: WorkerSlot::default() }, GpuControl(shared))
    }

    /// Windows' 2D driver (viogpudo) on a GPU other than the boot one
    /// clears "its framebuffer", BAR 0, with stores a hypervisor can't
    /// decode, and the VM stops. Give 2D GPUs like that `size` bytes of
    /// plain memory there (as QEMU's virtio-vga has), the registers next.
    /// The device never reads it. Windows' 3D driver (viogpu3d) wants it
    /// too: when it stops (vroom turns it off for a save) it hands
    /// Windows' basic display driver a 1024x768 framebuffer at BAR 0,
    /// which was the registers.
    pub fn with_vram(mut self, mapper: Arc<dyn ShmMapper>, size: u64) -> Self {
        self.vram = Some((mapper, size));
        self
    }

    /// The VM's `n`th GPU (from 0): its PCI subsystem id is 0x1100 + n,
    /// so a guest can tell which monitor it drives.
    pub fn with_index(mut self, n: u16) -> Self {
        self.index = n.min(0xff);
        self
    }

    /// Offer 3D (Venus, and GL if the renderer has it) on top of 2D.
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
            sinks: self.sinks.clone(),
            resources: HashMap::new(),
            used_bytes: 0,
            scanouts: vec![None; self.sinks.len()],
            modes: vec![(0, 0); self.sinks.len()],
            three_d: self.three_d.clone(),
            res3d: HashMap::new(),
            contexts: Vec::new(),
            cpu_contexts: Vec::new(),
            pending: Vec::new(),
            ctx_info: HashMap::new(),
            reading: None,
            read_buf: Vec::new(),
            cursor: None,
            cursor_shown: None,
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
    /// Format, width and height of a classic 3D resource (GL scanouts).
    tex: Option<(u32, u32, u32)>,
    /// How a classic 3D resource was made (a snapshot makes it again).
    create: Option<Create3d>,
    /// Its guest backing, as last attached.
    backing: Vec<(u64, u32)>,
    /// A blob: memory the guest may map.
    blob: bool,
    /// How a blob was made (a snapshot of a Vulkan context makes its
    /// blobs again).
    made: Option<Blob>,
}

/// What a snapshot needs to know about a renderer context.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CtxInfo {
    capset: u32,
    name: Vec<u8>,
    attached: std::collections::BTreeSet<u32>,
    /// GL (virgl) contexts: their state, followed from what they submit.
    gl: Option<GlState>,
}

#[derive(Debug, Clone, Copy)]
enum Scanout {
    /// A CPU resource; visible rect.
    Cpu { id: u32, view: Rect },
    /// A blob, read straight from its memory.
    Blob { id: u32, view: Rect, format: u32, stride: u32, offset: u32 },
    /// A GL resource, read back from the GPU.
    Gl { id: u32, view: Rect, format: u32 },
}

impl Scanout {
    fn id(&self) -> u32 {
        match *self {
            Scanout::Cpu { id, .. } | Scanout::Blob { id, .. } | Scanout::Gl { id, .. } => id,
        }
    }

    fn view(&self) -> Rect {
        match *self {
            Scanout::Cpu { view, .. } | Scanout::Blob { view, .. } | Scanout::Gl { view, .. } => view,
        }
    }
}

/// 2D state for snapshots. Pixels are not saved: they are re-read from
/// the guest backing (restored with RAM), like a full-frame transfer.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Snap2d {
    resources: Vec<SnapRes>,
    /// (resource id, visible rect x, y, w, h) on scanout 0.
    scanout: Option<(u32, u32, u32, u32, u32)>,
    /// The other scanouts: (scanout, resource, x, y, w, h).
    #[serde(default)]
    more: Vec<(u32, u32, u32, u32, u32, u32)>,
    /// The pointer: (resource id or 0 for hidden, hot x, hot y).
    #[serde(default)]
    cursor: Option<(u32, u32, u32)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapRes {
    id: u32,
    format: u32,
    width: u32,
    height: u32,
    backing: Vec<(u64, u32)>,
}

/// A snapshot with 3D in it: `SNAP3D_MAGIC`, the length of the JSON, the
/// JSON (this), then the contents of every resource level back to back.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Snap3d {
    /// The CPU (2D) side, as without 3D.
    cpu: Snap2d,
    resources: Vec<SnapRes3d>,
    contexts: Vec<SnapCtx>,
    /// A GL scanout: (resource, view x, y, w, h).
    gl_scanout: Option<(u32, u32, u32, u32, u32)>,
    /// GL on the other scanouts: (scanout, resource, x, y, w, h).
    #[serde(default)]
    gl_more: Vec<(u32, u32, u32, u32, u32, u32)>,
    /// Fenced commands not yet given back: (queue, head, len). The GPU is
    /// done with them by the time we have read everything back.
    pending: Vec<(usize, u16, u32)>,
    /// Blobs of Vulkan contexts.
    #[serde(default)]
    blobs: Vec<SnapBlob>,
    /// Scanouts showing a blob: (scanout, resource, x, y, w, h, format,
    /// stride, offset).
    #[serde(default)]
    blob_scanouts: Vec<[u32; 9]>,
}

const SNAP3D_MAGIC: &[u8; 4] = b"VG3D";

#[derive(Debug, Serialize, Deserialize)]
struct SnapRes3d {
    id: u32,
    create: Create3d,
    backing: Vec<(u64, u32)>,
    /// Where each level's contents are, after the JSON: (offset, length).
    /// Empty: none kept (multisampled, or it couldn't be read).
    levels: Vec<(u64, u64)>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapCtx {
    id: u32,
    info: CtxInfo,
    /// A Vulkan context: where the renderer's snapshot of it is, after
    /// the JSON: (offset, length, complete). Length 0: none was taken.
    #[serde(default)]
    venus: Option<(u64, u64, bool)>,
}

/// A blob of a Vulkan context, made again after its context.
#[derive(Debug, Serialize, Deserialize)]
struct SnapBlob {
    id: u32,
    ctx: u32,
    mem: u32,
    flags: u32,
    blob_id: u64,
    size: u64,
    /// Guest memory blobs: the guest RAM behind it.
    guest: Vec<(u64, u32)>,
    /// Where the guest had it mapped, from the start of the window.
    map: Option<u64>,
}

impl SnapBlob {
    /// Shared memory the context makes (rings, reply streams), or guest
    /// RAM: made before the context's own state comes back. The rest is
    /// Vulkan device memory, which needs that state.
    fn early(&self) -> bool {
        self.mem == BLOB_MEM_GUEST || (self.blob_id == 0 && self.flags == BLOB_FLAG_MAPPABLE)
    }
}

/// How a level of a classic 3D resource is laid out for a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    bx: Box3d,
    stride: u32,
    layer_stride: u32,
    bytes: usize,
}

// gallium pipe_texture_target.
const PIPE_BUFFER: u32 = 0;
const PIPE_TEXTURE_1D: u32 = 1;
const PIPE_TEXTURE_3D: u32 = 3;
const PIPE_TEXTURE_1D_ARRAY: u32 = 6;
/// No single resource level bigger than this in a snapshot.
const MAX_LEVEL_BYTES: u64 = 4 << 30;

/// Block width, height and bytes of virgl's compressed formats (virgl_hw.h).
fn compressed_block(format: u32) -> Option<(u32, u32, u32)> {
    Some(match format {
        105 | 106 | 109 | 110 | 113 | 114 | 143 | 144 | 226 | 269..=272 | 275 | 276 => (4, 4, 8),
        107 | 108 | 111 | 112 | 115 | 116 | 145 | 146 | 255..=258 | 273 | 274 | 277..=279 | 293 => (4, 4, 16),
        280 | 294 => (5, 4, 16),
        281 | 295 => (5, 5, 16),
        282 | 296 => (6, 5, 16),
        283 | 297 => (6, 6, 16),
        284 | 298 => (8, 5, 16),
        285 | 299 => (8, 6, 16),
        286 | 300 => (8, 8, 16),
        287 | 301 => (10, 5, 16),
        288 | 302 => (10, 6, 16),
        289 | 303 => (10, 8, 16),
        290 | 304 => (10, 10, 16),
        291 | 305 => (12, 10, 16),
        292 | 306 => (12, 12, 16),
        _ => return None,
    })
}

/// Level `level` of a resource made with `a`, tightly packed. `row0`:
/// virgl's bytes per row of level 0 (for the pixel size of uncompressed
/// formats). None if it can't be worked out.
fn level_layout(a: &Create3d, row0: Option<u32>, level: u32) -> Option<Layout> {
    if a.target == PIPE_BUFFER {
        let bx = Box3d { x: 0, y: 0, z: 0, w: a.width, h: 1, d: 1 };
        return Some(Layout { bx, stride: 0, layer_stride: 0, bytes: a.width as usize });
    }
    if level > a.last_level || level >= 16 || a.width == 0 {
        return None;
    }
    let mip = |v: u32| (v >> level).max(1);
    let w = mip(a.width);
    let h = if matches!(a.target, PIPE_TEXTURE_1D | PIPE_TEXTURE_1D_ARRAY) { 1 } else { mip(a.height) };
    let d = if a.target == PIPE_TEXTURE_3D { mip(a.depth) } else { a.array_size.max(1) };
    let (stride, rows) = match compressed_block(a.format) {
        Some((bw, bh, bytes)) => (w.div_ceil(bw) as u64 * bytes as u64, h.div_ceil(bh) as u64),
        None => {
            let row0 = row0? as u64;
            if row0 == 0 || !row0.is_multiple_of(a.width as u64) {
                return None;
            }
            (w as u64 * (row0 / a.width as u64), h as u64)
        }
    };
    let layer = stride.checked_mul(rows)?;
    let bytes = layer.checked_mul(d as u64)?;
    if bytes > MAX_LEVEL_BYTES || stride > u32::MAX as u64 || layer > u32::MAX as u64 {
        return None;
    }
    let bx = Box3d { x: 0, y: 0, z: 0, w, h, d };
    Some(Layout { bx, stride: stride as u32, layer_stride: layer as u32, bytes: bytes as usize })
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
    /// When it was queued, and whether we said it is late.
    since: std::time::Instant,
    late: bool,
}

/// A fence that takes this long means a stuck guest driver: say so once.
const FENCE_LATE: Duration = Duration::from_secs(5);

pub struct GpuHandler {
    shared: Arc<Mutex<Shared>>,
    /// One per scanout, as are the next two.
    sinks: Vec<Arc<dyn DisplaySink>>,
    resources: HashMap<u32, Resource>,
    used_bytes: usize,
    scanouts: Vec<Option<Scanout>>,
    modes: Vec<(u32, u32)>,
    three_d: Option<ThreeD>,
    res3d: HashMap<u32, Res3d>,
    contexts: Vec<u32>,
    /// Contexts with no renderer side (capset 0).
    cpu_contexts: Vec<u32>,
    pending: Vec<Pending>,
    ctx_info: HashMap<u32, CtxInfo>,
    /// A GL scanout on its way back from the GPU, and where it lands.
    reading: Option<Reading>,
    read_buf: Vec<u8>,
    /// The pointer as last set: (resource id or 0, hot x, hot y), and
    /// what the sink was last given.
    cursor: Option<(u32, u32, u32)>,
    cursor_shown: Option<Cursor>,
}

/// Where a GL frame being read back goes: the rect of resource `id`
/// read, and the scanouts that showed it (scanout, its view then).
#[derive(Debug, Clone)]
struct Reading {
    id: u32,
    rect: Rect,
    to: Vec<(usize, Rect)>,
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

    /// The part of this rect inside `v` (a scanout's view), if any.
    fn clip(&self, v: Rect) -> Option<Rect> {
        let x0 = self.x.max(v.x);
        let y0 = self.y.max(v.y);
        let x1 = self.x.saturating_add(self.w).min(v.x.saturating_add(v.w));
        let y1 = self.y.saturating_add(self.h).min(v.y.saturating_add(v.h));
        (x1 > x0 && y1 > y0).then(|| Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
    }

    /// The smallest rect holding both.
    fn union(&self, o: Rect) -> Rect {
        let (x0, y0) = (self.x.min(o.x), self.y.min(o.y));
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
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

/// Which source byte is alpha in a cursor. The X of an X format counts:
/// Linux makes every dumb buffer B8G8R8X8, its cursors too, and puts
/// alpha there (QEMU reads it that way as well).
fn cursor_alpha(format: u32) -> usize {
    match format {
        3 | 4 | 68 | 121 => 0, // A8R8G8B8 / X8R8G8B8 / X8B8G8R8 / A8B8G8R8
        _ => 3,
    }
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
    for (d, p) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
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
                let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                let mut b = vec![0u8; 16 * 24];
                match s.plug {
                    Plug::Out { seen, .. } => {
                        log::debug!("gpu: display info: unplugged");
                        if seen {
                            // The guest has seen the monitor go: bring it back.
                            s.plug_back();
                        }
                    }
                    _ => {
                        log::debug!("gpu: display info: {}x{}", s.width, s.height);
                        b[8..12].copy_from_slice(&s.width.to_le_bytes());
                        b[12..16].copy_from_slice(&s.height.to_le_bytes());
                        b[16..20].copy_from_slice(&1u32.to_le_bytes()); // enabled
                    }
                }
                for (i, m) in s.more.iter().enumerate().filter(|(_, m)| m.on) {
                    let o = (i + 1) * 24;
                    log::debug!("gpu: display info {}: {}x{}", i + 1, m.width, m.height);
                    b[o + 8..o + 12].copy_from_slice(&m.width.to_le_bytes());
                    b[o + 12..o + 16].copy_from_slice(&m.height.to_le_bytes());
                    b[o + 16..o + 20].copy_from_slice(&1u32.to_le_bytes());
                }
                (OK_DISPLAY_INFO, b)
            }
            RESOURCE_CREATE_2D => match self.three_d.clone() {
                Some(t) if t.gl => (self.create_2d_gl(r, &t), vec![]),
                _ => (self.create(r), vec![]),
            },
            RESOURCE_UNREF => {
                let Some(id) = rd32(r) else { return (ERR_INVALID_PARAMETER, vec![]) };
                (self.unref(id), vec![])
            }
            SET_SCANOUT => (self.set_scanout(r), vec![]),
            RESOURCE_FLUSH => (self.flush(mem, r), vec![]),
            TRANSFER_TO_HOST_2D => {
                let Some(rect) = Rect::read(r) else { return (ERR_INVALID_PARAMETER, vec![]) };
                let (Some(offset), Some(id)) = (rd64(r), rd32(r)) else { return (ERR_INVALID_PARAMETER, vec![]) };
                if let (true, Some(t)) = (self.res3d.contains_key(&id), &self.three_d) {
                    let bx = Box3d { x: rect.x, y: rect.y, z: 0, w: rect.w, h: rect.h, d: 1 };
                    let tr = Transfer3d { resource: id, level: 0, stride: 0, layer_stride: 0, bx, offset };
                    return (if t.r().transfer(true, 0, &tr) { OK_NODATA } else { ERR_INVALID_PARAMETER }, vec![]);
                }
                (self.transfer(mem, id, rect, offset, None), vec![])
            }
            RESOURCE_ATTACH_BACKING => (self.attach(mem, r), vec![]),
            RESOURCE_DETACH_BACKING => {
                let Some(id) = rd32(r) else { return (ERR_INVALID_PARAMETER, vec![]) };
                if let Some(res) = self.resources.get_mut(&id) {
                    res.backing.clear();
                    return (OK_NODATA, vec![]);
                }
                match (self.res3d.get_mut(&id), &self.three_d) {
                    (Some(res), Some(t)) => {
                        res.backing.clear();
                        t.r().detach_backing(id);
                        (OK_NODATA, vec![])
                    }
                    _ => (ERR_INVALID_RESOURCE_ID, vec![]),
                }
            }
            // The pointer follows the host's (tablet input): only its
            // picture matters, not where the guest says it is.
            UPDATE_CURSOR => {
                self.update_cursor(mem, r);
                (OK_NODATA, vec![])
            }
            MOVE_CURSOR => (OK_NODATA, vec![]),
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
                if capset == 0 && !t.gl {
                    // Classic virgl (Linux makes one for its own use). No GL
                    // here: a context that only ever touches CPU resources.
                    self.cpu_contexts.push(hdr.ctx);
                    return (OK_NODATA, vec![]);
                }
                let capset = if capset == 0 { CAPSET_VIRGL2 } else { capset };
                let n = (nlen as usize).min(64);
                if !t.r().context_create(hdr.ctx, capset, &name[..n]) {
                    return (ERR_UNSPEC, vec![]);
                }
                self.contexts.push(hdr.ctx);
                if capset == CAPSET_NEPTUNE {
                    self.shared.lock().unwrap_or_else(|e| e.into_inner()).replug = true;
                }
                let gl = (capset == 1 || capset == CAPSET_VIRGL2).then(GlState::default);
                self.ctx_info.insert(hdr.ctx, CtxInfo { capset, name: name[..n].to_vec(), attached: Default::default(), gl });
                self.note_direct3d();
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
                        self.ctx_info.remove(&hdr.ctx);
                        t.r().context_destroy(hdr.ctx);
                        self.note_direct3d();
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
                    let attached = self.ctx_info.get_mut(&hdr.ctx).map(|c| &mut c.attached);
                    if hdr.ty == CTX_ATTACH_RESOURCE {
                        t.r().context_attach(hdr.ctx, id);
                        attached.map(|a| a.insert(id));
                    } else {
                        t.r().context_detach(hdr.ctx, id);
                        attached.map(|a| a.remove(&id));
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
                // What the renderer gets may change in its hands: keep a copy
                // for the GL state, followed once the renderer took it.
                let gl = self.ctx_info.get_mut(&hdr.ctx).and_then(|c| c.gl.as_mut()).filter(|g| g.broken().is_none());
                let words: Option<Vec<u32>> = gl.is_some().then(|| buf.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect());
                if t.r().submit(hdr.ctx, &mut buf) {
                    if let (Some(w), Some(g)) = (words, self.ctx_info.get_mut(&hdr.ctx).and_then(|c| c.gl.as_mut())) {
                        g.follow(&w);
                    }
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

    /// Drop the Direct3D contexts a stopped driver left behind (see
    /// `Shared::driver_stopped`). How many there were.
    fn drop_direct3d(&mut self) -> usize {
        let Some(t) = self.three_d.clone() else { return 0 };
        let gone: Vec<u32> = self.ctx_info.iter().filter(|(_, c)| c.capset == CAPSET_NEPTUNE).map(|(&id, _)| id).collect();
        for id in &gone {
            self.contexts.retain(|c| c != id);
            self.ctx_info.remove(id);
            t.r().context_destroy(*id);
        }
        self.note_direct3d();
        gone.len()
    }

    fn note_direct3d(&self) {
        let on = self.ctx_info.values().any(|c| c.capset == CAPSET_NEPTUNE);
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).direct3d = on;
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

    /// With GL, a 2D resource (a dumb buffer) lives in virglrenderer, like
    /// in QEMU: guest GL draws into them too (Xorg's glamor does), and a
    /// context that touches one virglrenderer doesn't know is dead for good.
    fn create_2d_gl(&mut self, r: &mut Reader, t: &ThreeD) -> u32 {
        let (Some(id), Some(format), Some(w), Some(h)) = (rd32(r), rd32(r), rd32(r), rd32(r)) else {
            return ERR_INVALID_PARAMETER;
        };
        if swizzle(format).is_none() || w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
            return ERR_INVALID_PARAMETER;
        }
        let a = Create3d {
            target: PIPE_TEXTURE_2D,
            format,
            bind: BIND_RENDER_TARGET,
            width: w,
            height: h,
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            flags: RESOURCE_FLAG_Y_0_TOP,
        };
        self.create_tex(id, &a, t)
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
        // Without GL, plain 2D textures (dumb buffers, fbcon) stay on the CPU.
        if !t.gl && a.target == PIPE_TEXTURE_2D && a.depth <= 1 && a.array_size <= 1 && a.last_level == 0 && a.nr_samples <= 1 && swizzle(a.format).is_some() {
            return (self.create_cpu(id, a.format, a.width, a.height), vec![]);
        }
        (self.create_tex(id, &a, t), vec![])
    }

    fn create_tex(&mut self, id: u32, a: &Create3d, t: &ThreeD) -> u32 {
        if !self.id_free(id) || self.resource_count() >= MAX_RESOURCES {
            return ERR_INVALID_PARAMETER;
        }
        if !t.r().create_3d(id, a) {
            return ERR_UNSPEC;
        }
        self.res3d.insert(id, Res3d { tex: Some((a.format, a.width, a.height)), create: Some(*a), ..Default::default() });
        OK_NODATA
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
        let blob = Blob { resource: id, ctx: hdr.ctx, mem: blob_mem, flags, id: blob_id, size };
        (self.make_blob(mem, t, blob, entries), vec![])
    }

    /// A blob, its guest RAM checked (`create_blob`, and snapshots).
    fn make_blob(&mut self, mem: &GuestMemory, t: &ThreeD, blob: Blob, entries: Vec<(u64, u32)>) -> u32 {
        let total: u64 = entries.iter().map(|e| e.1 as u64).sum();
        if blob.mem != BLOB_MEM_HOST3D && total < blob.size {
            return ERR_INVALID_PARAMETER;
        }
        if entries.len() > MAX_BACKING_ENTRIES as usize || entries.iter().any(|&(a, l)| !mem.check_range(GuestAddress(a), l as u64)) {
            return ERR_INVALID_PARAMETER;
        }
        let Some(iov) = Self::host_iov(mem, &entries) else { return ERR_INVALID_PARAMETER };
        if !t.r().create_blob(&blob, iov) {
            return ERR_UNSPEC;
        }
        let guest = if blob.mem == BLOB_MEM_GUEST { entries } else { Vec::new() };
        self.res3d.insert(blob.resource, Res3d { guest, blob: true, made: Some(blob), ..Default::default() });
        OK_NODATA
    }

    fn map_blob(&mut self, t: &ThreeD, id: u32, offset: u64) -> (u32, Vec<u8>) {
        let bad = (ERR_INVALID_PARAMETER, vec![]);
        let Some(res) = self.res3d.get_mut(&id) else { return (ERR_INVALID_RESOURCE_ID, vec![]) };
        if res.mapped.is_some() {
            return bad;
        }
        let ps = t.mapper.granule();
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
        if self.reading.as_ref().is_some_and(|r| r.id == id) {
            self.finish_read(true);
        }
        for s in self.scanouts.iter_mut() {
            if s.is_some_and(|s| s.id() == id) {
                *s = None;
            }
        }
        if let Some(res) = self.resources.remove(&id) {
            self.used_bytes -= res.pixels.len();
            return OK_NODATA;
        }
        for c in self.ctx_info.values_mut() {
            c.attached.remove(&id);
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
        match (self.res3d.get_mut(&id), &self.three_d) {
            (Some(res), Some(t)) => match Self::host_iov(mem, &entries) {
                Some(iov) => {
                    if !t.r().attach_backing(id, iov) {
                        return ERR_UNSPEC;
                    }
                    res.backing = entries;
                    OK_NODATA
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
        self.finish_read(true);
        let Some(rect) = Rect::read(r) else { return ERR_INVALID_PARAMETER };
        let (Some(sid), Some(id)) = (rd32(r), rd32(r)) else { return ERR_INVALID_PARAMETER };
        log::trace!("gpu: set scanout {sid} to res {id} {rect:?}");
        let sid = sid as usize;
        if sid >= self.scanouts.len() {
            return ERR_INVALID_SCANOUT_ID;
        }
        if id == 0 {
            self.scanouts[sid] = None;
            self.set_mode(sid, 0, 0);
            return OK_NODATA;
        }
        let (scanout, w, h) = match (self.resources.get(&id), self.res3d.get(&id).and_then(|r| r.tex)) {
            (Some(res), _) => (Scanout::Cpu { id, view: rect }, res.width, res.height),
            (None, Some((format, w, h))) if swizzle(format).is_some() => (Scanout::Gl { id, view: rect, format }, w, h),
            _ => return ERR_INVALID_RESOURCE_ID,
        };
        if !rect.inside(w, h) || rect.w == 0 || rect.h == 0 {
            return ERR_INVALID_PARAMETER;
        }
        self.scanouts[sid] = Some(scanout);
        self.set_mode(sid, rect.w, rect.h);
        OK_NODATA
    }

    fn set_scanout_blob(&mut self, r: &mut Reader) -> u32 {
        self.finish_read(true);
        let Some(rect) = Rect::read(r) else { return ERR_INVALID_PARAMETER };
        let mut v = [0u32; 14];
        for x in v.iter_mut() {
            let Some(n) = rd32(r) else { return ERR_INVALID_PARAMETER };
            *x = n;
        }
        let [sid, id, width, height, format, _pad, stride, _, _, _, offset, ..] = v;
        log::trace!("gpu: set scanout blob {sid} to res {id} {width}x{height} format {format} stride {stride} offset {offset} {rect:?}");
        let sid = sid as usize;
        if sid >= self.scanouts.len() {
            return ERR_INVALID_SCANOUT_ID;
        }
        if id == 0 {
            self.scanouts[sid] = None;
            self.set_mode(sid, 0, 0);
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
        self.scanouts[sid] = Some(Scanout::Blob { id, view: rect, format, stride, offset });
        self.set_mode(sid, rect.w, rect.h);
        OK_NODATA
    }

    fn set_mode(&mut self, sid: usize, w: u32, h: u32) {
        if self.modes.get(sid).is_some_and(|m| *m != (w, h)) {
            if sid == 0 {
                self.shared.lock().unwrap_or_else(|e| e.into_inner()).shown = (w, h);
            }
            self.modes[sid] = (w, h);
            self.sinks[sid].set_mode(w, h);
        }
    }

    fn flush(&mut self, mem: &GuestMemory, r: &mut Reader) -> u32 {
        let Some(rect) = Rect::read(r) else { return ERR_INVALID_PARAMETER };
        let Some(id) = rd32(r) else { return ERR_INVALID_PARAMETER };
        log::trace!("gpu: flush res {id} {rect:?}");
        self.flush_rect(mem, rect, id)
    }

    fn flush_rect(&mut self, mem: &GuestMemory, rect: Rect, id: u32) -> u32 {
        // Every scanout that shows this resource, with the part of `rect`
        // it shows.
        let targets: Vec<(usize, Scanout, Rect)> = self
            .scanouts
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let s = (*s)?;
                (s.id() == id).then_some(())?;
                Some((i, s, rect.clip(s.view())?))
            })
            .collect();
        if targets.is_empty() {
            return OK_NODATA;
        }
        if let (Scanout::Gl { .. }, Some(t)) = (targets[0].1, self.three_d.clone()) {
            // Show the frame before, then let the GPU copy this one while
            // we go on with other commands: what all of them show, at once.
            self.finish_read(true);
            let all = targets.iter().skip(1).fold(targets[0].2, |u, t| u.union(t.2));
            let bx = Box3d { x: all.x, y: all.y, z: 0, w: all.w, h: all.h, d: 1 };
            if t.r().scanout_start(id, bx) {
                let to = targets.iter().map(|&(i, s, _)| (i, s.view())).collect();
                self.reading = Some(Reading { id, rect: all, to });
                return OK_NODATA;
            }
        }
        for (sid, sc, part) in targets {
            let r = self.flush_part(mem, sid, sc, part, id);
            if r != OK_NODATA {
                return r;
            }
        }
        OK_NODATA
    }

    /// Show `part` (resource coordinates, inside the view) of scanout `sid`.
    fn flush_part(&mut self, mem: &GuestMemory, sid: usize, sc: Scanout, part: Rect, id: u32) -> u32 {
        let view = sc.view();
        let Rect { x: x0, y: y0, w, h } = part;
        let row = w as usize * 4;
        let bx = Box3d { x: x0, y: y0, z: 0, w, h, d: 1 };
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
            Scanout::Gl { format, .. } => {
                let Some(sw) = swizzle(format) else { return ERR_UNSPEC };
                let Some(t) = &self.three_d else { return ERR_UNSPEC };
                if !t.r().read_back(id, bx, row as u32, &mut out) {
                    return ERR_INVALID_PARAMETER;
                } else if sw != [0, 1, 2, 3] {
                    for p in out.as_chunks_mut::<4>().0 {
                        *p = [p[sw[0]], p[sw[1]], p[sw[2]], 0];
                    }
                }
            }
        }
        self.sinks[sid].flush(x0 - view.x, y0 - view.y, w, h, &out, row);
        OK_NODATA
    }

    /// Show the GL frame being read back, once the GPU has it (or now,
    /// with `wait`). Dropped if the scanout moved on meanwhile.
    fn finish_read(&mut self, wait: bool) {
        let (Some(rd), Some(t)) = (&self.reading, &self.three_d) else { return };
        let row = rd.rect.w as usize * 4;
        // Reused: frames come often and are large.
        let mut out = std::mem::take(&mut self.read_buf);
        out.resize(row * rd.rect.h as usize, 0);
        let got = t.r().scanout_poll(&mut out, wait);
        if got.is_some() || wait {
            let Some(rd) = self.reading.take() else { return };
            for &(sid, view) in &rd.to {
                let current = matches!(self.scanouts.get(sid), Some(Some(Scanout::Gl { id, view: v, .. })) if *id == rd.id && *v == view);
                let Some(p) = rd.rect.clip(view) else { continue };
                if got != Some(true) || !current {
                    continue;
                }
                // The part this scanout shows, out of what was read.
                let at = (p.y - rd.rect.y) as usize * row + (p.x - rd.rect.x) as usize * 4;
                if let Some(px) = out.get(at..) {
                    self.sinks[sid].flush(p.x - view.x, p.y - view.y, p.w, p.h, px, row);
                }
            }
        }
        self.read_buf = out;
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
                    if hdr.ty == CTX_DESTROY && ty == OK_NODATA {
                        for (q, head, len) in self.drop_context_fences(hdr.ctx) {
                            ctx.add_used(q, head, len);
                        }
                    }
                    if ty >= ERR_UNSPEC {
                        log::debug!("gpu: command {:#x} ctx {} -> {ty:#x}", hdr.ty, hdr.ctx);
                    } else {
                        log::trace!("gpu: command {:#x} ctx {} ok", hdr.ty, hdr.ctx);
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
                    self.pending.push(Pending { q, head, len, ctx: fctx, ring, fence: id, since: std::time::Instant::now(), late: false });
                    continue;
                }
            }
            ctx.add_used(q, head, len);
        }
    }

    /// A context is gone: the renderer drops its fences, so commands still
    /// waiting on one of its rings go back now (they would wait forever).
    fn drop_context_fences(&mut self, id: u32) -> Vec<(usize, u16, u32)> {
        let mut out = Vec::new();
        self.pending.retain(|p| {
            let gone = p.ring.is_some() && p.ctx == id;
            if gone {
                out.push((p.q, p.head, p.len));
            }
            !gone
        });
        out
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
        for p in self.pending.iter_mut().filter(|p| !p.late && p.since.elapsed() > FENCE_LATE) {
            p.late = true;
            log::warn!("gpu: fence {} (context {}, ring {:?}) still not done after {:?}", p.fence, p.ctx, p.ring, FENCE_LATE);
        }
    }

    /// UPDATE_CURSOR: pos (scanout, x, y, padding), resource, hot x, hot y.
    fn update_cursor(&mut self, mem: &GuestMemory, r: &mut Reader) {
        let mut v = [0u32; 7];
        for x in v.iter_mut() {
            let Some(n) = rd32(r) else { return };
            *x = n;
        }
        let [scanout, _, _, _, id, hot_x, hot_y] = v;
        // One pointer, whichever monitor it is on.
        if scanout as usize >= self.scanouts.len() {
            return;
        }
        let before = self.cursor.replace((id, hot_x, hot_y));
        if !self.show_cursor(mem) {
            self.cursor = before;
        }
    }

    /// Hand the pointer as set to the sink, if it changed. False if its
    /// picture can't be read.
    fn show_cursor(&mut self, mem: &GuestMemory) -> bool {
        let c = match self.cursor {
            None => Cursor::Unset,
            Some((0, _, _)) => Cursor::Hidden,
            Some((id, hx, hy)) => match self.cursor_image(mem, id) {
                Some((w, h, rgba)) => Cursor::Image { w, h, hot_x: hx.min(w - 1), hot_y: hy.min(h - 1), rgba },
                None => {
                    log::debug!("gpu: cursor resource {id} can't be read");
                    return false;
                }
            },
        };
        if self.cursor_shown.as_ref() != Some(&c) {
            // One pointer for all the monitors: through the first.
            self.sinks[0].cursor(&c);
            self.cursor_shown = Some(c);
        }
        true
    }

    /// A cursor resource's pixels as RGBA: from our copy (2D), or from its
    /// guest backing (a dumb buffer in the renderer: the guest wrote it
    /// there, then transferred it).
    fn cursor_image(&self, mem: &GuestMemory, id: u32) -> Option<(u32, u32, Vec<u8>)> {
        let (format, w, h, src) = if let Some(res) = self.resources.get(&id) {
            (res.format, res.width, res.height, None)
        } else {
            let res = self.res3d.get(&id)?;
            let (format, w, h) = res.tex?;
            (format, w, h, Some(&res.backing))
        };
        if w == 0 || h == 0 || w > MAX_CURSOR || h > MAX_CURSOR {
            return None;
        }
        let sw = swizzle(format)?;
        let a = cursor_alpha(format);
        let mut px = vec![0u8; w as usize * h as usize * 4];
        match src {
            None => px.copy_from_slice(&self.resources.get(&id)?.pixels),
            Some(backing) => {
                if backing.is_empty() || !read_backing(mem, backing, 0, &mut px) {
                    return None;
                }
            }
        }
        // No alpha at all: a guest that really meant X. Opaque, not invisible.
        let opaque = px.as_chunks::<4>().0.iter().all(|p| p[a] == 0);
        for p in px.as_chunks_mut::<4>().0 {
            let s = *p;
            *p = [s[sw[2]], s[sw[1]], s[sw[0]], if opaque { 0xff } else { s[a] }];
        }
        Some((w, h, px))
    }

    fn save_2d(&self) -> Snap2d {
        let mut resources: Vec<SnapRes> = self
            .resources
            .iter()
            .map(|(&id, r)| SnapRes { id, format: r.format, width: r.width, height: r.height, backing: r.backing.clone() })
            .collect();
        resources.sort_by_key(|r| r.id);
        let scanout = match self.scanouts[0] {
            Some(Scanout::Cpu { id, view }) => Some((id, view.x, view.y, view.w, view.h)),
            _ => None,
        };
        let more = self.scanouts_of(|s| matches!(s, Scanout::Cpu { .. }));
        Snap2d { resources, scanout, more, cursor: self.cursor }
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
        if s.more.iter().any(|m| m.0 == 0) {
            return Err(bad("bad scanout"));
        }
        let all = s.scanout.map(|(id, x, y, w, h)| (0, id, x, y, w, h)).into_iter().chain(s.more.iter().copied());
        for (sid, id, x, y, w, h) in all {
            let view = Rect { x, y, w, h };
            let ok = self.resources.get(&id).is_some_and(|r| view.inside(r.width, r.height) && w > 0 && h > 0);
            let sid = sid as usize;
            if !ok || sid >= self.scanouts.len() {
                return Err(bad("bad scanout"));
            }
            self.scanouts[sid] = Some(Scanout::Cpu { id, view });
            self.set_mode(sid, w, h);
        }
        self.flush_all(mem);
        // Shown once every resource is back (it may be a 3D one).
        self.cursor = s.cursor;
        Ok(())
    }

    /// Push every visible scanout to its sink.
    fn flush_all(&mut self, mem: &GuestMemory) {
        for sid in 0..self.scanouts.len() {
            if let Some(sc @ (Scanout::Cpu { id, view } | Scanout::Gl { id, view, .. })) = self.scanouts[sid] {
                self.finish_read(true);
                let _ = self.flush_part(mem, sid, sc, view, id);
            }
        }
    }

    /// (scanout, resource, view) of scanouts after the first that `keep`
    /// says to save.
    fn scanouts_of(&self, keep: impl Fn(&Scanout) -> bool) -> Vec<(u32, u32, u32, u32, u32, u32)> {
        let mut v = Vec::new();
        for (i, s) in self.scanouts.iter().enumerate().skip(1) {
            if let Some(s) = s.filter(|s| keep(s)) {
                let r = s.view();
                v.push((i as u32, s.id(), r.x, r.y, r.w, r.h));
            }
        }
        v
    }

    /// Why a snapshot can't capture the GPU right now, if it can't.
    fn snapshot_blocker(&self) -> Option<String> {
        if self.ctx_info.values().any(|c| c.capset == CAPSET_NEPTUNE) {
            return Some("Windows draws with Direct3D, and Direct3D state can't be saved. With the display set to 2D, it can".into());
        }
        // Vulkan contexts are saved by the renderer, their blobs made again
        // after them. Other host blobs no context uses any more are left
        // out: a stopped driver (Windows, with its 3D device turned off for
        // a suspend) leaves them behind, its last frame still set as the
        // scanout, and never touches them again. Guests' own scanouts are
        // attached to the context that made them, or in guest memory.
        let in_use = |id: u32, r: &Res3d| !r.guest.is_empty() || self.ctx_info.values().any(|c| c.attached.contains(&id));
        if self.res3d.iter().any(|(&id, r)| r.blob && !self.venus_blob(r) && in_use(id, r)) {
            return Some("an app shares GPU memory with the guest (a blob), and that can't be saved".into());
        }
        if let Some(why) = self.ctx_info.values().find_map(|c| c.gl.as_ref().and_then(GlState::broken)) {
            return Some(format!("an OpenGL app did something vroom can't save ({why})"));
        }
        None
    }

    /// A blob a Vulkan context made, the context still there.
    fn venus_blob(&self, r: &Res3d) -> bool {
        r.made.is_some_and(|b| self.ctx_info.get(&b.ctx).is_some_and(|c| c.capset == CAPSET_VENUS))
    }

    fn has_3d_state(&self) -> bool {
        !self.contexts.is_empty() || !self.res3d.is_empty()
    }

    /// Everything on the GPU side, read back. Run with the GL context free
    /// for this thread (see `WorkerSlot::release_thread`).
    fn save_3d(&mut self) -> Result<Vec<u8>> {
        let bad = |m: String| Error::Device(format!("gpu snapshot: {m}"));
        let Some(t) = self.three_d.clone() else { return Err(bad("no 3D renderer".into())) };
        // The guest may wait on the result of an ended query: have it
        // written to its buffer now, before that buffer is read back.
        for (&id, c) in &self.ctx_info {
            let Some(gl) = &c.gl else { continue };
            let mut cmds: Vec<u8> = gl.fetch_query_results().iter().flat_map(|w| w.to_le_bytes()).collect();
            if !cmds.is_empty() && !t.r().submit(id, &mut cmds) {
                log::warn!("gpu snapshot: context {id}: query results not fetched");
            }
        }
        let mut blob: Vec<u8> = Vec::new();
        let mut resources = Vec::new();
        let mut ids: Vec<u32> = self.res3d.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let r = &self.res3d[&id];
            let Some(a) = r.create else { continue };
            let start = blob.len();
            let mut levels = Vec::new();
            if a.nr_samples <= 1 {
                let row0 = t.r().row_bytes(id);
                for level in 0..=a.last_level {
                    let read = level_layout(&a, row0, level).and_then(|l| {
                        let at = blob.len();
                        blob.resize(at + l.bytes, 0);
                        t.r().read_level(id, level, l.bx, l.stride, l.layer_stride, &mut blob[at..]).then_some((at as u64, l.bytes as u64))
                    });
                    match read {
                        Some(l) => levels.push(l),
                        None => {
                            // It comes back empty rather than not at all.
                            log::warn!("gpu snapshot: resource {id} (format {}, target {}) level {level} can't be read back", a.format, a.target);
                            blob.truncate(start);
                            levels.clear();
                            break;
                        }
                    }
                }
            }
            resources.push(SnapRes3d { id, create: a, backing: r.backing.clone(), levels });
        }
        let mut contexts: Vec<SnapCtx> = self.ctx_info.iter().map(|(&id, info)| SnapCtx { id, info: info.clone(), venus: None }).collect();
        contexts.sort_unstable_by_key(|c| c.id);
        // Vulkan contexts: the renderer stops each at a command boundary,
        // waits for the GPU and hands back all it needs.
        for c in contexts.iter_mut().filter(|c| c.info.capset == CAPSET_VENUS) {
            let started = std::time::Instant::now();
            match t.r().venus_save(c.id) {
                Some((complete, data)) => {
                    log::info!(
                        "gpu snapshot: Vulkan context {} ({}): {} KiB in {} ms{}",
                        c.id,
                        String::from_utf8_lossy(&c.info.name),
                        data.len() >> 10,
                        started.elapsed().as_millis(),
                        if complete { "" } else { ", saved as lost" }
                    );
                    c.venus = Some((blob.len() as u64, data.len() as u64, complete));
                    blob.extend_from_slice(&data);
                }
                None => {
                    log::warn!("gpu snapshot: Vulkan context {} ({}) not saved: its app will see the device lost", c.id, String::from_utf8_lossy(&c.info.name));
                    c.venus = Some((0, 0, false));
                }
            }
        }
        let base = t.shm_base.load(Ordering::Acquire);
        let mut blobs: Vec<SnapBlob> = self
            .res3d
            .iter()
            .filter(|(_, r)| self.venus_blob(r))
            .filter_map(|(&id, r)| {
                let b = r.made?;
                let map = r.mapped.and_then(|(gpa, _)| gpa.checked_sub(base));
                Some(SnapBlob { id, ctx: b.ctx, mem: b.mem, flags: b.flags, blob_id: b.id, size: b.size, guest: r.guest.clone(), map })
            })
            .collect();
        blobs.sort_unstable_by_key(|b| b.id);
        let blob_scanouts: Vec<[u32; 9]> = self
            .scanouts
            .iter()
            .enumerate()
            .filter_map(|(sid, s)| match *s {
                Some(Scanout::Blob { id, view, format, stride, offset }) if blobs.iter().any(|b| b.id == id) => {
                    Some([sid as u32, id, view.x, view.y, view.w, view.h, format, stride, offset])
                }
                _ => None,
            })
            .collect();
        let gl_scanout = match self.scanouts[0] {
            Some(Scanout::Gl { id, view, .. }) => Some((id, view.x, view.y, view.w, view.h)),
            _ => None,
        };
        let gl_more = self.scanouts_of(|s| matches!(s, Scanout::Gl { .. }));
        let snap = Snap3d {
            cpu: self.save_2d(),
            resources,
            contexts,
            gl_scanout,
            gl_more,
            pending: self.pending.iter().map(|p| (p.q, p.head, p.len)).collect(),
            blobs,
            blob_scanouts,
        };
        // Hand GL back to the worker thread.
        t.r().release_thread();
        let json = serde_json::to_vec(&snap).map_err(|e| bad(e.to_string()))?;
        let mut out = Vec::with_capacity(12 + json.len() + blob.len());
        out.extend_from_slice(SNAP3D_MAGIC);
        out.extend_from_slice(&(json.len() as u64).to_le_bytes());
        out.extend_from_slice(&json);
        out.extend_from_slice(&blob);
        log::info!("gpu snapshot: {} resources, {} contexts, {} MiB of GPU contents", snap.resources.len(), snap.contexts.len(), blob.len() >> 20);
        Ok(out)
    }

    /// Build the GPU side again from [`save_3d`]'s output (untrusted input:
    /// checked like commands; GL state goes through virglrenderer's checks
    /// like anything the guest submits).
    fn restore_3d(&mut self, mem: &GuestMemory, data: &[u8]) -> Result<Vec<(usize, u16, u32)>> {
        let bad = |m: &str| Error::Device(format!("gpu snapshot: {m}"));
        let Some(t) = self.three_d.clone() else { return Err(bad("it has 3D state, but this VM has no 3D")) };
        let n = data.get(4..12).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes).ok_or_else(|| bad("truncated"))?;
        let json_end = 12usize.checked_add(usize::try_from(n).map_err(|_| bad("bad length"))?).filter(|&e| e <= data.len()).ok_or_else(|| bad("truncated"))?;
        let snap: Snap3d = serde_json::from_slice(&data[12..json_end]).map_err(|e| bad(&e.to_string()))?;
        let blob = &data[json_end..];
        self.restore_2d(mem, &snap.cpu)?;
        if snap.resources.len() > MAX_RESOURCES || snap.contexts.len() > MAX_RESOURCES {
            return Err(bad("too many objects"));
        }
        for r in &snap.resources {
            let a = &r.create;
            // A buffer's width is its size in bytes.
            let max_w = if a.target == PIPE_BUFFER { u32::MAX >> 1 } else { MAX_DIM };
            if !self.id_free(r.id) || a.width > max_w || a.height > MAX_DIM || a.depth > MAX_DIM || a.array_size > MAX_DIM {
                return Err(bad("bad resource"));
            }
            if r.backing.len() > MAX_BACKING_ENTRIES as usize || r.backing.iter().any(|&(g, l)| !mem.check_range(GuestAddress(g), l as u64)) {
                return Err(bad("bad backing"));
            }
            if !t.r().create_3d(r.id, a) {
                return Err(bad("can't make a resource again"));
            }
            self.res3d.insert(r.id, Res3d { tex: Some((a.format, a.width, a.height)), create: Some(*a), backing: r.backing.clone(), ..Default::default() });
            if !r.backing.is_empty() {
                let Some(iov) = Self::host_iov(mem, &r.backing) else { return Err(bad("bad backing")) };
                if !t.r().attach_backing(r.id, iov) {
                    return Err(bad("can't attach backing"));
                }
            }
            let row0 = t.r().row_bytes(r.id);
            for (level, &(off, len)) in r.levels.iter().enumerate() {
                let l = level_layout(a, row0, level as u32).filter(|l| l.bytes as u64 == len).ok_or_else(|| bad("bad level"))?;
                let src = usize::try_from(off).ok().and_then(|o| blob.get(o..o.checked_add(l.bytes)?)).ok_or_else(|| bad("truncated contents"))?;
                if !t.r().write_level(r.id, level as u32, l.bx, l.stride, l.layer_stride, src) {
                    log::warn!("gpu snapshot: resource {} level {level}: contents not written back", r.id);
                }
            }
        }
        if snap.blobs.len() > MAX_RESOURCES {
            return Err(bad("too many objects"));
        }
        // Vulkan contexts first: GL contexts may use their blobs (windows).
        for c in snap.contexts.iter().filter(|c| c.info.capset == CAPSET_VENUS) {
            self.restore_venus(mem, &t, c, &snap.blobs, blob)?;
        }
        for b in &snap.blobs {
            if !self.res3d.contains_key(&b.id) {
                return Err(bad("a blob without its context"));
            }
            if let Some(off) = b.map {
                // As the guest had it: checked like its own request.
                if self.map_blob(&t, b.id, off).0 != OK_MAP_INFO {
                    return Err(bad("can't map a blob where the guest had it"));
                }
            }
        }
        for c in snap.contexts.iter().filter(|c| c.info.capset != CAPSET_VENUS) {
            if c.id == 0 || self.contexts.contains(&c.id) || c.info.name.len() > 64 {
                return Err(bad("bad context"));
            }
            if !t.r().context_create(c.id, c.info.capset, &c.info.name) {
                return Err(bad("can't make a context again"));
            }
            self.contexts.push(c.id);
            for &rid in c.info.attached.iter().filter(|r| self.res3d.contains_key(r)) {
                t.r().context_attach(c.id, rid);
            }
            if let Some(gl) = &c.info.gl {
                let words = gl.replay(&|r| self.res3d.contains_key(&r));
                if !self.submit_words(&t, c.id, &words) {
                    // That app's GL is gone; the rest of the desktop isn't.
                    log::warn!("gpu snapshot: context {} ({}): GL state not rebuilt", c.id, String::from_utf8_lossy(&c.info.name));
                }
            }
            self.ctx_info.insert(c.id, c.info.clone());
        }
        if snap.gl_more.iter().any(|m| m.0 == 0) {
            return Err(bad("bad scanout"));
        }
        let gl = snap.gl_scanout.map(|(id, x, y, w, h)| (0, id, x, y, w, h)).into_iter().chain(snap.gl_more.iter().copied());
        let mut any = false;
        for (sid, id, x, y, w, h) in gl {
            let view = Rect { x, y, w, h };
            let sid = sid as usize;
            match self.res3d.get(&id).and_then(|r| r.tex) {
                Some((format, rw, rh)) if sid < self.scanouts.len() && view.inside(rw, rh) && w > 0 && h > 0 && swizzle(format).is_some() => {
                    self.scanouts[sid] = Some(Scanout::Gl { id, view, format });
                    self.set_mode(sid, w, h);
                    any = true;
                }
                _ => return Err(bad("bad scanout")),
            }
        }
        for &[sid, id, x, y, w, h, format, stride, offset] in &snap.blob_scanouts {
            let view = Rect { x, y, w, h };
            let sid = sid as usize;
            let ok = self.res3d.get(&id).is_some_and(|r| r.blob)
                && sid < self.scanouts.len()
                && swizzle(format).is_some()
                && w > 0
                && h > 0
                && x.checked_add(w).is_some_and(|e| e <= MAX_DIM)
                && y.checked_add(h).is_some_and(|e| e <= MAX_DIM)
                // reads of the view stay inside the blob (read_mapped checks)
                && (stride as u64) >= (x as u64 + w as u64) * 4;
            if !ok {
                return Err(bad("bad scanout"));
            }
            self.scanouts[sid] = Some(Scanout::Blob { id, view, format, stride, offset });
            self.set_mode(sid, w, h);
        }
        if any {
            self.flush_all(mem);
        }
        t.r().release_thread();
        Ok(snap.pending)
    }

    /// A Vulkan context and its blobs, again (see `Snap3d::blobs`). A
    /// context the renderer can't build again comes back empty, its blobs
    /// plain shared memory: its app sees the device lost, nothing else
    /// does.
    fn restore_venus(&mut self, mem: &GuestMemory, t: &ThreeD, c: &SnapCtx, blobs: &[SnapBlob], data: &[u8]) -> Result<()> {
        let bad = |m: &str| Error::Device(format!("gpu snapshot: {m}"));
        if c.id == 0 || self.contexts.contains(&c.id) || c.info.name.len() > 64 {
            return Err(bad("bad context"));
        }
        if !t.r().context_create(c.id, CAPSET_VENUS, &c.info.name) {
            return Err(bad("can't make a context again"));
        }
        self.contexts.push(c.id);
        self.ctx_info.insert(c.id, CtxInfo { capset: CAPSET_VENUS, name: c.info.name.clone(), attached: c.info.attached.clone(), gl: None });
        let mine = || blobs.iter().filter(|b| b.ctx == c.id);
        let check = |b: &SnapBlob| -> Result<Blob> {
            if b.size == 0 || !matches!(b.mem, BLOB_MEM_GUEST | BLOB_MEM_HOST3D | BLOB_MEM_HOST3D_GUEST) {
                return Err(bad("bad blob"));
            }
            Ok(Blob { resource: b.id, ctx: c.id, mem: b.mem, flags: b.flags, id: b.blob_id, size: b.size })
        };
        for b in mine().filter(|b| b.early()) {
            if !self.id_free(b.id) || self.make_blob(mem, t, check(b)?, b.guest.clone()) != OK_NODATA {
                return Err(bad("can't make a blob again"));
            }
        }
        for &rid in c.info.attached.iter().filter(|r| self.res3d.contains_key(r)) {
            t.r().context_attach(c.id, rid);
        }
        let name = String::from_utf8_lossy(&c.info.name).into_owned();
        let state = c.venus.and_then(|(off, len, _)| {
            let start = usize::try_from(off).ok()?;
            data.get(start..start.checked_add(usize::try_from(len).ok()?)?)
        });
        let started = std::time::Instant::now();
        let restored = match state {
            Some(s) if !s.is_empty() => t.r().venus_restore(c.id, s),
            _ => false,
        };
        if !restored {
            log::warn!("gpu snapshot: Vulkan context {} ({name}) not built again: its app will see the device lost", c.id);
        }
        for b in mine().filter(|b| !b.early()) {
            if !self.id_free(b.id) {
                return Err(bad("bad blob"));
            }
            let blob = check(b)?;
            if restored && self.make_blob(mem, t, blob, Vec::new()) == OK_NODATA {
                continue;
            }
            // Plain shared memory where the guest maps it, so its pages
            // stay backed.
            let stand_in = Blob { mem: BLOB_MEM_HOST3D, flags: BLOB_FLAG_MAPPABLE, id: 0, ..blob };
            if self.make_blob(mem, t, stand_in, Vec::new()) != OK_NODATA {
                return Err(bad("can't make a blob again"));
            }
        }
        if restored {
            if !t.r().venus_resume(c.id) {
                log::warn!("gpu snapshot: Vulkan context {} ({name}) didn't start again", c.id);
            }
            if c.venus.is_some_and(|v| v.2) {
                log::info!("gpu snapshot: Vulkan context {} ({name}) built again in {} ms", c.id, started.elapsed().as_millis());
            } else {
                log::warn!("gpu snapshot: Vulkan context {} ({name}) came back lost: its app sees the device lost", c.id);
            }
        }
        for &rid in c.info.attached.iter().filter(|r| self.res3d.contains_key(r)) {
            t.r().context_attach(c.id, rid);
        }
        Ok(())
    }

    /// Submit a command stream in pieces the renderer takes, split between
    /// commands.
    fn submit_words(&mut self, t: &ThreeD, ctx: u32, words: &[u32]) -> bool {
        let max = MAX_SUBMIT / 4;
        let mut i = 0;
        while i < words.len() {
            let mut end = i;
            while end < words.len() {
                let next = end + 1 + (words[end] >> 16) as usize;
                if next - i > max && end > i {
                    break;
                }
                end = next;
            }
            let mut bytes: Vec<u8> = words[i..end.min(words.len())].iter().flat_map(|w| w.to_le_bytes()).collect();
            if !t.r().submit(ctx, &mut bytes) {
                return false;
            }
            i = end;
        }
        true
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

    fn deregister(&mut self, _reg: &mio::Registry) {
        // Runs on the worker thread as it stops: hand the GL context back.
        if let Some(t) = &self.three_d {
            t.r().release_thread();
        }
    }

    fn release_thread(&mut self) {
        // A snapshot wants GL on its own thread.
        if let Some(t) = &self.three_d {
            t.r().release_thread();
        }
    }

    fn attach(&mut self, poker: Poker) {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).poker = Some(poker);
    }

    fn backend_ready(&mut self, ctx: &mut QueueCtx, _token: usize) {
        self.complete_fences(ctx);
        self.finish_read(false);
        // Now, while the guest runs: its apps stuck waiting on those
        // contexts' rings get the "fatal" status and let go.
        if self.shared.lock().unwrap_or_else(|e| e.into_inner()).driver_stopped {
            let n = self.drop_direct3d();
            if n > 0 {
                log::info!("gpu: {n} Direct3D contexts of the stopped driver dropped");
            }
        }
    }

    fn tick_interval(&mut self) -> Option<Duration> {
        (!self.pending.is_empty() || self.reading.is_some()).then_some(FENCE_POLL)
    }

    fn tick(&mut self, ctx: &mut QueueCtx) {
        self.complete_fences(ctx);
        self.finish_read(false);
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
    fn pci_subsystem(&self) -> u16 {
        0x1100 + self.index
    }
    fn pci_shm_first(&self) -> bool {
        self.vram.is_some()
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
        c[8..12].copy_from_slice(&(self.sinks.len() as u32).to_le_bytes()); // num_scanouts
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
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            s.events_read &= !v;
            if let (Plug::Out { seen, .. }, true) = (&mut s.plug, v & EVENT_DISPLAY != 0) {
                *seen = true;
            }
        }
    }
    fn shm_regions(&self) -> Vec<(u8, u64)> {
        // VRAM first: it goes in BAR 0 (`pci_shm_first`).
        let vram = self.vram.as_ref().map(|(_, size)| (SHM_VRAM, *size));
        let host = self.three_d.as_ref().map(|t| (SHM_HOST_VISIBLE, t.shm_size));
        vram.into_iter().chain(host).collect()
    }
    fn set_shm_base(&mut self, id: u8, gpa: u64) {
        if let (SHM_VRAM, Some((m, size))) = (id, &self.vram) {
            m.place(gpa, *size);
        }
        if let (SHM_HOST_VISIBLE, Some(t)) = (id, &self.three_d) {
            t.shm_base.store(gpa, Ordering::Release);
            t.mapper.place(gpa, t.shm_size);
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
        let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        s.irq = None;
        s.direct3d = false;
        s.driver_stopped = false;
        // The next driver starts with the monitor plugged in.
        s.replug = false;
        if let Plug::Out { size, .. } | Plug::Back { next: Some(size), .. } = s.plug {
            (s.width, s.height) = size;
        }
        s.plug = Plug::In;
        drop(s);
        for s in &self.sinks {
            s.set_mode(0, 0);
            s.cursor(&Cursor::Unset);
        }
    }
    fn pause(&mut self) {
        self.worker.pause();
    }
    fn resume(&mut self) {
        self.worker.resume();
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        if self.shared.lock().unwrap_or_else(|e| e.into_inner()).driver_stopped {
            let n = self.worker.with(|_, h| h.drop_direct3d()).unwrap_or(0);
            if n > 0 {
                log::info!("gpu: {n} Direct3D contexts of the stopped driver dropped");
            }
        }
        if let Some(why) = self.worker.with(|_, h| h.snapshot_blocker()).flatten() {
            return Err(Error::Device(format!("the GPU can't be saved: {why}")));
        }
        if self.worker.with(|_, h| h.has_3d_state()).unwrap_or(false) {
            // GL is bound to the worker's thread: have it let go first.
            if !self.worker.release_thread() {
                return Err(Error::Device("the GPU worker didn't answer".into()));
            }
            let data = self.worker.with(|_, h| h.save_3d()).unwrap_or_else(|| Err(Error::Device("no GPU worker".into())))?;
            return Ok(DeviceState { queues: self.worker.queues(), data });
        }
        // Drivers (Linux, viogpudo) never re-create resources on their own:
        // keep the 2D layout; pixels come back from guest RAM.
        let snap = self.worker.with(|_, h| h.save_2d()).unwrap_or_default();
        let data = serde_json::to_vec(&snap).map_err(|e| Error::Device(e.to_string()))?;
        Ok(DeviceState { queues: self.worker.queues(), data })
    }
    fn restore_active(&mut self, ctx: ActivateContext, data: &[u8]) -> Result<()> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).irq = Some(ctx.irq.clone());
        let irq = ctx.irq.clone();
        let mut h = self.handler();
        let mut shown = None;
        let mut pending = vec![];
        if data.starts_with(SNAP3D_MAGIC) {
            pending = h.restore_3d(&ctx.mem, data)?;
        } else if !data.is_empty() {
            let snap: Snap2d = serde_json::from_slice(data).map_err(|e| Error::Device(e.to_string()))?;
            h.restore_2d(&ctx.mem, &snap)?;
        }
        if !data.is_empty() {
            shown = (h.modes[0] != (0, 0)).then_some(h.modes[0]);
        }
        let modes = h.modes.clone();
        h.show_cursor(&ctx.mem);
        self.worker.start("gpu", ctx, h, true)?;
        // Fenced commands the GPU finished before the snapshot: return them.
        if !pending.is_empty() {
            self.worker.with(|q, _| {
                for (qi, head, len) in pending {
                    q.add_used(qi, head, len);
                }
                q.flush();
            });
        }
        // A display event makes drivers rebuild their surfaces; Windows then
        // repaints only what it thinks changed (a mostly black screen). With
        // a picture restored, keep its mode: a viewer that wants another
        // size asks, which raises the event the normal way.
        let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        for (m, &(w, h)) in s.more.iter_mut().zip(modes.iter().skip(1)) {
            if w != 0 {
                (m.width, m.height) = (w, h);
            }
        }
        match shown {
            Some((w, h)) => (s.width, s.height) = (w, h),
            None => {
                s.events_read |= EVENT_DISPLAY;
                drop(s);
                irq.signal_config_changed();
            }
        }
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

    /// A GL renderer that only records what reaches it.
    struct FakeGl(Arc<Mutex<Vec<String>>>);
    impl Renderer3d for FakeGl {
        fn capsets(&self) -> Vec<(u32, u32, u32)> {
            vec![(CAPSET_VIRGL2, 2, 0)]
        }
        fn capset(&mut self, _: u32, _: u32) -> Option<Vec<u8>> {
            None
        }
        fn context_create(&mut self, _: u32, _: u32, _: &[u8]) -> bool {
            true
        }
        fn context_destroy(&mut self, _: u32) {}
        fn context_attach(&mut self, ctx: u32, res: u32) {
            self.0.lock().unwrap().push(format!("attach {ctx} {res}"));
        }
        fn context_detach(&mut self, _: u32, _: u32) {}
        fn submit(&mut self, ctx: u32, cmds: &mut [u8]) -> bool {
            let words: Vec<u32> = cmds.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
            self.0.lock().unwrap().push(format!("submit {ctx} {words:x?}"));
            true
        }
        fn create_3d(&mut self, res: u32, a: &Create3d) -> bool {
            self.0.lock().unwrap().push(format!("create {res} {}x{} fmt {} target {}", a.width, a.height, a.format, a.target));
            true
        }
        fn attach_backing(&mut self, _: u32, _: crate::devices::gpu3d::HostIov) -> bool {
            true
        }
        fn detach_backing(&mut self, _: u32) {}
        fn transfer(&mut self, to_host: bool, ctx: u32, t: &Transfer3d) -> bool {
            self.0.lock().unwrap().push(format!("transfer {to_host} ctx {ctx} res {} {}x{}", t.resource, t.bx.w, t.bx.h));
            true
        }
        fn create_blob(&mut self, _: &Blob, _: crate::devices::gpu3d::HostIov) -> bool {
            false
        }
        fn map_blob(&mut self, _: u32) -> Option<crate::devices::gpu3d::Mapping> {
            None
        }
        fn unmap_blob(&mut self, _: u32) {}
        fn read_mapped(&mut self, _: u32, _: u64, _: &mut [u8]) -> bool {
            false
        }
        fn read_back(&mut self, _: u32, _: Box3d, _: u32, _: &mut [u8]) -> bool {
            false
        }
        fn row_bytes(&mut self, _: u32) -> Option<u32> {
            Some(8 * 4) // the test's textures are 8 wide, 4 bytes a pixel
        }
        fn read_level(&mut self, res: u32, level: u32, _: Box3d, _: u32, _: u32, out: &mut [u8]) -> bool {
            out.fill(res as u8 + level as u8);
            true
        }
        fn write_level(&mut self, res: u32, level: u32, bx: Box3d, stride: u32, _: u32, data: &[u8]) -> bool {
            self.0.lock().unwrap().push(format!("write {res} level {level} {}x{} stride {stride}: {} bytes of {}", bx.w, bx.h, data.len(), data[0]));
            true
        }
        fn unref(&mut self, _: u32) {}
        fn create_fence(&mut self, _: u32, _: Option<u8>, _: u64) -> bool {
            true
        }
        fn poll_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }
        fn poll(&mut self) -> Vec<crate::devices::gpu3d::FenceDone> {
            vec![]
        }
        fn reset(&mut self) {}
    }
    struct NoMap;
    impl ShmMapper for NoMap {
        fn map(&self, _: u64, _: &crate::devices::gpu3d::Mapping) -> bool {
            false
        }
        fn unmap(&self, _: u64, _: u64) {}
    }

    /// Run one command (header + body) through the handler.
    fn run(h: &mut GpuHandler, ty: u32, ctx: u32, body: &[u32]) -> u32 {
        use crate::queue::tests::*;
        let mut ring = TestRing::new(4);
        let hdr = Hdr { ty, flags: 0, fence: 0, ctx, ring: 0 };
        let b: Vec<u8> = body.iter().flat_map(|v| v.to_le_bytes()).collect();
        ring.add_chain(&[(b.len() as u32, false)]);
        ring.mem.write(GuestAddress(DATA), &b).unwrap();
        let mem = ring.mem.clone();
        let (mut r, _) = ring.q.clone().pop(&mem).unwrap().unwrap().into_rw().unwrap();
        h.command(&mem, &mut r, &hdr).0
    }

    #[test]
    fn context_fences_go_back_with_the_context() {
        let (gpu, _ctl) = Gpu::new(64, 64, Arc::new(Rec::default()));
        let mut h = gpu.handler();
        let now = std::time::Instant::now();
        for (head, ctx, ring) in [(1, 3, Some(1)), (2, 3, None), (3, 4, Some(1)), (4, 3, Some(0))] {
            h.pending.push(Pending { q: 0, head, len: 24, ctx, ring, fence: 7, since: now, late: false });
        }
        assert_eq!(h.drop_context_fences(3), [(0, 1, 24), (0, 4, 24)]);
        // Global fences and other contexts' stay.
        assert_eq!(h.pending.iter().map(|p| p.head).collect::<Vec<_>>(), [2, 3]);
    }

    #[test]
    fn gl_dumb_buffers_live_in_the_renderer() {
        // Xorg's glamor draws into dumb buffers (RESOURCE_CREATE_2D). With
        // GL they must exist in the renderer, or the first GL use of one
        // kills the guest's context and every window turns black.
        let log = Arc::new(Mutex::new(vec![]));
        let t = ThreeD::new(Box::new(FakeGl(log.clone())), Arc::new(NoMap), 0);
        let (gpu, _ctl) = Gpu::new(64, 64, Arc::new(Rec::default()));
        let gpu = gpu.with_3d(t);
        let mut h = gpu.handler();
        assert_eq!(run(&mut h, RESOURCE_CREATE_2D, 0, &[5, 2, 64, 32]), OK_NODATA);
        assert!(h.resources.is_empty() && h.res3d.contains_key(&5));
        assert_eq!(run(&mut h, TRANSFER_TO_HOST_2D, 0, &[0, 0, 64, 32, 0, 0, 5, 0]), OK_NODATA);
        let mut name = [0u32; 18];
        name[0] = 4;
        name[1] = CAPSET_VIRGL2;
        assert_eq!(run(&mut h, CTX_CREATE, 1, &name), OK_NODATA);
        assert_eq!(run(&mut h, CTX_ATTACH_RESOURCE, 1, &[5, 0]), OK_NODATA);
        assert_eq!(
            *log.lock().unwrap(),
            ["create 5 64x32 fmt 2 target 2", "transfer true ctx 0 res 5 64x32", "attach 1 5"]
        );
        // Without GL nothing changes: the CPU keeps them.
        let (gpu, _ctl) = Gpu::new(64, 64, Arc::new(Rec::default()));
        let mut h = gpu.handler();
        assert_eq!(run(&mut h, RESOURCE_CREATE_2D, 0, &[5, 2, 64, 32]), OK_NODATA);
        assert!(h.resources.contains_key(&5));
    }

    #[test]
    fn gl_state_survives_a_snapshot() {
        let log = Arc::new(Mutex::new(vec![]));
        let (gpu, _ctl) = Gpu::new(64, 64, Arc::new(Rec::default()));
        let gpu = gpu.with_3d(ThreeD::new(Box::new(FakeGl(log.clone())), Arc::new(NoMap), 0));
        let mut h = gpu.handler();
        let mut name = [0u32; 18];
        (name[0], name[1], name[2]) = (3, CAPSET_VIRGL2, u32::from_le_bytes(*b"app\0"));
        assert_eq!(run(&mut h, CTX_CREATE, 1, &name), OK_NODATA);
        // An 8x4 B8G8R8X8 texture with one more mip level.
        assert_eq!(run(&mut h, RESOURCE_CREATE_3D, 0, &[9, 2, 2, 2, 8, 4, 1, 1, 1, 0, 0, 0]), OK_NODATA);
        assert_eq!(run(&mut h, CTX_ATTACH_RESOURCE, 1, &[9, 0]), OK_NODATA);
        // Create blend state 5 and bind it, draw.
        let cmds = [1 | 1 << 8 | 4 << 16, 5, 0, 0, 0, 2 | 1 << 8 | 1 << 16, 5, 8 | 12 << 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut body = vec![(cmds.len() * 4) as u32, 0];
        body.extend(cmds);
        assert_eq!(run(&mut h, SUBMIT_3D, 1, &body), OK_NODATA);
        assert!(h.snapshot_blocker().is_none());
        let data = h.save_3d().unwrap();

        log.lock().unwrap().clear();
        let (gpu, _ctl) = Gpu::new(64, 64, Arc::new(Rec::default()));
        let gpu = gpu.with_3d(ThreeD::new(Box::new(FakeGl(log.clone())), Arc::new(NoMap), 0));
        let mut h = gpu.handler();
        let mem = GuestMemory::new(&[(GuestAddress(0), 1 << 20)]).unwrap();
        assert!(h.restore_3d(&mem, &data).unwrap().is_empty());
        let log = log.lock().unwrap().clone();
        assert_eq!(log[0], "create 9 8x4 fmt 2 target 2");
        assert_eq!(log[1], "write 9 level 0 8x4 stride 32: 128 bytes of 9");
        assert_eq!(log[2], "write 9 level 1 4x2 stride 16: 32 bytes of 10");
        assert_eq!(log[3], "attach 1 9");
        // Sub-context 0, the blend state and its binding; no draw.
        assert_eq!(log[4], "submit 1 [1001c, 0, 40101, 5, 0, 0, 0, 10102, 5, 1001c, 0]");
        assert_eq!(h.contexts, [1]);
        // And it goes on following what the context does.
        assert!(h.ctx_info[&1].gl.as_ref().is_some_and(|g| g.broken().is_none()));
    }

    #[test]
    fn direct3d_blocks_snapshots_vulkan_doesnt() {
        let log = Arc::new(Mutex::new(vec![]));
        let (gpu, _ctl) = Gpu::new(64, 64, Arc::new(Rec::default()));
        let gpu = gpu.with_3d(ThreeD::new(Box::new(FakeGl(log)), Arc::new(NoMap), 0));
        let mut h = gpu.handler();
        h.ctx_info.insert(2, CtxInfo { capset: CAPSET_VENUS, name: vec![], attached: Default::default(), gl: None });
        // A Vulkan context's blob, shown by a GL context (a window).
        let blob = Blob { resource: 9, ctx: 2, mem: BLOB_MEM_HOST3D, flags: 1, id: 5, size: 4096 };
        h.res3d.insert(9, Res3d { blob: true, made: Some(blob), ..Default::default() });
        let mut attached = std::collections::BTreeSet::new();
        attached.insert(9);
        h.ctx_info.insert(1, CtxInfo { capset: 2, name: vec![], attached, gl: Some(GlState::default()) });
        assert_eq!(h.snapshot_blocker(), None);
        // Its context gone, nothing can make it again.
        h.ctx_info.remove(&2);
        assert!(h.snapshot_blocker().unwrap().contains("blob"));
        h.ctx_info.insert(3, CtxInfo { capset: CAPSET_NEPTUNE, name: vec![], attached: Default::default(), gl: None });
        assert!(h.snapshot_blocker().unwrap().contains("Direct3D"));
    }

    /// A Vulkan renderer that remembers what it was asked.
    struct FakeVk {
        log: Arc<Mutex<Vec<String>>>,
        restores: bool,
    }
    impl Renderer3d for FakeVk {
        fn capsets(&self) -> Vec<(u32, u32, u32)> {
            vec![(CAPSET_VENUS, 0, 160)]
        }
        fn capset(&mut self, _: u32, _: u32) -> Option<Vec<u8>> {
            None
        }
        fn context_create(&mut self, ctx: u32, capset: u32, _: &[u8]) -> bool {
            self.log.lock().unwrap().push(format!("ctx {ctx} capset {capset}"));
            true
        }
        fn context_destroy(&mut self, _: u32) {}
        fn context_attach(&mut self, ctx: u32, res: u32) {
            self.log.lock().unwrap().push(format!("attach {ctx} {res}"));
        }
        fn context_detach(&mut self, _: u32, _: u32) {}
        fn submit(&mut self, _: u32, _: &mut [u8]) -> bool {
            true
        }
        fn create_3d(&mut self, _: u32, _: &Create3d) -> bool {
            false
        }
        fn attach_backing(&mut self, _: u32, _: crate::devices::gpu3d::HostIov) -> bool {
            true
        }
        fn detach_backing(&mut self, _: u32) {}
        fn transfer(&mut self, _: bool, _: u32, _: &Transfer3d) -> bool {
            true
        }
        fn create_blob(&mut self, b: &Blob, _: crate::devices::gpu3d::HostIov) -> bool {
            self.log.lock().unwrap().push(format!("blob {} ctx {} id {} flags {} size {}", b.resource, b.ctx, b.id, b.flags, b.size));
            true
        }
        fn map_blob(&mut self, _: u32) -> Option<crate::devices::gpu3d::Mapping> {
            None
        }
        fn unmap_blob(&mut self, _: u32) {}
        fn read_mapped(&mut self, _: u32, _: u64, _: &mut [u8]) -> bool {
            false
        }
        fn read_back(&mut self, _: u32, _: Box3d, _: u32, _: &mut [u8]) -> bool {
            false
        }
        fn venus_save(&mut self, ctx: u32) -> Option<(bool, Vec<u8>)> {
            Some((true, format!("state of {ctx}").into_bytes()))
        }
        fn venus_restore(&mut self, ctx: u32, data: &[u8]) -> bool {
            self.log.lock().unwrap().push(format!("restore {ctx}: {}", String::from_utf8_lossy(data)));
            self.restores
        }
        fn venus_resume(&mut self, ctx: u32) -> bool {
            self.log.lock().unwrap().push(format!("resume {ctx}"));
            true
        }
        fn unref(&mut self, _: u32) {}
        fn create_fence(&mut self, _: u32, _: Option<u8>, _: u64) -> bool {
            true
        }
        fn poll_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }
        fn poll(&mut self) -> Vec<crate::devices::gpu3d::FenceDone> {
            vec![]
        }
        fn reset(&mut self) {}
    }

    /// A Vulkan context with a ring (shared memory, blob id 0) and a
    /// memory blob (blob id 9), saved, then built again in a fresh handler:
    /// its shared memory first, then its state, then its memory, then it
    /// starts. A context the renderer can't build again gets shared memory
    /// where its memory was.
    #[test]
    fn vulkan_snapshot_order() {
        for restores in [true, false] {
            let mem = GuestMemory::new(&[(GuestAddress(0), 1 << 20)]).unwrap();
            let log = Arc::new(Mutex::new(vec![]));
            let vk = || ThreeD::new(Box::new(FakeVk { log: log.clone(), restores }), Arc::new(NoMap), 0);
            let (gpu, _ctl) = Gpu::new(64, 64, Arc::new(Rec::default()));
            let gpu = gpu.with_3d(vk());
            let mut h = gpu.handler();
            let mut create = vec![0u32, CAPSET_VENUS];
            create.extend([0u32; 16]);
            assert_eq!(run(&mut h, CTX_CREATE, 2, &create), OK_NODATA);
            // id, mem, flags, entries, blob id (2 words), size (2 words)
            assert_eq!(run(&mut h, RESOURCE_CREATE_BLOB, 2, &[3, BLOB_MEM_HOST3D, 1, 0, 0, 0, 65536, 0]), OK_NODATA);
            assert_eq!(run(&mut h, RESOURCE_CREATE_BLOB, 2, &[4, BLOB_MEM_HOST3D, 1, 0, 9, 0, 4096, 0]), OK_NODATA);
            assert_eq!(h.snapshot_blocker(), None);
            let data = h.save_3d().unwrap();
            log.lock().unwrap().clear();

            let (gpu, _ctl) = Gpu::new(64, 64, Arc::new(Rec::default()));
            let gpu = gpu.with_3d(vk());
            let mut h = gpu.handler();
            h.restore_3d(&mem, &data).unwrap();
            let got = log.lock().unwrap().clone();
            let mut want = vec![
                "ctx 2 capset 4".to_string(),
                "blob 3 ctx 2 id 0 flags 1 size 65536".into(),
                "restore 2: state of 2".into(),
            ];
            if restores {
                want.push("blob 4 ctx 2 id 9 flags 1 size 4096".into());
                want.push("resume 2".into());
            } else {
                want.push("blob 4 ctx 2 id 0 flags 1 size 4096".into());
            }
            assert_eq!(got, want);
            assert!(h.res3d.contains_key(&4) && h.ctx_info[&2].capset == CAPSET_VENUS);
        }
    }

    #[test]
    fn only_blobs_in_use_block_snapshots() {
        let log = Arc::new(Mutex::new(vec![]));
        let (gpu, _ctl) = Gpu::new(64, 64, Arc::new(Rec::default()));
        let gpu = gpu.with_3d(ThreeD::new(Box::new(FakeGl(log)), Arc::new(NoMap), 0));
        let mut h = gpu.handler();
        // Left behind by a driver that stopped: no context, not ours to keep.
        h.res3d.insert(9, Res3d { blob: true, ..Default::default() });
        h.scanouts[0] = Some(Scanout::Blob { id: 9, view: Rect { x: 0, y: 0, w: 8, h: 8 }, format: 1, stride: 32, offset: 0 });
        assert_eq!(h.snapshot_blocker(), None);
        // A context still uses it.
        let mut attached = std::collections::BTreeSet::new();
        attached.insert(9);
        h.ctx_info.insert(2, CtxInfo { capset: 2, name: vec![], attached, gl: Some(GlState::default()) });
        assert!(h.snapshot_blocker().unwrap().contains("blob"));
        h.ctx_info.clear();
        // In guest memory: a guest's own scanout.
        h.res3d.insert(10, Res3d { blob: true, guest: vec![(0x1000, 4096)], ..Default::default() });
        assert!(h.snapshot_blocker().unwrap().contains("blob"));
    }

    #[test]
    fn level_layouts() {
        let tex = Create3d { target: 2, format: 2, width: 10, height: 6, depth: 1, array_size: 1, last_level: 3, ..Default::default() };
        let l = level_layout(&tex, Some(40), 2).unwrap();
        assert_eq!((l.bx.w, l.bx.h, l.bx.d, l.stride, l.bytes), (2, 1, 1, 8, 8));
        assert!(level_layout(&tex, Some(40), 4).is_none());
        assert!(level_layout(&tex, Some(41), 0).is_none(), "row not a whole number of pixels");
        // DXT5 (4x4 blocks of 16 bytes), a 2D array of 3.
        let dxt = Create3d { target: 7, format: 108, width: 10, height: 6, depth: 1, array_size: 3, ..Default::default() };
        let l = level_layout(&dxt, None, 0).unwrap();
        assert_eq!((l.stride, l.layer_stride, l.bytes, l.bx.d), (48, 96, 288, 3));
        let buf = Create3d { target: 0, width: 1000, ..Default::default() };
        assert_eq!(level_layout(&buf, None, 0).unwrap().bytes, 1000);
        let vol = Create3d { target: 3, format: 2, width: 4, height: 4, depth: 8, array_size: 1, last_level: 1, ..Default::default() };
        assert_eq!(level_layout(&vol, Some(16), 1).unwrap().bx.d, 4);
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
        cursors: Mutex<Vec<Cursor>>,
    }
    impl DisplaySink for Rec {
        fn set_mode(&self, w: u32, h: u32) {
            self.modes.lock().unwrap().push((w, h));
        }
        fn flush(&self, _x: u32, _y: u32, w: u32, h: u32, rows: &[u8], stride: usize) {
            let px: Vec<u8> = (0..h as usize).flat_map(|i| rows[i * stride..i * stride + w as usize * 4].to_vec()).collect();
            self.frames.lock().unwrap().push((w, h, px));
        }
        fn cursor(&self, c: &Cursor) {
            self.cursors.lock().unwrap().push(c.clone());
        }
    }

    #[test]
    fn cursor_pictures_reach_the_sink() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let sink = Arc::new(Rec::default());
        let (gpu, _ctl) = Gpu::new(64, 64, sink.clone());
        let mut h = gpu.handler();
        // A 2x1 B8G8R8X8 dumb buffer (what Linux makes) with alpha in X.
        assert_eq!(h.create_cpu(5, 2, 2, 1), OK_NODATA);
        mem.write(GuestAddress(0x1000), &[1, 2, 3, 0xff, 4, 5, 6, 0]).unwrap();
        h.resources.get_mut(&5).unwrap().backing = vec![(0x1000, 8)];
        assert_eq!(h.transfer(&mem, 5, Rect { x: 0, y: 0, w: 2, h: 1 }, 0, None), OK_NODATA);
        let upd = |h: &mut GpuHandler, id: u32, hx: u32| {
            let b: Vec<u8> = [0u32, 10, 20, 0, id, hx, 7].iter().flat_map(|v| v.to_le_bytes()).collect();
            let mut ring = crate::queue::tests::TestRing::new(4);
            ring.add_chain(&[(b.len() as u32, false)]);
            ring.mem.write(GuestAddress(crate::queue::tests::DATA), &b).unwrap();
            let m = ring.mem.clone();
            let (mut r, _) = ring.q.clone().pop(&m).unwrap().unwrap().into_rw().unwrap();
            h.update_cursor(&mem, &mut r);
        };
        upd(&mut h, 5, 1);
        upd(&mut h, 5, 1); // same picture: not sent again
        upd(&mut h, 0, 0);
        upd(&mut h, 99, 0); // no such resource: ignored
        let c = sink.cursors.lock().unwrap().clone();
        // RGBA; the hot spot is kept inside the picture.
        let img = Cursor::Image { w: 2, h: 1, hot_x: 1, hot_y: 0, rgba: vec![3, 2, 1, 0xff, 6, 5, 4, 0] };
        assert_eq!(c, vec![img, Cursor::Hidden]);
        // It's in the snapshot and comes back with the resources.
        assert_eq!(h.save_2d().cursor, Some((0, 0, 7)));
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
        h.scanouts[0] = Some(Scanout::Cpu { id: 7, view: full });
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

    /// Two monitors (X11's way: one resource, a part of it each).
    #[test]
    fn a_resource_on_two_scanouts() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let (a, b) = (Arc::new(Rec::default()), Arc::new(Rec::default()));
        let (mut gpu, ctl) = Gpu::with_heads(&[(4, 2), (2, 2)], vec![a.clone(), b.clone()]);
        assert_eq!(ctl.heads(), 2);
        let mut c = [0u8; 4];
        gpu.read_config(8, &mut c);
        assert_eq!(u32::from_le_bytes(c), 2, "num_scanouts");
        let mut h = gpu.handler();
        // Both plugged in at their sizes.
        assert_eq!(display_info_at(&mut h, 1), (2, 2, 1));
        // A 6x2 framebuffer: scanout 0 shows x 0..4, scanout 1 x 4..6.
        assert_eq!(h.create_cpu(3, 2, 6, 2), OK_NODATA);
        let px: Vec<u8> = (0..48).collect();
        mem.write(GuestAddress(0x1000), &px).unwrap();
        h.resources.get_mut(&3).unwrap().backing = vec![(0x1000, 48)];
        assert_eq!(h.transfer(&mem, 3, Rect { x: 0, y: 0, w: 6, h: 2 }, 0, None), OK_NODATA);
        assert_eq!(run(&mut h, SET_SCANOUT, 0, &[0, 0, 4, 2, 0, 3]), OK_NODATA);
        assert_eq!(run(&mut h, SET_SCANOUT, 0, &[4, 0, 2, 2, 1, 3]), OK_NODATA);
        assert_eq!(run(&mut h, SET_SCANOUT, 0, &[0, 0, 2, 2, 2, 3]), ERR_INVALID_SCANOUT_ID);
        assert_eq!((a.modes.lock().unwrap().clone(), b.modes.lock().unwrap().clone()), (vec![(4, 2)], vec![(2, 2)]));
        // A flush across both: each gets its part, at its own origin.
        assert_eq!(h.flush_rect(&mem, Rect { x: 3, y: 1, w: 2, h: 1 }, 3), OK_NODATA);
        let fa = a.frames.lock().unwrap().last().cloned().unwrap();
        let fb = b.frames.lock().unwrap().last().cloned().unwrap();
        assert_eq!((fa.0, fa.1, &fa.2[..3]), (1, 1, &px[36..39]), "pixel (3,1)");
        assert_eq!((fb.0, fb.1, &fb.2[..3]), (1, 1, &px[40..43]), "pixel (4,1)");
        // Both come back from a snapshot.
        let snap: Snap2d = serde_json::from_slice(&serde_json::to_vec(&h.save_2d()).unwrap()).unwrap();
        assert_eq!(snap.more, vec![(1, 3, 4, 0, 2, 2)]);
        let mut h2 = gpu.handler();
        h2.restore_2d(&mem, &snap).unwrap();
        assert!(h2.scanouts[1].is_some_and(|s| s.view() == Rect { x: 4, y: 0, w: 2, h: 2 }));
        let mut bad = serde_json::from_slice::<Snap2d>(&serde_json::to_vec(&snap).unwrap()).unwrap();
        bad.more[0].0 = 5;
        assert!(gpu.handler().restore_2d(&mem, &bad).is_err(), "no such scanout");
        // Unplugging the second monitor is a display event.
        gpu.write_config(4, &EVENT_DISPLAY.to_le_bytes());
        ctl.resize_head(1, 0, 0);
        assert_eq!(driver_reads(&mut gpu, &mut h).0, 4);
        assert_eq!(display_info_at(&mut h, 1), (0, 0, 0));
        ctl.resize_head(1, 800, 600);
        assert_eq!(display_info_at(&mut h, 1), (800, 600, 1));
        ctl.resize_head(7, 800, 600); // no such head: ignored
        ctl.resize_head(1, 1 << 20, 600);
        assert_eq!(display_info_at(&mut h, 1), (800, 600, 1));
    }

    /// GET_DISPLAY_INFO: (width, height, enabled) of scanout 0.
    fn display_info(h: &mut GpuHandler) -> (u32, u32, u32) {
        display_info_at(h, 0)
    }

    fn display_info_at(h: &mut GpuHandler, sid: usize) -> (u32, u32, u32) {
        use crate::queue::tests::*;
        let mut ring = TestRing::new(4);
        ring.add_chain(&[(4, false)]);
        let mem = ring.mem.clone();
        let (mut r, _) = ring.q.clone().pop(&mem).unwrap().unwrap().into_rw().unwrap();
        let hdr = Hdr { ty: GET_DISPLAY_INFO, flags: 0, fence: 0, ctx: 0, ring: 0 };
        let (ty, b) = h.command(&mem, &mut r, &hdr);
        assert_eq!(ty, OK_DISPLAY_INFO);
        let u = |i: usize| u32::from_le_bytes(b[sid * 24 + i..sid * 24 + i + 4].try_into().unwrap());
        (u(8), u(12), u(16))
    }

    fn events(gpu: &Gpu) -> u32 {
        let mut c = [0u8; 4];
        gpu.read_config(0, &mut c);
        u32::from_le_bytes(c)
    }

    /// What the guest's driver does on a display event: read, clear, read.
    fn driver_reads(gpu: &mut Gpu, h: &mut GpuHandler) -> (u32, u32, u32) {
        assert_eq!(events(gpu), EVENT_DISPLAY);
        display_info(h);
        gpu.write_config(4, &EVENT_DISPLAY.to_le_bytes());
        display_info(h)
    }

    #[test]
    fn resize_is_a_display_event() {
        let (mut gpu, ctl) = Gpu::new(1280, 800, Arc::new(Rec::default()));
        let mut h = gpu.handler();
        assert_eq!(display_info(&mut h), (1280, 800, 1));
        ctl.resize(1440, 900);
        assert_eq!(driver_reads(&mut gpu, &mut h), (1440, 900, 1));
        assert_eq!(events(&gpu), 0);
        assert_eq!(ctl.replugs_done(), Some(0));
    }

    #[test]
    fn windows_3d_driver_gets_its_monitor_replugged() {
        let (mut gpu, ctl) = Gpu::new(1280, 800, Arc::new(Rec::default()));
        let mut h = gpu.handler();
        gpu.shared.lock().unwrap().replug = true;
        ctl.resize(1440, 900);
        assert_eq!(ctl.replugs_done(), None);
        // Read, clear, read: the second read finds it unplugged and plugs
        // it back in, with another event.
        assert_eq!(driver_reads(&mut gpu, &mut h), (0, 0, 0));
        assert_eq!(driver_reads(&mut gpu, &mut h), (1440, 900, 1));
        // A resize while Windows picks its mode waits for it.
        ctl.resize(1600, 1000);
        assert_eq!(events(&gpu), 0);
        assert_eq!(ctl.replugs_done(), None);
        h.set_mode(0, 1440, 900);
        assert_eq!(ctl.replugs_done(), Some(1));
        ctl.tick();
        // Mid-replug: it comes back with the latest size.
        ctl.resize(1700, 1050);
        assert_eq!(driver_reads(&mut gpu, &mut h), (0, 0, 0));
        assert_eq!(driver_reads(&mut gpu, &mut h), (1700, 1050, 1));
        // Windows let it pass: after a while it's tried again, a few times.
        let ago = Instant::now() - REPLUG_CHECK - Duration::from_millis(1);
        for _ in 1..REPLUG_TRIES {
            gpu.shared.lock().unwrap().plug = Plug::Back { since: ago, next: None };
            assert_eq!(ctl.replugs_done(), Some(gpu.shared.lock().unwrap().replugs));
            ctl.tick();
            assert_eq!(driver_reads(&mut gpu, &mut h), (0, 0, 0));
            assert_eq!(driver_reads(&mut gpu, &mut h), (1700, 1050, 1));
        }
        gpu.shared.lock().unwrap().plug = Plug::Back { since: ago, next: None };
        ctl.tick();
        assert_eq!(events(&gpu), 0, "gave up");
        assert_eq!(gpu.shared.lock().unwrap().plug, Plug::In);
        // A guest that never reads gets its monitor back.
        ctl.resize(800, 600);
        gpu.shared.lock().unwrap().plug = Plug::Out { size: (800, 600), seen: false, since: Instant::now() - REPLUG_TIMEOUT - Duration::from_millis(1) };
        ctl.tick();
        assert_eq!(display_info(&mut h), (800, 600, 1));
        // A new driver (reset) starts plugged in, without replugs.
        ctl.resize(1024, 768);
        gpu.reset();
        assert_eq!(display_info(&mut h), (1024, 768, 1));
        assert!(!gpu.shared.lock().unwrap().replug);
    }
}
