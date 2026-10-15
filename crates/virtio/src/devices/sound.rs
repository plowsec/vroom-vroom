//! virtio-snd: a playback stream (S16, 1-2 channels, 44.1 or 48 kHz) and,
//! only when the VM may use the microphone, a capture stream (same
//! formats). Without it the guest sees no input device at all.
//!
//! Playback buffers are completed at playback speed (a clock started on
//! PCM_START), which paces the guest; the PCM itself goes to an
//! [`AudioSink`] as soon as it arrives (the sink buffers a little).
//! Capture buffers are filled from an [`AudioSource`] and returned at
//! recording speed, the same way.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::chain::Writer;
use crate::worker::{QueueCtx, QueueHandler, WorkerSlot};
use crate::{types, ActivateContext, DeviceState, Error, Result, VirtioDevice};

pub use vroom_devices::audio::{AudioSink, AudioSource};

const Q_CTL: usize = 0;
const Q_TX: usize = 2;
const Q_RX: usize = 3;

const R_JACK_INFO: u32 = 1;
const R_PCM_INFO: u32 = 0x100;
const R_PCM_SET_PARAMS: u32 = 0x101;
const R_PCM_PREPARE: u32 = 0x102;
const R_PCM_RELEASE: u32 = 0x103;
const R_PCM_START: u32 = 0x104;
const R_PCM_STOP: u32 = 0x105;
const R_CHMAP_INFO: u32 = 0x200;

const S_OK: u32 = 0x8000;
const S_BAD_MSG: u32 = 0x8001;
const S_NOT_SUPP: u32 = 0x8002;
const S_IO_ERR: u32 = 0x8003;

const FMT_S16: u8 = 5;
const RATE_44100: u8 = 6;
const RATE_48000: u8 = 7;
const DIR_OUTPUT: u8 = 0;
const DIR_INPUT: u8 = 1;

/// Stream ids: playback, then capture (if any).
const OUT: usize = 0;
const IN: usize = 1;

/// Most bytes one transfer may carry.
const MAX_XFER: usize = 1 << 20;
const MAX_PENDING: usize = 64;
const TICK: Duration = Duration::from_millis(5);
/// A capture stream more than this far behind (VM stalled) skips ahead.
const MAX_CAPTURE_LAG: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Params {
    buffer_bytes: u32,
    period_bytes: u32,
    channels: u8,
    rate: u32,
}

impl Params {
    fn frame_bytes(&self) -> u64 {
        2 * self.channels as u64
    }
    fn ok(&self) -> bool {
        self.period_bytes != 0 && self.buffer_bytes >= self.period_bytes && (1..=2).contains(&self.channels)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum State {
    #[default]
    Idle,
    Prepared,
    Running,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Snap {
    params: Option<Params>,
    state: State,
    #[serde(default)]
    in_params: Option<Params>,
    #[serde(default)]
    in_state: State,
}

struct Pending {
    head: u16,
    status: Writer,
    /// Stream position (bytes) once this buffer has played.
    end: u64,
}

/// A capture buffer waiting for audio.
struct RxPending {
    head: u16,
    data: Writer,
    status: Writer,
}

#[derive(Debug, Default, Clone, Copy)]
struct Stream {
    params: Option<Params>,
    state: State,
    started: Option<Instant>,
}

impl Stream {
    fn restored(params: Option<Params>, state: State) -> Self {
        // Running streams come back prepared: the guest restarts them.
        let state = if state == State::Running { State::Prepared } else { state };
        Stream { params, state, started: None }
    }
}

pub struct SoundHandler {
    sink: Box<dyn AudioSink>,
    source: Option<Box<dyn AudioSource>>,
    streams: [Stream; 2],
    pending: VecDeque<Pending>,
    queued: u64,
    /// PCM queued before START (drivers pre-fill): sent on START.
    prefill: Vec<u8>,
    rx: VecDeque<RxPending>,
    /// Bytes handed to the guest since the capture START.
    captured: u64,
    paused: bool,
}

impl std::fmt::Debug for SoundHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoundHandler").field("streams", &self.streams).field("pending", &self.pending.len()).finish()
    }
}

fn rd32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn status_bytes(st: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&st.to_le_bytes());
    b
}

