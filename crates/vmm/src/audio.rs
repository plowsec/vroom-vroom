//! Guest audio out: a helper process (`bulwark audio-out`) plays what the
//! virtio-snd device sends it. Started before the VMM's own sandbox, in
//! its own (CoreAudio only) sandbox on macOS.

use std::io::ErrorKind;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bulwark_audio::Msg;
use bulwark_virtio::devices::sound::AudioSink;

use crate::{Error, Result};

#[derive(Debug)]
pub struct AudioHelper {
    sock: UnixDatagram,
    child: std::sync::Mutex<Child>,
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
    /// `wav`: test mode, write `<dir>/audio.wav` instead of playing.
    pub fn spawn(wav_dir: Option<&std::path::Path>) -> Result<Arc<AudioHelper>> {
        let exe = std::env::current_exe().map_err(|e| Error::Setup(format!("audio: {e}")))?;
        let (ours, theirs) = bulwark_audio::pair()?;
        let mut cmd = command(&exe, wav_dir)?;
        cmd.arg("audio-out");
        if let Some(d) = wav_dir {
            cmd.env("BULWARK_AUDIO_WAV", d.join("audio.wav"));
        }
        let child = cmd
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| Error::Setup(format!("audio helper: {e}")))?;
        ours.set_nonblocking(true)?;
        log::info!("audio: helper {}", child.id());
        Ok(Arc::new(AudioHelper { sock: ours, child: std::sync::Mutex::new(child) }))
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
}

fn command(exe: &std::path::Path, wav_dir: Option<&std::path::Path>) -> Result<Command> {
    #[cfg(target_os = "macos")]
    {
        let prof = bulwark_sandbox::audio_profile(exe, wav_dir).map_err(|e| Error::Setup(format!("audio sandbox: {e}")))?;
        let mut c = Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg(prof).arg(exe);
        Ok(c)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = wav_dir;
        Ok(Command::new(exe))
    }
}

/// The device's view of the helper. Never blocks: audio is dropped rather
/// than stall the device worker.
pub struct HelperSink(pub Arc<AudioHelper>);

impl AudioSink for HelperSink {
    fn start(&mut self, rate: u32, channels: u8) {
        self.0.send_ctl(&Msg::Start { rate, channels });
    }
    fn write(&mut self, pcm: &[u8]) {
        if !bulwark_audio::send_pcm(&self.0.sock, pcm) {
            log::trace!("audio: helper behind, dropped some audio");
        }
    }
    fn stop(&mut self) {
        self.0.send_ctl(&Msg::Stop);
    }
}
