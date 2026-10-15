//! Playback and recording through AudioQueues (AudioToolbox). The output
//! callback refills each buffer from the shared ring, with silence when it
//! runs dry; the input callback sends what was recorded to the VMM.
//!
//! Recording needs the microphone permission (TCC). macOS asks the
//! "responsible" app, the one that started us: Terminal (or iTerm...) for
//! `vroom run`, vroom-gui when it runs as an app. If that app was refused,
//! the queue records silence.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::sync::{Arc, Mutex};

use super::{CaptureOut, Input, Output, Ring};

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioQueueBuffer {
    capacity: u32,
    data: *mut c_void,
    byte_size: u32,
    user_data: *mut c_void,
    packet_desc_capacity: u32,
    packet_descs: *mut c_void,
    packet_desc_count: u32,
}

type AudioQueueRef = *mut c_void;
type Callback = unsafe extern "C" fn(*mut c_void, AudioQueueRef, *mut AudioQueueBuffer);
type InputCallback = unsafe extern "C" fn(*mut c_void, AudioQueueRef, *mut AudioQueueBuffer, *const c_void, u32, *const c_void);

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioQueueNewOutput(
        format: *const AudioStreamBasicDescription,
        cb: Callback,
        user: *mut c_void,
        run_loop: *const c_void,
        run_loop_mode: *const c_void,
        flags: u32,
        out: *mut AudioQueueRef,
    ) -> i32;
    fn AudioQueueNewInput(
        format: *const AudioStreamBasicDescription,
        cb: InputCallback,
        user: *mut c_void,
        run_loop: *const c_void,
        run_loop_mode: *const c_void,
        flags: u32,
        out: *mut AudioQueueRef,
    ) -> i32;
    fn AudioQueueAllocateBuffer(q: AudioQueueRef, size: u32, out: *mut *mut AudioQueueBuffer) -> i32;
    fn AudioQueueEnqueueBuffer(q: AudioQueueRef, b: *mut AudioQueueBuffer, n: u32, descs: *const c_void) -> i32;
    fn AudioQueueStart(q: AudioQueueRef, at: *const c_void) -> i32;
    fn AudioQueueDispose(q: AudioQueueRef, immediate: u8) -> i32;
}

const LPCM: u32 = u32::from_be_bytes(*b"lpcm");
const SIGNED_PACKED: u32 = 4 | 8;
const BUFFERS: usize = 3;
const BUFFER_MS: u32 = 20;

fn format(rate: u32, channels: u8) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        sample_rate: rate as f64,
        format_id: LPCM,
        format_flags: SIGNED_PACKED,
        bytes_per_packet: 2 * channels as u32,
        frames_per_packet: 1,
        bytes_per_frame: 2 * channels as u32,
        channels_per_frame: channels as u32,
        bits_per_channel: 16,
        reserved: 0,
    }
}

/// The ring, boxed so its address stays put for the callback.
struct Shared(Arc<Mutex<Ring>>);

unsafe extern "C" fn refill(user: *mut c_void, q: AudioQueueRef, b: *mut AudioQueueBuffer) {
    // SAFETY: `user` is the Box<Shared> owned by CoreAudioOut, which
    // disposes the queue (stopping callbacks) before freeing it; `b` is a
    // buffer of this queue with `capacity` bytes at `data`.
    unsafe {
        let shared = &*(user as *const Shared);
        let buf = std::slice::from_raw_parts_mut((*b).data as *mut u8, (*b).capacity as usize);
        shared.0.lock().unwrap_or_else(|e| e.into_inner()).pull(buf);
        (*b).byte_size = (*b).capacity;
        AudioQueueEnqueueBuffer(q, b, 0, std::ptr::null());
    }
}

pub struct CoreAudioOut {
    shared: Box<Shared>,
    queue: Option<AudioQueueRef>,
}

impl CoreAudioOut {
    pub fn new(ring: Arc<Mutex<Ring>>) -> Self {
        CoreAudioOut { shared: Box::new(Shared(ring)), queue: None }
    }
}

fn check(what: &str, st: i32) -> io::Result<()> {
    if st == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!("{what}: OSStatus {st}")))
    }
}

impl Output for CoreAudioOut {
    fn start(&mut self, rate: u32, channels: u8) -> io::Result<()> {
        self.stop();
        let fmt = format(rate, channels);
        let mut q: AudioQueueRef = std::ptr::null_mut();
        let user = &*self.shared as *const Shared as *mut c_void;
        // SAFETY: valid format; callback + user pointer live as long as the
        // queue (see `refill`); NULL run loop = CoreAudio's own thread.
        check("AudioQueueNewOutput", unsafe {
            AudioQueueNewOutput(&fmt, refill, user, std::ptr::null(), std::ptr::null(), 0, &mut q)
        })?;
        self.queue = Some(q);
        let size = rate * BUFFER_MS / 1000 * 2 * channels as u32;
        for _ in 0..BUFFERS {
            let mut b: *mut AudioQueueBuffer = std::ptr::null_mut();
            // SAFETY: q is a live queue; out pointer valid.
            check("AudioQueueAllocateBuffer", unsafe { AudioQueueAllocateBuffer(q, size, &mut b) })?;
            // SAFETY: b is a fresh buffer of q (prime it like a callback).
            unsafe { refill(user, q, b) };
        }
        // SAFETY: live queue; NULL = start now.
        check("AudioQueueStart", unsafe { AudioQueueStart(q, std::ptr::null()) })
    }

