//! Guest sound: a helper process (`vroom audio-out`) plays what the sound
//! card sends it and, if the VM may use the microphone, records for it.
//! Started before the VMM's own sandbox, in its own (CoreAudio only)
//! sandbox on macOS. Whether it may record at all is fixed when it starts
//! (`--input`, and the microphone in its sandbox profile).
//!
//! The microphone gate is here: the helper is asked to record only while
//! the guest records, the VM runs (sound cards stop their source when
//! paused) and a VM window is open. The window is told whenever the
//! microphone goes on or off, for its indicator.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vroom_audio::Msg;
use vroom_devices::audio::{AudioSink, AudioSource};
use vroom_display::ToViewer;

use crate::display::ViewerTx;
use crate::{Error, Result};

/// Recorded audio buffered before the guest gets any (absorbs the host's
/// delivery jitter), and the most kept (older audio is dropped).
const MIC_PRIME_MS: u64 = 40;
const MIC_MAX_MS: u64 = 250;

#[derive(Default)]
struct Mic {
    window: bool,
    /// The guest records in this format.
    want: Option<(u32, u8)>,
    /// The helper was asked to record (in `want`'s format, generation `gen`).
    on: bool,
    gen: u32,
    buf: VecDeque<u8>,
    primed: bool,
    viewer: Option<ViewerTx>,
}

pub struct AudioHelper {
    sock: UnixDatagram,
    child: Mutex<Child>,
    input: bool,
    mic: Mutex<Mic>,
}

impl std::fmt::Debug for AudioHelper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioHelper").field("input", &self.input).finish_non_exhaustive()
    }
}

