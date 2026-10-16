//! Host 3D for virtio-gpu: virglrenderer, loaded with dlopen.
//!
//! Two protocols: Venus (Vulkan), where the guest's Vulkan driver talks to
//! the host's (MoltenVK on macOS) through serialized command streams, and
//! virgl (OpenGL), where Mesa's virgl driver sends Gallium commands that
//! virglrenderer replays on OpenGL ES (ANGLE on Metal, see [`egl`]). The
//! guest's Mesa picks virgl for GL and Venus for Vulkan.
//!
//! virglrenderer is native code that parses guest-controlled command
//! streams, so it adds attack surface to the VMM process. It only loads
//! when the VM asks for `gpu = "virtio3d"`; [`load`] must run before the
//! sandbox goes up (it needs to open the libraries and the GPU driver).
//!
//! The library is global state (one renderer per process), which matches
//! one GPU per VMM process.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use vroom_virtio::devices::gpu3d::{Blob, Box3d, Create3d, FenceDone, HostIov, Mapping, Renderer3d, Transfer3d};

#[cfg(target_os = "macos")]
mod gl;
#[cfg(unix)]
pub mod remote;

#[cfg(target_os = "macos")]
pub use gl::Host as GlHost;

const CAPSET_VIRGL: u32 = 1;
const CAPSET_VIRGL2: u32 = 2;
const CAPSET_VENUS: u32 = 4;
/// Direct3D 11 (Neptune: Windows guests with the viogpu3d driver).
const CAPSET_NEPTUNE: u32 = 7;
const KNOWN_CAPSETS: [u32; 4] = [CAPSET_VIRGL, CAPSET_VIRGL2, CAPSET_VENUS, CAPSET_NEPTUNE];
/// Where `char renderer[64]` sits in virgl caps v2 (virgl_hw.h; the
/// layout is guest ABI, it only grows).
const CAPS_V2_RENDERER: std::ops::Range<usize> = 696..760;

// virglrenderer.h flags.
const THREAD_SYNC: c_int = 2;
const VENUS: c_int = 1 << 6;
const NO_VIRGL: c_int = 1 << 7;
const ASYNC_FENCE_CB: c_int = 1 << 8;
const RENDER_SERVER: c_int = 1 << 9;
const NEPTUNE: c_int = 1 << 15;

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
    create_gl_context: Option<extern "C" fn(*mut c_void, c_int, *mut GlCtxParam) -> *mut c_void>,
    destroy_gl_context: Option<extern "C" fn(*mut c_void, *mut c_void)>,
    make_current: Option<extern "C" fn(*mut c_void, c_int, *mut c_void) -> c_int>,
    get_drm_fd: Option<extern "C" fn(*mut c_void) -> c_int>,
    write_context_fence: Option<extern "C" fn(*mut c_void, u32, u32, u64)>,
    get_server_fd: Option<extern "C" fn(*mut c_void, u32) -> c_int>,
}

#[cfg(target_os = "macos")]
use gl::GlCtxParam;
#[cfg(not(target_os = "macos"))]
enum GlCtxParam {}

/// Resolved entry points.
struct Lib {
    /// The directory it was loaded from (ANGLE must sit there too).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    dir: Option<PathBuf>,
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
    get_info: unsafe extern "C" fn(c_int, *mut c_void) -> c_int,
    /// macOS fork (krunkit): blobs come with a host pointer already.
    get_map_ptr: Option<unsafe extern "C" fn(u32, *mut u64) -> c_int>,
    /// virgl_renderer_resource_export_blob: an fd for a blob (the render
    /// server's are shared memory).
    export_blob: Option<unsafe extern "C" fn(u32, *mut u32, *mut c_int) -> c_int>,
}

// SAFETY: plain function pointers into a library that stays loaded.
unsafe impl Send for Lib {}
// SAFETY: see above.
unsafe impl Sync for Lib {}

static LIB: OnceLock<Option<Lib>> = OnceLock::new();

