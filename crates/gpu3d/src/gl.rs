//! GL contexts for virglrenderer's GL side (virgl), from one of two hosts:
//!
//! - ANGLE (`opengl_host = "angle"`): OpenGL ES 3.0 from ANGLE's EGL on
//!   its Metal backend. ANGLE's libEGL must sit next to virglrenderer: the
//!   libepoxy built into it loads libEGL and libGLESv2 from there, and the
//!   two must be the same library instance. ANGLE validates every GL call
//!   it gets (it was made for WebGL), so guest commands that slip past
//!   virglrenderer still meet a second check before the Metal driver. The
//!   guest gets OpenGL 2.1.
//! - Apple (`opengl_host = "apple"`): macOS's own OpenGL 4.1 core profile
//!   through CGL. The guest gets OpenGL 4.1, but calls reach Apple's driver
//!   with virglrenderer's checks only. Our libepoxy uses Apple's OpenGL
//!   when a CGL context is current at the first GL call, which ours is.
//!
//! virglrenderer asks for contexts through callbacks; these create them
//! all in one share group, without surfaces (it only renders to its own
//! framebuffer objects).

use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};

type Ptr = *mut c_void;

const EGL_NONE: i32 = 0x3038;
const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_OPENGL_ES_API: u32 = 0x30A0;
const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
const EGL_CONTEXT_MINOR_VERSION: i32 = 0x30FB;
const EGL_CONTEXT_OPENGL_ROBUST_ACCESS: i32 = 0x30BF;
const EGL_PLATFORM_ANGLE_ANGLE: u32 = 0x3202;
const EGL_PLATFORM_ANGLE_TYPE_ANGLE: isize = 0x3203;
const EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE: isize = 0x3489;

/// Which GL the host runs guest GL on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Angle,
    Apple,
}

/// The virglrenderer.h context request.
#[repr(C)]
pub(crate) struct GlCtxParam {
    version: c_int,
    shared: bool,
    major_ver: c_int,
    minor_ver: c_int,
    compat_ctx: c_int,
}

/// How contexts are made, per host.
enum Api {
    Angle {
        get_error: unsafe extern "C" fn() -> i32,
        create_context: unsafe extern "C" fn(Ptr, Ptr, Ptr, *const i32) -> Ptr,
        destroy_context: unsafe extern "C" fn(Ptr, Ptr) -> u32,
        make_current: unsafe extern "C" fn(Ptr, Ptr, Ptr, Ptr) -> u32,
        display: Ptr,
        robust: bool,
    },
    Apple {
        pixel_format: Ptr,
        create_context: unsafe extern "C" fn(Ptr, Ptr, *mut Ptr) -> c_int,
        destroy_context: unsafe extern "C" fn(Ptr) -> c_int,
        set_current: unsafe extern "C" fn(Ptr) -> c_int,
    },
}

struct Egl {
    api: Api,
    /// Never destroyed: every context shares with it, so the share group
    /// outlives any one guest context.
    root: Ptr,
    /// Scanout readback (None: read through virglrenderer instead).
    reader: Option<Mutex<Reader>>,
}

// SAFETY: EGL and CGL handles are process-wide and their calls are
// thread-safe; the struct is only read after init.
unsafe impl Send for Egl {}
// SAFETY: see above.
unsafe impl Sync for Egl {}

static EGL: OnceLock<Option<Egl>> = OnceLock::new();

/// Bring up `host`'s GL and a first context (ANGLE from `dir`). Call
/// before the sandbox, once per process. Returns whether GL is usable.
pub(crate) fn load(dir: &Path, host: Host) -> bool {
    EGL.get_or_init(|| match host {
        Host::Angle => open(dir),
        Host::Apple => open_apple(),
    })
    .is_some()
}

fn egl() -> Option<&'static Egl> {
    EGL.get().and_then(|e| e.as_ref())
}

const APPLE_OPENGL: &str = "/System/Library/Frameworks/OpenGL.framework/Versions/Current/OpenGL";
// CGL pixel format attributes.
const CGL_PFA_ACCELERATED: c_int = 73;
const CGL_PFA_ALLOW_OFFLINE_RENDERERS: c_int = 96;
const CGL_PFA_OPENGL_PROFILE: c_int = 99;
const CGL_OGL_PVERSION_GL4_CORE: c_int = 0x4100;

