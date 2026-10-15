//! Guest audio, out of the VMM.
//!
//! The VMM's sound card (virtio-snd or HD Audio) sends PCM over a socket
//! to a helper process (`vroom audio-out`), which plays it. Audio
//! frameworks (CoreAudio and the plug-ins it loads) thus never run inside
//! the VMM, and the helper runs in its own sandbox.
//!
//! The microphone goes the other way, and only if the helper was started
//! with input allowed (`[audio] input`, fixed before the guest runs): the
//! VMM asks with `InStart` while the guest records, the helper sends
//! `Captured` PCM back until `InStop`. A helper started without input
//! never opens a capture device, whatever the VMM asks.
//!
//! Transport: a Unix datagram socketpair, one message per datagram
//! (`[kind u8][len u32 le][payload]`), so a sender that never blocks (the
//! VMM drops audio rather than stall) can't tear a message in half.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const K_START: u8 = 1;
const K_PCM: u8 = 2;
const K_STOP: u8 = 3;
const K_IN_START: u8 = 4;
const K_IN_STOP: u8 = 5;
const K_CAPTURED: u8 = 6;
/// Largest payload accepted.
pub const MAX_MSG: usize = 1 << 20;
/// PCM bytes per datagram: macOS caps Unix datagrams at 2048 bytes.
pub const PCM_CHUNK: usize = 2040;
/// Captured PCM bytes per datagram (the generation number takes 4).
pub const CAPTURE_CHUNK: usize = 2032;
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

/// Send without blocking, whatever the socket's mode (the helper shares
/// its socket between a blocking reader and capture threads).
#[allow(unsafe_code)]
fn send_nowait(s: &UnixDatagram, b: &[u8]) -> bool {
    // SAFETY: valid socket; pointer and length describe `b`.
    let n = unsafe { libc::send(s.as_raw_fd(), b.as_ptr().cast(), b.len(), libc::MSG_DONTWAIT) };
    n == b.len() as isize
}

/// What the helper keeps buffered at most; older audio is dropped.
const MAX_BUFFERED_SECS: u32 = 1;

/// 8 to 192 kHz, mono or stereo: all we ever ask a host device for.
pub fn format_ok(rate: u32, channels: u8) -> bool {
    (8000..=192_000).contains(&rate) && (1..=2).contains(&channels)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Start { rate: u32, channels: u8 },
    Pcm(Vec<u8>),
    Stop,
    /// Start recording. `gen` tags the audio that comes back, so the VMM
    /// can drop what was recorded for an earlier request.
    InStart { gen: u32, rate: u32, channels: u8 },
    InStop,
    /// Helper to VMM: recorded S16LE frames.
    Captured { gen: u32, pcm: Vec<u8> },
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
            Msg::InStart { gen, rate, channels } => {
                let mut p = gen.to_le_bytes().to_vec();
                p.extend_from_slice(&rate.to_le_bytes());
                p.push(*channels);
                (K_IN_START, p)
            }
            Msg::InStop => (K_IN_STOP, Vec::new()),
            Msg::Captured { gen, pcm } => {
                let mut p = gen.to_le_bytes().to_vec();
                p.extend_from_slice(pcm);
                (K_CAPTURED, p)
            }
        };
        let mut v = vec![k];
        v.extend_from_slice(&(p.len() as u32).to_le_bytes());
        v.extend_from_slice(&p);
        v
    }
}

fn le32(p: &[u8]) -> u32 {
    u32::from_le_bytes([p[0], p[1], p[2], p[3]])
}

