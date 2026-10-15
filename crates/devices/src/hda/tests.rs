use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::pci::IntxRouter;
use crate::IrqLine;

const RAM: u64 = 0x4000_0000;
const CORB: u64 = RAM;
const RIRB: u64 = RAM + 0x800;
const BDL: u64 = RAM + 0x1000;
const POS: u64 = RAM + 0x1800;
const BUF: u64 = RAM + 0x10000;

#[derive(Default)]
struct Line(AtomicBool);
impl IrqLine for Line {
    fn set_level(&self, l: bool) {
        self.0.store(l, Ordering::SeqCst);
    }
}

struct NoMsi;
impl MsiSender for NoMsi {
    fn send(&self, _a: u64, _d: u32) {}
}

#[derive(Default)]
struct Rec {
    started: Option<(u32, u8)>,
    data: Vec<u8>,
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Rec>>);

impl AudioSink for Shared {
    fn start(&mut self, rate: u32, channels: u8) {
        let mut r = self.0.lock().unwrap();
        r.started = Some((rate, channels));
        r.data.clear();
    }
    fn write(&mut self, pcm: &[u8]) {
        self.0.lock().unwrap().data.extend_from_slice(pcm);
    }
    fn stop(&mut self) {
        self.0.lock().unwrap().started = None;
    }
}

impl AudioSource for Shared {
    fn start(&mut self, rate: u32, channels: u8) {
        self.0.lock().unwrap().started = Some((rate, channels));
    }
    fn read(&mut self, out: &mut [u8]) {
        for s in out.as_chunks_mut::<2>().0 {
            s.copy_from_slice(&0x1234i16.to_le_bytes());
        }
    }
    fn stop(&mut self) {
        self.0.lock().unwrap().started = None;
    }
}

struct T {
    hda: Hda,
    mem: GuestMemory,
    line: Arc<Line>,
    sink: Shared,
    mic: Shared,
}

impl T {
    fn with_mic(has_mic: bool) -> T {
        let mem = GuestMemory::new(&[(GuestAddress(RAM), 1 << 20)]).unwrap();
        let line = Arc::new(Line::default());
        let lines: Vec<Arc<dyn IrqLine>> = (0..4).map(|_| line.clone() as Arc<dyn IrqLine>).collect();
        let router = IntxRouter::new(lines);
        let (sink, mic) = (Shared::default(), Shared::default());
        let mut hda = Hda::new(
            4,
            mem.clone(),
            router.pin(4, 1),
            Arc::new(NoMsi),
            HdaIds::default(),
            has_mic,
            Box::new(sink.clone()),
            Box::new(mic.clone()),
        );
        // Memory decoding + bus master.
        hda.config_mut().write(4, &6u16.to_le_bytes());
        T { hda, mem, line, sink, mic }
    }

    fn w(&mut self, off: u64, v: u64, len: usize) {
        self.hda.bar_write(0, off, &v.to_le_bytes()[..len]);
    }

    fn r(&mut self, off: u64, len: usize) -> u64 {
        self.hda.peek(off, len)
    }

    /// Out of reset, rings set up, interrupts on.
    fn up() -> T {
        T::up_with(true)
    }

    fn up_with(has_mic: bool) -> T {
        let mut t = T::with_mic(has_mic);
        t.w(0x08, 1, 4);
        assert_eq!(t.r(0x0e, 2), 1, "codec 0 present");
        t.w(0x0e, 1, 2);
        t.w(0x40, CORB, 4);
        t.w(0x44, 0, 4);
        t.w(0x4e, 2, 1);
        t.w(0x4a, 0x8000, 2);
        assert_eq!(t.r(0x4a, 2), 0x8000);
        t.w(0x4a, 0, 2);
        t.w(0x50, RIRB, 4);
        t.w(0x54, 0, 4);
        t.w(0x5e, 2, 1);
        t.w(0x58, 0x8000, 2);
        t.w(0x5a, 1, 2);
        t.w(0x5c, 3, 1);
        t.w(0x4c, 2, 1);
        t.w(0x20, 0xc000_0003, 4);
        t
    }

    /// Send verbs through the CORB; return their responses.
    fn verbs(&mut self, v: &[u32]) -> Vec<u32> {
        let mut out = Vec::new();
        for &cmd in v {
            let wp = (self.r(0x48, 2) as u16 + 1) % 256;
            self.mem.write(GuestAddress(CORB + wp as u64 * 4), &cmd.to_le_bytes()).unwrap();
            self.w(0x48, wp as u64, 2);
            let rp = self.r(0x58, 2);
            let mut b = [0u8; 4];
            self.mem.read(GuestAddress(RIRB + rp * 8), &mut b).unwrap();
            out.push(u32::from_le_bytes(b));
            assert!(self.line.0.load(Ordering::SeqCst), "response interrupt");
            self.w(0x5d, 1, 1);
        }
        out
    }

