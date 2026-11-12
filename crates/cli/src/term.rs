//! Raw terminal mode for the serial console. Restored on drop.

#[derive(Debug)]
pub struct RawTerminal {
    saved: Option<libc::termios>,
}

impl RawTerminal {
    pub fn enable() -> RawTerminal {
        // SAFETY: isatty has no memory-safety preconditions.
        if unsafe { libc::isatty(0) } != 1 {
            return RawTerminal { saved: None };
        }
        // SAFETY: zeroed termios is a valid out buffer for tcgetattr.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd 0 is a tty; t is a valid termios.
        if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
            return RawTerminal { saved: None };
        }
        let saved = t;
        // SAFETY: t is valid.
        unsafe { libc::cfmakeraw(&mut t) };
        // Keep output post-processing so "\n" still returns the carriage.
        t.c_oflag |= libc::OPOST;
        // SAFETY: fd 0 is a tty; t is valid.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        RawTerminal { saved: Some(saved) }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let Some(t) = self.saved {
            // SAFETY: restoring the attributes we read at enable().
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        }
    }
}

/// Guest bytes for a terminal, with what lets a guest act on the host
/// dropped: OSC 52 (sets the clipboard), DCS, SOS, PM, APC strings, ENQ
/// (answerback). CSI, other OSC and UTF-8 pass untouched (full-screen
/// programs on the serial console keep working). Stateful: a sequence
/// may be split across writes. A dropped string ends at BEL, ST, CAN,
/// SUB, another ESC, or after `DROP_MAX` bytes (the rest then passes:
/// the terminal never saw it start).
#[derive(Debug)]
pub struct GuestFilter<W: std::io::Write> {
    inner: W,
    state: State,
    /// UTF-8 continuation bytes still expected in ground state.
    cont: u8,
    out: Vec<u8>,
}

const DROP_MAX: usize = 64 << 10;
const ESC: u8 = 0x1b;
const ENQ: u8 = 0x05;

#[derive(Debug)]
enum State {
    Ground,
    Esc,
    /// After 0xC2: maybe a C1 control in UTF-8.
    C2,
    /// After an OSC introducer: its number decides. `held`: the bytes so far.
    OscNum { held: Vec<u8>, num: u32 },
    /// Inside a string being dropped; `esc`/`c2`: its last byte began a terminator.
    Drop { n: usize, esc: bool, c2: bool },
}

impl<W: std::io::Write> GuestFilter<W> {
    pub fn new(inner: W) -> Self {
        GuestFilter { inner, state: State::Ground, cont: 0, out: Vec::new() }
    }

    fn drop_string() -> State {
        State::Drop { n: 0, esc: false, c2: false }
    }

    fn filter(&mut self, input: &[u8]) {
        let out = &mut self.out;
        let mut i = 0;
        while i < input.len() {
            let b = input[i];
            match &mut self.state {
                State::Ground => match b {
                    ESC => self.state = State::Esc,
                    ENQ => {}
                    0x80..=0xbf if self.cont > 0 => {
                        self.cont -= 1;
                        out.push(b);
                    }
                    0xc2 => {
                        self.cont = 0;
                        self.state = State::C2;
                    }
                    0x90 | 0x98 | 0x9e | 0x9f => self.state = Self::drop_string(),
                    0x9d => self.state = State::OscNum { held: vec![b], num: 0 },
                    _ => {
                        self.cont = match b {
                            0xc3..=0xdf => 1,
                            0xe0..=0xef => 2,
                            0xf0..=0xf7 => 3,
                            _ => 0,
                        };
                        out.push(b);
                    }
                },
                State::Esc => match b {
                    b'P' | b'X' | b'^' | b'_' => self.state = Self::drop_string(),
                    b']' => self.state = State::OscNum { held: vec![ESC, b], num: 0 },
                    // ESC ESC: the first one does nothing.
                    ESC | ENQ => {}
                    _ => {
                        out.extend([ESC, b]);
                        self.state = State::Ground;
                    }
                },
                State::C2 => match b {
                    0x90 | 0x98 | 0x9e | 0x9f => self.state = Self::drop_string(),
                    0x9d => self.state = State::OscNum { held: vec![0xc2, b], num: 0 },
                    0x80..=0xbf => {
                        out.extend([0xc2, b]);
                        self.state = State::Ground;
                    }
                    _ => {
                        out.push(0xc2);
                        self.state = State::Ground;
                        continue;
                    }
                },
                State::OscNum { held, num } => match b {
                    b'0'..=b'9' if held.len() < 12 => {
                        held.push(b);
                        *num = *num * 10 + u32::from(b - b'0');
                    }
                    b';' if *num == 52 && held.last().is_some_and(u8::is_ascii_digit) => self.state = Self::drop_string(),
                    _ => {
                        out.extend_from_slice(held);
                        self.state = State::Ground;
                        continue;
                    }
                },
                State::Drop { n, esc, c2 } => {
                    let (was_esc, was_c2) = (*esc, *c2);
                    (*esc, *c2) = (b == ESC, b == 0xc2);
                    *n += 1;
                    if was_esc && b != b'\\' {
                        // A new sequence: the terminal would end the string too.
                        self.state = State::Esc;
                        continue;
                    }
                    let end = matches!(b, 0x07 | 0x18 | 0x1a | 0x9c) || (was_esc && b == b'\\') || (was_c2 && b == 0x9c);
                    if end {
                        self.state = State::Ground;
                    } else if *n >= DROP_MAX {
                        self.state = State::Ground;
                        self.cont = 0;
                    }
                }
            }
            i += 1;
        }
    }
}