/// Read one message. None at end of stream.
pub fn read_msg(r: &mut impl Read) -> io::Result<Option<Msg>> {
    let mut h = [0u8; 5];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = le32(&h[1..]) as usize;
    if len > MAX_MSG {
        return Err(io::Error::other("audio message too large"));
    }
    let mut p = vec![0u8; len];
    r.read_exact(&mut p)?;
    let bad_format = || io::Error::other("bad audio format");
    Ok(Some(match h[0] {
        K_START if len == 5 => {
            let rate = le32(&p);
            if !format_ok(rate, p[4]) {
                return Err(bad_format());
            }
            Msg::Start { rate, channels: p[4] }
        }
        K_PCM => Msg::Pcm(p),
        K_STOP => Msg::Stop,
        K_IN_START if len == 9 => {
            let (gen, rate) = (le32(&p), le32(&p[4..]));
            if !format_ok(rate, p[8]) {
                return Err(bad_format());
            }
            Msg::InStart { gen, rate, channels: p[8] }
        }
        K_IN_STOP => Msg::InStop,
        K_CAPTURED if len >= 4 => Msg::Captured { gen: le32(&p), pcm: p[4..].to_vec() },
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

/// Where recorded audio goes: the VMM's end of the socket. Never blocks
/// (a VMM that doesn't read loses audio, the recorder doesn't stall).
#[derive(Debug, Clone)]
pub struct CaptureOut {
    sock: Arc<UnixDatagram>,
    gen: u32,
}

impl CaptureOut {
    pub fn new(sock: Arc<UnixDatagram>, gen: u32) -> Self {
        CaptureOut { sock, gen }
    }

    /// Send whole S16LE frames. False if some were dropped.
    pub fn send(&self, pcm: &[u8]) -> bool {
        pcm.chunks(CAPTURE_CHUNK).all(|c| send_nowait(&self.sock, &Msg::Captured { gen: self.gen, pcm: c.to_vec() }.encode()))
    }
}

/// Records for the VMM.
pub trait Input {
    fn start(&mut self, rate: u32, channels: u8, out: CaptureOut) -> io::Result<()>;
    fn stop(&mut self);
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

/// A 16-bit PCM WAV file: its rate and its first channel.
pub fn read_wav(b: &[u8]) -> io::Result<(u32, Vec<i16>)> {
    let bad = |m: &str| io::Error::other(format!("wav: {m}"));
    if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(bad("not a RIFF/WAVE file"));
    }
    let (mut at, mut fmt, mut data) = (12usize, None, None);
    while at + 8 <= b.len() {
        let len = le32(&b[at + 4..]) as usize;
        let body = b.get(at + 8..at + 8 + len).unwrap_or(&b[at + 8..]);
        match &b[at..at + 4] {
            b"fmt " if body.len() >= 16 => fmt = Some(body),
            b"data" => data = Some(body),
            _ => {}
        }
        at = at.saturating_add(8).saturating_add(len).saturating_add(len & 1);
    }
    let (fmt, data) = (fmt.ok_or_else(|| bad("no fmt chunk"))?, data.ok_or_else(|| bad("no data chunk"))?);
    let (kind, ch, rate, bits) = (u16::from_le_bytes([fmt[0], fmt[1]]), u16::from_le_bytes([fmt[2], fmt[3]]), le32(&fmt[4..]), u16::from_le_bytes([fmt[14], fmt[15]]));
    if kind != 1 || bits != 16 || ch == 0 || !(8000..=192_000).contains(&rate) {
        return Err(bad("want 16-bit PCM"));
    }
    let frame = 2 * ch as usize;
    let samples: Vec<i16> = data.chunks_exact(frame).map(|f| i16::from_le_bytes([f[0], f[1]])).collect();
    if samples.is_empty() {
        return Err(bad("empty"));
    }
    Ok((rate, samples))
}

/// Test input: loops a WAV file at the requested rate, in real time.
#[derive(Debug)]
pub struct WavIn {
    rate: u32,
    samples: Arc<Vec<i16>>,
    run: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WavIn {
    pub fn open(p: &Path) -> io::Result<WavIn> {
        if std::fs::metadata(p)?.len() > 64 << 20 {
            return Err(io::Error::other("wav: too large"));
        }
        let (rate, samples) = read_wav(&std::fs::read(p)?)?;
        Ok(WavIn { rate, samples: Arc::new(samples), run: Arc::new(AtomicBool::new(false)), thread: None })
    }
}

impl Input for WavIn {
    fn start(&mut self, rate: u32, channels: u8, out: CaptureOut) -> io::Result<()> {
        self.stop();
        self.run.store(true, Ordering::Release);
        let (run, samples, src_rate) = (self.run.clone(), self.samples.clone(), self.rate as u64);
        self.thread = Some(std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut sent = 0u64;
            while run.load(Ordering::Acquire) {
                let due = t0.elapsed().as_micros() as u64 * rate as u64 / 1_000_000;
                let mut pcm = Vec::with_capacity((due - sent) as usize * 2 * channels as usize);
                for f in sent..due {
                    // Nearest sample: a tone keeps its pitch at any rate.
                    let s = samples[(f * src_rate / rate as u64 % samples.len() as u64) as usize].to_le_bytes();
                    for _ in 0..channels {
                        pcm.extend_from_slice(&s);
                    }
                }
                out.send(&pcm);
                sent = due;
                std::thread::sleep(Duration::from_millis(10));
            }
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

impl Drop for WavIn {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How the helper was started (fixed before the guest runs).
#[derive(Debug, Clone, Default)]
pub struct HelperOpts {
    /// Test mode: write what the guest plays here instead of playing it.
    pub wav_out: Option<PathBuf>,
    /// Test mode: what the "microphone" hears, instead of the real one.
    pub wav_in: Option<PathBuf>,
    /// The VM may use the microphone at all (`[audio] input`).
    pub input: bool,
}

/// The real microphone of this host.
fn host_input() -> Option<Box<dyn Input>> {
    #[cfg(target_os = "macos")]
    return Some(Box::new(coreaudio::CoreAudioIn::default()));
    #[cfg(target_os = "linux")]
    return Some(Box::new(alsa::AlsaIn::new()));
    #[allow(unreachable_code)]
    None
}

/// The helper: receive messages until the VMM goes away, play them, and
/// record when asked (if allowed).
pub fn helper_main(sock: UnixDatagram, opts: HelperOpts) -> io::Result<()> {
    let sock = Arc::new(sock);
    let ring = Arc::new(Mutex::new(Ring::default()));
    let mut wav_out = opts.wav_out.map(|p| WavOut::new(p, ring.clone()));
    #[cfg(target_os = "macos")]
    let mut host = coreaudio::CoreAudioOut::new(ring.clone());
    #[cfg(target_os = "linux")]
    let mut host = alsa::AlsaOut::new(ring.clone());
    let out: &mut dyn Output = match wav_out.as_mut() {
        Some(w) => w,
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        None => &mut host,
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        None => return Err(io::Error::other("no audio output on this host yet")),
    };
    let mut input: Option<Box<dyn Input>> = match (opts.input, opts.wav_in) {
        (false, _) => None,
        (true, Some(p)) => match WavIn::open(&p) {
            Ok(w) => Some(Box::new(w)),
            Err(e) => {
                log::warn!("audio: {}: {e}", p.display());
                None
            }
        },
        (true, None) => host_input(),
    };
    let mut buf = vec![0u8; 4096];
    // A datagram socket doesn't report its peer's death on every system:
    // also notice being orphaned (the VMM killed) and leave.
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;
    let parent = parent_pid();
    loop {
        let n = match sock.recv(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if parent_pid() != parent {
                    break;
                }
                continue;
            }
            Err(_) => break,
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
            Msg::InStart { gen, rate, channels } => match input.as_mut() {
                Some(i) => {
                    i.stop();
                    if let Err(e) = i.start(rate, channels, CaptureOut::new(sock.clone(), gen)) {
                        log::warn!("audio: microphone: {e}");
                    }
                }
                None => log::warn!("audio: the VM asked for the microphone, but input is off"),
            },
            Msg::InStop => {
                if let Some(i) = input.as_mut() {
                    i.stop();
                }
            }
            Msg::Captured { .. } => {}
        }
    }
    if let Some(i) = input.as_mut() {
        i.stop();
    }
    out.stop();
    Ok(())
}

#[allow(unsafe_code)]
fn parent_pid() -> i32 {
    // SAFETY: no preconditions.
    unsafe { libc::getppid() }
}

#[cfg(target_os = "macos")]
mod coreaudio;
#[cfg(target_os = "linux")]
mod alsa;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_bad_ones_fail() {
        let mut v = Vec::new();
        let all = [
            Msg::Start { rate: 48000, channels: 2 },
            Msg::Pcm(vec![1, 2, 3, 4]),
            Msg::Stop,
            Msg::InStart { gen: 7, rate: 44100, channels: 1 },
            Msg::InStop,
            Msg::Captured { gen: 7, pcm: vec![5, 6] },
        ];
        for m in &all {
            v.extend(m.encode());
        }
        let mut r = &v[..];
        for m in all {
            assert_eq!(read_msg(&mut r).unwrap(), Some(m));
        }
        assert_eq!(read_msg(&mut r).unwrap(), None);
        let mut big = vec![K_PCM];
        big.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_msg(&mut &big[..]).is_err());
        let bad = Msg::Start { rate: 7, channels: 9 }.encode();
        assert!(read_msg(&mut &bad[..]).is_err());
        let bad = Msg::InStart { gen: 0, rate: 48000, channels: 3 }.encode();
        assert!(read_msg(&mut &bad[..]).is_err());
        assert!(read_msg(&mut &[K_CAPTURED, 2, 0, 0, 0, 1, 2][..]).is_err());
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
        helper_main(helper, HelperOpts { wav_out: Some(p.clone()), ..Default::default() }).unwrap();
        let w = std::fs::read(&p).unwrap();
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 4000);
        assert!(w[44..].iter().all(|&b| b == 7));
    }

    fn tone_wav(rate: u32) -> Vec<u8> {
        let n = rate as usize / 10;
        let mut w = wav_header(rate, 2, (n * 4) as u32);
        for i in 0..n {
            let s = ((i as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin() * 10000.0) as i16;
            w.extend_from_slice(&s.to_le_bytes());
            w.extend_from_slice(&0i16.to_le_bytes());
        }
        w
    }

    #[test]
    fn wav_input_records_only_when_allowed() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("mic.wav");
        std::fs::write(&p, tone_wav(8000)).unwrap();
        let (rate, s) = read_wav(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!((rate, s.len()), (8000, 800));
        assert!(read_wav(b"RIFF....WAVEjunk").is_err());

        for allowed in [false, true] {
            let (vmm, helper) = pair().unwrap();
            let opts = HelperOpts { wav_in: Some(p.clone()), input: allowed, ..Default::default() };
            let t = std::thread::spawn(move || helper_main(helper, opts));
            vmm.send(&Msg::InStart { gen: 3, rate: 16000, channels: 1 }.encode()).unwrap();
            std::thread::sleep(Duration::from_millis(150));
            vmm.send(&Msg::InStop.encode()).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            vmm.set_nonblocking(true).unwrap();
            let mut got = Vec::new();
            let mut b = [0u8; 4096];
            while let Ok(n) = vmm.recv(&mut b) {
                match read_msg(&mut &b[..n]).unwrap() {
                    Some(Msg::Captured { gen: 3, pcm }) => got.extend(pcm),
                    other => panic!("{other:?}"),
                }
            }
            drop(vmm);
            t.join().unwrap().unwrap();
            if allowed {
                // ~150 ms at 16 kHz mono: ~2400 samples.
                assert!((1500..=4000).contains(&(got.len() / 2)), "{}", got.len());
                assert_eq!(got.len() % 2, 0);
                assert!(got.chunks(2).any(|c| i16::from_le_bytes([c[0], c[1]]).abs() > 5000));
            } else {
                assert!(got.is_empty());
            }
        }
    }
}
