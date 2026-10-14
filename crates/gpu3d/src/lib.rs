//! Host 3D for virtio-gpu: virglrenderer, loaded with dlopen.
//!
//! Only Venus (Vulkan) is enabled: the guest's Vulkan driver talks to the
//! host's (MoltenVK on macOS) through serialized command streams. OpenGL in
//! the guest goes through Zink on top of that.
//!
//! virglrenderer is native code that parses guest-controlled command
//! streams, so it adds attack surface to the VMM process. It only loads
//! when the VM asks for `gpu = "virtio3d"`; [`load`] must run before the
//! sandbox goes up (it needs to open the library and the GPU driver).
//!
//! The library is global state (one renderer per process), which matches
//! one GPU per VMM process.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Mutex, OnceLock};

use bulwark_virtio::devices::gpu3d::{Blob, Create3d, FenceDone, HostIov, Mapping, Renderer3d, Transfer3d};

const CAPSET_VENUS: u32 = 4;
// Venus only: GL (virgl) is off, its capsets would be empty.
const KNOWN_CAPSETS: [u32; 1] = [CAPSET_VENUS];

// virglrenderer.h flags.
const THREAD_SYNC: c_int = 2;
const VENUS: c_int = 1 << 6;
const NO_VIRGL: c_int = 1 << 7;
const ASYNC_FENCE_CB: c_int = 1 << 8;

#[repr(C)]
struct CreateArgs {
    handle: u32,
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    nr_samples: u32,
    flags: u32,
}

#[repr(C)]
struct VirglBox {
    x: u32,
    y: u32,
    z: u32,
    w: u32,
    h: u32,
    d: u32,
}

#[repr(C)]
struct BlobArgs {
    res_handle: u32,
    ctx_id: u32,
    blob_mem: u32,
    blob_flags: u32,
    blob_id: u64,
    size: u64,
    iovecs: *const libc::iovec,
    num_iovs: u32,
}

#[repr(C)]
struct Callbacks {
    version: c_int,
    write_fence: Option<extern "C" fn(*mut c_void, u32)>,
    create_gl_context: Option<extern "C" fn(*mut c_void, c_int, *mut c_void) -> *mut c_void>,
    destroy_gl_context: Option<extern "C" fn(*mut c_void, *mut c_void)>,
    make_current: Option<extern "C" fn(*mut c_void, c_int, *mut c_void) -> c_int>,
    get_drm_fd: Option<extern "C" fn(*mut c_void) -> c_int>,
    write_context_fence: Option<extern "C" fn(*mut c_void, u32, u32, u64)>,
    get_server_fd: Option<extern "C" fn(*mut c_void, u32) -> c_int>,
}

/// Resolved entry points.
struct Lib {
    init: unsafe extern "C" fn(*mut c_void, c_int, *mut Callbacks) -> c_int,
    poll: unsafe extern "C" fn(),
    reset: unsafe extern "C" fn(),
    get_cap_set: unsafe extern "C" fn(u32, *mut u32, *mut u32),
    fill_caps: unsafe extern "C" fn(u32, u32, *mut c_void),
    ctx_create_with_flags: unsafe extern "C" fn(u32, u32, u32, *const c_char) -> c_int,
    ctx_destroy: unsafe extern "C" fn(u32),
    ctx_attach: unsafe extern "C" fn(c_int, c_int),
    ctx_detach: unsafe extern "C" fn(c_int, c_int),
    submit: unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int,
    res_create: unsafe extern "C" fn(*mut CreateArgs, *mut libc::iovec, u32) -> c_int,
    res_unref: unsafe extern "C" fn(u32),
    attach_iov: unsafe extern "C" fn(c_int, *mut libc::iovec, c_int) -> c_int,
    detach_iov: unsafe extern "C" fn(c_int, *mut *mut libc::iovec, *mut c_int),
    read_iov: unsafe extern "C" fn(u32, u32, u32, u32, u32, *mut VirglBox, u64, *mut libc::iovec, c_int) -> c_int,
    write_iov: unsafe extern "C" fn(u32, u32, c_int, u32, u32, *mut VirglBox, u64, *mut libc::iovec, u32) -> c_int,
    create_blob: unsafe extern "C" fn(*const BlobArgs) -> c_int,
    map: unsafe extern "C" fn(u32, *mut *mut c_void, *mut u64) -> c_int,
    unmap: unsafe extern "C" fn(u32) -> c_int,
    get_map_info: unsafe extern "C" fn(u32, *mut u32) -> c_int,
    create_fence: unsafe extern "C" fn(c_int, u32) -> c_int,
    ctx_create_fence: unsafe extern "C" fn(u32, u32, u32, u64) -> c_int,
    /// macOS fork (krunkit): blobs come with a host pointer already.
    get_map_ptr: Option<unsafe extern "C" fn(u32, *mut u64) -> c_int>,
}