impl SoundHandler {
    fn new(sink: Box<dyn AudioSink>, source: Option<Box<dyn AudioSource>>, snap: Snap) -> Self {
        SoundHandler {
            sink,
            source,
            streams: [Stream::restored(snap.params, snap.state), Stream::restored(snap.in_params, snap.in_state)],
            pending: VecDeque::new(),
            queued: 0,
            prefill: Vec::new(),
            rx: VecDeque::new(),
            captured: 0,
            paused: false,
        }
    }

    fn n_streams(&self) -> u32 {
        if self.source.is_some() {
            2
        } else {
            1
        }
    }

    fn frame_bytes(&self, s: usize) -> u64 {
        self.streams[s].params.map_or(4, |p| p.frame_bytes())
    }

    /// Bytes stream `s` has moved since START, by the clock (plus two
    /// periods of lead for playback, so the sink never runs dry).
    fn elapsed_bytes(&self, s: usize, lead: bool) -> u64 {
        let (Some(t0), Some(p)) = (self.streams[s].started, self.streams[s].params) else { return 0 };
        let us = t0.elapsed().as_micros() as u64;
        us * p.rate as u64 / 1_000_000 * p.frame_bytes() + if lead { 2 * p.period_bytes as u64 } else { 0 }
    }

    fn complete(&mut self, ctx: &mut QueueCtx, all: bool) {
        let played = self.elapsed_bytes(OUT, true);
        while let Some(p) = self.pending.front() {
            if !all && p.end > played {
                break;
            }
            let mut p = self.pending.pop_front().expect("front exists");
            let len = if p.status.write_all(&status_bytes(S_OK)).is_ok() { 8 } else { 0 };
            ctx.add_used(Q_TX, p.head, len);
        }
    }

    /// Fill capture buffers whose time has come (or, `all`, hand every
    /// one back empty: stream stopped).
    fn capture(&mut self, ctx: &mut QueueCtx, all: bool) {
        if all {
            while let Some(mut b) = self.rx.pop_front() {
                let len = if b.status.write_all(&status_bytes(S_OK)).is_ok() { 8 } else { 0 };
                ctx.add_used(Q_RX, b.head, len);
            }
            return;
        }
        if self.streams[IN].state != State::Running || self.paused {
            return;
        }
        let (Some(p), true) = (self.streams[IN].params, self.streams[IN].started.is_some()) else { return };
        let due = self.elapsed_bytes(IN, false);
        let lag = MAX_CAPTURE_LAG.as_micros() as u64 * p.rate as u64 / 1_000_000 * p.frame_bytes();
        if due.saturating_sub(self.captured) > lag {
            self.captured = due - lag;
        }
        while let Some(b) = self.rx.front() {
            let n = b.data.available() as u64 / p.frame_bytes() * p.frame_bytes();
            if self.captured + n > due {
                break;
            }
            let mut b = self.rx.pop_front().expect("front exists");
            let mut pcm = vec![0u8; n as usize];
            if let Some(src) = self.source.as_mut() {
                src.read(&mut pcm);
            }
            let ok = b.data.write_all(&pcm).is_ok() && b.status.write_all(&status_bytes(S_OK)).is_ok();
            ctx.add_used(Q_RX, b.head, if ok { n as u32 + 8 } else { 0 });
            self.captured += n;
        }
    }

    fn stream_info(&self, s: u32, size: usize) -> Vec<u8> {
        let mut info = vec![0u8; size];
        // features 0; formats: S16; rates: 44.1k, 48k.
        info[8..16].copy_from_slice(&(1u64 << FMT_S16).to_le_bytes());
        info[16..24].copy_from_slice(&((1u64 << RATE_44100) | (1u64 << RATE_48000)).to_le_bytes());
        info[24] = if s as usize == IN { DIR_INPUT } else { DIR_OUTPUT };
        info[25] = 1;
        info[26] = 2;
        info
    }

    fn host_start(&mut self, s: usize) {
        let Some(p) = self.streams[s].params else { return };
        if s == OUT {
            self.sink.start(p.rate, p.channels);
        } else if let Some(src) = self.source.as_mut() {
            src.start(p.rate, p.channels);
        }
    }

    fn host_stop(&mut self, s: usize) {
        if s == OUT {
            self.sink.stop();
        } else if let Some(src) = self.source.as_mut() {
            src.stop();
        }
    }