/// macOS's OpenGL 4.1 core profile, through CGL.
fn open_apple() -> Option<Egl> {
    let c = CString::new(APPLE_OPENGL).ok()?;
    // SAFETY: dlopen with a valid C string. Global: our libepoxy finds
    // CGLGetCurrentContext with dlsym(RTLD_DEFAULT).
    let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if h.is_null() {
        log::warn!("gpu3d: can't load Apple's OpenGL");
        return None;
    }
    let sym = |name: &str| -> Option<Ptr> {
        let c = CString::new(name).ok()?;
        // SAFETY: valid handle and name.
        let f = unsafe { libc::dlsym(h, c.as_ptr()) };
        if f.is_null() {
            log::warn!("gpu3d: {name} missing");
        }
        (!f.is_null()).then_some(f)
    };
    // SAFETY: the CGL signature it is cast to.
    let choose: unsafe extern "C" fn(*const c_int, *mut Ptr, *mut c_int) -> c_int = unsafe { std::mem::transmute(sym("CGLChoosePixelFormat")?) };
    // SAFETY: as above.
    let create_context: unsafe extern "C" fn(Ptr, Ptr, *mut Ptr) -> c_int = unsafe { std::mem::transmute(sym("CGLCreateContext")?) };
    // SAFETY: as above.
    let destroy_context: unsafe extern "C" fn(Ptr) -> c_int = unsafe { std::mem::transmute(sym("CGLDestroyContext")?) };
    // SAFETY: as above.
    let set_current: unsafe extern "C" fn(Ptr) -> c_int = unsafe { std::mem::transmute(sym("CGLSetCurrentContext")?) };
    let attrs = [CGL_PFA_OPENGL_PROFILE, CGL_OGL_PVERSION_GL4_CORE, CGL_PFA_ACCELERATED, CGL_PFA_ALLOW_OFFLINE_RENDERERS, 0];
    let (mut pixel_format, mut n) = (std::ptr::null_mut(), 0);
    // SAFETY: zero-terminated attributes, valid out pointers.
    let r = unsafe { choose(attrs.as_ptr(), &mut pixel_format, &mut n) };
    if r != 0 || pixel_format.is_null() {
        log::warn!("gpu3d: no OpenGL 4.1 pixel format ({r})");
        return None;
    }
    let mut e = Egl { api: Api::Apple { pixel_format, create_context, destroy_context, set_current }, root: std::ptr::null_mut(), reader: None };
    e.root = e.context(4, 1)?;
    let get_proc = move |name: &CStr| -> Ptr {
        // SAFETY: valid handle and name.
        unsafe { libc::dlsym(h, name.as_ptr()) }
    };
    e.reader = Gl::load(&get_proc).and_then(|gl| {
        // Desktop GL reads BGRA without an extension.
        let ctx = e.context(4, 1)?;
        let present = Present::load(&get_proc, ctx);
        Some(Mutex::new(Reader { gl, ctx, fbo: 0, pbo: 0, pbo_size: 0, bgra: Some(true), pending: None, present }))
    });
    log::info!("gpu3d: Apple OpenGL 4.1 (CGL)");
    Some(e)
}

fn open(dir: &Path) -> Option<Egl> {
    let path = dir.join("libEGL.dylib");
    if !path.exists() || !dir.join("libGLESv2.dylib").exists() {
        log::info!("gpu3d: no ANGLE in {}: OpenGL off", dir.display());
        return None;
    }
    let c = CString::new(path.to_str()?).ok()?;
    // SAFETY: dlopen with a valid C string.
    let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        log::warn!("gpu3d: can't load {}", path.display());
        return None;
    }
    macro_rules! sym {
        ($name:literal) => {{
            let c = CString::new($name).ok()?;
            // SAFETY: valid handle and name.
            let f = unsafe { libc::dlsym(h, c.as_ptr()) };
            if f.is_null() {
                log::warn!("gpu3d: {} missing", $name);
                return None;
            }
            // SAFETY: the symbol has the EGL 1.5 signature it is cast to.
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                std::mem::transmute::<Ptr, _>(f)
            }
        }};
    }
    let get_platform_display: unsafe extern "C" fn(u32, Ptr, *const isize) -> Ptr = sym!("eglGetPlatformDisplay");
    let initialize: unsafe extern "C" fn(Ptr, *mut i32, *mut i32) -> u32 = sym!("eglInitialize");
    let bind_api: unsafe extern "C" fn(u32) -> u32 = sym!("eglBindAPI");
    let query_string: unsafe extern "C" fn(Ptr, i32) -> *const c_char = sym!("eglQueryString");
    let get_error: unsafe extern "C" fn() -> i32 = sym!("eglGetError");
    let (create_context, destroy_context, make_current) = (sym!("eglCreateContext"), sym!("eglDestroyContext"), sym!("eglMakeCurrent"));
    let attrs = [EGL_PLATFORM_ANGLE_TYPE_ANGLE, EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE, EGL_NONE as isize];
    // SAFETY: EGL_DEFAULT_DISPLAY (null) and a NONE-terminated list.
    let d = unsafe { get_platform_display(EGL_PLATFORM_ANGLE_ANGLE, std::ptr::null_mut(), attrs.as_ptr()) };
    let (mut major, mut minor) = (0, 0);
    // SAFETY: d is a display handle (or null, which fails cleanly).
    if d.is_null() || unsafe { initialize(d, &mut major, &mut minor) } == 0 {
        // SAFETY: plain call.
        log::warn!("gpu3d: ANGLE (Metal) display failed: {:#x}", unsafe { get_error() });
        return None;
    }
    // SAFETY: plain calls on an initialized display.
    let exts = unsafe {
        bind_api(EGL_OPENGL_ES_API);
        let s = query_string(d, EGL_EXTENSIONS);
        if s.is_null() { String::new() } else { CStr::from_ptr(s).to_string_lossy().into_owned() }
    };
    let has = |x: &str| exts.split(' ').any(|e| e == x);
    // Contexts without config or surface: virglrenderer draws to FBOs.
    if !has("EGL_KHR_no_config_context") || !has("EGL_KHR_surfaceless_context") {
        log::warn!("gpu3d: ANGLE lacks surfaceless contexts: OpenGL off");
        return None;
    }
    let robust = has("EGL_EXT_create_context_robustness");
    let api = Api::Angle { get_error, create_context, destroy_context, make_current, display: d, robust };
    let mut e = Egl { api, root: std::ptr::null_mut(), reader: None };
    e.root = e.context(3, 0)?;
    log::info!("gpu3d: ANGLE EGL {major}.{minor} on Metal{}", if robust { ", robust access" } else { "" });
    let egl_get_proc: unsafe extern "C" fn(*const c_char) -> Ptr = sym!("eglGetProcAddress");
    // SAFETY: valid C string; ANGLE returns core entry points too.
    let get_proc = move |name: &CStr| unsafe { egl_get_proc(name.as_ptr()) };
    e.reader = Gl::load(&get_proc).and_then(|gl| Some(Mutex::new(Reader { gl, ctx: e.context(3, 0)?, fbo: 0, pbo: 0, pbo_size: 0, bgra: None, pending: None, present: None })));
    Some(e)
}

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_EXTENSIONS: u32 = 0x1F03;
const GL_PACK_ALIGNMENT: u32 = 0x0D05;
const GL_BGRA_EXT: u32 = 0x80E1;
const GL_PIXEL_PACK_BUFFER: u32 = 0x88EB;
const GL_STREAM_READ: u32 = 0x88E1;
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_MAP_READ_BIT: u32 = 0x0001;
const GL_SYNC_GPU_COMMANDS_COMPLETE: u32 = 0x9117;
const GL_SYNC_FLUSH_COMMANDS_BIT: u32 = 0x0001;
const GL_ALREADY_SIGNALED: u32 = 0x911A;
const GL_CONDITION_SATISFIED: u32 = 0x911C;
/// Longest wait for a readback the caller insists on (a stuck GPU).
const READ_WAIT_NS: u64 = 1_000_000_000;

