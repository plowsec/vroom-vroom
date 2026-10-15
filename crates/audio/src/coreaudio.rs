//! Playback through an AudioQueue (AudioToolbox). The callback refills
//! each buffer from the shared ring, with silence when it runs dry.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::sync::{Arc, Mutex};

use super::{Output, Ring};

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
    fn AudioQueueAllocateBuffer(q: AudioQueueRef, size: u32, out: *mut *mut AudioQueueBuffer) -> i32;
    fn AudioQueueEnqueueBuffer(q: AudioQueueRef, b: *mut AudioQueueBuffer, n: u32, descs: *const c_void) -> i32;
    fn AudioQueueStart(q: AudioQueueRef, at: *const c_void) -> i32;
    fn AudioQueueDispose(q: AudioQueueRef, immediate: u8) -> i32;
}

const LPCM: u32 = u32::from_be_bytes(*b"lpcm");
const SIGNED_PACKED: u32 = 4 | 8;
const BUFFERS: usize = 3;
const BUFFER_MS: u32 = 20;

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
        let fmt = AudioStreamBasicDescription {
            sample_rate: rate as f64,
            format_id: LPCM,
            format_flags: SIGNED_PACKED,
            bytes_per_packet: 2 * channels as u32,
            frames_per_packet: 1,
            bytes_per_frame: 2 * channels as u32,
            channels_per_frame: channels as u32,
            bits_per_channel: 16,
            reserved: 0,
        };
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