    /// VM paused: the microphone stops; it starts again on resume.
    fn pause_capture(&mut self) {
        self.paused = true;
        if self.streams[IN].state == State::Running {
            self.host_stop(IN);
        }
    }

    fn resume_capture(&mut self) {
        self.paused = false;
        if self.streams[IN].state == State::Running {
            self.streams[IN].started = Some(Instant::now());
            self.captured = 0;
            self.host_start(IN);
        }
    }

    fn control(&mut self, ctx: &mut QueueCtx, req: &[u8], w: &mut Writer) -> u32 {
        let Some(code) = rd32(req, 0) else { return S_BAD_MSG };
        match code {
            R_PCM_INFO => {
                let (Some(start), Some(count), Some(size)) = (rd32(req, 4), rd32(req, 8), rd32(req, 12)) else { return S_BAD_MSG };
                if count == 0 || start.checked_add(count).is_none_or(|e| e > self.n_streams()) || size < 32 {
                    return S_BAD_MSG;
                }
                let mut out = S_OK.to_le_bytes().to_vec();
                for s in start..start + count {
                    out.extend_from_slice(&self.stream_info(s, size.min(64) as usize));
                }
                if w.write_all(&out).is_err() {
                    return S_IO_ERR;
                }
                return u32::MAX; // written already
            }
            R_JACK_INFO | R_CHMAP_INFO => return S_NOT_SUPP,
            _ => {}
        }
        // PCM requests carry the stream id.
        let s = match rd32(req, 4) {
            Some(id) if id < self.n_streams() => id as usize,
            _ => return S_BAD_MSG,
        };
        match code {
            R_PCM_SET_PARAMS => {
                let (Some(buffer_bytes), Some(period_bytes)) = (rd32(req, 8), rd32(req, 12)) else { return S_BAD_MSG };
                let (channels, format, rate) = (req.get(20).copied(), req.get(21).copied(), req.get(22).copied());
                let rate = match rate {
                    Some(RATE_44100) => 44100,
                    Some(RATE_48000) => 48000,
                    _ => return S_NOT_SUPP,
                };
                let Some(channels @ 1..=2) = channels else { return S_NOT_SUPP };
                if format != Some(FMT_S16) {
                    return S_NOT_SUPP;
                }
                if period_bytes == 0 || buffer_bytes < period_bytes || buffer_bytes as usize > 4 * MAX_XFER {
                    return S_BAD_MSG;
                }
                if self.streams[s].state == State::Running {
                    return S_BAD_MSG;
                }
                self.streams[s].params = Some(Params { buffer_bytes, period_bytes, channels, rate });
                S_OK
            }
            R_PCM_PREPARE => {
                if self.streams[s].params.is_none() || self.streams[s].state == State::Running {
                    return S_BAD_MSG;
                }
                self.streams[s].state = State::Prepared;
                S_OK
            }
            R_PCM_START => {
                if self.streams[s].params.is_none() || self.streams[s].state != State::Prepared {
                    return S_BAD_MSG;
                }
                self.streams[s].state = State::Running;
                // Time counts from START; for playback, buffers queued
                // before it (the usual pre-fill) play first.
                self.streams[s].started = Some(Instant::now());
                if s == IN {
                    self.captured = 0;
                    if !self.paused {
                        self.host_start(IN);
                    }
                    return S_OK;
                }
                self.host_start(OUT);
                let pre = std::mem::take(&mut self.prefill);
                if !pre.is_empty() {
                    self.sink.write(&pre);
                }
                S_OK
            }
            R_PCM_STOP => {
                if self.streams[s].state != State::Running {
                    return S_BAD_MSG;
                }
                self.streams[s].state = State::Prepared;
                self.streams[s].started = None;
                self.host_stop(s);
                self.finish(ctx, s);
                S_OK
            }
            R_PCM_RELEASE => {
                if self.streams[s].state == State::Running {
                    self.host_stop(s);
                }
                self.streams[s].state = State::Idle;
                self.streams[s].started = None;
                self.finish(ctx, s);
                S_OK
            }
            _ => S_NOT_SUPP,
        }
    }

