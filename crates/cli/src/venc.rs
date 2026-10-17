//! `vroom video-helper --encode`: encodes the guest's raw frames with
//! VideoToolbox for the VMM's virtio-media encoder.
//!
//! Started by the VMM like the decoding helper, in a sandbox of its own
//! (VideoToolbox's encoder service, nothing else), with the encoder
//! device's buffer window. Each raw frame is read from the guest buffer
//! the VMM names (inside the window, as big as the session's layout) and
//! copied into one of VideoToolbox's pixel buffers; each coded frame goes
//! into the oldest guest buffer the VMM granted, if it fits. A malformed
//! message ends the helper.
//!
//! `vroom video-helper --encode --file IN --size WxH [--codec h264|hevc]
//! [--bitrate BITS] [--fps N] [--cbr] [--gop N] [--frames N] [--out OUT]`
//! encodes a file of NV12 frames (packed, `W*H*3/2` bytes each) the same
//! way, for tests: frames a second, and how long each frame took from
//! going in to coming out, with `--cbr` in real time as a call would.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vroom_video::proto::{EncConfig, FromHelper, RateControl, ToHelper, ENC_REALTIME, H264_HIGH, HEVC_MAIN, MAX_GRANTS, MAX_HELD};
use vroom_video::{Codec, Layout, PixFmt};

use crate::video::{map_window, Window};

#[cfg(target_os = "macos")]
use crate::vtenc::{caps, Encoder, Wake};

#[cfg(not(target_os = "macos"))]
mod novt {
    //! No VideoToolbox: nothing is encoded.
    use vroom_video::proto::EncConfig;

    pub type Wake = std::sync::Arc<dyn Fn() + Send + Sync>;

    pub struct Coded {
        pub tag: u64,
        pub data: Option<Vec<u8>>,
        pub key: bool,
    }

    pub struct Encoder;

    impl Encoder {
        pub fn new(_: &EncConfig, _: Option<Wake>) -> Option<Encoder> {
            None
        }
        pub fn update(&mut self, _: &EncConfig) -> bool {
            false
        }
        /// # Safety
        /// Never called: there are no encoders.
        pub unsafe fn encode(&mut self, _: *const u8, _: usize, _: u64, _: bool) -> bool {
            false
        }
        pub fn finish(&mut self) {}
        pub fn take(&mut self) -> Vec<Coded> {
            Vec::new()
        }
    }

    pub fn caps() -> u32 {
        0
    }
}
#[cfg(not(target_os = "macos"))]
use novt::{caps, Encoder, Wake};

/// Most raw frames waiting to be read per session (the VMM sends 4).
const MAX_INPUTS: usize = 64;

/// A raw frame the VMM named.
#[derive(Debug, Clone, Copy)]
struct Frame {
    cookie: u32,
    offset: u32,
    len: u32,
    key: bool,
}

#[derive(Debug, Clone, Copy)]
struct Grant {
    offset: u32,
    len: u32,
}

struct Stream {
    config: EncConfig,
    enc: Option<Encoder>,
    inputs: VecDeque<Frame>,
    /// Frames at VideoToolbox: tag -> (cookie, when it went in).
    coding: BTreeMap<u64, (u32, Instant)>,
    /// Coded frames waiting for a guest buffer: (cookie, Annex B, key).
    out: VecDeque<(u32, Option<Vec<u8>>, bool)>,
    grants: VecDeque<Grant>,
    epoch: u32,
    draining: bool,
    dead: bool,
    tag: u64,
    /// Frames in, coded frames out, the slowest and the sum of their
    /// times in VideoToolbox (debug logs).
    stats: (u64, u64, Duration, Duration),
}

impl Stream {
    fn new(config: EncConfig) -> Self {
        Stream {
            config,
            enc: None,
            inputs: VecDeque::new(),
            coding: BTreeMap::new(),
            out: VecDeque::new(),
            grants: VecDeque::new(),
            epoch: 0,
            draining: false,
            dead: false,
            tag: 0,
            stats: (0, 0, Duration::ZERO, Duration::ZERO),
        }
    }

    fn held(&self) -> usize {
        self.coding.len() + self.out.len()
    }