fn candidates() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("VROOM_VIRGLRENDERER") {
        v.push(p);
    }
    // Our patched build (scripts/build-virglrenderer.sh) ships next to the
    // binary; whoever can write there can replace the binary anyway.
    let lib = if cfg!(target_os = "macos") { "libvirglrenderer.1.dylib" } else { "libvirglrenderer.so.1" };
    if let Some(dir) = std::env::current_exe().and_then(std::fs::canonicalize).ok().and_then(|e| e.parent().map(|d| d.to_path_buf())) {
        v.push(dir.join(lib).to_string_lossy().into_owned());
        // In vroom.app the libraries are in Contents/Frameworks, next to
        // Contents/MacOS: still our own bundle, not a search path.
        if let Some(fw) = bundle_frameworks(&dir) {
            v.push(fw.join(lib).to_string_lossy().into_owned());
        }
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

/// `X.app/Contents/Frameworks` for a binary in `X.app/Contents/MacOS`.
fn bundle_frameworks(exe_dir: &std::path::Path) -> Option<PathBuf> {
    let contents = exe_dir.parent()?;
    let app = contents.parent()?;
    let is_bundle = exe_dir.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension().is_some_and(|e| e == "app");
    is_bundle.then(|| contents.join("Frameworks"))
}

fn open_lib() -> Option<Lib> {
    let (handle, path) = candidates().into_iter().find_map(|p| {
        let c = CString::new(p.clone()).ok()?;
        // SAFETY: dlopen with a valid C string.
        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if h.is_null() {
            None
        } else {
            log::info!("gpu3d: loaded {p}");
            Some((h, p))
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
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                std::mem::transmute::<*mut c_void, _>(f)
            }
        }};
    }
    // Its messages into ours (its default writes to stderr, and its
    // errors got lost there). Older libraries lack this: then they don't.
    let c = CString::new("virgl_set_log_callback").ok()?;
    // SAFETY: valid handle and name.
    let set_log = unsafe { libc::dlsym(handle, c.as_ptr()) };
    if !set_log.is_null() {
        type SetLog = unsafe extern "C" fn(extern "C" fn(c_int, *const c_char, *mut c_void), *mut c_void, *mut c_void);
        // SAFETY: virglrenderer.h's virgl_set_log_callback; our callback
        // only reads the message, and needs no user data or free hook.
        unsafe { std::mem::transmute::<*mut c_void, SetLog>(set_log)(on_log, std::ptr::null_mut(), std::ptr::null_mut()) };
    }
    // Bare names were found through the search path: ask dyld where.
    let dir = std::path::Path::new(&path).is_absolute().then(|| PathBuf::from(&path)).and_then(|p| p.parent().map(PathBuf::from));
    Some(Lib {
        dir,
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
        get_info: sym!("virgl_renderer_resource_get_info"),
        get_map_ptr: {
            let c = CString::new("virgl_renderer_resource_get_map_ptr").ok()?;
            // SAFETY: valid handle and name.
            let f = unsafe { libc::dlsym(handle, c.as_ptr()) };
            // SAFETY: signature from the fork's virglrenderer.h.
            (!f.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32, *mut u64) -> c_int>(f) })
        },
        export_blob: {
            let c = CString::new("virgl_renderer_resource_export_blob").ok()?;
            // SAFETY: valid handle and name.
            let f = unsafe { libc::dlsym(handle, c.as_ptr()) };
            // SAFETY: signature from virglrenderer.h.
            (!f.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn(u32, *mut u32, *mut c_int) -> c_int>(f) })
        },
    })
}

/// Load the library, and with `gl` that host's GL (ANGLE from next to
/// it, or Apple's). Call before the sandbox, once per process. Returns
/// whether 3D is usable (GL or not).
#[cfg(target_os = "macos")]
pub fn load(gl: Option<GlHost>) -> bool {
    let Some(lib) = LIB.get_or_init(open_lib) else { return false };
    if let Some(host) = gl {
        GL.get_or_init(|| load_gl(lib, host));
    }
    if let Some(dir) = &lib.dir {
        preload(dir, "libMoltenVK.dylib", "Vulkan");
        preload(dir, "libdxmt-native.dylib", "Direct3D 11 for Windows guests");
    }
    true
}