    /// Hand back everything in flight on stream `s`.
    fn finish(&mut self, ctx: &mut QueueCtx, s: usize) {
        if s == OUT {
            self.prefill.clear();
            self.complete(ctx, true);
            self.queued = 0;
        } else {
            self.capture(ctx, true);
            self.captured = 0;
        }
    }

    /// Error status for a bad transfer, using whatever room it has.
    fn reject(ctx: &mut QueueCtx, q: usize, head: u16, w: &mut Writer) {
        let _ = w.write_all(&status_bytes(S_BAD_MSG));
        ctx.add_used(q, head, w.written() as u32);
    }

    fn tx(&mut self, ctx: &mut QueueCtx) {
        loop {
            if self.pending.len() >= MAX_PENDING {
                // The guest outran us badly: make room (audio glitch).
                self.complete(ctx, true);
            }
            let chain = match ctx.queues[Q_TX].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                _ => break,
            };
            let head = chain.head();
            let Ok((mut r, mut w)) = chain.into_rw() else {
                ctx.add_used(Q_TX, head, 0);
                continue;
            };
            let ok_stream = r.read_obj::<u32>().is_ok_and(|v| u32::from_le(v) == OUT as u32);
            let n = r.available().min(MAX_XFER);
            let mut pcm = vec![0u8; n];
            let st = self.streams[OUT].state;
            let ok = ok_stream && st != State::Idle && r.read_exact(&mut pcm).is_ok() && w.available() >= 8;
            if !ok {
                Self::reject(ctx, Q_TX, head, &mut w);
                continue;
            }
            // Whole frames only.
            let fb = self.frame_bytes(OUT) as usize;
            pcm.truncate(pcm.len() / fb * fb);
            if st == State::Running {
                self.sink.write(&pcm);
            } else if self.prefill.len() + pcm.len() <= self.streams[OUT].params.map_or(0, |p| p.buffer_bytes as usize) {
                self.prefill.extend_from_slice(&pcm);
            }
            self.queued += pcm.len() as u64;
            // Keep only the 8-byte status part for later.
            let status = match w.split_tail(8) {
                Ok(s) => s,
                Err(_) => w,
            };
            self.pending.push_back(Pending { head, status, end: self.queued });
        }
        if self.streams[OUT].state == State::Running {
            self.complete(ctx, false);
        }
    }

    fn rx(&mut self, ctx: &mut QueueCtx) {
        loop {
            let chain = match ctx.queues[Q_RX].pop(&ctx.mem) {
                Ok(Some(c)) => c,
                _ => break,
            };
            let head = chain.head();
            let Ok((mut r, mut w)) = chain.into_rw() else {
                ctx.add_used(Q_RX, head, 0);
                continue;
            };
            let ok_stream = r.read_obj::<u32>().is_ok_and(|v| u32::from_le(v) == IN as u32);
            let room = w.available();
            let ok = ok_stream
                && self.source.is_some()
                && self.streams[IN].state != State::Idle
                && room >= 8
                && room - 8 <= MAX_XFER
                && self.rx.len() < MAX_PENDING;
            if !ok {
                Self::reject(ctx, Q_RX, head, &mut w);
                continue;
            }
            let Ok(status) = w.split_tail(8) else {
                Self::reject(ctx, Q_RX, head, &mut w);
                continue;
            };
            self.rx.push_back(RxPending { head, data: w, status });
        }
        self.capture(ctx, false);
    }
}

impl QueueHandler for SoundHandler {
    fn process_queue(&mut self, ctx: &mut QueueCtx, q: usize) {
        match q {
            Q_CTL => loop {
                let chain = match ctx.queues[Q_CTL].pop(&ctx.mem) {
                    Ok(Some(c)) => c,
                    _ => break,
                };
                let head = chain.head();
                let mut len = 0;
                if let Ok((mut r, mut w)) = chain.into_rw() {
                    let n = r.available().min(256);
                    let mut req = vec![0u8; n];
                    let st = if r.read_exact(&mut req).is_ok() { self.control(ctx, &req, &mut w) } else { S_BAD_MSG };
                    if st != u32::MAX {
                        let _ = w.write_all(&st.to_le_bytes());
                    }
                    len = w.written() as u32;
                }
                ctx.add_used(Q_CTL, head, len);
            },
            Q_TX => self.tx(ctx),
            Q_RX => self.rx(ctx),
            // Event queue: we never have events; keep the buffers.
            _ => {}
        }
    }