/// The few GL ES 3.0 / GL 4.1 calls scanout readback needs.
struct Gl {
    get_error: unsafe extern "C" fn() -> u32,
    get_string: unsafe extern "C" fn(u32) -> *const c_char,
    gen_framebuffers: unsafe extern "C" fn(i32, *mut u32),
    bind_framebuffer: unsafe extern "C" fn(u32, u32),
    framebuffer_texture_2d: unsafe extern "C" fn(u32, u32, u32, u32, i32),
    check_framebuffer_status: unsafe extern "C" fn(u32) -> u32,
    gen_buffers: unsafe extern "C" fn(i32, *mut u32),
    bind_buffer: unsafe extern "C" fn(u32, u32),
    buffer_data: unsafe extern "C" fn(u32, isize, *const c_void, u32),
    pixel_storei: unsafe extern "C" fn(u32, i32),
    read_pixels: unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, Ptr),
    map_buffer_range: unsafe extern "C" fn(u32, isize, isize, u32) -> Ptr,
    unmap_buffer: unsafe extern "C" fn(u32) -> u8,
    fence_sync: unsafe extern "C" fn(u32, u32) -> Ptr,
    client_wait_sync: unsafe extern "C" fn(Ptr, u32, u64) -> u32,
    delete_sync: unsafe extern "C" fn(Ptr),
}

impl Gl {
    fn load(get_proc: &dyn Fn(&CStr) -> Ptr) -> Option<Gl> {
        macro_rules! gl {
            ($name:literal) => {{
                let c = CString::new($name).ok()?;
                let f = get_proc(&c);
                if f.is_null() {
                    log::warn!("gpu3d: {} missing", $name);
                    return None;
                }
                // SAFETY: the GL signature it is cast to (same in ES 3.0
                // and GL 4.1).
                #[allow(clippy::missing_transmute_annotations)]
                unsafe {
                    std::mem::transmute::<Ptr, _>(f)
                }
            }};
        }
        Some(Gl {
            get_error: gl!("glGetError"),
            get_string: gl!("glGetString"),
            gen_framebuffers: gl!("glGenFramebuffers"),
            bind_framebuffer: gl!("glBindFramebuffer"),
            framebuffer_texture_2d: gl!("glFramebufferTexture2D"),
            check_framebuffer_status: gl!("glCheckFramebufferStatus"),
            gen_buffers: gl!("glGenBuffers"),
            bind_buffer: gl!("glBindBuffer"),
            buffer_data: gl!("glBufferData"),
            pixel_storei: gl!("glPixelStorei"),
            read_pixels: gl!("glReadPixels"),
            map_buffer_range: gl!("glMapBufferRange"),
            unmap_buffer: gl!("glUnmapBuffer"),
            fence_sync: gl!("glFenceSync"),
            client_wait_sync: gl!("glClientWaitSync"),
            delete_sync: gl!("glDeleteSync"),
        })
    }
}

/// A copy on its way into the pixel buffer.
struct Pending {
    sync: Ptr,
    size: usize,
    row: usize,
    flip: bool,
}

/// Reads scanout textures through a pixel buffer, in a context of its own
/// so virglrenderer's GL state is never touched. ANGLE then copies on the
/// GPU (which also untiles; a plain glReadPixels to memory untiles on the
/// CPU, several times slower), and the GPU worker goes on with other
/// commands until the copy is done.
struct Reader {
    gl: Gl,
    ctx: Ptr,
    fbo: u32,
    pbo: u32,
    pbo_size: usize,
    /// GL_EXT_read_format_bgra, checked on first use.
    bgra: Option<bool>,
    pending: Option<Pending>,
    /// Frames into IOSurfaces instead (Apple's GL only).
    present: Option<Present>,
}