    fn stop(&mut self) {
        if let Some(q) = self.queue.take() {
            // SAFETY: live queue; immediate dispose stops callbacks first.
            unsafe { AudioQueueDispose(q, 1) };
        }
    }
}

impl Drop for CoreAudioOut {
    fn drop(&mut self) {
        self.stop();
    }
}

unsafe extern "C" fn recorded(
    user: *mut c_void,
    q: AudioQueueRef,
    b: *mut AudioQueueBuffer,
    _start: *const c_void,
    _packets: u32,
    _descs: *const c_void,
) {
    // SAFETY: `user` is the Box<CaptureOut> owned by CoreAudioIn, which
    // disposes the queue (stopping callbacks) before dropping it; `b` is a
    // buffer of this queue holding `byte_size` (<= capacity) bytes.
    unsafe {
        let out = &*(user as *const CaptureOut);
        let n = (*b).byte_size.min((*b).capacity) as usize;
        let pcm = std::slice::from_raw_parts((*b).data as *const u8, n);
        // LPCM packets are whole frames.
        out.send(pcm);
        AudioQueueEnqueueBuffer(q, b, 0, std::ptr::null());
    }
}

/// A running input queue and the sender its callback uses. Lives on the
/// microphone thread.
struct Recorder {
    queue: Option<(AudioQueueRef, Box<CaptureOut>)>,
}

impl Recorder {
    fn start(&mut self, rate: u32, channels: u8, out: CaptureOut) -> io::Result<()> {
        self.stop();
        let fmt = format(rate, channels);
        let out = Box::new(out);
        let user = &*out as *const CaptureOut as *mut c_void;
        let mut q: AudioQueueRef = std::ptr::null_mut();
        // SAFETY: valid format; `out` (the user pointer) lives in
        // self.queue until the queue is disposed; NULL run loop =
        // CoreAudio's own thread.
        check("AudioQueueNewInput", unsafe {
            AudioQueueNewInput(&fmt, recorded, user, std::ptr::null(), std::ptr::null(), 0, &mut q)
        })?;
        self.queue = Some((q, out));
        let size = rate * BUFFER_MS / 1000 * 2 * channels as u32;
        for _ in 0..BUFFERS {
            let mut b: *mut AudioQueueBuffer = std::ptr::null_mut();
            // SAFETY: q is a live queue; out pointer valid.
            check("AudioQueueAllocateBuffer", unsafe { AudioQueueAllocateBuffer(q, size, &mut b) })?;
            // SAFETY: b is a fresh, empty buffer of q.
            check("AudioQueueEnqueueBuffer", unsafe { AudioQueueEnqueueBuffer(q, b, 0, std::ptr::null()) })?;
        }
        // SAFETY: live queue; NULL = start now. The first time, this is
        // where macOS asks for the microphone permission, and it waits
        // for the answer.
        check("AudioQueueStart", unsafe { AudioQueueStart(q, std::ptr::null()) })
    }

    fn stop(&mut self) {
        if let Some((q, out)) = self.queue.take() {
            // SAFETY: live queue; immediate dispose stops callbacks before
            // `out` (their user pointer) is dropped below.
            unsafe { AudioQueueDispose(q, 1) };
            drop(out);
        }
    }
}

enum Cmd {
    Start(u32, u8, CaptureOut),
    Stop,
}

/// The default input device (the Mac's microphone unless the user picked
/// another one in System Settings). Starting can block for as long as the
/// permission prompt is up, so a thread of its own does it: the helper
/// keeps playing and answering the VMM meanwhile.
#[derive(Default)]
pub struct CoreAudioIn {
    tx: Option<std::sync::mpsc::Sender<Cmd>>,
}

impl CoreAudioIn {
    fn send(&mut self, c: Cmd) -> io::Result<()> {
        if self.tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
            std::thread::Builder::new().name("mic".into()).spawn(move || {
                let mut r = Recorder { queue: None };
                // Ends when the helper drops its sender.
                while let Ok(c) = rx.recv() {
                    match c {
                        Cmd::Start(rate, ch, out) => {
                            if let Err(e) = r.start(rate, ch, out) {
                                log::warn!("audio: microphone: {e}");
                                r.stop();
                            }
                        }
                        Cmd::Stop => r.stop(),
                    }
                }
                r.stop();
            })?;
            self.tx = Some(tx);
        }
        match &self.tx {
            Some(t) => t.send(c).map_err(|_| io::Error::other("microphone thread gone")),
            None => Ok(()),
        }
    }
}

impl Input for CoreAudioIn {
    fn start(&mut self, rate: u32, channels: u8, out: CaptureOut) -> io::Result<()> {
        self.send(Cmd::Start(rate, channels, out))
    }

    fn stop(&mut self) {
        if self.tx.is_some() {
            let _ = self.send(Cmd::Stop);
        }
    }
}