impl<W: std::io::Write> std::io::Write for GuestFilter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.filter(buf);
        let r = self.inner.write_all(&self.out);
        self.out.clear();
        r.map(|_| buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty has no memory-safety preconditions.
    unsafe { libc::isatty(fd) == 1 }
}

/// Where guest output goes on stdout: filtered when it is a terminal.
pub fn guest_out() -> Box<dyn std::io::Write + Send> {
    if is_tty(1) {
        Box::new(GuestFilter::new(std::io::stdout()))
    } else {
        Box::new(std::io::stdout())
    }
}

/// Same for stderr.
pub fn guest_err() -> Box<dyn std::io::Write + Send> {
    if is_tty(2) {
        Box::new(GuestFilter::new(std::io::stderr()))
    } else {
        Box::new(std::io::stderr())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Through the filter whole, and one byte per write: the same out.
    fn run(input: &[u8]) -> Vec<u8> {
        let mut whole = GuestFilter::new(Vec::new());
        whole.write_all(input).unwrap();
        let mut split = GuestFilter::new(Vec::new());
        for b in input {
            split.write_all(std::slice::from_ref(b)).unwrap();
        }
        assert_eq!(whole.inner, split.inner, "split writes differ for {input:?}");
        whole.inner
    }

    #[test]
    fn clipboard_and_strings_go() {
        assert_eq!(run(b"a\x1b]52;c;aGVsbG8=\x07b"), b"ab", "OSC 52, BEL");
        assert_eq!(run(b"a\x1b]52;c;aGVsbG8=\x1b\\b"), b"ab", "OSC 52, ST");
        assert_eq!(run(b"a\x1b]052;c;eA==\x07b"), b"ab");
        assert_eq!(run(b"a\x1bP+q544e\x1b\\b"), b"ab", "DCS");
        assert_eq!(run(b"a\x1b_Gf=24;AAAA\x1b\\b"), b"ab", "APC");
        assert_eq!(run(b"a\x1b^pm\x1b\\b\x1bXsos\x07c"), b"abc", "PM, SOS");
        assert_eq!(run("a\u{9d}52;c;eA==\u{9c}b".as_bytes()), b"ab", "C1 in UTF-8");
        assert_eq!(run(b"a\x9d52;c;eA==\x9cb"), b"ab", "raw C1");
        assert_eq!(run(b"a\x90q\x9cb"), b"ab");
        assert_eq!(run(b"a\x05b\x1b\x05[1m"), b"ab\x1b[1m", "ENQ");
        // A new sequence ends a dropped string; it is then read as usual.
        assert_eq!(run(b"\x1bPx\x1b[1mz"), b"\x1b[1mz");
        assert_eq!(run(b"\x1bPx\x1b\x1bPy\x1b\\z"), b"z");
        assert_eq!(run(b"\x1b\x1b[A"), b"\x1b[A");
    }

    #[test]
    fn the_rest_passes() {
        for s in [
            &b"\x1b[2J\x1b[1;1H\x1b[38;5;196mred\x1b[0m"[..],
            b"\x1b]0;title\x07\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\",
            b"\x1b]5;x\x07\x1b]522;y\x07\x1b]52\x07",
            b"\x1b(B\x1b7\x1b8\x1bc",
            "é 日本 🎉".as_bytes(),
            "a\u{75d}b \u{e9d}".as_bytes(),
            b"\r\n\t\x08\x07",
        ] {
            assert_eq!(run(s), s, "{s:?}");
        }
    }

    #[test]
    fn an_unterminated_string_is_capped() {
        let mut f = GuestFilter::new(Vec::new());
        f.write_all(b"\x1b]52;c;").unwrap();
        f.write_all(&vec![b'A'; DROP_MAX + 10]).unwrap();
        f.write_all(b"\x1b[1mvisible").unwrap();
        assert!(f.inner.ends_with(b"\x1b[1mvisible"));
        assert_eq!(f.inner.len(), 12 + b"\x1b[1mvisible".len(), "dropped: the cap, from after \"52;\"");
    }
}