// SAFETY: plain function pointers into a library that stays loaded.
unsafe impl Send for Lib {}
// SAFETY: see above.
unsafe impl Sync for Lib {}

static LIB: OnceLock<Option<Lib>> = OnceLock::new();

fn candidates() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("BULWARK_VIRGLRENDERER") {
        v.push(p);
    }
    // Our patched build (scripts/build-virglrenderer.sh) ships next to the
    // binary; whoever can write there can replace the binary anyway.
    let lib = if cfg!(target_os = "macos") { "libvirglrenderer.1.dylib" } else { "libvirglrenderer.so.1" };
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf())) {
        v.push(dir.join(lib).to_string_lossy().into_owned());
    }
    if cfg!(target_os = "macos") {
        v.extend(
            ["/opt/homebrew/lib/libvirglrenderer.1.dylib", "/usr/local/lib/libvirglrenderer.1.dylib", "libvirglrenderer.1.dylib"]
                .map(String::from),
        );
    } else {
        v.push("libvirglrenderer.so.1".into());
    }
    v
}

fn open_lib() -> Option<Lib> {
    let handle = candidates().into_iter().find_map(|p| {
        let c = CString::new(p.clone()).ok()?;
        // SAFETY: dlopen with a valid C string.
        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if h.is_null() {
            None
        } else {
            log::info!("gpu3d: loaded {p}");
            Some(h)
        }
    })?;
    macro_rules! sym {
        ($name:literal) => {{
            let c = CString::new($name).ok()?;
            // SAFETY: valid handle and name.
            let f = unsafe { libc::dlsym(handle, c.as_ptr()) };
            if f.is_null() {
                log::warn!("gpu3d: {} missing", $name);
                return None;
            }
            // SAFETY: the symbol has the C signature declared in `Lib`
            // (virglrenderer.h); the transmute just names it.
            unsafe { std::mem::transmute::<*mut c_void, _>(f) }
        }};
    }
    Some(Lib {
        init: sym!("virgl_renderer_init"),
        poll: sym!("virgl_renderer_poll"),
        reset: sym!("virgl_renderer_reset"),
        get_cap_set: sym!("virgl_renderer_get_cap_set"),
        fill_caps: sym!("virgl_renderer_fill_caps"),
        ctx_create_with_flags: sym!("virgl_renderer_context_create_with_flags"),
        ctx_destroy: sym!("virgl_renderer_context_destroy"),
        ctx_attach: sym!("virgl_renderer_ctx_attach_resource"),
        ctx_detach: sym!("virgl_renderer_ctx_detach_resource"),
        submit: sym!("virgl_renderer_submit_cmd"),
        res_create: sym!("virgl_renderer_resource_create"),
        res_unref: sym!("virgl_renderer_resource_unref"),
        attach_iov: sym!("virgl_renderer_resource_attach_iov"),
        detach_iov: sym!("virgl_renderer_resource_detach_iov"),
        read_iov: sym!("virgl_renderer_transfer_read_iov"),
        write_iov: sym!("virgl_renderer_transfer_write_iov"),
        create_blob: sym!("virgl_renderer_resource_create_blob"),
        map: sym!("virgl_renderer_resource_map"),
        unmap: sym!("virgl_renderer_resource_unmap"),
        get_map_info: sym!("virgl_renderer_resource_get_map_info"),
        create_fence: sym!("virgl_renderer_create_fence"),
        ctx_create_fence: sym!("virgl_renderer_context_create_fence"),
        get_map_ptr: {
            let c = CString::new("virgl_renderer_resource_get_map_ptr").ok()?;
            // SAFETY: valid handle and name.
            let f = unsafe { libc::dlsym(handle, c.as_ptr()) };
            // SAFETY: signature from the fork's virglrenderer.h.
            (!f.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32, *mut u64) -> c_int>(f) })
        },
    })
}