impl Drop for AudioHelper {
    fn drop(&mut self) {
        // Inside the sandbox we may not signal other processes: tell the
        // helper by closing its socket (it also watches for us dying),
        // and never block on it.
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
        let mut c = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = c.kill();
        for _ in 0..50 {
            if !matches!(c.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        log::warn!("audio: helper {} did not exit", c.id());
    }
}

impl AudioHelper {
    /// `wav_dir`: test mode, write `<dir>/audio.wav` instead of playing,
    /// and record `<dir>/mic.wav` (looped) instead of the microphone.
    /// `input`: the VM may use the microphone.
    pub fn spawn(wav_dir: Option<&std::path::Path>, input: bool) -> Result<Arc<AudioHelper>> {
        let exe = std::env::current_exe().map_err(|e| Error::Setup(format!("audio: {e}")))?;
        let (ours, theirs) = vroom_audio::pair()?;
        let mut cmd = command(&exe, wav_dir, input)?;
        cmd.arg("audio-out");
        if input {
            cmd.arg("--input");
        }
        if let Some(d) = wav_dir {
            cmd.env("VROOM_AUDIO_WAV", d.join("audio.wav"));
            if input && d.join("mic.wav").is_file() {
                cmd.env("VROOM_AUDIO_WAV_IN", d.join("mic.wav"));
            }
        }
        let child = cmd
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| Error::Setup(format!("audio helper: {e}")))?;
        ours.set_nonblocking(true)?;
        log::info!("audio: helper {}{}", child.id(), if input { " (microphone allowed)" } else { "" });
        crate::note_helper(child.id());
        Ok(Arc::new(AudioHelper { sock: ours, child: Mutex::new(child), input, mic: Mutex::default() }))
    }

    fn send_ctl(&self, m: &Msg) {
        // Control messages are tiny; give a full socket a moment.
        let t0 = Instant::now();
        while let Err(e) = self.sock.send(&m.encode()) {
            if e.kind() != ErrorKind::WouldBlock || t0.elapsed() > Duration::from_millis(50) {
                log::debug!("audio: {e}");
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn mic(&self) -> std::sync::MutexGuard<'_, Mic> {
        self.mic.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Where "microphone in use" goes.
    pub fn set_viewer(&self, tx: ViewerTx) {
        self.mic().viewer = Some(tx);
    }

    /// A VM window is open (true) or gone (false).
    pub fn set_window(&self, open: bool) {
        let mut m = self.mic();
        m.window = open;
        self.update(&mut m);
    }

    /// Ask the helper to record, or stop, to match the gate.
    fn update(&self, m: &mut Mic) {
        let should = self.input && m.window && m.want.is_some();
        if m.on && !should {
            self.send_ctl(&Msg::InStop);
            m.on = false;
            m.buf.clear();
            log::info!("audio: microphone off");
            if let Some(v) = &m.viewer {
                v.send(&ToViewer::Mic(false));
            }
        } else if !m.on && should {
            let Some((rate, channels)) = m.want else { return };
            m.gen = m.gen.wrapping_add(1);
            m.buf.clear();
            m.primed = false;
            self.send_ctl(&Msg::InStart { gen: m.gen, rate, channels });
            m.on = true;
            log::info!("audio: microphone on ({rate} Hz, {channels} ch)");
            if let Some(v) = &m.viewer {
                v.send(&ToViewer::Mic(true));
            }
        }
    }

    fn capture_start(&self, rate: u32, channels: u8) {
        let mut m = self.mic();
        if m.on && m.want != Some((rate, channels)) {
            // New format: restart.
            m.want = None;
            self.update(&mut m);
        }
        m.want = Some((rate, channels));
        self.update(&mut m);
    }

    fn capture_stop(&self) {
        let mut m = self.mic();
        m.want = None;
        self.update(&mut m);
    }

    /// What the helper recorded so far, as the guest's format. Anything
    /// from an earlier request (another generation) is dropped.
    fn capture_read(&self, out: &mut [u8]) {
        let mut m = self.mic();
        // Sound cards read in their own chunks (HD Audio: a few ms;
        // virtio-snd: a whole period, maybe 100 ms): keep a cushion on
        // top of one read, so reads don't run dry between deliveries.
        let ms = |rate: u32, ch: u8, t: u64| (rate as u64 * t / 1000) as usize * 2 * ch as usize;
        let mut b = [0u8; 4096];
        while let Ok(n) = self.sock.recv(&mut b) {
            let (gen, pcm) = match vroom_audio::read_msg(&mut &b[..n]) {
                Ok(Some(Msg::Captured { gen, pcm })) => (gen, pcm),
                Ok(Some(Msg::MicNote(code))) => {
                    mic_note(&m, code);
                    continue;
                }
                _ => continue,
            };
            let Some((rate, ch)) = m.want.filter(|_| m.on && gen == m.gen) else { continue };
            let frame = 2 * ch as usize;
            m.buf.extend(&pcm[..pcm.len() / frame * frame]);
            let max = ms(rate, ch, MIC_MAX_MS).max(2 * out.len() + ms(rate, ch, MIC_PRIME_MS));
            if m.buf.len() > max {
                let drop = (m.buf.len() - max).next_multiple_of(frame).min(m.buf.len());
                m.buf.drain(..drop);
            }
        }
        out.fill(0);
        let Some((rate, ch)) = m.want.filter(|_| m.on) else { return };
        if !m.primed {
            if m.buf.len() < out.len() + ms(rate, ch, MIC_PRIME_MS) {
                return;
            }
            m.primed = true;
        }
        let n = out.len().min(m.buf.len());
        for (o, v) in out.iter_mut().zip(m.buf.drain(..n)) {
            *o = v;
        }
        if m.buf.is_empty() {
            // Ran dry: build the cushion up again.
            m.primed = false;
        }
    }
}

/// The helper says the microphone looks blocked: say what to do, in
/// the log and in the VM window.
fn mic_note(m: &Mic, code: u8) {
    #[cfg(target_os = "macos")]
    let fix = "Allow the app that started vroom (Terminal, iTerm...) in System Settings > Privacy & Security > Microphone.";
    #[cfg(not(target_os = "macos"))]
    let fix = "Check the host's default capture device (ALSA).";
    let what = if code == vroom_audio::MIC_NO_DATA { "no sound arrives" } else { "only silence so far" };
    log::warn!("audio: microphone: {what}. {fix}");
    if let Some(v) = &m.viewer {
        v.send(&ToViewer::Status(format!("Microphone: {what}. {fix}")));
    }
}

fn command(exe: &std::path::Path, wav_dir: Option<&std::path::Path>, input: bool) -> Result<Command> {
    #[cfg(target_os = "macos")]
    {
        let prof =
            vroom_sandbox::audio_profile(exe, wav_dir, input).map_err(|e| Error::Setup(format!("audio sandbox: {e}")))?;
        let mut c = Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg(prof).arg(exe);
        Ok(c)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (wav_dir, input);
        Ok(Command::new(exe))
    }
}

/// The sound card's view of the helper. Never blocks: audio is dropped
/// rather than stall the device.
pub struct HelperSink(pub Arc<AudioHelper>);

impl AudioSink for HelperSink {
    fn start(&mut self, rate: u32, channels: u8) {
        self.0.send_ctl(&Msg::Start { rate, channels });
    }
    fn write(&mut self, pcm: &[u8]) {
        if !vroom_audio::send_pcm(&self.0.sock, pcm) {
            log::trace!("audio: helper behind, dropped some audio");
        }
    }
    fn stop(&mut self) {
        self.0.send_ctl(&Msg::Stop);
    }
}

/// The microphone as a sound card sees it: silence unless the gate is open.
pub struct HelperSource(pub Arc<AudioHelper>);

impl AudioSource for HelperSource {
    fn start(&mut self, rate: u32, channels: u8) {
        self.0.capture_start(rate, channels);
    }
    fn read(&mut self, out: &mut [u8]) {
        self.0.capture_read(out);
    }
    fn stop(&mut self) {
        self.0.capture_stop();
    }
}
