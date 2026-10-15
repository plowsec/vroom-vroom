//! Guest screen -> Slint image, converting only what changed.
//!
//! The VMM says which rectangle of the shared framebuffer changed. We keep
//! two RGBA buffers and alternate: the one on screen stays untouched (Slint
//! may still be reading it), the other gets the rectangles that changed
//! since it was last shown and becomes the next image. A blinking cursor
//! on a 5K screen then costs a few rows, not a 59 MB conversion.

use slint::{Rgba8Pixel, SharedPixelBuffer};
use vroom_display::SharedFb;

/// A rectangle in guest pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn full(w: u32, h: u32) -> Rect {
        Rect { x: 0, y: 0, w, h }
    }

    /// The part inside a `mw` x `mh` screen; None if nothing is. Values
    /// come from the VMM: no overflow whatever they are.
    pub fn clip(self, mw: u32, mh: u32) -> Option<Rect> {
        let x2 = self.x.saturating_add(self.w).min(mw);
        let y2 = self.y.saturating_add(self.h).min(mh);
        (self.x < x2 && self.y < y2).then(|| Rect { x: self.x, y: self.y, w: x2 - self.x, h: y2 - self.y })
    }

    pub fn union(self, o: Rect) -> Rect {
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        let x2 = (self.x + self.w).max(o.x + o.w);
        let y2 = (self.y + self.h).max(o.y + o.h);
        Rect { x, y, w: x2 - x, h: y2 - y }
    }
}

/// `a` grown by `b` (either may be empty).
pub fn add(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.union(b)),
        (a, b) => a.or(b),
    }
}

/// 0RGB (little-endian u32, as the framebuffer holds it) to RGBA bytes.
#[inline]
fn rgba(p: u32) -> [u8; 4] {
    [(p >> 16) as u8, (p >> 8) as u8, p as u8, 255]
}

#[derive(Default)]
pub struct FrameBufs {
    size: (u32, u32),
    bufs: [Option<SharedPixelBuffer<Rgba8Pixel>>; 2],
    /// What each buffer lacks compared with the framebuffer.
    stale: [Option<Rect>; 2],
    /// The buffer last handed out (on screen).
    shown: usize,
    row: Vec<u32>,
}

