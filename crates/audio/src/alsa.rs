//! Playback and recording through ALSA (libasound, loaded at run time: no
//! build dependency, and hosts without it just have no sound). A thread
//! per direction moves audio with blocking calls, which pace it.

#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, c_long, c_uint, c_void, CStr};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use super::{CaptureOut, Input, Output, Ring};

type Pcm = *mut c_void;

struct Lib {
    open: unsafe extern "C" fn(*mut Pcm, *const c_char, c_int, c_int) -> c_int,
    set_params: unsafe extern "C" fn(Pcm, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int,
    writei: unsafe extern "C" fn(Pcm, *const c_void, c_long) -> c_long,
    readi: unsafe extern "C" fn(Pcm, *mut c_void, c_long) -> c_long,
    recover: unsafe extern "C" fn(Pcm, c_int, c_int) -> c_int,
    drop: unsafe extern "C" fn(Pcm) -> c_int,
    close: unsafe extern "C" fn(Pcm) -> c_int,
}

const STREAM_PLAYBACK: c_int = 0;
const STREAM_CAPTURE: c_int = 1;
const FORMAT_S16_LE: c_int = 2;
const ACCESS_RW_INTERLEAVED: c_int = 3;
const LATENCY_US: c_uint = 60_000;
const CHUNK_FRAMES: usize = 480;

fn load() -> io::Result<Lib> {
    // SAFETY: dlopen/dlsym with NUL-terminated names; each symbol gets
    // the signature from alsa/pcm.h.
    unsafe {
        let h = libc::dlopen(c"libasound.so.2".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if h.is_null() {
            return Err(io::Error::other("libasound.so.2 not found"));
        }
        let sym = |n: &CStr| -> io::Result<*mut c_void> {
            let p = libc::dlsym(h, n.as_ptr());
            if p.is_null() {
                Err(io::Error::other(format!("libasound: {} missing", n.to_string_lossy())))
            } else {
                Ok(p)
            }
        };
        Ok(Lib {
            open: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut Pcm, *const c_char, c_int, c_int) -> c_int>(sym(
                c"snd_pcm_open",
            )?),
            set_params: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Pcm, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int>(
                sym(c"snd_pcm_set_params")?,
            ),
            writei: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Pcm, *const c_void, c_long) -> c_long>(sym(
                c"snd_pcm_writei",
            )?),
            readi: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Pcm, *mut c_void, c_long) -> c_long>(sym(
                c"snd_pcm_readi",
            )?),
            recover: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Pcm, c_int, c_int) -> c_int>(sym(c"snd_pcm_recover")?),
            drop: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Pcm) -> c_int>(sym(c"snd_pcm_drop")?),
            close: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Pcm) -> c_int>(sym(c"snd_pcm_close")?),
        })
    }
}

fn load_or_warn() -> Option<Arc<Lib>> {
    match load() {
        Ok(l) => Some(Arc::new(l)),
        Err(e) => {
            log::warn!("audio: {e}");
            None
        }
    }
}

struct Handle(Pcm);
// SAFETY: the PCM handle is used by one thread at a time (the player or
// recorder thread; closed by it).
unsafe impl Send for Handle {}

// SAFETY: Lib holds plain function pointers.
unsafe impl Send for Lib {}
// SAFETY: as above.
unsafe impl Sync for Lib {}

/// Open the "default" device for `stream`, S16LE interleaved.
fn open(lib: &Lib, stream: c_int, rate: u32, channels: u8) -> io::Result<Handle> {
    let mut pcm: Pcm = std::ptr::null_mut();
    // SAFETY: out pointer valid; "default" device; blocking mode.
    let r = unsafe { (lib.open)(&mut pcm, c"default".as_ptr(), stream, 0) };
    if r < 0 {
        return Err(io::Error::other(format!("snd_pcm_open: {r}")));
    }
    // SAFETY: valid pcm; plain parameters (resample allowed).
    let r = unsafe { (lib.set_params)(pcm, FORMAT_S16_LE, ACCESS_RW_INTERLEAVED, channels as c_uint, rate, 1, LATENCY_US) };
    if r < 0 {
        // SAFETY: valid pcm, closed once.
        unsafe { (lib.close)(pcm) };
        return Err(io::Error::other(format!("snd_pcm_set_params: {r}")));
    }
    Ok(Handle(pcm))
}