/// Load the library. Call before the sandbox. Returns whether 3D is usable.
pub fn load() -> bool {
    LIB.get_or_init(open_lib).is_some()
}

fn lib() -> Option<&'static Lib> {
    LIB.get().and_then(|l| l.as_ref())
}

/// Where fence callbacks land (any thread), plus a pipe to wake the worker.
struct FenceSink {
    done: Mutex<Vec<FenceDone>>,
    wake_rx: OwnedFd,
    wake_tx: OwnedFd,
}

static SINK: OnceLock<FenceSink> = OnceLock::new();

fn sink() -> &'static FenceSink {
    SINK.get_or_init(|| {
        let mut fds = [0; 2];
        // SAFETY: valid out array.
        let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(r, 0, "pipe: {}", std::io::Error::last_os_error());
        for fd in fds {
            // SAFETY: fds we just created.
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        // SAFETY: fresh fds, owned from here on.
        let (rx, tx) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        FenceSink { done: Mutex::new(Vec::new()), wake_rx: rx, wake_tx: tx }
    })
}

fn signal(f: FenceDone) {
    let s = sink();
    s.done.lock().unwrap_or_else(|e| e.into_inner()).push(f);
    // SAFETY: write one byte to our own non-blocking pipe; a full pipe
    // already means "wake up".
    unsafe { libc::write(s.wake_tx.as_raw_fd(), [1u8].as_ptr() as *const c_void, 1) };
}

extern "C" fn on_fence(_cookie: *mut c_void, fence: u32) {
    signal(FenceDone { ctx: 0, ring: None, fence: fence as u64 });
}

extern "C" fn on_context_fence(_cookie: *mut c_void, ctx: u32, ring: u32, fence: u64) {
    signal(FenceDone { ctx, ring: Some(ring as u8), fence });
}

/// Size to map for a host buffer: `size` rounded up to the host page, but
/// only when the tail of the last page provably belongs to the same
/// page-granular allocation (anonymous mmap, Metal/IOKit buffers). Never
/// for malloc zones: the slack there may be someone else's heap data.
#[cfg(target_os = "macos")]
fn safe_map_size(ptr: *mut u8, size: u64) -> u64 {
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedInfo {
        protection: c_int,
        user_tag: u32,
        pages_resident: u32,
        pages_shared_now_private: u32,
        pages_swapped_out: u32,
        pages_dirtied: u32,
        ref_count: u32,
        shadow_depth: u16,
        external_pager: u8,
        share_mode: u8,
        pages_reusable: u32,
    }
    extern "C" {
        fn mach_vm_region(
            task: libc::mach_port_t,
            address: *mut u64,
            size: *mut u64,
            flavor: c_int,
            info: *mut c_int,
            count: *mut u32,
            object: *mut libc::mach_port_t,
        ) -> c_int;
    }
    const VM_REGION_EXTENDED_INFO: c_int = 13;
    let ps = page_size();
    let rounded = size.next_multiple_of(ps);
    if rounded == size || (ptr as u64) % ps != 0 {
        return size;
    }
    let last = (ptr as u64 + size) & !(ps - 1);
    let (mut addr, mut len) = (last, 0u64);
    let mut info = ExtendedInfo::default();
    let mut count = (std::mem::size_of::<ExtendedInfo>() / 4) as u32;
    let mut obj: libc::mach_port_t = 0;
    // SAFETY: queries our own task; out pointers are valid and sized.
    #[allow(deprecated)]
    let kr = unsafe {
        mach_vm_region(libc::mach_task_self(), &mut addr, &mut len, VM_REGION_EXTENDED_INFO, &mut info as *mut _ as *mut c_int, &mut count, &mut obj)
    };
    let inside = kr == 0 && addr <= last && addr + len >= last + ps;
    // VM_MEMORY_MALLOC .. VM_MEMORY_MALLOC_PROB_GUARD (1..=13).
    let heap = (1..=13).contains(&info.user_tag);
    log::debug!("gpu3d: tail page {last:#x}: region {addr:#x}+{len:#x} tag {} -> {}", info.user_tag, inside && !heap);
    if inside && !heap {
        rounded
    } else {
        size
    }
}

