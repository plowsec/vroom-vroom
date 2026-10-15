//! Guest audio, out of the VMM.
//!
//! The VMM's virtio-snd device sends PCM over a socket to a helper process
//! (`bulwark audio-out`), which plays it. Audio frameworks (CoreAudio and
//! the plug-ins it loads) thus never run inside the VMM, and the helper
//! runs in its own sandbox. Output only: there is no way to ask for input.
//!
//! Transport: a Unix datagram socketpair, one message per datagram
//! (`[kind u8][len u32 le][payload]`), so a sender that never blocks (the
//! VMM drops audio rather than stall) can't tear a message in half.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const K_START: u8 = 1;
const K_PCM: u8 = 2;
const K_STOP: u8 = 3;
/// Largest payload accepted.
pub const MAX_MSG: usize = 1 << 20;
/// PCM bytes per datagram: macOS caps Unix datagrams at 2048 bytes.
pub const PCM_CHUNK: usize = 2040;
const SOCK_BUF: libc::c_int = 512 << 10;

/// Connected datagram pair with room for ~2.5 s of 48 kHz stereo.
#[allow(unsafe_code)]
pub fn pair() -> io::Result<(UnixDatagram, UnixDatagram)> {
    let (a, b) = UnixDatagram::pair()?;
    for s in [&a, &b] {
        for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            // SAFETY: valid socket; option value is a c_int we own.
            unsafe {
                libc::setsockopt(s.as_raw_fd(), libc::SOL_SOCKET, opt, (&SOCK_BUF as *const libc::c_int).cast(), 4);
            }
        }
    }
    Ok((a, b))
}

/// Send PCM in datagram-sized pieces (whole frames). Never blocks if the
/// socket is non-blocking; returns false if some audio was dropped.
pub fn send_pcm(s: &UnixDatagram, pcm: &[u8]) -> bool {
    pcm.chunks(PCM_CHUNK).all(|c| s.send(&Msg::Pcm(c.to_vec()).encode()).is_ok())
}
/// What the helper keeps buffered at most; older audio is dropped.
const MAX_BUFFERED_SECS: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Start { rate: u32, channels: u8 },
    Pcm(Vec<u8>),
    Stop,
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        let (k, p) = match self {
            Msg::Start { rate, channels } => {
                let mut p = rate.to_le_bytes().to_vec();
                p.push(*channels);
                (K_START, p)
            }
            Msg::Pcm(d) => (K_PCM, d.clone()),
            Msg::Stop => (K_STOP, Vec::new()),
        };
        let mut v = vec![k];
        v.extend_from_slice(&(p.len() as u32).to_le_bytes());
        v.extend_from_slice(&p);
        v
    }
}

/// Read one message. None at end of stream.
pub fn read_msg(r: &mut impl Read) -> io::Result<Option<Msg>> {
    let mut h = [0u8; 5];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes([h[1], h[2], h[3], h[4]]) as usize;
    if len > MAX_MSG {
        return Err(io::Error::other("audio message too large"));
    }
    let mut p = vec![0u8; len];
    r.read_exact(&mut p)?;
    Ok(Some(match h[0] {
        K_START if len == 5 => {
            let rate = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
            if !(8000..=192_000).contains(&rate) || !(1..=2).contains(&p[4]) {
                return Err(io::Error::other("bad audio format"));
            }
            Msg::Start { rate, channels: p[4] }
        }
        K_PCM => Msg::Pcm(p),
        K_STOP => Msg::Stop,
        _ => return Err(io::Error::other("bad audio message")),
    }))
}

/// Buffered S16LE samples shared with the playback callback.
#[derive(Debug, Default)]
pub struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
}

impl Ring {
    pub fn set_format(&mut self, rate: u32, channels: u8) {
        self.buf.clear();
        self.cap = (rate * MAX_BUFFERED_SECS) as usize * 2 * channels as usize;
    }
    pub fn push(&mut self, pcm: &[u8]) {
        self.buf.extend(pcm);
        // Clock drift / a stalled player: keep the newest audio (whole
        // frames: cap and drops are multiples of 4 bytes).
        if self.buf.len() > self.cap {
            let excess = ((self.buf.len() - self.cap) + 3) & !3;
            self.buf.drain(..excess.min(self.buf.len()));
        }
    }
    /// Fill `out`; silence where nothing is buffered.
    pub fn pull(&mut self, out: &mut [u8]) {
        let n = out.len().min(self.buf.len());
        for (o, b) in out.iter_mut().zip(self.buf.drain(..n)) {
            *o = b;
        }
        out[n..].fill(0);
    }
}

/// Plays what the helper receives.
pub trait Output {
    fn start(&mut self, rate: u32, channels: u8) -> io::Result<()>;
    fn stop(&mut self);
    /// New samples are in the ring.
    fn fed(&mut self) {}
}

/// Test output: a WAV file per stream start (overwritten).
#[derive(Debug)]
pub struct WavOut {
    path: PathBuf,
    ring: Arc<Mutex<Ring>>,
    file: Option<(std::fs::File, u32, u8)>,
}