impl Reader {
    /// Queue a copy of `w` x `h` pixels at (`x`, `y`) of texture `tex`
    /// into the pixel buffer, as BGRX. `flip`: the texture's height, when
    /// its rows are stored bottom-up.
    ///
    /// # Safety
    /// `self.ctx` must be current on this thread.
    unsafe fn start(&mut self, tex: u32, flip: Option<u32>, (x, y, w, h): (u32, u32, u32, u32)) -> bool {
        let gl = &self.gl;
        let row = w as usize * 4;
        let size = row * h as usize;
        let gl_y = match flip {
            Some(height) => match height.checked_sub(y).and_then(|v| v.checked_sub(h)) {
                Some(v) => v,
                None => return false,
            },
            None => y,
        };
        if size == 0 {
            return false;
        }
        // SAFETY: GL calls on our current context (whole block), with
        // names we generated.
        unsafe {
            // One copy at a time: an unclaimed one (device reset) goes.
            if let Some(p) = self.pending.take() {
                (gl.delete_sync)(p.sync);
            }
            let bgra = *self.bgra.get_or_insert_with(|| {
                let s = (gl.get_string)(GL_EXTENSIONS);
                !s.is_null() && CStr::from_ptr(s).to_string_lossy().split(' ').any(|e| e == "GL_EXT_read_format_bgra")
            });
            if !bgra {
                return false;
            }
            while (gl.get_error)() != 0 {}
            if self.fbo == 0 {
                (gl.gen_framebuffers)(1, &mut self.fbo);
                (gl.gen_buffers)(1, &mut self.pbo);
            }
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, self.fbo);
            (gl.framebuffer_texture_2d)(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
            let mut ok = (gl.check_framebuffer_status)(GL_READ_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
            if ok {
                (gl.bind_buffer)(GL_PIXEL_PACK_BUFFER, self.pbo);
                if self.pbo_size < size {
                    (gl.buffer_data)(GL_PIXEL_PACK_BUFFER, size as isize, std::ptr::null(), GL_STREAM_READ);
                    self.pbo_size = size;
                }
                (gl.pixel_storei)(GL_PACK_ALIGNMENT, 4);
                // Offset 0 in the bound pack buffer.
                (gl.read_pixels)(x as i32, gl_y as i32, w as i32, h as i32, GL_BGRA_EXT, GL_UNSIGNED_BYTE, std::ptr::null_mut());
                (gl.bind_buffer)(GL_PIXEL_PACK_BUFFER, 0);
                let sync = (gl.fence_sync)(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
                ok = !sync.is_null() && (gl.get_error)() == 0;
                if ok {
                    self.pending = Some(Pending { sync, size, row, flip: flip.is_some() });
                } else if !sync.is_null() {
                    (gl.delete_sync)(sync);
                }
            }
            // Never keep a guest texture alive through our framebuffer.
            (gl.framebuffer_texture_2d)(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 0, 0);
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, 0);
            ok
        }
    }

    /// Is the pending copy done? Any context of the share group may be
    /// current (sync objects are shared).
    ///
    /// # Safety
    /// A context of ours must be current on this thread.
    unsafe fn done(&self, wait: bool) -> bool {
        let Some(p) = &self.pending else { return false };
        let (flags, timeout) = if wait { (GL_SYNC_FLUSH_COMMANDS_BIT, READ_WAIT_NS) } else { (0, 0) };
        // SAFETY: a live sync object of the share group.
        let r = unsafe { (self.gl.client_wait_sync)(p.sync, flags, timeout) };
        r == GL_ALREADY_SIGNALED || r == GL_CONDITION_SATISFIED
    }

    /// Copy the finished pixel buffer into `out`, top row first.
    ///
    /// # Safety
    /// `self.ctx` must be current on this thread.
    unsafe fn finish(&mut self, out: &mut [u8]) -> bool {
        let Some(p) = self.pending.take() else { return false };
        let gl = &self.gl;
        // SAFETY: GL calls on our current context; the mapping is read only
        // within `p.size`, which the buffer was sized for.
        unsafe {
            (gl.delete_sync)(p.sync);
            if out.len() < p.size {
                return false;
            }
            (gl.bind_buffer)(GL_PIXEL_PACK_BUFFER, self.pbo);
            let m = (gl.map_buffer_range)(GL_PIXEL_PACK_BUFFER, 0, p.size as isize, GL_MAP_READ_BIT) as *const u8;
            let ok = !m.is_null();
            if ok {
                let src = std::slice::from_raw_parts(m, p.size);
                if p.flip {
                    for (d, s) in out.chunks_exact_mut(p.row).zip(src.chunks_exact(p.row).rev()) {
                        d.copy_from_slice(s);
                    }
                } else {
                    out[..p.size].copy_from_slice(src);
                }
                (gl.unmap_buffer)(GL_PIXEL_PACK_BUFFER);
            }
            (gl.bind_buffer)(GL_PIXEL_PACK_BUFFER, 0);
            ok
        }
    }
}

/// Run `f` with the reader's context current, then put back what
/// virglrenderer expects to find bound.
fn with_reader<R>(f: impl FnOnce(&mut Reader) -> R) -> Option<R> {
    let e = egl()?;
    let reader = e.reader.as_ref()?;
    let mut r = reader.lock().unwrap_or_else(|p| p.into_inner());
    let prev = CURRENT.with(|c| c.get());
    if !set_current(e, r.ctx) {
        return None;
    }
    let out = f(&mut r);
    set_current(e, prev);
    Some(out)
}

/// Start copying a rect of GL texture `tex` (a scanout) out as BGRX rows;
/// [`read_poll`] delivers it. `flip`: see [`Reader::start`]. False if that
/// isn't possible here (the caller then reads through virglrenderer).
pub(crate) fn read_start(tex: u32, flip: Option<u32>, rect: (u32, u32, u32, u32)) -> bool {
    // SAFETY: with_reader makes r.ctx current.
    with_reader(|r| unsafe { r.start(tex, flip, rect) }).unwrap_or(false)
}

/// The copy [`read_start`] queued: None while the GPU is still at it
/// (unless `wait`), else whether `out` got it.
pub(crate) fn read_poll(out: &mut [u8], wait: bool) -> Option<bool> {
    let reader = egl()?.reader.as_ref()?;
    {
        let r = reader.lock().unwrap_or_else(|p| p.into_inner());
        r.pending.as_ref()?;
        // Poll in whatever context is bound: no switch (which flushes)
        // until there is something to copy.
        // SAFETY: one of our contexts is current on this thread.
        if !wait && !CURRENT.with(|c| c.get()).is_null() && !unsafe { r.done(false) } {
            return None;
        }
    }
    // SAFETY: with_reader makes r.ctx current.
    with_reader(|r| unsafe {
        if !r.done(wait) {
            if !wait {
                return None;
            }
            // Give up on it: the GPU never got there.
            if let Some(p) = r.pending.take() {
                (r.gl.delete_sync)(p.sync);
            }
            return Some(false);
        }
        Some(r.finish(out))
    })
    .flatten()
}

const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const GL_TEXTURE_RECTANGLE: u32 = 0x84F5;
const GL_RGBA8: u32 = 0x8058;
const GL_UNSIGNED_INT_8_8_8_8_REV: u32 = 0x8367;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_NEAREST: u32 = 0x2600;

/// Surfaces per monitor: one shown, one on its way, one spare for the
/// window server still showing an old one.
const SURFACE_SLOTS: usize = vroom_ipc::surface::MAX_SLOTS as usize;
/// Monitors (virtio-gpu scanouts) with surfaces of their own.
const SURFACE_HEADS: usize = vroom_ipc::surface::MAX_HEADS as usize;

/// A surface made for a monitor, for the app (through the VMM).
#[derive(Debug)]
pub struct NewSurface {
    pub head: u32,
    pub gen: u32,
    pub slot: u32,
    pub surface: vroom_ipc::surface::Surface,
}

/// One monitor's surfaces: all the same size, made together.
struct Pool {
    gen: u32,
    size: (u32, u32),
    slots: Vec<(vroom_ipc::surface::Surface, u32)>,
    /// The slot last handed out (the app shows it, or soon will).
    last: Option<usize>,
}

/// A frame on its way into a surface.
struct PendingSurface {
    sync: Ptr,
    head: u32,
    gen: u32,
    slot: u32,
}

/// Scanout frames into IOSurfaces the app shows itself (a CALayer), with
/// Apple's GL: a blit on the GPU, no pixels back to the CPU. The surfaces
/// are made here and go to the app as mach ports, through the VMM.
struct Present {
    tex_image: unsafe extern "C" fn(Ptr, u32, u32, i32, i32, u32, u32, *const c_void, u32) -> c_int,
    gen_textures: unsafe extern "C" fn(i32, *mut u32),
    delete_textures: unsafe extern "C" fn(i32, *const u32),
    bind_texture: unsafe extern "C" fn(u32, u32),
    delete_framebuffers: unsafe extern "C" fn(i32, *const u32),
    blit_framebuffer: unsafe extern "C" fn(i32, i32, i32, i32, i32, i32, i32, i32, u32, u32),
    color_mask: unsafe extern "C" fn(u8, u8, u8, u8),
    clear_color: unsafe extern "C" fn(f32, f32, f32, f32),
    clear: unsafe extern "C" fn(u32),
    flush: unsafe extern "C" fn(),
    /// The reader's CGL context (the one these textures are made in).
    cgl: Ptr,
    draw_fbo: u32,
    pools: [Option<Pool>; SURFACE_HEADS],
    next_gen: u32,
    /// Made, not yet collected by [`take_surfaces`].
    new: Vec<NewSurface>,
    pending: Option<PendingSurface>,
}

impl Present {
    fn load(get_proc: &dyn Fn(&CStr) -> Ptr, cgl: Ptr) -> Option<Present> {
        macro_rules! f {
            ($name:literal) => {{
                let c = CString::new($name).ok()?;
                let f = get_proc(&c);
                if f.is_null() {
                    log::info!("gpu3d: {} missing: frames come back through the CPU", $name);
                    return None;
                }
                // SAFETY: the CGL / GL 4.1 signature it is cast to.
                #[allow(clippy::missing_transmute_annotations)]
                unsafe {
                    std::mem::transmute::<Ptr, _>(f)
                }
            }};
        }
        Some(Present {
            tex_image: f!("CGLTexImageIOSurface2D"),
            gen_textures: f!("glGenTextures"),
            delete_textures: f!("glDeleteTextures"),
            bind_texture: f!("glBindTexture"),
            delete_framebuffers: f!("glDeleteFramebuffers"),
            blit_framebuffer: f!("glBlitFramebuffer"),
            color_mask: f!("glColorMask"),
            clear_color: f!("glClearColor"),
            clear: f!("glClear"),
            flush: f!("glFlush"),
            cgl,
            draw_fbo: 0,
            pools: Default::default(),
            next_gen: 1,
            new: Vec::new(),
            pending: None,
        })
    }