/// The render server (Venus, and Direct3D 11 for Windows guests) loads
/// MoltenVK and DXMT from the library's own directory when a guest first
/// asks. That is after the sandbox is up, so load them now: the later
/// dlopen finds them already there and opens nothing.
#[cfg(target_os = "macos")]
fn preload(dir: &std::path::Path, name: &str, what: &str) {
    let path = dir.join(name);
    if !path.exists() {
        return;
    }
    let Ok(c) = CString::new(path.as_os_str().as_encoded_bytes()) else { return };
    // SAFETY: dlopen with a valid C string; the handle is kept for good.
    let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        log::warn!("gpu3d: can't load {}: no {what}", path.display());
    } else {
        log::info!("gpu3d: loaded {} ({what})", path.display());
    }
}

/// Load the library (no GL off macOS). See the macOS one.
#[cfg(not(target_os = "macos"))]
pub fn load(gl: bool) -> bool {
    let Some(_lib) = LIB.get_or_init(open_lib) else { return false };
    if gl {
        log::info!("gpu3d: OpenGL (virgl) is only wired up on macOS for now");
    }
    true
}

/// Whether OpenGL (virgl) came up in [`load`].
static GL: OnceLock<bool> = OnceLock::new();

#[cfg(target_os = "macos")]
fn load_gl(lib: &Lib, host: GlHost) -> bool {
    match (&lib.dir, host) {
        (Some(dir), _) => gl::load(dir, host),
        (None, GlHost::Apple) => gl::load(std::path::Path::new(""), host),
        (None, GlHost::Angle) => {
            log::info!("gpu3d: virglrenderer from the search path: no ANGLE, OpenGL off");
            false
        }
    }
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

/// virglrenderer's log, by its level (debug, info, warning, error).
extern "C" fn on_log(level: c_int, msg: *const c_char, _user: *mut c_void) {
    if msg.is_null() {
        return;
    }
    // SAFETY: a NUL-terminated message, valid for this call.
    let m = unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy();
    let m = m.trim_end();
    match level {
        0 => log::debug!("virgl: {m}"),
        1 => log::info!("virgl: {m}"),
        _ => log::warn!("virgl: {m}"),
    }
}

extern "C" fn on_fence(_cookie: *mut c_void, fence: u32) {
    signal(FenceDone { ctx: 0, ring: None, fence: fence as u64 });
}

extern "C" fn on_context_fence(_cookie: *mut c_void, ctx: u32, ring: u32, fence: u64) {
    signal(FenceDone { ctx, ring: Some(ring as u8), fence });
}

/// The step the VMM maps guest memory in (0: the host page). With 4 KiB
/// pages (`VmParams::small_pages`), buffers are rounded to 4 KiB: a whole
/// host page could reach into the next buffer's place in the guest.
static MAP_GRANULE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// See [`MAP_GRANULE`]. Call before mapping anything.
pub fn set_map_granule(bytes: u64) {
    MAP_GRANULE.store(bytes, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
fn map_granule() -> u64 {
    match MAP_GRANULE.load(std::sync::atomic::Ordering::Relaxed) {
        0 => page_size(),
        g => g,
    }
}

/// Size to map for a host buffer: `size` rounded up to the mapping
/// granule, but only when the tail provably belongs to the same
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
    let ps = map_granule();
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

/// Bind the GL context to the calling thread (see egl.rs). No-op without GL.
fn enter() {
    #[cfg(target_os = "macos")]
    gl::bind();
}

/// The calling thread won't make renderer calls for a while.
fn leave() {
    #[cfg(target_os = "macos")]
    gl::release();
}

/// Set once virglrenderer is up: whether with GL.
static INIT: Mutex<Option<bool>> = Mutex::new(None);

/// virgl_renderer_init, with GL contexts from [`gl`] or without GL, and
/// with or without the render server (`proxy`: Venus and Neptune there).
fn init(lib: &Lib, gl: bool, proxy: bool) -> c_int {
    type Create = extern "C" fn(*mut c_void, c_int, *mut GlCtxParam) -> *mut c_void;
    type Destroy = extern "C" fn(*mut c_void, *mut c_void);
    type Current = extern "C" fn(*mut c_void, c_int, *mut c_void) -> c_int;
    #[cfg(target_os = "macos")]
    let (create, destroy, current): (Option<Create>, Option<Destroy>, Option<Current>) =
        if gl { (Some(gl::create_gl_context), Some(gl::destroy_gl_context), Some(gl::make_current)) } else { (None, None, None) };
    #[cfg(not(target_os = "macos"))]
    let (create, destroy, current): (Option<Create>, Option<Destroy>, Option<Current>) = (None, None, None);
    // virglrenderer keeps the callbacks pointer: make it 'static.
    let cb: &'static mut Callbacks = Box::leak(Box::new(Callbacks {
        version: 3,
        write_fence: Some(on_fence),
        create_gl_context: create,
        destroy_gl_context: destroy,
        make_current: current,
        get_drm_fd: None,
        write_context_fence: Some(on_context_fence),
        get_server_fd: None,
    }));
    // With GL, no fence thread: ANGLE serializes the contexts of a share
    // group, so a thread blocked on a GL fence would stall every submit.
    // GL fences are polled by the GPU worker instead (`poll`).
    // The render server's contexts only get a fence thread with
    // THREAD_SYNC, and with ASYNC_FENCE_CB the poll skips them: with GL
    // and the server, no async callbacks, so the poll retires those too.
    // (Venus in place has fence threads of its own either way.)
    let async_cb = if gl && proxy { 0 } else { ASYNC_FENCE_CB };
    let flags = VENUS | async_cb | if gl { 0 } else { NO_VIRGL | THREAD_SYNC } | if proxy { RENDER_SERVER | NEPTUNE } else { 0 };
    // virgl refuses a null cookie; ours is never used (callbacks reach statics).
    static COOKIE: u8 = 0;
    // SAFETY: valid callbacks for the process lifetime; the cookie is only
    // passed back to us.
    unsafe { (lib.init)(&COOKIE as *const u8 as *mut c_void, flags, cb) }
}

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
    /// Read-only views of blobs the guest doesn't map but shows (Direct3D
    /// frames, in the render server's shared memory): resource -> (ptr, len).
    views: HashMap<u32, (*mut u8, u64)>,
    /// GL renderer name the guest sees instead of the host's.
    gl_renderer: Option<String>,
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
    /// A read-only view of a blob in shared memory, made the first time it
    /// is shown and kept until the resource goes.
    fn view(&mut self, resource: u32) -> Option<(*mut u8, u64)> {
        if let Some(&v) = self.views.get(&resource) {
            return Some(v);
        }
        let (fd, len) = self.export(resource)?;
        // SAFETY: a fresh read-only shared mapping of the whole object.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len as usize, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED {
            return None;
        }
        self.views.insert(resource, (p as *mut u8, len));
        Some((p as *mut u8, len))
    }

    /// A blob's shared memory (render server resources: Direct3D, Venus)
    /// and its size. None for anything else.
    pub fn export(&mut self, resource: u32) -> Option<(OwnedFd, u64)> {
        let export = self.lib.export_blob?;
        let (mut ty, mut fd) = (0u32, -1 as c_int);
        // SAFETY: valid out pointers; a returned fd is ours to close.
        if unsafe { export(resource, &mut ty, &mut fd) } != 0 || fd < 0 {
            return None;
        }
        // SAFETY: a fresh fd, ours from here on.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        const FD_TYPE_SHM: u32 = 3;
        // SAFETY: stat is plain old data; all zeroes is a valid value.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fd is open; st is a valid out buffer.
        if ty != FD_TYPE_SHM || unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 || st.st_size <= 0 {
            return None;
        }
        Some((fd, st.st_size as u64))
    }

    fn drop_view(&mut self, resource: u32) {
        if let Some((p, len)) = self.views.remove(&resource) {
            // SAFETY: a mapping view() made, used by nothing else now.
            unsafe { libc::munmap(p as *mut c_void, len as usize) };
        }
    }

    /// Initialise virglrenderer (once per process): Venus, plus virgl if
    /// [`load`] brought up GL. `gl_renderer` replaces the host's GL
    /// renderer name in what the guest reads.
    pub fn new(gl_renderer: Option<String>) -> Option<Virgl> {
        let lib = lib()?;
        let mut inited = INIT.lock().unwrap_or_else(|e| e.into_inner());
        let gl = match *inited {
            Some(gl) => gl,
            None => {
                let _ = sink();
                enter();
                let mut gl = GL.get().copied().unwrap_or(false);
                // Venus and Neptune through the render server (a thread
                // in this process) where the library has one; older
                // libraries run Venus in place and have no Neptune.
                let mut r = -1;
                for proxy in [true, false] {
                    if gl && r != 0 {
                        r = init(lib, true, proxy);
                    }
                }
                if gl && r != 0 {
                    log::warn!("gpu3d: OpenGL (virgl) init failed ({r}): Venus only");
                    gl = false;
                }
                for proxy in [true, false] {
                    if r != 0 {
                        r = init(lib, false, proxy);
                    }
                }
                if r != 0 {
                    log::warn!("gpu3d: virgl_renderer_init failed: {r}");
                    leave();
                    return None;
                }
                *inited = Some(gl);
                // The GPU worker thread takes over from here.
                leave();
                gl
            }
        };
        let mut capsets = Vec::new();
        // virglrenderer reports the GL sets even with GL off (then empty).
        for id in KNOWN_CAPSETS.into_iter().filter(|&id| gl || id == CAPSET_VENUS || id == CAPSET_NEPTUNE) {
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
        Some(Virgl { lib, capsets, iovs: HashMap::new(), contexts: Vec::new(), maps: HashMap::new(), blob_sizes: HashMap::new(), views: HashMap::new(), gl_renderer })
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
        // Filling the virgl sets queries GL.
        enter();
        // SAFETY: buf is exactly the size the library reported for this set.
        unsafe { (self.lib.fill_caps)(id, version, buf.as_mut_ptr() as *mut c_void) };
        if let (CAPSET_VIRGL2, Some(name), Some(field)) = (id, &self.gl_renderer, buf.get_mut(CAPS_V2_RENDERER)) {
            // NUL-terminated, the rest zeroed.
            field.fill(0);
            let n = name.len().min(field.len() - 1);
            field[..n].copy_from_slice(&name.as_bytes()[..n]);
        }
        Some(buf)
    }

    fn context_create(&mut self, ctx: u32, capset: u32, name: &[u8]) -> bool {
        enter();
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
        enter();
        if let Some(i) = self.contexts.iter().position(|&c| c == ctx) {
            self.contexts.swap_remove(i);
            // SAFETY: plain call with an id.
            unsafe { (self.lib.ctx_destroy)(ctx) };
        }
    }

    fn context_attach(&mut self, ctx: u32, resource: u32) {
        enter();
        // SAFETY: plain call with ids; the library validates them.
        unsafe { (self.lib.ctx_attach)(ctx as c_int, resource as c_int) };
    }

    fn context_detach(&mut self, ctx: u32, resource: u32) {
        enter();
        // SAFETY: as above.
        unsafe { (self.lib.ctx_detach)(ctx as c_int, resource as c_int) };
    }

    fn submit(&mut self, ctx: u32, cmds: &mut [u8]) -> bool {
        enter();
        if cmds.len() % 4 != 0 {
            return false;
        }
        // Nothing to run (Windows sends these to carry a fence). An empty
        // slice's pointer is dangling and unaligned: the library would
        // refuse it.
        if cmds.is_empty() {
            return true;
        }
        // SAFETY: cmds is valid for len bytes = ndw dwords.
        let r = unsafe { (self.lib.submit)(cmds.as_mut_ptr() as *mut c_void, ctx as c_int, (cmds.len() / 4) as c_int) };
        if r != 0 {
            log::debug!("gpu3d: submit of {} bytes to context {ctx}: {r}", cmds.len());
        }
        r == 0
    }

    fn create_3d(&mut self, resource: u32, a: &Create3d) -> bool {
        enter();
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
        enter();
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
        enter();
        if self.iovs.contains_key(&resource) {
            let (mut p, mut n) = (std::ptr::null_mut(), 0);
            // SAFETY: valid out pointers; the array stays ours.
            unsafe { (self.lib.detach_iov)(resource as c_int, &mut p, &mut n) };
            self.iovs.remove(&resource);
        }
    }

    fn transfer(&mut self, to_host: bool, ctx: u32, t: &Transfer3d) -> bool {
        enter();
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
        enter();
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
        enter();
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
        enter();
        if let Some((_, _, true)) = self.maps.remove(&resource) {
            // SAFETY: plain call.
            unsafe { (self.lib.unmap)(resource) };
        }
    }

    fn read_mapped(&mut self, resource: u32, offset: u64, out: &mut [u8]) -> bool {
        enter();
        let (p, size) = match self.maps.get(&resource) {
            Some(&(p, size, _)) => (p, size),
            None => match self.view(resource) {
                Some(v) => v,
                None => return false,
            },
        };
        let Some(end) = offset.checked_add(out.len() as u64) else { return false };
        if end > size {
            return false;
        }
        // SAFETY: offset..end is inside the live mapping (checked above).
        unsafe { std::ptr::copy_nonoverlapping(p.add(offset as usize), out.as_mut_ptr(), out.len()) };
        true
    }

    fn read_back(&mut self, resource: u32, bx: Box3d, stride: u32, out: &mut [u8]) -> bool {
        enter();
        let need = (bx.h as u64).saturating_sub(1) * stride as u64 + bx.w as u64 * 4;
        if bx.w == 0 || bx.h == 0 || (stride as u64) < bx.w as u64 * 4 || need > out.len() as u64 {
            return false;
        }
        let mut b = VirglBox { x: bx.x, y: bx.y, z: 0, w: bx.w, h: bx.h, d: 1 };
        let mut iov = libc::iovec { iov_base: out.as_mut_ptr() as *mut c_void, iov_len: out.len() };
        // SAFETY: one iovec over `out`, which outlives the call; the library
        // checks the box against the resource and the iovec's size.
        unsafe { (self.lib.read_iov)(resource, 0, 0, stride, 0, &mut b, 0, &mut iov, 1) == 0 }
    }

    fn row_bytes(&mut self, resource: u32) -> Option<u32> {
        enter();
        // virgl_renderer_resource_info: ..., tex_id, stride, ...
        let mut info = [0u32; 16];
        // SAFETY: the buffer is larger than the struct the library fills.
        (unsafe { (self.lib.get_info)(resource as c_int, info.as_mut_ptr() as *mut c_void) } == 0).then_some(info[7])
    }

    fn read_level(&mut self, resource: u32, level: u32, bx: Box3d, stride: u32, layer_stride: u32, out: &mut [u8]) -> bool {
        enter();
        let mut b = VirglBox { x: bx.x, y: bx.y, z: bx.z, w: bx.w, h: bx.h, d: bx.d };
        let mut iov = libc::iovec { iov_base: out.as_mut_ptr() as *mut c_void, iov_len: out.len() };
        // SAFETY: one iovec over `out`, which outlives the call; the library
        // checks the box against the resource and the iovec's size.
        unsafe { (self.lib.read_iov)(resource, 0, level, stride, layer_stride, &mut b, 0, &mut iov, 1) == 0 }
    }

    fn write_level(&mut self, resource: u32, level: u32, bx: Box3d, stride: u32, layer_stride: u32, data: &[u8]) -> bool {
        enter();
        let mut b = VirglBox { x: bx.x, y: bx.y, z: bx.z, w: bx.w, h: bx.h, d: bx.d };
        // The library only reads through it.
        let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut c_void, iov_len: data.len() };
        // SAFETY: as in read_level; `data` is only read.
        unsafe { (self.lib.write_iov)(resource, 0, level as c_int, stride, layer_stride, &mut b, 0, &mut iov, 1) == 0 }
    }

    fn scanout_start(&mut self, resource: u32, bx: Box3d) -> bool {
        enter();
        // virgl_renderer_resource_info: handle, virgl_format, width,
        // height, depth, flags, tex_id, ... (room to spare for forks).
        let mut info = [0u32; 16];
        // SAFETY: the buffer is larger than the struct the library fills.
        if unsafe { (self.lib.get_info)(resource as c_int, info.as_mut_ptr() as *mut c_void) } != 0 || info[6] == 0 {
            return false;
        }
        const Y_0_TOP: u32 = 1;
        let flip = (info[5] & Y_0_TOP != 0).then_some(info[3]);
        #[cfg(target_os = "macos")]
        return gl::read_start(info[6], flip, (bx.x, bx.y, bx.w, bx.h));
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (flip, bx);
            false
        }
    }

    fn scanout_poll(&mut self, out: &mut [u8], wait: bool) -> Option<bool> {
        enter();
        #[cfg(target_os = "macos")]
        return gl::read_poll(out, wait);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (out, wait);
            None
        }
    }

    fn unref(&mut self, resource: u32) {
        enter();
        self.unmap_blob(resource);
        self.drop_view(resource);
        self.detach_backing(resource);
        // SAFETY: plain call; unknown ids are ignored by the library.
        unsafe { (self.lib.res_unref)(resource) };
        self.iovs.remove(&resource);
        self.blob_sizes.remove(&resource);
    }

    fn create_fence(&mut self, ctx: u32, ring: Option<u8>, fence: u64) -> bool {
        enter();
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
        enter();
        let s = sink();
        let mut buf = [0u8; 64];
        // SAFETY: drain our non-blocking pipe into a local buffer.
        while unsafe { libc::read(s.wake_rx.as_raw_fd(), buf.as_mut_ptr() as *mut c_void, buf.len()) } > 0 {}
        // SAFETY: plain call; may run fence callbacks on this thread.
        unsafe { (self.lib.poll)() };
        std::mem::take(&mut *s.done.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn reset(&mut self) {
        enter();
        // SAFETY: drops every context and resource inside the library.
        unsafe { (self.lib.reset)() };
        self.iovs.clear();
        self.contexts.clear();
        self.maps.clear();
        self.blob_sizes.clear();
        for id in self.views.keys().copied().collect::<Vec<_>>() {
            self.drop_view(id);
        }
        sink().done.lock().unwrap_or_else(|e| e.into_inner()).clear();
        leave();
    }

    fn release_thread(&mut self) {
        leave();
    }
}

#[cfg(test)]
mod bundle_tests {
    use super::bundle_frameworks;
    use std::path::Path;

    #[test]
    fn frameworks_only_inside_an_app() {
        let fw = bundle_frameworks(Path::new("/Applications/vroom.app/Contents/MacOS"));
        assert_eq!(fw.as_deref(), Some(Path::new("/Applications/vroom.app/Contents/Frameworks")));
        assert_eq!(bundle_frameworks(Path::new("/x/target/release")), None);
        assert_eq!(bundle_frameworks(Path::new("/x/Contents/MacOS")), None);
        assert_eq!(bundle_frameworks(Path::new("/MacOS")), None);
    }
}