    fn bdl(&mut self, entries: &[(u64, u32, bool)]) {
        for (k, &(a, l, ioc)) in entries.iter().enumerate() {
            let mut e = [0u8; 16];
            e[..8].copy_from_slice(&a.to_le_bytes());
            e[8..12].copy_from_slice(&l.to_le_bytes());
            e[12] = ioc as u8;
            self.mem.write(GuestAddress(BDL + k as u64 * 16), &e).unwrap();
        }
    }

    /// Program stream `sd` (0 in, 1 out) with tag 1, 48 kHz stereo 16-bit.
    fn stream(&mut self, sd: u64, cbl: u32, lvi: u16) {
        let b = 0x80 + sd * 0x20;
        self.w(b, 1, 1);
        assert_eq!(self.r(b, 1) & 1, 1);
        self.w(b, 0, 1);
        self.w(b + 0x18, BDL, 4);
        self.w(b + 0x1c, 0, 4);
        self.w(b + 0x08, cbl as u64, 4);
        self.w(b + 0x0c, lvi as u64, 2);
        self.w(b + 0x12, 0x0011, 2);
        // Tag 1, IOC + error interrupts, then run.
        self.w(b, 0x10_001c, 3);
        self.w(b, 0x10_001e, 3);
    }
}

#[test]
fn codec_answers_through_corb_and_immediate_registers() {
    let mut t = T::up();
    let ids = HdaIds::default();
    let r = t.verbs(&[0x000f_0000, 0x001f_0004, 0x001f_2000]);
    assert_eq!(r[0], (ids.codec.vendor as u32) << 16 | ids.codec.device as u32);
    assert_eq!(r[1], 2 << 16 | 4);
    assert_eq!(r[2], ids.codec.subsys);
    // Immediate command interface.
    t.w(0x60, 0x002f_0009, 4);
    t.w(0x68, 1, 2);
    assert_eq!(t.r(0x68, 2), 2);
    assert_eq!(t.r(0x64, 4), 0x1d);
    t.w(0x68, 2, 2);
    assert_eq!(t.r(0x68, 2), 0);
    // A codec address with nothing there: no response.
    let before = t.r(0x58, 2);
    let wp = (t.r(0x48, 2) + 1) % 256;
    t.mem.write(GuestAddress(CORB + wp * 4), &0x100f_0000u32.to_le_bytes()).unwrap();
    t.w(0x48, wp, 2);
    assert_eq!(t.r(0x58, 2), before);
    assert_eq!(t.r(0x4a, 2), wp, "but it was consumed");
    // PCI identity.
    let mut d = [0u8; 4];
    t.hda.config().read(0, &mut d);
    assert_eq!(u32::from_le_bytes(d), 0x2668_8086);
    t.hda.config().read(8, &mut d);
    assert_eq!(u32::from_le_bytes(d) >> 8, 0x04_03_00);
}

#[test]
fn playback_moves_buffers_at_real_time() {
    let mut t = T::up();
    t.verbs(&[0x0027_0610, 0x0022_0011]);
    let pcm: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
    t.mem.write(GuestAddress(BUF), &pcm).unwrap();
    t.bdl(&[(BUF, 2048, true), (BUF + 2048, 2048, true)]);
    t.w(0x70, POS | 1, 4);
    t.stream(1, 4096, 1);
    assert_eq!(t.sink.0.lock().unwrap().started, Some((48000, 2)));
    let lead = t.sink.0.lock().unwrap().data.len();
    assert_eq!(lead, 48 * 30 * 4, "silence ahead of the stream");
    // 10 ms: 480 frames, 1920 bytes.
    t.hda.tick(Instant::now() + Duration::from_millis(10));
    let got = t.sink.0.lock().unwrap().data[lead..].to_vec();
    assert!((1900..=2048).contains(&got.len()), "{}", got.len());
    assert_eq!(got[..], pcm[..got.len()]);
    let lpib = t.r(0x84 + 0x20, 4);
    assert_eq!(lpib as usize, got.len());
    assert_eq!(t.r(0x2084 + 0x20, 4), lpib, "alias");
    let mut p = [0u8; 4];
    t.mem.read(GuestAddress(POS + 8), &mut p).unwrap();
    assert_eq!(u32::from_le_bytes(p) as u64, lpib, "position buffer");
    // 30 ms: wrapped past both entries, completion interrupts raised.
    t.hda.tick(Instant::now() + Duration::from_millis(30));
    assert_ne!(t.r(0xa3, 1) & 4, 0);
    assert_ne!(t.r(0x24, 4) & 2, 0);
    assert!(t.line.0.load(Ordering::SeqCst));
    t.w(0xa3, 4, 1);
    assert_eq!(t.r(0x24, 4) & 2, 0);
    assert!(!t.line.0.load(Ordering::SeqCst));
    // Pause stops the player, resume starts it again.
    t.hda.pause();
    assert_eq!(t.sink.0.lock().unwrap().started, None);
    t.hda.resume();
    assert_eq!(t.sink.0.lock().unwrap().started, Some((48000, 2)));
    // Stop.
    t.w(0xa0, 0x10_001c, 3);
    assert_eq!(t.sink.0.lock().unwrap().started, None);
}