impl FrameBufs {
    /// Bring the off-screen buffer up to date with `fb` (mode `w` x `h`;
    /// `dirty` changed since the last call) and return it as the next
    /// image. None if nothing changed.
    pub fn update(&mut self, fb: &SharedFb, w: u32, h: u32, dirty: Option<Rect>) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
        if w == 0 || h == 0 {
            return None;
        }
        if self.size != (w, h) || self.bufs.iter().any(|b| b.is_none()) {
            self.size = (w, h);
            self.bufs = [Some(SharedPixelBuffer::new(w, h)), Some(SharedPixelBuffer::new(w, h))];
            self.stale = [Some(Rect::full(w, h)); 2];
        } else {
            let d = dirty.and_then(|r| r.clip(w, h));
            self.stale = [add(self.stale[0], d), add(self.stale[1], d)];
        }
        let next = 1 - self.shown;
        let r = self.stale[next].take()?;
        let buf = self.bufs[next].as_mut()?;
        // Not shared any more once Slint let go of it; if it still holds
        // it, this copies (correct, just slower).
        let px = buf.make_mut_slice();
        self.row.resize(r.w as usize, 0);
        for y in r.y..r.y + r.h {
            if !fb.read_row(w, r.x, y, &mut self.row) {
                break;
            }
            let o = y as usize * w as usize + r.x as usize;
            for (d, &p) in px[o..o + r.w as usize].iter_mut().zip(&self.row) {
                let [r, g, b, a] = rgba(p);
                *d = Rgba8Pixel { r, g, b, a };
            }
        }
        self.shown = next;
        Some(buf.clone())
    }

    /// The picture on screen, if any.
    pub fn current(&self) -> Option<&SharedPixelBuffer<Rgba8Pixel>> {
        self.bufs[self.shown].as_ref().filter(|_| self.size.0 > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(fb: &SharedFb, w: u32, r: Rect, v: u32) {
        let row = vec![v; r.w as usize];
        let bytes: Vec<u8> = row.iter().flat_map(|p| p.to_le_bytes()).collect();
        for y in r.y..r.y + r.h {
            fb.write_rect(w, r.x, y, r.w, 1, &bytes, bytes.len());
        }
    }

    fn px(b: &SharedPixelBuffer<Rgba8Pixel>, w: u32, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let p = b.as_slice()[(y * w + x) as usize];
        (p.r, p.g, p.b, p.a)
    }

    #[test]
    fn rects_clip_and_grow() {
        assert_eq!(Rect { x: 5, y: 5, w: 10, h: 10 }.clip(8, 100), Some(Rect { x: 5, y: 5, w: 3, h: 10 }));
        assert_eq!(Rect { x: 9, y: 0, w: 1, h: 1 }.clip(8, 8), None);
        assert_eq!(Rect { x: u32::MAX, y: u32::MAX, w: u32::MAX, h: 9 }.clip(8, 8), None);
        assert_eq!(Rect { x: 0, y: 0, w: 0, h: 5 }.clip(8, 8), None);
        let u = Rect { x: 1, y: 1, w: 2, h: 2 }.union(Rect { x: 5, y: 0, w: 1, h: 1 });
        assert_eq!(u, Rect { x: 1, y: 0, w: 5, h: 3 });
        assert_eq!(add(None, Some(u)), Some(u));
    }

    #[test]
    fn both_buffers_catch_up() {
        let fb = SharedFb::create().unwrap();
        let (w, h) = (64, 32);
        fill(&fb, w, Rect::full(w, h), 0x00102030);
        let mut f = FrameBufs::default();
        let a = f.update(&fb, w, h, None).unwrap();
        assert_eq!(px(&a, w, 63, 31), (0x10, 0x20, 0x30, 255));
        // A change shows in the next image...
        fill(&fb, w, Rect { x: 3, y: 4, w: 2, h: 2 }, 0x00ff0000);
        let b = f.update(&fb, w, h, Some(Rect { x: 3, y: 4, w: 2, h: 2 })).unwrap();
        assert_eq!(px(&b, w, 4, 5), (0xff, 0, 0, 255));
        assert_eq!(px(&b, w, 63, 31), (0x10, 0x20, 0x30, 255));
        // ...and in the one after, which is the other buffer.
        fill(&fb, w, Rect { x: 0, y: 0, w: 1, h: 1 }, 0x000000ff);
        drop((a, b));
        let c = f.update(&fb, w, h, Some(Rect { x: 0, y: 0, w: 1, h: 1 })).unwrap();
        assert_eq!(px(&c, w, 4, 5), (0xff, 0, 0, 255));
        assert_eq!(px(&c, w, 0, 0), (0, 0, 0xff, 255));
        // Nothing changed: nothing to show. Hostile rects are clipped.
        assert!(f.update(&fb, w, h, None).is_some(), "the other buffer still lags one rect");
        assert!(f.update(&fb, w, h, None).is_none());
        assert!(f.update(&fb, w, h, Some(Rect { x: u32::MAX, y: 0, w: 5, h: 5 })).is_none());
        // A new mode starts over.
        let d = f.update(&fb, 16, 16, None).unwrap();
        assert_eq!((d.width(), d.height()), (16, 16));
    }

    /// `cargo test --release -p vroom-gui frame_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn frame_cost() {
        use std::time::Instant;
        let fb = SharedFb::create().unwrap();
        let (w, h) = (5120u32, 2880u32);
        fill(&fb, w, Rect::full(w, h), 0x00336699);
        let n = 30;
        // What the window did before: copy it all, convert it all into a
        // fresh buffer, every frame.
        let mut pixels = vec![0u32; (w * h) as usize];
        let t = Instant::now();
        for _ in 0..n {
            fb.read_into(w, h, &mut pixels);
            let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w, h);
            for (o, p) in buf.make_mut_slice().iter_mut().zip(&pixels) {
                *o = Rgba8Pixel { r: (p >> 16) as u8, g: (p >> 8) as u8, b: *p as u8, a: 255 };
            }
            std::hint::black_box(&buf);
        }
        let old = t.elapsed() / n;
        let mut f = FrameBufs::default();
        f.update(&fb, w, h, None);
        let full = Rect::full(w, h);
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(f.update(&fb, w, h, Some(full)));
        }
        let new_full = t.elapsed() / n;
        // A terminal line changing (1920 x 40 px).
        let line = Rect { x: 100, y: 1000, w: 1920, h: 40 };
        let t = Instant::now();
        for _ in 0..n {
            std::hint::black_box(f.update(&fb, w, h, Some(line)));
        }
        let new_line = t.elapsed() / n;
        println!("5120x2880 frame: before {old:?}, now {new_full:?} (whole screen changed), {new_line:?} (one text line)");
    }
}