    fn tick_interval(&mut self) -> Option<Duration> {
        let play = self.streams[OUT].state == State::Running && !self.pending.is_empty();
        let rec = self.streams[IN].state == State::Running && !self.rx.is_empty();
        (play || rec).then_some(TICK)
    }

    fn tick(&mut self, ctx: &mut QueueCtx) {
        self.complete(ctx, false);
        self.capture(ctx, false);
    }
}

type MakeSource = Box<dyn FnMut() -> Box<dyn AudioSource> + Send>;

pub struct Sound {
    make_sink: Box<dyn FnMut() -> Box<dyn AudioSink> + Send>,
    make_source: Option<MakeSource>,
    worker: WorkerSlot<SoundHandler>,
}

impl std::fmt::Debug for Sound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sound").finish_non_exhaustive()
    }
}

impl Sound {
    /// `make_sink` is called on every activation (a guest reboot gets a
    /// fresh one).
    pub fn new(make_sink: Box<dyn FnMut() -> Box<dyn AudioSink> + Send>) -> Self {
        Sound { make_sink, make_source: None, worker: WorkerSlot::default() }
    }

    /// Add a capture stream fed by `make_source` (called like `make_sink`).
    pub fn with_capture(mut self, make_source: MakeSource) -> Self {
        self.make_source = Some(make_source);
        self
    }

    fn handler(&mut self, snap: Snap) -> SoundHandler {
        let source = self.make_source.as_mut().map(|m| m());
        SoundHandler::new((self.make_sink)(), source, snap)
    }

    /// Fuzzing: run every queue once, synchronously, then a tick and a
    /// release. Not for anything else.
    #[doc(hidden)]
    pub fn fuzz_queues(&mut self, ctx: ActivateContext) {
        let mut h = self.handler(Snap::default());
        let mut c = crate::worker::QueueCtx::new(ctx);
        for _ in 0..2 {
            for q in 0..4 {
                h.process_queue(&mut c, q);
            }
            h.tick(&mut c);
        }
        h.complete(&mut c, true);
        h.capture(&mut c, true);
    }
}

