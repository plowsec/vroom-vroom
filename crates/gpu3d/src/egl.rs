//! OpenGL ES contexts for virglrenderer's GL side (virgl), from ANGLE's
//! EGL on its Metal backend.
//!
//! virglrenderer asks for contexts through callbacks; these create them
//! on one ANGLE display, all in one share group, without surfaces (it
//! only renders to its own framebuffer objects). ANGLE's libEGL must sit
//! next to virglrenderer: the libepoxy built into it loads libEGL and
//! libGLESv2 from there, and the two must be the same library instance.
//!
//! ANGLE validates every GL call it gets (it was made for WebGL), so guest
//! commands that slip past virglrenderer still meet a second check before
//! the Metal driver.

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

/// The virglrenderer.h context request.
#[repr(C)]
pub(crate) struct GlCtxParam {
    version: c_int,
    shared: bool,
    major_ver: c_int,
    minor_ver: c_int,
    compat_ctx: c_int,
}

struct Egl {
    get_error: unsafe extern "C" fn() -> i32,
    create_context: unsafe extern "C" fn(Ptr, Ptr, Ptr, *const i32) -> Ptr,
    destroy_context: unsafe extern "C" fn(Ptr, Ptr) -> u32,
    make_current: unsafe extern "C" fn(Ptr, Ptr, Ptr, Ptr) -> u32,
    display: Ptr,
    /// Never destroyed: every context shares with it, so the share group
    /// outlives any one guest context.
    root: Ptr,
    robust: bool,
    /// Scanout readback (None: read through virglrenderer instead).
    reader: Option<Mutex<Reader>>,
}

// SAFETY: EGL handles are process-wide and EGL calls are thread-safe; the
// struct is only read after init.
unsafe impl Send for Egl {}
// SAFETY: see above.
unsafe impl Sync for Egl {}

static EGL: OnceLock<Option<Egl>> = OnceLock::new();

/// Open ANGLE's libEGL in `dir`, bring up a Metal display and a first
/// context. Call before the sandbox. Returns whether GL is usable.
pub(crate) fn load(dir: &Path) -> bool {
    EGL.get_or_init(|| open(dir)).is_some()
}

fn egl() -> Option<&'static Egl> {
    EGL.get().and_then(|e| e.as_ref())
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
    let mut e = Egl {
        get_error: sym!("eglGetError"),
        create_context: sym!("eglCreateContext"),
        destroy_context: sym!("eglDestroyContext"),
        make_current: sym!("eglMakeCurrent"),
        display: std::ptr::null_mut(),
        root: std::ptr::null_mut(),
        robust: false,
        reader: None,
    };
    let attrs = [EGL_PLATFORM_ANGLE_TYPE_ANGLE, EGL_PLATFORM_ANGLE_TYPE_METAL_ANGLE, EGL_NONE as isize];
    // SAFETY: EGL_DEFAULT_DISPLAY (null) and a NONE-terminated list.
    let d = unsafe { get_platform_display(EGL_PLATFORM_ANGLE_ANGLE, std::ptr::null_mut(), attrs.as_ptr()) };
    let (mut major, mut minor) = (0, 0);
    // SAFETY: d is a display handle (or null, which fails cleanly).
    if d.is_null() || unsafe { initialize(d, &mut major, &mut minor) } == 0 {
        // SAFETY: plain call.
        log::warn!("gpu3d: ANGLE (Metal) display failed: {:#x}", unsafe { (e.get_error)() });
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
    e.display = d;
    e.robust = has("EGL_EXT_create_context_robustness");
    e.root = e.context(3, 0)?;
    log::info!("gpu3d: ANGLE EGL {major}.{minor} on Metal{}", if e.robust { ", robust access" } else { "" });
    let get_proc: unsafe extern "C" fn(*const c_char) -> Ptr = sym!("eglGetProcAddress");
    e.reader = Gl::load(get_proc).and_then(|gl| Some(Mutex::new(Reader { gl, ctx: e.context(3, 0)?, fbo: 0, pbo: 0, pbo_size: 0, bgra: None, pending: None })));
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

/// The few GL ES 3.0 calls scanout readback needs.
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
    fn load(get_proc: unsafe extern "C" fn(*const c_char) -> Ptr) -> Option<Gl> {
        macro_rules! gl {
            ($name:literal) => {{
                let c = CString::new($name).ok()?;
                // SAFETY: valid C string; ANGLE returns core entry points too.
                let f = unsafe { get_proc(c.as_ptr()) };
                if f.is_null() {
                    log::warn!("gpu3d: {} missing", $name);
                    return None;
                }
                // SAFETY: the GL ES 3.0 signature it is cast to.
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

impl Egl {
    fn context(&self, major: c_int, minor: c_int) -> Option<Ptr> {
        let mut attrs = vec![EGL_CONTEXT_MAJOR_VERSION, major, EGL_CONTEXT_MINOR_VERSION, minor];
        if self.robust {
            // Out-of-bounds buffer reads return zeros instead of whatever
            // memory follows.
            attrs.extend([EGL_CONTEXT_OPENGL_ROBUST_ACCESS, 1]);
        }
        attrs.push(EGL_NONE);
        // SAFETY: valid display; EGL_NO_CONFIG_KHR (null); share with the
        // root (null for the root itself); NONE-terminated attributes.
        let c = unsafe { (self.create_context)(self.display, std::ptr::null_mut(), self.root, attrs.as_ptr()) };
        (!c.is_null()).then_some(c)
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
    // SAFETY: a context of ours (or null to release); no surfaces.
    let ok = unsafe { (e.make_current)(e.display, std::ptr::null_mut(), std::ptr::null_mut(), ctx) } != 0;
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
        // SAFETY: plain call.
        log::warn!("gpu3d: can't bind the GL context on this thread: {:#x}", unsafe { (e.get_error)() });
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
    // virglrenderer walks down from GL 4.6; ANGLE answers for ES 3.x.
    e.context(p.major_ver, p.minor_ver).unwrap_or(std::ptr::null_mut())
}

pub(crate) extern "C" fn destroy_gl_context(_cookie: Ptr, ctx: Ptr) {
    if let Some(e) = egl() {
        if !ctx.is_null() && ctx != e.root {
            let _ = LAST.compare_exchange(ctx, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire);
            // SAFETY: a context we created on this display (EGL defers the
            // delete while it is still bound somewhere).
            unsafe { (e.destroy_context)(e.display, ctx) };
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