    /// New settings: rates go to the running session; anything else
    /// ends it (its frames still come out) and the next frame starts a
    /// new one, from a key frame.
    fn configure(&mut self, config: EncConfig) {
        if let Some(e) = &mut self.enc {
            if !e.update(&config) {
                self.retire();
            }
        }
        self.config = config;
    }

    /// Take what VideoToolbox has coded.
    fn collect(&mut self) {
        let Some(e) = &mut self.enc else { return };
        for c in e.take() {
            let Some((cookie, t0)) = self.coding.remove(&c.tag) else { continue };
            let dt = t0.elapsed();
            self.stats.1 += 1;
            self.stats.2 = self.stats.2.max(dt);
            self.stats.3 += dt;
            self.out.push_back((cookie, c.data, c.key));
        }
    }

    /// Wait for every frame sent; the ones that never came failed.
    fn finish(&mut self) {
        if let Some(e) = &mut self.enc {
            e.finish();
        }
        self.collect();
        for (_, (cookie, _)) in std::mem::take(&mut self.coding) {
            self.out.push_back((cookie, None, false));
        }
    }

    fn retire(&mut self) {
        self.finish();
        self.enc = None;
    }

    /// Encode one frame from the window. False: it failed (its coded
    /// frame comes back as failed).
    fn encode(&mut self, f: Frame, window: &Window, wake: &Wake) -> bool {
        let need = self.config.layout.size(self.config.format);
        let Some(src) = window.range(f.offset, f.len).filter(|_| f.len >= need) else {
            log::info!("video: a frame outside the window or too small");
            return false;
        };
        if self.enc.is_none() {
            self.enc = Encoder::new(&self.config, Some(wake.clone()));
            if self.enc.is_none() {
                log::info!("video: VideoToolbox won't encode {:?}", self.config);
                self.dead = true;
                return false;
            }
        }
        self.tag += 1;
        let tag = self.tag;
        let Some(e) = &mut self.enc else { return false };
        // In before the call: VideoToolbox may hand the frame out before
        // it returns.
        self.coding.insert(tag, (f.cookie, Instant::now()));
        // SAFETY: `src` is `f.len` bytes inside our mapping of the window
        // (checked by `range`); nothing in this process holds a reference
        // into it.
        #[allow(unsafe_code)]
        let ok = unsafe { e.encode(src, f.len as usize, tag, f.key) };
        if ok {
            self.stats.0 += 1;
        } else {
            self.coding.remove(&tag);
        }
        ok
    }

    /// Drop everything queued, coding or coded (the guest stopped).
    fn drop_all(&mut self) {
        self.inputs.clear();
        if let Some(e) = &mut self.enc {
            e.finish();
            e.take();
        }
        self.coding.clear();
        self.out.clear();
        self.grants.clear();
        self.draining = false;
    }
}

/// Sends to the VMM; a broken socket ends the helper.
struct Link(UnixStream);

impl Link {
    fn send(&mut self, m: FromHelper) {
        if m.write_to(&mut self.0).is_err() {
            std::process::exit(0);
        }
    }
}

enum Msg {
    Vmm(ToHelper),
    /// VideoToolbox coded something.
    Wake,
    /// The VMM is gone.
    Gone,
}