impl WavOut {
    pub fn new(path: PathBuf, ring: Arc<Mutex<Ring>>) -> Self {
        WavOut { path, ring, file: None }
    }

    /// Move what's buffered into the file.
    pub fn drain(&mut self) -> io::Result<()> {
        if let Some((f, _, _)) = self.file.as_mut() {
            let data: Vec<u8> = self.ring.lock().unwrap_or_else(|e| e.into_inner()).buf.drain(..).collect();
            f.write_all(&data)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        self.drain()?;
        if let Some((mut f, rate, ch)) = self.file.take() {
            use std::io::Seek;
            let len = f.stream_position()? as u32 - 44;
            f.seek(io::SeekFrom::Start(0))?;
            f.write_all(&wav_header(rate, ch, len))?;
        }
        Ok(())
    }
}

pub fn wav_header(rate: u32, ch: u8, data_len: u32) -> Vec<u8> {
    let ch = ch as u32;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + data_len).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&(ch as u16).to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(rate * ch * 2).to_le_bytes());
    h.extend_from_slice(&((ch * 2) as u16).to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_len.to_le_bytes());
    h
}

impl Output for WavOut {
    fn start(&mut self, rate: u32, channels: u8) -> io::Result<()> {
        self.finish()?;
        let mut f = std::fs::File::create(&self.path)?;
        f.write_all(&wav_header(rate, channels, 0))?;
        self.file = Some((f, rate, channels));
        Ok(())
    }
    fn stop(&mut self) {
        let _ = self.finish();
    }
    fn fed(&mut self) {
        let _ = self.drain();
    }
}

impl Drop for WavOut {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// The helper: receive messages until the VMM goes away, play them.
pub fn helper_main(sock: UnixDatagram, wav: Option<PathBuf>) -> io::Result<()> {
    let ring = Arc::new(Mutex::new(Ring::default()));
    let mut wav_out = wav.map(|p| WavOut::new(p, ring.clone()));
    #[cfg(target_os = "macos")]
    let mut ca = coreaudio::CoreAudioOut::new(ring.clone());
    let out: &mut dyn Output = match wav_out.as_mut() {
        Some(w) => w,
        #[cfg(target_os = "macos")]
        None => &mut ca,
        #[cfg(not(target_os = "macos"))]
        None => return Err(io::Error::other("no audio output on this host yet")),
    };
    let mut buf = vec![0u8; 4096];
    loop {
        let n = match sock.recv(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let Some(m) = read_msg(&mut &buf[..n])? else { break };
        match m {
            Msg::Start { rate, channels } => {
                ring.lock().unwrap_or_else(|e| e.into_inner()).set_format(rate, channels);
                if let Err(e) = out.start(rate, channels) {
                    log::warn!("audio: {e}");
                }
            }
            Msg::Pcm(d) => {
                ring.lock().unwrap_or_else(|e| e.into_inner()).push(&d);
                out.fed();
            }
            Msg::Stop => out.stop(),
        }
    }
    out.stop();
    Ok(())
}

#[cfg(target_os = "macos")]
mod coreaudio;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_bad_ones_fail() {
        let mut v = Vec::new();
        for m in [Msg::Start { rate: 48000, channels: 2 }, Msg::Pcm(vec![1, 2, 3, 4]), Msg::Stop] {
            v.extend(m.encode());
        }
        let mut r = &v[..];
        assert_eq!(read_msg(&mut r).unwrap(), Some(Msg::Start { rate: 48000, channels: 2 }));
        assert_eq!(read_msg(&mut r).unwrap(), Some(Msg::Pcm(vec![1, 2, 3, 4])));
        assert_eq!(read_msg(&mut r).unwrap(), Some(Msg::Stop));
        assert_eq!(read_msg(&mut r).unwrap(), None);
        let mut big = vec![K_PCM];
        big.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_msg(&mut &big[..]).is_err());
        let bad = Msg::Start { rate: 7, channels: 9 }.encode();
        assert!(read_msg(&mut &bad[..]).is_err());
    }

    #[test]
    fn ring_keeps_newest_and_pads_with_silence() {
        let mut r = Ring::default();
        r.set_format(8000, 1); // 16000 bytes max
        r.push(&vec![1u8; 20000]);
        assert!(r.buf.len() <= 16000);
        let mut out = vec![9u8; 20000];
        r.pull(&mut out);
        assert!(out[..16000].iter().all(|&b| b == 1));
        assert!(out[16000..].iter().all(|&b| b == 0));
    }

    #[test]
    fn helper_writes_a_wav() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("out.wav");
        let (vmm, helper) = pair().unwrap();
        vmm.send(&Msg::Start { rate: 48000, channels: 2 }.encode()).unwrap();
        assert!(send_pcm(&vmm, &[7u8; 4000]));
        vmm.send(&Msg::Stop.encode()).unwrap();
        drop(vmm);
        helper_main(helper, Some(p.clone())).unwrap();
        let w = std::fs::read(&p).unwrap();
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 4000);
        assert!(w[44..].iter().all(|&b| b == 7));
    }
}
