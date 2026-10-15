//! Logging with a ceiling. Much of what the VMM logs, a guest can trigger
//! (bad queues, bad agent messages...): without a limit a hostile guest
//! could flood the terminal or fill the disk behind a log file. Errors
//! always get through; the rest at most `PER_SEC` lines a second, then one
//! line saying how many were dropped.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use log::{Level, Log, Metadata, Record};

const PER_SEC: u32 = 50;

pub struct Limited {
    inner: env_logger::Logger,
    win: Mutex<(Instant, u32, u64)>,
}

impl Limited {
    pub fn install(inner: env_logger::Logger) {
        let max = inner.filter();
        let l = Limited { inner, win: Mutex::new((Instant::now(), 0, 0)) };
        if log::set_boxed_logger(Box::new(l)).is_ok() {
            log::set_max_level(max);
        }
    }

    /// Room for one more line? Reports what was dropped when a new
    /// window opens.
    fn admit(&self) -> bool {
        let mut w = self.win.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if now.duration_since(w.0) >= Duration::from_secs(1) {
            let dropped = w.2;
            *w = (now, 0, 0);
            if dropped > 0 {
                drop(w);
                self.inner.log(
                    &Record::builder()
                        .level(Level::Warn)
                        .target("bulwark")
                        .args(format_args!("log flood: {dropped} lines dropped"))
                        .build(),
                );
                w = self.win.lock().unwrap_or_else(|e| e.into_inner());
            }
        }
        if w.1 < PER_SEC {
            w.1 += 1;
            true
        } else {
            w.2 += 1;
            false
        }
    }
}

impl Log for Limited {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        self.inner.enabled(m)
    }
    fn log(&self, r: &Record<'_>) {
        if self.inner.matches(r) && (r.level() == Level::Error || self.admit()) {
            self.inner.log(r);
        }
    }
    fn flush(&self) {
        self.inner.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn floods_are_cut_and_counted() {
        let buf = Buf::default();
        let inner = env_logger::Builder::new()
            .filter_level(log::LevelFilter::Info)
            .target(env_logger::Target::Pipe(Box::new(buf.clone())))
            .build();
        let l = Limited { inner, win: Mutex::new((Instant::now(), 0, 0)) };
        let warn = |l: &Limited| l.log(&Record::builder().level(Level::Warn).target("t").args(format_args!("guest did x")).build());
        for _ in 0..200 {
            warn(&l);
        }
        l.log(&Record::builder().level(Level::Error).target("t").args(format_args!("real error")).build());
        let lines = |b: &Buf| String::from_utf8(b.0.lock().unwrap().clone()).unwrap();
        let out = lines(&buf);
        assert_eq!(out.matches("guest did x").count(), PER_SEC as usize);
        assert!(out.contains("real error"), "errors always pass");
        std::thread::sleep(Duration::from_millis(1100));
        warn(&l);
        let out = lines(&buf);
        assert!(out.contains("log flood: 150 lines dropped"), "{out}");
    }
}