#[cfg(not(target_os = "macos"))]
fn safe_map_size(_ptr: *mut u8, size: u64) -> u64 {
    size
}

#[cfg(target_os = "macos")]
fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
}

static INIT: Mutex<bool> = Mutex::new(false);

/// The renderer. Owns the iovec arrays virglrenderer points into.
pub struct Virgl {
    lib: &'static Lib,
    capsets: Vec<(u32, u32, u32)>,
    iovs: HashMap<u32, Box<[libc::iovec]>>,
    contexts: Vec<u32>,
    /// resource -> (host pointer, size, mapped by us via virgl) while mapped.
    maps: HashMap<u32, (*mut u8, u64, bool)>,
    /// Blob sizes, for pointer-only mappings.
    blob_sizes: HashMap<u32, u64>,
}

// SAFETY: the raw pointers (iovecs into guest RAM) stay valid for the VM's
// life and are only used from the GPU worker thread that owns this.
unsafe impl Send for Virgl {}

impl std::fmt::Debug for Virgl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Virgl").field("capsets", &self.capsets).finish()
    }
}

impl Virgl {
    /// Initialise virglrenderer (once per process) for Venus.
    pub fn new() -> Option<Virgl> {
        let lib = lib()?;
        let mut inited = INIT.lock().unwrap_or_else(|e| e.into_inner());
        if !*inited {
            // virglrenderer keeps the callbacks pointer: make it 'static.
            let cb: &'static mut Callbacks = Box::leak(Box::new(Callbacks {
                version: 3,
                write_fence: Some(on_fence),
                create_gl_context: None,
                destroy_gl_context: None,
                make_current: None,
                get_drm_fd: None,
                write_context_fence: Some(on_context_fence),
                get_server_fd: None,
            }));
            let _ = sink();
            let flags = VENUS | NO_VIRGL | THREAD_SYNC | ASYNC_FENCE_CB;
            // SAFETY: valid callbacks for the process lifetime; cookie unused.
            let r = unsafe { (lib.init)(std::ptr::null_mut(), flags, cb) };
            if r != 0 {
                log::warn!("gpu3d: virgl_renderer_init failed: {r}");
                return None;
            }
            *inited = true;
        }
        let mut capsets = Vec::new();
        for id in KNOWN_CAPSETS {
            let (mut ver, mut size) = (0u32, 0u32);
            // SAFETY: valid out pointers.
            unsafe { (lib.get_cap_set)(id, &mut ver, &mut size) };
            if size > 0 && size <= 1 << 16 {
                capsets.push((id, ver, size));
            }
        }
        log::info!("gpu3d: capsets {capsets:?}");
        if capsets.is_empty() {
            return None;
        }
        Some(Virgl { lib, capsets, iovs: HashMap::new(), contexts: Vec::new(), maps: HashMap::new(), blob_sizes: HashMap::new() })
    }

    fn iov(&mut self, resource: u32, iov: HostIov) -> (*mut libc::iovec, usize) {
        let v: Box<[libc::iovec]> = iov.into_iter().map(|(p, n)| libc::iovec { iov_base: p as *mut c_void, iov_len: n }).collect();
        let e = self.iovs.entry(resource).or_insert(v);
        (e.as_mut_ptr(), e.len())
    }
}

impl Renderer3d for Virgl {
    fn capsets(&self) -> Vec<(u32, u32, u32)> {
        self.capsets.clone()
    }