impl VirtioDevice for Sound {
    fn device_type(&self) -> u32 {
        types::SOUND
    }
    fn name(&self) -> String {
        "sound".into()
    }
    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![64, 64, 64, 64]
    }
    fn device_features(&self) -> u64 {
        0
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // jacks 0, streams 1 or 2, chmaps 0.
        let streams = if self.make_source.is_some() { 2 } else { 1 };
        let c: [u8; 12] = [0, 0, 0, 0, streams, 0, 0, 0, 0, 0, 0, 0];
        data.fill(0);
        let o = offset as usize;
        if o < c.len() {
            let n = data.len().min(c.len() - o);
            data[..n].copy_from_slice(&c[o..o + n]);
        }
    }
    fn activate(&mut self, ctx: ActivateContext) -> Result<()> {
        let h = self.handler(Snap::default());
        self.worker.start("sound", ctx, h, false)
    }
    fn queue_notify(&mut self, q: u16) {
        self.worker.notify(q);
    }
    fn reset(&mut self) {
        self.worker.stop_with(|h| {
            h.sink.stop();
            if let Some(s) = h.source.as_mut() {
                s.stop();
            }
        });
    }
    fn pause(&mut self) {
        self.worker.with(|_, h| h.pause_capture());
        self.worker.pause();
    }
    fn resume(&mut self) {
        self.worker.resume();
        self.worker.with(|_, h| h.resume_capture());
    }
    fn save_active(&mut self) -> Result<DeviceState> {
        // In-flight buffers can't be carried over: hand them back now.
        let snap = self
            .worker
            .with(|ctx, h| {
                h.complete(ctx, true);
                h.capture(ctx, true);
                ctx.flush();
                let [o, i] = h.streams;
                Snap { params: o.params, state: o.state, in_params: i.params, in_state: i.state }
            })
            .unwrap_or_default();
        let data = serde_json::to_vec(&snap).map_err(|e| Error::Device(e.to_string()))?;
        Ok(DeviceState { queues: self.worker.queues(), data })
    }
    fn restore_active(&mut self, ctx: ActivateContext, data: &[u8]) -> Result<()> {
        let snap: Snap = serde_json::from_slice(data).map_err(|e| Error::Device(e.to_string()))?;
        if snap.params.is_some_and(|p| !p.ok()) || snap.in_params.is_some_and(|p| !p.ok()) {
            return Err(Error::Device("sound snapshot: bad params".into()));
        }
        let h = self.handler(snap);
        self.worker.start("sound", ctx, h, true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use vroom_devices::audio::NullSink;
    use vroom_memory::GuestAddress;

    use super::*;
    use crate::queue::tests::{TestRing, DATA};
    use crate::{Queue, VirtioInterrupt};

    struct NoIrq;
    impl VirtioInterrupt for NoIrq {
        fn signal_used_queue(&self, _q: u16) {}
        fn signal_config_changed(&self) {}
    }

    #[derive(Clone, Default)]
    struct Mic(Arc<Mutex<Option<(u32, u8)>>>);
    impl AudioSource for Mic {
        fn start(&mut self, rate: u32, channels: u8) {
            *self.0.lock().unwrap() = Some((rate, channels));
        }
        fn read(&mut self, out: &mut [u8]) {
            out.fill(0x5a);
        }
        fn stop(&mut self) {
            *self.0.lock().unwrap() = None;
        }
    }

    #[test]
    fn capture_fills_buffers_at_recording_speed() {
        let mut ring = TestRing::new(8);
        // Header (stream 1), 960 bytes of data (10 ms mono 48 kHz), status.
        let head = ring.add_chain(&[(4, false), (960, true), (8, true)]);
        ring.mem.write_obj(GuestAddress(DATA), &1u32).unwrap();
        // A buffer for the playback stream id is refused.
        let bad = ring.add_chain(&[(4, false), (64, true), (8, true)]);
        ring.mem.write_obj(GuestAddress(DATA + 0x3000), &0u32).unwrap();
        let mut queues = vec![Queue::new(8); 4];
        queues[Q_RX] = ring.q.clone();
        let mut ctx = QueueCtx::new(ActivateContext { mem: ring.mem.clone(), queues, irq: Arc::new(NoIrq), features: 0 });
        let mic = Mic::default();
        let mut h = SoundHandler::new(Box::new(NullSink), Some(Box::new(mic.clone())), Snap::default());
        assert_eq!(h.n_streams(), 2);
        assert_eq!(h.stream_info(1, 32)[24], DIR_INPUT);
        h.streams[IN].params = Some(Params { buffer_bytes: 3840, period_bytes: 960, channels: 1, rate: 48000 });
        h.streams[IN].state = State::Prepared;
        h.process_queue(&mut ctx, Q_RX);
        assert_eq!(ring.used_idx(), 1, "bad stream id rejected at once");
        assert_eq!(ring.used(0).0, bad as u32);
        // START: the microphone opens; the buffer fills after 10 ms.
        h.streams[IN].state = State::Running;
        h.streams[IN].started = Some(Instant::now());
        h.host_start(IN);
        assert_eq!(*mic.0.lock().unwrap(), Some((48000, 1)));
        h.tick(&mut ctx);
        assert_eq!(ring.used_idx(), 1, "too early");
        std::thread::sleep(Duration::from_millis(15));
        h.tick(&mut ctx);
        assert_eq!(ring.used_idx(), 2);
        assert_eq!(ring.used(1), (head as u32, 968));
        let mut b = vec![0u8; 960];
        ring.mem.read(GuestAddress(DATA + 0x1000), &mut b).unwrap();
        assert!(b.iter().all(|&x| x == 0x5a));
        // Pause closes the microphone, resume opens it again.
        h.pause_capture();
        assert_eq!(*mic.0.lock().unwrap(), None);
        h.resume_capture();
        assert_eq!(*mic.0.lock().unwrap(), Some((48000, 1)));
    }

    #[test]
    fn no_capture_stream_without_a_source() {
        let h = SoundHandler::new(Box::new(NullSink), None, Snap::default());
        assert_eq!(h.n_streams(), 1);
        let mut d = [0u8; 4];
        Sound::new(Box::new(|| Box::new(NullSink) as Box<dyn AudioSink>)).read_config(4, &mut d);
        assert_eq!(d, [1, 0, 0, 0]);
    }
}