#[test]
fn capture_writes_the_source_into_guest_buffers() {
    let mut t = T::up();
    // ADC on tag 1.
    t.verbs(&[0x0047_0610]);
    t.bdl(&[(BUF, 1920, true)]);
    t.stream(0, 1920, 0);
    assert_eq!(t.mic.0.lock().unwrap().started, Some((48000, 2)));
    t.hda.tick(Instant::now() + Duration::from_millis(5));
    let lpib = t.r(0x84, 4) as usize;
    assert!(lpib > 800, "{lpib}");
    let mut b = vec![0u8; lpib];
    t.mem.read(GuestAddress(BUF), &mut b).unwrap();
    assert!(b.chunks(2).all(|c| c == [0x34, 0x12]));
    // Muting the ADC records silence.
    t.verbs(&[0x0043_7080]);
    t.hda.tick(Instant::now() + Duration::from_millis(9));
    let lpib2 = t.r(0x84, 4) as usize;
    let mut b = vec![0u8; lpib2 - lpib];
    t.mem.read(GuestAddress(BUF + lpib as u64), &mut b).unwrap();
    assert!(b.iter().all(|&x| x == 0));
}

#[test]
fn no_mic_means_silence_and_no_source() {
    let mut t = T::up_with(false);
    t.verbs(&[0x0047_0610]);
    t.bdl(&[(BUF, 1920, true)]);
    t.stream(0, 1920, 0);
    assert_eq!(t.mic.0.lock().unwrap().started, None);
    t.hda.tick(Instant::now() + Duration::from_millis(5));
    assert!(t.r(0x84, 4) > 0, "the stream still moves");
}

#[test]
fn hostile_descriptors_stop_the_stream_without_panicking() {
    let mut t = T::up();
    t.verbs(&[0x0027_0610]);
    // Entry pointing outside RAM.
    t.bdl(&[(0xdead_0000_0000, 4096, true)]);
    t.stream(1, 4096, 0);
    t.hda.tick(Instant::now() + Duration::from_millis(10));
    assert_ne!(t.r(0xa3, 1) & 0x10, 0, "descriptor error");
    assert!(t.line.0.load(Ordering::SeqCst));
    // Zero-length entries, LVI past the list, huge CBL: also an error.
    let mut t = T::up();
    t.bdl(&[(BUF, 0, true), (BUF, 0, true)]);
    t.stream(1, u32::MAX, 0xffff);
    t.hda.tick(Instant::now() + Duration::from_secs(100));
    assert_ne!(t.r(0xa3, 1) & 0x10, 0);
    // BDL itself unreachable; odd register widths and offsets.
    let mut t = T::up();
    t.w(0xb8, 0xffff_ff80, 4);
    t.w(0xbc, 0xffff_ffff, 4);
    t.w(0xa8, 64, 4);
    t.w(0xa0, 0x10_0002, 3);
    t.hda.tick(Instant::now() + Duration::from_millis(50));
    for off in 0..0x200 {
        for len in [1, 2, 4, 8] {
            t.r(off, len);
        }
    }
    for off in (0..0x100).chain(0x2000..0x2100) {
        t.w(off, u64::MAX, 1);
    }
    t.hda.tick(Instant::now() + Duration::from_secs(1));
    // CORB at an unreachable address: memory error, CORB stopped.
    let mut t = T::up();
    t.w(0x44, 0xffff_ffff, 4);
    t.w(0x48, 5, 2);
    assert_eq!(t.r(0x4d, 1), 1);
    assert_eq!(t.r(0x4c, 1) & 2, 0);
}

#[test]
fn controller_reset_and_snapshot() {
    let mut t = T::up();
    t.verbs(&[0x0027_0610]);
    t.bdl(&[(BUF, 512, true), (BUF + 512, 512, true)]);
    t.stream(1, 1024, 1);
    t.hda.tick(Instant::now() + Duration::from_millis(3));
    let snap = t.hda.save().unwrap();
    let lpib = t.r(0xa4, 4);
    t.w(0x08, 0, 4);
    assert_eq!(t.r(0xa0, 3), 0, "streams reset");
    assert_eq!(t.sink.0.lock().unwrap().started, None);
    assert_eq!(t.r(0x40, 4), 0);
    t.hda.restore(&snap).unwrap();
    assert_eq!(t.r(0xa4, 4), lpib);
    assert_eq!(t.r(0x40, 4), CORB);
    assert_eq!(t.sink.0.lock().unwrap().started, Some((48000, 2)));
    assert!(t.hda.restore(b"junk").is_err());
}

#[test]
fn formats() {
    assert_eq!(fmt_info(0x0011), Some((48000, 2, 2)));
    assert_eq!(fmt_info(0x4010), Some((44100, 1, 2)));
    assert_eq!(fmt_info(0x0811), Some((96000, 2, 2)));
    assert_eq!(fmt_info(0x0511), Some((8000, 2, 2)));
    assert_eq!(fmt_info(0x2011), None);
    assert_eq!(fmt_info(0x8011), None);
    assert_eq!(to_host(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], 3), vec![1, 2, 3, 4, 7, 8, 9, 10]);
}