    fn capset(&mut self, id: u32, version: u32) -> Option<Vec<u8>> {
        let &(_, max_ver, size) = self.capsets.iter().find(|c| c.0 == id)?;
        if version > max_ver {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: buf is exactly the size the library reported for this set.
        unsafe { (self.lib.fill_caps)(id, version, buf.as_mut_ptr() as *mut c_void) };
        Some(buf)
    }

    fn context_create(&mut self, ctx: u32, capset: u32, name: &[u8]) -> bool {
        if !self.capsets.iter().any(|c| c.0 == capset) {
            return false;
        }
        let name: Vec<u8> = name.iter().copied().take_while(|&b| b != 0).take(64).collect();
        // SAFETY: name is a valid buffer of the given length.
        let r = unsafe { (self.lib.ctx_create_with_flags)(ctx, capset, name.len() as u32, name.as_ptr() as *const c_char) };
        if r == 0 {
            self.contexts.push(ctx);
        }
        r == 0
    }

    fn context_destroy(&mut self, ctx: u32) {
        if let Some(i) = self.contexts.iter().position(|&c| c == ctx) {
            self.contexts.swap_remove(i);
            // SAFETY: plain call with an id.
            unsafe { (self.lib.ctx_destroy)(ctx) };
        }
    }

    fn context_attach(&mut self, ctx: u32, resource: u32) {
        // SAFETY: plain call with ids; the library validates them.
        unsafe { (self.lib.ctx_attach)(ctx as c_int, resource as c_int) };
    }

    fn context_detach(&mut self, ctx: u32, resource: u32) {
        // SAFETY: as above.
        unsafe { (self.lib.ctx_detach)(ctx as c_int, resource as c_int) };
    }

    fn submit(&mut self, ctx: u32, cmds: &mut [u8]) -> bool {
        if cmds.len() % 4 != 0 {
            return false;
        }
        // SAFETY: cmds is valid for len bytes = ndw dwords.
        unsafe { (self.lib.submit)(cmds.as_mut_ptr() as *mut c_void, ctx as c_int, (cmds.len() / 4) as c_int) == 0 }
    }

    fn create_3d(&mut self, resource: u32, a: &Create3d) -> bool {
        let mut args = CreateArgs {
            handle: resource,
            target: a.target,
            format: a.format,
            bind: a.bind,
            width: a.width,
            height: a.height,
            depth: a.depth,
            array_size: a.array_size,
            last_level: a.last_level,
            nr_samples: a.nr_samples,
            flags: a.flags,
        };
        // SAFETY: valid args; no backing yet.
        unsafe { (self.lib.res_create)(&mut args, std::ptr::null_mut(), 0) == 0 }
    }

    fn attach_backing(&mut self, resource: u32, iov: HostIov) -> bool {
        if self.iovs.contains_key(&resource) {
            return false;
        }
        let (p, n) = self.iov(resource, iov);
        // SAFETY: p..p+n is our boxed array, kept until detach/unref; the
        // ranges are guest RAM (validated by the caller).
        let ok = unsafe { (self.lib.attach_iov)(resource as c_int, p, n as c_int) } == 0;
        if !ok {
            self.iovs.remove(&resource);
        }
        ok
    }

    fn detach_backing(&mut self, resource: u32) {
        if self.iovs.contains_key(&resource) {
            let (mut p, mut n) = (std::ptr::null_mut(), 0);
            // SAFETY: valid out pointers; the array stays ours.
            unsafe { (self.lib.detach_iov)(resource as c_int, &mut p, &mut n) };
            self.iovs.remove(&resource);
        }
    }

    fn transfer(&mut self, to_host: bool, ctx: u32, t: &Transfer3d) -> bool {
        let mut b = VirglBox { x: t.bx.x, y: t.bx.y, z: t.bx.z, w: t.bx.w, h: t.bx.h, d: t.bx.d };
        // SAFETY: the library bounds-checks box/offset against the resource
        // and its attached backing (our arrays).
        let r = unsafe {
            if to_host {
                (self.lib.write_iov)(t.resource, ctx, t.level as c_int, t.stride, t.layer_stride, &mut b, t.offset, std::ptr::null_mut(), 0)
            } else {
                (self.lib.read_iov)(t.resource, ctx, t.level, t.stride, t.layer_stride, &mut b, t.offset, std::ptr::null_mut(), 0)
            }
        };
        r == 0
    }

    fn create_blob(&mut self, blob: &Blob, iov: HostIov) -> bool {
        if self.iovs.contains_key(&blob.resource) {
            return false;
        }
        let (p, n) = if iov.is_empty() { (std::ptr::null_mut(), 0) } else { self.iov(blob.resource, iov) };
        let args = BlobArgs {
            res_handle: blob.resource,
            ctx_id: blob.ctx,
            blob_mem: blob.mem,
            blob_flags: blob.flags,
            blob_id: blob.id,
            size: blob.size,
            iovecs: p,
            num_iovs: n as u32,
        };
        // SAFETY: valid args; iovecs kept until unref.
        let ok = unsafe { (self.lib.create_blob)(&args) } == 0;
        if ok {
            self.blob_sizes.insert(blob.resource, blob.size);
        } else {
            self.iovs.remove(&blob.resource);
        }
        ok
    }

    fn map_blob(&mut self, resource: u32) -> Option<Mapping> {
        if self.maps.contains_key(&resource) {
            return None;
        }
        let mut ptr = 0u64;
        // SAFETY: valid out pointer.
        let have_ptr = self.lib.get_map_ptr.is_some_and(|f| unsafe { f(resource, &mut ptr) } == 0 && ptr != 0);
        let (p, size, ours) = match (have_ptr, self.blob_sizes.get(&resource)) {
            (true, Some(&size)) => (ptr as *mut u8, size, false),
            _ => {
                let (mut p, mut size) = (std::ptr::null_mut(), 0u64);
                // SAFETY: valid out pointers.
                if unsafe { (self.lib.map)(resource, &mut p, &mut size) } != 0 || p.is_null() {
                    return None;
                }
                (p as *mut u8, size, true)
            }
        };
        let mut info = 0u32;
        // SAFETY: valid out pointer.
        unsafe { (self.lib.get_map_info)(resource, &mut info) };
        self.maps.insert(resource, (p, size, ours));
        Some(Mapping { ptr: p, size: safe_map_size(p, size), map_info: info })
    }

    fn unmap_blob(&mut self, resource: u32) {
        if let Some((_, _, true)) = self.maps.remove(&resource) {
            // SAFETY: plain call.
            unsafe { (self.lib.unmap)(resource) };
        }
    }

    fn read_mapped(&mut self, resource: u32, offset: u64, out: &mut [u8]) -> bool {
        let Some(&(p, size, _)) = self.maps.get(&resource) else { return false };
        let Some(end) = offset.checked_add(out.len() as u64) else { return false };
        if end > size {
            return false;
        }
        // SAFETY: offset..end is inside the live mapping (checked above).
        unsafe { std::ptr::copy_nonoverlapping(p.add(offset as usize), out.as_mut_ptr(), out.len()) };
        true
    }

    fn unref(&mut self, resource: u32) {
        self.unmap_blob(resource);
        self.detach_backing(resource);
        // SAFETY: plain call; unknown ids are ignored by the library.
        unsafe { (self.lib.res_unref)(resource) };
        self.iovs.remove(&resource);
        self.blob_sizes.remove(&resource);
    }

    fn create_fence(&mut self, ctx: u32, ring: Option<u8>, fence: u64) -> bool {
        // SAFETY: plain calls.
        let r = unsafe {
            match ring {
                Some(ring) => (self.lib.ctx_create_fence)(ctx, 0, ring as u32, fence),
                None => (self.lib.create_fence)(fence as u32 as c_int, ctx),
            }
        };
        r == 0
    }

    fn poll_fd(&self) -> Option<RawFd> {
        Some(sink().wake_rx.as_raw_fd())
    }

    fn poll(&mut self) -> Vec<FenceDone> {
        let s = sink();
        let mut buf = [0u8; 64];
        // SAFETY: drain our non-blocking pipe into a local buffer.
        while unsafe { libc::read(s.wake_rx.as_raw_fd(), buf.as_mut_ptr() as *mut c_void, buf.len()) } > 0 {}
        // SAFETY: plain call; may run fence callbacks on this thread.
        unsafe { (self.lib.poll)() };
        std::mem::take(&mut *s.done.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn reset(&mut self) {
        // SAFETY: drops every context and resource inside the library.
        unsafe { (self.lib.reset)() };
        self.iovs.clear();
        self.contexts.clear();
        self.maps.clear();
        self.blob_sizes.clear();
        sink().done.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}
