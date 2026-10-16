//! The parts of Coherence on X11 that are plain arithmetic, apart so they
//! are tested everywhere: where a window can be seen, its icon, the order
//! of windows on screen.

use crate::coherence::Icon;

/// A rectangle: x, y, width, height (pixels of the root window).
pub type Rect = (i32, i32, u32, u32);

/// Borders around a window: left, right, top, bottom.
pub type Extents = [u32; 4];

/// What can be seen of a window: the client's rectangle, plus the window
/// manager's frame around it (`_NET_FRAME_EXTENTS`), less the shadows a
/// GTK app draws inside its own window (`_GTK_FRAME_EXTENTS`).
pub fn visible(client: Rect, frame: Extents, shadow: Extents) -> Rect {
    let (x, y, w, h) = client;
    let [fl, fr, ft, fb] = frame.map(|v| v.min(4096) as i64);
    let [sl, sr, st, sb] = shadow.map(|v| v.min(4096) as i64);
    let w = (w as i64 + fl + fr - sl - sr).max(1);
    let h = (h as i64 + ft + fb - st - sb).max(1);
    ((x as i64 - fl + sl) as i32, (y as i64 - ft + st) as i32, w as u32, h as u32)
}

/// The other way: the client rectangle that shows as `vis`.
pub fn client(vis: Rect, frame: Extents, shadow: Extents) -> Rect {
    let (x, y, w, h) = vis;
    let [fl, fr, ft, fb] = frame.map(|v| v.min(4096) as i64);
    let [sl, sr, st, sb] = shadow.map(|v| v.min(4096) as i64);
    let w = (w as i64 - fl - fr + sl + sr).max(1);
    let h = (h as i64 - ft - fb + st + sb).max(1);
    ((x as i64 + fl - sl) as i32, (y as i64 + ft - st) as i32, w as u32, h as u32)
}

/// The icon to use from `_NET_WM_ICON` (width, height, then that many
/// ARGB pixels, again for each size): the smallest at least `want` on a
/// side, else the biggest, as RGBA. Anything that doesn't add up ends the
/// list.
pub fn pick_icon(data: &[u32], want: u32) -> Option<Icon> {
    let mut sizes = Vec::new();
    let mut o = 0usize;
    while o + 2 <= data.len() && sizes.len() < 32 {
        let (w, h) = (data[o], data[o + 1]);
        if w == 0 || h == 0 || w > 1024 || h > 1024 {
            break;
        }
        let n = w as usize * h as usize;
        if data.len() - o - 2 < n {
            break;
        }
        sizes.push((w, h, o + 2));
        o += 2 + n;
    }
    let side = |s: &(u32, u32, usize)| s.0.max(s.1);
    let best = sizes.iter().filter(|s| side(s) >= want).min_by_key(|s| side(s)).or_else(|| sizes.iter().max_by_key(|s| side(s)))?;
    let (w, h, at) = *best;
    let rgba: Vec<u8> = data[at..at + (w * h) as usize].iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, (p >> 24) as u8]).collect();
    crate::coherence::fit(w, h, &rgba, want)
}

/// A main window of an app: a normal window (or one without a type), not
/// a dialog of another (`WM_TRANSIENT_FOR`), not kept off the taskbar.
pub fn is_app(normal: bool, transient: bool, skip_taskbar: bool) -> bool {
    normal && !transient && !skip_taskbar
}

/// What one of the root's children is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Top {
    /// The window manager's frame of this client (or the client itself),
    /// and whether it is on screen (mapped).
    Frame(u32, bool),
    /// A window the window manager leaves alone (menus, tooltips) that is
    /// on screen.
    Popup,
    Other,
}

/// The order windows are in, front to back: the root's children
/// (`tree`, back to front as X gives them) as clients and pop-ups. Pop-ups
/// below every client on screen are the window manager's own (mutter's
/// guard window, which has minimized windows under it) and are left out.
/// Clients whose frame wasn't found go last.
pub fn order(tree: &[(u32, Top)], clients: &[u32]) -> Vec<u32> {
    let lowest = tree.iter().position(|(_, t)| matches!(t, Top::Frame(_, true))).unwrap_or(tree.len());
    let mut out = Vec::new();
    for (i, (w, t)) in tree.iter().enumerate().rev() {
        match t {
            Top::Frame(c, _) if clients.contains(c) && !out.contains(c) => out.push(*c),
            Top::Popup if i > lowest => out.push(*w),
            _ => {}
        }
    }
    for c in clients {
        if !out.contains(c) {
            out.push(*c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_shadows() {
        // A window with a title bar from the window manager.
        let v = visible((100, 50, 400, 300), [1, 1, 30, 1], [0; 4]);
        assert_eq!(v, (99, 20, 402, 331));
        assert_eq!(client(v, [1, 1, 30, 1], [0; 4]), (100, 50, 400, 300));
        // A GTK app drawing its own title bar, with shadows around.
        let v = visible((100, 50, 452, 352), [0; 4], [26, 26, 23, 29]);
        assert_eq!(v, (126, 73, 400, 300));
        assert_eq!(client(v, [0; 4], [26, 26, 23, 29]), (100, 50, 452, 352));
        // Nonsense extents don't wrap.
        assert_eq!(visible((0, 0, 10, 10), [0; 4], [u32::MAX; 4]).2, 1);
    }

    #[test]
    fn icons_are_picked_and_checked() {
        // 1x1 red, then 2x2 green (ARGB).
        let mut d = vec![1, 1, 0xffff0000, 2, 2];
        d.extend([0x8000ff00; 4]);
        let i = pick_icon(&d, 2).unwrap();
        assert_eq!((i.w, i.h), (2, 2));
        assert_eq!(&i.rgba[..4], &[0, 255, 0, 128]);
        let i = pick_icon(&d, 1).unwrap();
        assert_eq!(i.rgba, vec![255, 0, 0, 255], "the smallest big enough");
        let i = pick_icon(&d, 64).unwrap();
        assert_eq!(i.w, 2, "none big enough: the biggest");
        // Lies about the size: the good part is kept.
        let mut bad = d.clone();
        bad.extend([5000, 1, 1]);
        assert_eq!(pick_icon(&bad, 2).unwrap().w, 2);
        assert!(pick_icon(&[3, 3, 1], 2).is_none());
        assert!(pick_icon(&[], 2).is_none());
        // Big ones are made smaller.
        let mut big = vec![256, 256];
        big.extend(vec![0xff000000u32; 256 * 256]);
        assert_eq!(pick_icon(&big, 32).unwrap().w, 32);
    }

    #[test]
    fn windows_go_front_to_back() {
        use Top::*;
        // Back to front: a minimized window, the window manager's guard,
        // frames of 10 and 11, a menu, something unknown.
        let tree = [(9, Frame(13, false)), (1, Popup), (2, Frame(10, true)), (3, Other), (4, Frame(11, true)), (5, Popup), (6, Other)];
        assert_eq!(order(&tree, &[10, 11, 12, 13]), vec![5, 11, 10, 13, 12]);
        assert_eq!(order(&tree, &[11]), vec![5, 11], "frames of windows no longer listed are skipped");
    }
}