pub fn helper(args: &[String]) -> Result<(), String> {
    if args.iter().any(|a| a == "--file") {
        return encode_file(args);
    }
    // SAFETY: fd 0 is the socket the VMM set up for us; nothing else here
    // uses stdin.
    #[allow(unsafe_code)]
    let sock = unsafe { UnixStream::from_raw_fd(0) };
    let window = map_window(&sock)?;
    let mut rx = sock.try_clone().map_err(|e| e.to_string())?;
    let mut link = Link(sock);
    link.send(FromHelper::Caps { codecs: caps() });
    // The VMM's messages and VideoToolbox's wakeups, in one queue. A
    // wakeup that doesn't fit is no loss: the loop has messages to handle
    // and looks at every encoder after them.
    let (tx, msgs) = mpsc::sync_channel::<Msg>(256);
    let waker = tx.clone();
    let wake: Wake = Arc::new(move || {
        let _ = waker.try_send(Msg::Wake);
    });
    std::thread::Builder::new()
        .name("video-rx".into())
        .spawn(move || {
            while let Ok(m) = ToHelper::read_from(&mut rx) {
                if tx.send(Msg::Vmm(m)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Msg::Gone);
        })
        .map_err(|e| e.to_string())?;
    let mut streams: BTreeMap<u32, Stream> = BTreeMap::new();
    loop {
        let busy = streams.values().any(|s| {
            !s.dead && ((!s.inputs.is_empty() && s.held() < MAX_HELD) || (s.draining && s.coding.is_empty()) || (!s.grants.is_empty() && !s.out.is_empty()))
        });
        let first = if busy { msgs.try_recv().ok() } else { msgs.recv().ok() };
        for m in first.into_iter().chain(std::iter::from_fn(|| msgs.try_recv().ok())) {
            match m {
                Msg::Vmm(m) => handle(&mut streams, &mut link, m),
                Msg::Wake => {}
                Msg::Gone => return Ok(()),
            }
        }
        for (&id, s) in streams.iter_mut() {
            work(id, s, &mut link, &window, &wake);
        }
    }
}

fn handle(streams: &mut BTreeMap<u32, Stream>, link: &mut Link, m: ToHelper) {
    match m {
        ToHelper::Configure { session, config } => match streams.get_mut(&session) {
            Some(s) => s.configure(config),
            None => {
                streams.insert(session, Stream::new(config));
            }
        },
        ToHelper::Encode { session, cookie, offset, len, key } => match streams.get_mut(&session) {
            Some(s) if !s.dead && s.inputs.len() < MAX_INPUTS => s.inputs.push_back(Frame { cookie, offset, len, key }),
            _ => link.send(FromHelper::Consumed { session, cookie, ok: false }),
        },
        ToHelper::Grant { session, epoch, offset, len, .. } => {
            if let Some(s) = streams.get_mut(&session) {
                if epoch != s.epoch {
                    s.epoch = epoch;
                    s.grants.clear();
                }
                if len > 0 && s.grants.len() < MAX_GRANTS {
                    s.grants.push_back(Grant { offset, len });
                }
            }
        }
        ToHelper::Drain { session } => {
            if let Some(s) = streams.get_mut(&session) {
                s.draining = true;
            }
        }
        ToHelper::Flush { session } => {
            if let Some(s) = streams.get_mut(&session) {
                s.drop_all();
            }
        }
        ToHelper::Close { session } => {
            streams.remove(&session);
        }
        // A decoder's messages: not for us.
        ToHelper::Open { .. } | ToHelper::Decode { .. } | ToHelper::Resume { .. } => {}
    }
}

/// Encode what we may, send what we can.
fn work(session: u32, s: &mut Stream, link: &mut Link, window: &Window, wake: &Wake) {
    if s.dead {
        return;
    }
    s.collect();
    while s.held() < MAX_HELD {
        let Some(f) = s.inputs.pop_front() else { break };
        let ok = s.encode(f, window, wake);
        link.send(FromHelper::Consumed { session, cookie: f.cookie, ok });
        if s.dead {
            s.inputs.clear();
            link.send(FromHelper::Error { session, errno: 22 });
            return;
        }
        if !ok {
            s.out.push_back((f.cookie, None, false));
        }
        s.collect();
    }
    if s.draining && s.inputs.is_empty() {
        s.finish();
    }
    while !s.out.is_empty() {
        let Some(g) = s.grants.pop_front() else { break };
        let Some((cookie, data, key)) = s.out.pop_front() else { break };
        let bytes = data.and_then(|d| window.put(g.offset, g.len, &d));
        link.send(FromHelper::Coded { session, epoch: s.epoch, cookie, offset: g.offset, bytes: bytes.unwrap_or(0), key, ok: bytes.is_some() });
    }
    if s.draining && s.inputs.is_empty() && s.coding.is_empty() && s.out.is_empty() {
        s.draining = false;
        let (n, m, worst, sum) = s.stats;
        log::debug!("video: encoder {session}: {n} frames, {m} coded, {:.1} ms each, at most {:.1} ms", sum.as_secs_f64() * 1e3 / m.max(1) as f64, worst.as_secs_f64() * 1e3);
        link.send(FromHelper::DrainDone { session });
    }
}

fn arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(|s| s.as_str())
}

