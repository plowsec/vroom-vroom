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
