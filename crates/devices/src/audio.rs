//! What a sound card (virtio-snd, HD Audio) talks to on the host side.
//! The VMM implements both with its audio helper process.

/// Where guest audio goes. Must not block: drop data rather than stall.
pub trait AudioSink: Send {
    fn start(&mut self, rate: u32, channels: u8);
    /// Interleaved signed 16-bit little-endian frames.
    fn write(&mut self, pcm: &[u8]);
    fn stop(&mut self);
}

/// Where recorded audio comes from. Must not block. Whether anything real
/// flows (microphone allowed, window open) is the implementation's call:
/// the device just gets silence otherwise.
pub trait AudioSource: Send {
    /// The guest started recording.
    fn start(&mut self, rate: u32, channels: u8);
    /// Fill `out` (whole S16LE frames) with what was recorded, silence
    /// where there is nothing.
    fn read(&mut self, out: &mut [u8]);
    /// The guest stopped recording (or the VM paused).
    fn stop(&mut self);
}

/// Plays nothing.
#[derive(Debug, Default)]
pub struct NullSink;

impl AudioSink for NullSink {
    fn start(&mut self, _rate: u32, _channels: u8) {}
    fn write(&mut self, _pcm: &[u8]) {}
    fn stop(&mut self) {}
}

/// Records silence (no microphone).
#[derive(Debug, Default)]
pub struct Silence;

impl AudioSource for Silence {
    fn start(&mut self, _rate: u32, _channels: u8) {}
    fn read(&mut self, out: &mut [u8]) {
        out.fill(0);
    }
    fn stop(&mut self) {}
}
