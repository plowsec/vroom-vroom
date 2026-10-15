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
    e.reader = Gl::load(get_proc).and_then(|gl| Some(Mutex::new(Reader { gl, ctx: e.context(3, 0)?, fbo: 0, pbo: 0, pbo_size: 0, bgra: None })));
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
        })
    }
}

/// Reads scanout textures through a pixel buffer, in a context of its own
/// so virglrenderer's GL state is never touched. ANGLE then copies on the
/// GPU (which also untiles); a plain glReadPixels to memory untiles on
/// the CPU, several times slower.
struct Reader {
    gl: Gl,
    ctx: Ptr,
    fbo: u32,
    pbo: u32,
    pbo_size: usize,
    /// GL_EXT_read_format_bgra, checked on first use.
    bgra: Option<bool>,
}

impl Reader {
    /// With our context current: copy `w` x `h` pixels at (`x`, `y`) of
    /// texture `tex` into `out` as BGRX rows, top row first. `flip`: the
    /// texture's height, when its rows are stored bottom-up.
    ///
    /// # Safety
    /// `self.ctx` must be current on this thread.
    unsafe fn read(&mut self, tex: u32, flip: Option<u32>, (x, y, w, h): (u32, u32, u32, u32), out: &mut [u8]) -> bool {
        let gl = &self.gl;
        let row = w as usize * 4;
        let size = row * h as usize;
        if out.len() < size || size == 0 {
            return false;
        }
        let gl_y = match flip {
            Some(height) => match height.checked_sub(y).and_then(|v| v.checked_sub(h)) {
                Some(v) => v,
                None => return false,
            },
            None => y,
        };
        // SAFETY: GL calls on our current context (whole block), with
        // names we generated; the mapping is read only within `size`.
        unsafe {
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
                let p = (gl.map_buffer_range)(GL_PIXEL_PACK_BUFFER, 0, size as isize, GL_MAP_READ_BIT) as *const u8;
                ok = !p.is_null();
                if ok {
                    let src = std::slice::from_raw_parts(p, size);
                    if flip.is_some() {
                        for (d, s) in out.chunks_exact_mut(row).zip(src.chunks_exact(row).rev()) {
                            d.copy_from_slice(s);
                        }
                    } else {
                        out[..size].copy_from_slice(src);
                    }
                    (gl.unmap_buffer)(GL_PIXEL_PACK_BUFFER);
                }
                (gl.bind_buffer)(GL_PIXEL_PACK_BUFFER, 0);
                ok &= (gl.get_error)() == 0;
            }
            // Never keep a guest texture alive through our framebuffer.
            (gl.framebuffer_texture_2d)(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 0, 0);
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, 0);
            ok
        }
    }
}

/// Copy a rect of GL texture `tex` (a scanout) into `out` as BGRX rows,
/// top first. `flip`: see [`Reader::read`]. False if that isn't possible
/// here (the caller then reads through virglrenderer).
pub(crate) fn read_texture(tex: u32, flip: Option<u32>, rect: (u32, u32, u32, u32), out: &mut [u8]) -> bool {
    let Some(e) = egl() else { return false };
    let Some(reader) = &e.reader else { return false };
    let mut r = reader.lock().unwrap_or_else(|p| p.into_inner());
    let prev = CURRENT.with(|c| c.get());
    if !set_current(e, r.ctx) {
        return false;
    }
    // SAFETY: r.ctx is current on this thread (just above).
    let ok = unsafe { r.read(tex, flip, rect, out) };
    // Back to what virglrenderer expects to find bound.
    set_current(e, prev);
    ok
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