    /// Head `head`'s surfaces at `w` x `h`, made anew (a new generation)
    /// if it has none of that size.
    ///
    /// # Safety
    /// The reader's context must be current.
    unsafe fn pool(&mut self, head: usize, w: u32, h: u32) -> Option<&mut Pool> {
        if self.pools[head].as_ref().is_some_and(|p| p.size == (w, h)) {
            return self.pools[head].as_mut();
        }
        if let Some(old) = self.pools[head].take() {
            for (_, t) in &old.slots {
                // SAFETY: a texture we made in this share group.
                unsafe { (self.delete_textures)(1, t) };
            }
        }
        let gen = self.next_gen;
        self.next_gen = self.next_gen.wrapping_add(1).max(1);
        let mut slots = Vec::new();
        for _ in 0..SURFACE_SLOTS {
            let s = vroom_ipc::surface::Surface::new(w, h)?;
            let mut t = 0;
            // SAFETY: GL calls on the current context; the surface lives as
            // long as the texture (both in the pool).
            let err = unsafe {
                (self.gen_textures)(1, &mut t);
                (self.bind_texture)(GL_TEXTURE_RECTANGLE, t);
                let e = (self.tex_image)(self.cgl, GL_TEXTURE_RECTANGLE, GL_RGBA8, w as i32, h as i32, GL_BGRA_EXT, GL_UNSIGNED_INT_8_8_8_8_REV, s.as_ptr(), 0);
                (self.bind_texture)(GL_TEXTURE_RECTANGLE, 0);
                e
            };
            if err != 0 {
                log::warn!("gpu3d: GL can't take an IOSurface ({err}): frames come back through the CPU");
                // SAFETY: the texture just made.
                unsafe { (self.delete_textures)(1, &t) };
                for (_, t) in &slots {
                    // SAFETY: textures made above.
                    unsafe { (self.delete_textures)(1, t) };
                }
                return None;
            }
            slots.push((s, t));
        }
        for (i, (s, _)) in slots.iter().enumerate() {
            self.new.push(NewSurface { head: head as u32, gen, slot: i as u32, surface: s.clone() });
        }
        log::info!("gpu3d: monitor {} on IOSurfaces, {w}x{h} (generation {gen})", head + 1);
        self.pools[head] = Some(Pool { gen, size: (w, h), slots, last: None });
        self.pools[head].as_mut()
    }
}

impl Reader {
    /// Queue a copy of `w` x `h` pixels at (`x`, `y`) of texture `tex`
    /// into one of head `head`'s surfaces, made here first if it has none
    /// of that size. `flip` as for [`Reader::start`].
    ///
    /// # Safety
    /// `self.ctx` must be current on this thread.
    unsafe fn surface_start(&mut self, head: u32, tex: u32, flip: Option<u32>, (x, y, w, h): (u32, u32, u32, u32)) -> bool {
        let gl = &self.gl;
        let Some(p) = self.present.as_mut() else { return false };
        let hd = head as usize;
        if hd >= SURFACE_HEADS || w == 0 || h == 0 {
            return false;
        }
        // Bottom-up texture rows: the box's GL rows, the other way round.
        let (src_y0, src_y1, dst_y0, dst_y1) = match flip {
            Some(height) => match height.checked_sub(y).and_then(|top| top.checked_sub(h).map(|b| (b, top))) {
                Some((b, t)) => (b as i32, t as i32, h as i32, 0),
                None => return false,
            },
            None => (y as i32, (y + h) as i32, 0, h as i32),
        };
        // SAFETY: GL calls on the reader's current context (whole block),
        // with names we made; the surfaces stay alive in the pool.
        unsafe {
            if let Some(old) = p.pending.take() {
                (gl.delete_sync)(old.sync);
            }
            while (gl.get_error)() != 0 {}
            let Some(pool) = p.pool(hd, w, h) else { return false };
            // A slot the window server isn't showing, and not the last one
            // handed out (the app may not have shown it yet).
            let n = pool.slots.len();
            let start = pool.last.map_or(0, |l| l + 1);
            let slot = (0..n).map(|i| (start + i) % n).filter(|&i| Some(i) != pool.last).find(|&i| !pool.slots[i].0.in_use()).unwrap_or(start % n);
            let (gen, dst) = (pool.gen, pool.slots[slot].1);
            let _ = pool;
            if self.fbo == 0 {
                (gl.gen_framebuffers)(1, &mut self.fbo);
            }
            if p.draw_fbo == 0 {
                (gl.gen_framebuffers)(1, &mut p.draw_fbo);
            }
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, self.fbo);
            (gl.framebuffer_texture_2d)(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
            (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, p.draw_fbo);
            (gl.framebuffer_texture_2d)(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_RECTANGLE, dst, 0);
            let mut ok = (gl.check_framebuffer_status)(GL_READ_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE
                && (gl.check_framebuffer_status)(GL_DRAW_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
            if ok {
                (p.blit_framebuffer)(x as i32, src_y0, (x + w) as i32, src_y1, 0, dst_y0, w as i32, dst_y1, GL_COLOR_BUFFER_BIT, GL_NEAREST);
                // Opaque: scanouts are BGRX, and the window server
                // blends what alpha says.
                (p.color_mask)(0, 0, 0, 1);
                (p.clear_color)(0.0, 0.0, 0.0, 1.0);
                (p.clear)(GL_COLOR_BUFFER_BIT);
                (p.color_mask)(1, 1, 1, 1);
                let sync = (gl.fence_sync)(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
                ok = !sync.is_null() && (gl.get_error)() == 0;
                if ok {
                    (p.flush)();
                    if let Some(pool) = p.pools[hd].as_mut() {
                        pool.last = Some(slot);
                    }
                    p.pending = Some(PendingSurface { sync, head, gen, slot: slot as u32 });
                } else if !sync.is_null() {
                    (gl.delete_sync)(sync);
                }
            }
            // Never keep a guest texture alive through our framebuffers.
            (gl.framebuffer_texture_2d)(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 0, 0);
            (gl.framebuffer_texture_2d)(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_RECTANGLE, 0, 0);
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, 0);
            (gl.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, 0);
            ok
        }
    }

    /// The pending surface frame, once the GPU has it (or given up on
    /// after a second, with `wait`): `Some(None)` when it failed.
    ///
    /// # Safety
    /// A context of ours must be current on this thread.
    unsafe fn surface_done(&mut self, wait: bool) -> Option<Option<(u32, u32, u32)>> {
        let p = self.present.as_mut()?;
        let pending = p.pending.as_ref()?;
        let (flags, timeout) = if wait { (GL_SYNC_FLUSH_COMMANDS_BIT, READ_WAIT_NS) } else { (0, 0) };
        // SAFETY: a live sync object of the share group.
        let r = unsafe { (self.gl.client_wait_sync)(pending.sync, flags, timeout) };
        let done = r == GL_ALREADY_SIGNALED || r == GL_CONDITION_SATISFIED;
        if !done && !wait {
            return None;
        }
        let pending = p.pending.take()?;
        // SAFETY: the sync object, no longer used.
        unsafe { (self.gl.delete_sync)(pending.sync) };
        Some(done.then_some((pending.head, pending.gen, pending.slot)))
    }
}

/// Start putting a rect of GL texture `tex` (what monitor `head` shows)
/// into one of its IOSurfaces; [`surface_poll`] says which. False if that
/// isn't possible here (no Apple GL): read it back instead.
pub(crate) fn surface_start(head: u32, tex: u32, flip: Option<u32>, rect: (u32, u32, u32, u32)) -> bool {
    // SAFETY: with_reader makes r.ctx current.
    with_reader(|r| unsafe { r.surface_start(head, tex, flip, rect) }).unwrap_or(false)
}

/// The frame [`surface_start`] queued: None while the GPU is at it (unless
/// `wait`) or if there is none, else (head, generation, slot), or None
/// inside when it failed.
pub(crate) fn surface_poll(wait: bool) -> Option<Option<(u32, u32, u32)>> {
    {
        let r = egl()?.reader.as_ref()?.lock().unwrap_or_else(|p| p.into_inner());
        r.present.as_ref()?.pending.as_ref()?;
    }
    // SAFETY: with_reader makes r.ctx current.
    with_reader(|r| unsafe { r.surface_done(wait) }).flatten()
}

/// Surfaces made since the last call, for the app.
pub(crate) fn take_surfaces() -> Vec<NewSurface> {
    let Some(reader) = egl().and_then(|e| e.reader.as_ref()) else { return Vec::new() };
    let mut r = reader.lock().unwrap_or_else(|p| p.into_inner());
    r.present.as_mut().map(|p| std::mem::take(&mut p.new)).unwrap_or_default()
}

/// Drop every monitor's surfaces (device reset): new ones next frame.
pub(crate) fn surfaces_reset() {
    // SAFETY: with_reader makes r.ctx current; textures of ours.
    with_reader(|r| unsafe {
        if let Some(p) = r.present.as_mut() {
            if let Some(old) = p.pending.take() {
                (r.gl.delete_sync)(old.sync);
            }
            for pool in p.pools.iter_mut().filter_map(Option::take) {
                for (_, t) in &pool.slots {
                    (p.delete_textures)(1, t);
                }
            }
            if p.draw_fbo != 0 {
                (p.delete_framebuffers)(1, &p.draw_fbo);
                p.draw_fbo = 0;
            }
            p.new.clear();
        }
    });
}

impl Egl {
    fn context(&self, major: c_int, minor: c_int) -> Option<Ptr> {
        match &self.api {
            Api::Angle { create_context, display, robust, .. } => {
                let mut attrs = vec![EGL_CONTEXT_MAJOR_VERSION, major, EGL_CONTEXT_MINOR_VERSION, minor];
                if *robust {
                    // Out-of-bounds buffer reads return zeros instead of
                    // whatever memory follows.
                    attrs.extend([EGL_CONTEXT_OPENGL_ROBUST_ACCESS, 1]);
                }
                attrs.push(EGL_NONE);
                // SAFETY: valid display; EGL_NO_CONFIG_KHR (null); share
                // with the root (null for the root itself); NONE-terminated.
                let c = unsafe { create_context(*display, std::ptr::null_mut(), self.root, attrs.as_ptr()) };
                (!c.is_null()).then_some(c)
            }
            Api::Apple { pixel_format, create_context, .. } => {
                // A 4.1 core context is all macOS has: say no to more, so
                // virglrenderer walks down to what it gets.
                if (major, minor) > (4, 1) {
                    return None;
                }
                let mut c = std::ptr::null_mut();
                // SAFETY: valid pixel format; share with the root (null for
                // the root itself); valid out pointer.
                let r = unsafe { create_context(*pixel_format, self.root, &mut c) };
                (r == 0 && !c.is_null()).then_some(c)
            }
        }
    }

    fn destroy(&self, ctx: Ptr) {
        match &self.api {
            // SAFETY: a context we created on this display (EGL defers the
            // delete while it is still bound somewhere).
            Api::Angle { destroy_context, display, .. } => unsafe {
                destroy_context(*display, ctx);
            },
            // SAFETY: a context we created and nobody uses any more.
            Api::Apple { destroy_context, .. } => unsafe {
                destroy_context(ctx);
            },
        }
    }

    fn make_current(&self, ctx: Ptr) -> bool {
        match &self.api {
            // SAFETY: a context of ours (or null to release); no surfaces.
            Api::Angle { make_current, display, .. } => unsafe { make_current(*display, std::ptr::null_mut(), std::ptr::null_mut(), ctx) != 0 },
            // SAFETY: a context of ours, or null to release.
            Api::Apple { set_current, .. } => unsafe { set_current(ctx) == 0 },
        }
    }

    fn error(&self) -> i32 {
        match &self.api {
            // SAFETY: plain call.
            Api::Angle { get_error, .. } => unsafe { get_error() },
            Api::Apple { .. } => 0,
        }
    }
}

// virglrenderer assumes one thread: it remembers which context it made
// current and skips switching when it thinks the right one is bound. We
// call it from the thread that sets it up, then the GPU worker (which
// changes on device reset), so the binding has to follow the caller.
// Its fence thread binds its own context; that one stays out of this.
thread_local! {
    /// Context bound on this thread.
    static CURRENT: Cell<Ptr> = const { Cell::new(std::ptr::null_mut()) };
    /// This thread runs renderer calls (not virglrenderer's own thread).
    static RENDERER: Cell<bool> = const { Cell::new(false) };
}
/// The context virglrenderer last made current from a renderer call.
static LAST: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn set_current(e: &Egl, ctx: Ptr) -> bool {
    let ok = e.make_current(ctx);
    if ok {
        CURRENT.with(|c| c.set(ctx));
    }
    ok
}

/// Before a renderer call: bind what virglrenderer thinks is current.
pub(crate) fn bind() {
    let Some(e) = egl() else { return };
    RENDERER.with(|r| r.set(true));
    let want = LAST.load(Ordering::Acquire);
    if !want.is_null() && CURRENT.with(|c| c.get()) != want && !set_current(e, want) {
        log::warn!("gpu3d: can't bind the GL context on this thread: {:#x}", e.error());
    }
}

/// This thread is done with the renderer: let another one bind.
pub(crate) fn release() {
    let Some(e) = egl() else { return };
    if !CURRENT.with(|c| c.get()).is_null() {
        set_current(e, std::ptr::null_mut());
    }
    RENDERER.with(|r| r.set(false));
}

pub(crate) extern "C" fn create_gl_context(_cookie: Ptr, _scanout: c_int, param: *mut GlCtxParam) -> Ptr {
    let Some(e) = egl() else { return std::ptr::null_mut() };
    if param.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: virglrenderer passes a valid parameter block.
    let p = unsafe { &*param };
    // virglrenderer walks down from GL 4.6; ANGLE answers for ES 3.x,
    // Apple for 4.1 and below. Apple has no compatibility profile.
    if p.compat_ctx != 0 && matches!(e.api, Api::Apple { .. }) {
        return std::ptr::null_mut();
    }
    e.context(p.major_ver, p.minor_ver).unwrap_or(std::ptr::null_mut())
}

pub(crate) extern "C" fn destroy_gl_context(_cookie: Ptr, ctx: Ptr) {
    if let Some(e) = egl() {
        if !ctx.is_null() && ctx != e.root {
            let _ = LAST.compare_exchange(ctx, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire);
            e.destroy(ctx);
        }
    }
}

pub(crate) extern "C" fn make_current(_cookie: Ptr, _scanout: c_int, ctx: Ptr) -> c_int {
    let Some(e) = egl() else { return -1 };
    if !set_current(e, ctx) {
        return -1;
    }
    if RENDERER.with(|r| r.get()) {
        LAST.store(ctx, Ordering::Release);
    }
    0
}