/// Test mode: encode a file of packed NV12 frames.
fn encode_file(args: &[String]) -> Result<(), String> {
    let path = arg(args, "--file").ok_or("--file needs a path")?;
    let (w, h) = arg(args, "--size").and_then(|s| s.split_once('x')).and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?))).ok_or("--size WxH")?;
    let codec = if matches!(arg(args, "--codec"), Some("hevc") | Some("h265")) { Codec::Hevc } else { Codec::H264 };
    let num = |name: &str, d: u32| arg(args, name).and_then(|v| v.parse().ok()).unwrap_or(d);
    let cbr = args.iter().any(|a| a == "--cbr");
    let layout = Layout::for_frame(w, h).ok_or("bad size")?;
    let config = EncConfig {
        codec,
        format: PixFmt::Nv12,
        layout,
        visible: (w, h),
        fps: (num("--fps", 30), 1),
        rc: if cbr { RateControl::Cbr } else { RateControl::Vbr },
        bitrate: num("--bitrate", 4_000_000),
        peak: 0,
        quality: 50,
        gop: num("--gop", 0),
        profile: if codec == Codec::Hevc { HEVC_MAIN } else { H264_HIGH },
        level: 11,
        flags: if cbr { ENC_REALTIME } else { 0 },
    };
    if !config.valid() {
        return Err(format!("not a config the encoder takes: {config:?}"));
    }
    let data = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let frame = (w * h * 3 / 2) as usize;
    let count = (data.len() / frame).min(num("--frames", u32::MAX) as usize);
    let mut out = match arg(args, "--out") {
        Some(o) => Some(std::fs::File::create(o).map_err(|e| format!("{o}: {e}"))?),
        None => None,
    };
    let (tx, woken) = mpsc::sync_channel::<()>(1);
    let wake: Wake = Arc::new(move || {
        let _ = tx.try_send(());
    });
    let mut s = Stream::new(config);
    s.enc = Some(Encoder::new(&config, Some(wake.clone())).ok_or("VideoToolbox won't encode that")?);
    // Each frame copied into the guest's layout first, as it would be.
    let mut room = vec![0u8; layout.size(PixFmt::Nv12) as usize];
    let window = Window::of(&mut room);
    let stride = layout.width as usize;
    let (mut bytes, mut keys, mut failed) = (0usize, 0u32, 0u32);
    let mut take = |s: &mut Stream| -> Result<(), String> {
        while let Some((_, d, key)) = s.out.pop_front() {
            match d {
                Some(d) => {
                    bytes += d.len();
                    keys += key as u32;
                    if let Some(f) = &mut out {
                        f.write_all(&d).map_err(|e| e.to_string())?;
                    }
                }
                None => failed += 1,
            }
        }
        Ok(())
    };
    let t0 = Instant::now();
    for i in 0..count {
        let src = &data[i * frame..(i + 1) * frame];
        for y in 0..h as usize {
            room[y * stride..y * stride + w as usize].copy_from_slice(&src[y * w as usize..(y + 1) * w as usize]);
        }
        let chroma = &src[(w * h) as usize..];
        for y in 0..(h / 2) as usize {
            let at = stride * layout.height as usize + y * stride;
            room[at..at + w as usize].copy_from_slice(&chroma[y * w as usize..(y + 1) * w as usize]);
        }
        let f = Frame { cookie: i as u32, offset: 0, len: room.len() as u32, key: false };
        if !s.encode(f, &window, &wake) {
            s.out.push_back((f.cookie, None, false));
        }
        // Real time: each frame out before the next goes in, as in a call.
        if cbr {
            while !s.coding.is_empty() {
                let _ = woken.recv_timeout(Duration::from_millis(100));
                s.collect();
            }
        } else {
            s.collect();
        }
        take(&mut s)?;
    }
    s.finish();
    take(&mut s)?;
    let dt = t0.elapsed().as_secs_f64();
    let (_, coded, worst, sum) = s.stats;
    eprintln!(
        "{count} frames {w}x{h} in {dt:.2} s: {:.0} fps; {coded} coded ({keys} key frames, {failed} failed), {:.0} kbit/s at {} fps; in VideoToolbox {:.2} ms a frame on average, {:.2} ms at most",
        count as f64 / dt,
        bytes as f64 * 8.0 / 1000.0 / (count as f64 / config.fps.0 as f64),
        config.fps.0,
        sum.as_secs_f64() * 1e3 / coded.max(1) as f64,
        worst.as_secs_f64() * 1e3
    );
    Ok(())
}