/// Drop and close, once, from the thread that owns the handle.
fn close(lib: &Lib, h: Handle) {
    // SAFETY: valid pcm, dropped and closed once, here.
    unsafe {
        (lib.drop)(h.0);
        (lib.close)(h.0);
    }
}

pub struct AlsaOut {
    ring: Arc<Mutex<Ring>>,
    lib: Option<Arc<Lib>>,
    run: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AlsaOut {
    pub fn new(ring: Arc<Mutex<Ring>>) -> Self {
        AlsaOut { ring, lib: load_or_warn(), run: Arc::new(AtomicBool::new(false)), thread: None }
    }
}

impl Output for AlsaOut {
    fn start(&mut self, rate: u32, channels: u8) -> io::Result<()> {
        self.stop();
        let lib = self.lib.clone().ok_or_else(|| io::Error::other("no libasound"))?;
        let h = open(&lib, STREAM_PLAYBACK, rate, channels)?;
        self.run.store(true, Ordering::Release);
        let (ring, run) = (self.ring.clone(), self.run.clone());
        let frame = 2 * channels as usize;
        self.thread = Some(std::thread::spawn(move || {
            let h = h;
            let mut buf = vec![0u8; CHUNK_FRAMES * frame];
            while run.load(Ordering::Acquire) {
                ring.lock().unwrap_or_else(|e| e.into_inner()).pull(&mut buf);
                // SAFETY: valid pcm owned by this thread; buf holds
                // CHUNK_FRAMES whole frames.
                let n = unsafe { (lib.writei)(h.0, buf.as_ptr().cast(), CHUNK_FRAMES as c_long) };
                // SAFETY: as above; recover from underrun / suspend.
                if n < 0 && unsafe { (lib.recover)(h.0, n as c_int, 1) } < 0 {
                    break;
                }
            }
            close(&lib, h);
        }));
        Ok(())
    }

    fn stop(&mut self) {
        self.run.store(false, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AlsaOut {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Records from the "default" capture device.
pub struct AlsaIn {
    lib: Option<Arc<Lib>>,
    run: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AlsaIn {
    pub fn new() -> Self {
        AlsaIn { lib: load_or_warn(), run: Arc::new(AtomicBool::new(false)), thread: None }
    }
}

impl Input for AlsaIn {
    fn start(&mut self, rate: u32, channels: u8, out: CaptureOut) -> io::Result<()> {
        self.stop();
        let lib = self.lib.clone().ok_or_else(|| io::Error::other("no libasound"))?;
        let h = open(&lib, STREAM_CAPTURE, rate, channels)?;
        self.run.store(true, Ordering::Release);
        let run = self.run.clone();
        let frame = 2 * channels as usize;
        self.thread = Some(std::thread::spawn(move || {
            let h = h;
            let mut buf = vec![0u8; CHUNK_FRAMES * frame];
            while run.load(Ordering::Acquire) {
                // SAFETY: valid pcm owned by this thread; buf has room for
                // CHUNK_FRAMES frames. Blocks for ~10 ms of audio.
                let n = unsafe { (lib.readi)(h.0, buf.as_mut_ptr().cast(), CHUNK_FRAMES as c_long) };
                if n > 0 {
                    out.send(&buf[..(n as usize).min(CHUNK_FRAMES) * frame]);
                // SAFETY: as above; recover from overrun / suspend.
                } else if n < 0 && unsafe { (lib.recover)(h.0, n as c_int, 1) } < 0 {
                    break;
                }
            }
            close(&lib, h);
        }));
        Ok(())
    }

    fn stop(&mut self) {
        self.run.store(false, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AlsaIn {
    fn drop(&mut self) {
        self.stop();
    }
}
