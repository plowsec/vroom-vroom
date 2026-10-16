//! Service side of the split agent (Windows): the privileged service owns
//! the host port and does what needs privileges (clock, shutdown); what
//! needs the user's desktop (clipboard, resolution, scale) goes to an
//! unprivileged helper in the console session, over a local pipe.
//!
//! The helper is less trusted than the service: it may only send
//! clipboard text towards the host, size-capped like everything else.

use std::io::{self, Write};
use std::sync::mpsc::Receiver;

use vroom_proto::{Decoder, Message};

use crate::port::Writer;

/// Something happened on one of the links.
pub enum Ev {
    Host(Vec<u8>),
    HostGone,
    /// A helper connected; `id` tells its later events apart from an
    /// older helper's.
    HelperUp(u64, Writer),
    Helper(u64, Vec<u8>),
    HelperGone(u64),
}

/// Privileged actions, behind a trait so tests can watch them.
pub trait System {
    fn set_time(&mut self, secs: u64, nanos: u32);
    fn shutdown(&mut self, reboot: bool);
    /// Turn the 3D display device on or off; whether it is on now.
    fn gpu3d(&mut self, on: bool) -> Option<bool>;
}

pub struct RealSystem;

impl System for RealSystem {
    fn set_time(&mut self, secs: u64, nanos: u32) {
        if let Err(e) = crate::sys::set_time(secs, nanos) {
            log::warn!("time sync failed: {e}");
        }
    }
    fn shutdown(&mut self, reboot: bool) {
        crate::sys::shutdown(reboot);
    }
    fn gpu3d(&mut self, on: bool) -> Option<bool> {
        crate::sys::gpu3d(on)
    }
}

pub struct Relay<S: System> {
    sys: S,
    helper: Option<(u64, Writer, Decoder)>,
    max: usize,
    /// The host's last resolution and scale: a helper that starts later
    /// (logon) gets them, so the desktop matches the window right away.
    display: (Option<Message>, Option<Message>),
}

impl<S: System> Relay<S> {
    pub fn new(sys: S, max: usize) -> Self {
        Relay { sys, helper: None, max, display: (None, None) }
    }

    /// Serve one host connection until it goes away. Helper state
    /// survives, so a reconnecting host finds the helper still there.
    pub fn run(&mut self, host: &mut Writer, rx: &Receiver<Ev>) -> io::Result<()> {
        host.write_all(&crate::hello().encode())?;
        let mut dec = Decoder::new(self.max + 64);
        loop {
            match rx.recv().map_err(|_| io::Error::other("event channel closed"))? {
                Ev::Host(b) => {
                    dec.push(&b);
                    loop {
                        match dec.next() {
                            Ok(Some(m)) => self.on_host(m, host)?,
                            Ok(None) => break,
                            Err(e) => log::warn!("bad message from host: {e}"),
                        }
                    }
                }
                Ev::HostGone => return Err(io::Error::other("port closed")),
                Ev::HelperUp(id, w) => {
                    log::info!("session helper {id} connected");
                    self.helper = Some((id, w, Decoder::new(self.max + 64)));
                    for m in [self.display.0.clone(), self.display.1.clone()].into_iter().flatten() {
                        self.send_helper(&m);
                    }
                }
                Ev::Helper(id, b) => self.on_helper(id, &b, host)?,
                Ev::HelperGone(id) => {
                    if self.helper.as_ref().is_some_and(|h| h.0 == id) {
                        log::info!("session helper {id} left");
                        self.helper = None;
                    }
                }
            }
        }
    }

    fn on_host(&mut self, m: Message, host: &mut Writer) -> io::Result<()> {
        match m {
            Message::Hello { agent, .. } => {
                log::info!("host: {agent}");
                host.write_all(&crate::hello().encode())?;
            }
            Message::Ping(v) => host.write_all(&Message::Pong(v).encode())?,
            Message::TimeSync { unix_secs, nanos } => self.sys.set_time(unix_secs, nanos),
            Message::Shutdown { reboot } => self.sys.shutdown(reboot),
            Message::SetResolution { .. } => {
                self.display.0 = Some(m.clone());
                self.send_helper(&m);
            }
            Message::SetScale { .. } => {
                self.display.1 = Some(m.clone());
                self.send_helper(&m);
            }
            m @ Message::ClipboardText(_) => self.send_helper(&m),
            Message::Gpu3d { on } => {
                if let Some(on) = self.sys.gpu3d(on) {
                    host.write_all(&Message::Gpu3d { on }.encode())?;
                }
            }
            Message::Pong(_) => {}
        }
        Ok(())
    }

    fn send_helper(&mut self, m: &Message) {
        if let Some((id, w, _)) = self.helper.as_mut() {
            if let Err(e) = w.write_all(&m.encode()) {
                log::info!("session helper {id}: {e}");
                self.helper = None;
            }
        }
    }

    fn on_helper(&mut self, id: u64, b: &[u8], host: &mut Writer) -> io::Result<()> {
        let Some((cur, _, dec)) = self.helper.as_mut() else { return Ok(()) };
        if *cur != id {
            return Ok(());
        }
        dec.push(b);
        loop {
            match dec.next() {
                Ok(Some(Message::ClipboardText(t))) if t.len() <= self.max => {
                    host.write_all(&Message::ClipboardText(t).encode())?;
                }
                Ok(Some(_)) => log::debug!("session helper {id}: ignoring a message it may not send"),
                Ok(None) => return Ok(()),
                Err(e) => {
                    log::warn!("session helper {id}: {e}; dropping it");
                    self.helper = None;
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Seen {
        time: Vec<u64>,
        shutdown: Vec<bool>,
        gpu3d: Vec<bool>,
    }
    struct Fake(Arc<Mutex<Seen>>);
    impl System for Fake {
        fn set_time(&mut self, secs: u64, _n: u32) {
            self.0.lock().unwrap().time.push(secs);
        }
        fn shutdown(&mut self, reboot: bool) {
            self.0.lock().unwrap().shutdown.push(reboot);
        }
        fn gpu3d(&mut self, on: bool) -> Option<bool> {
            self.0.lock().unwrap().gpu3d.push(on);
            Some(on)
        }
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Sink {
        fn msgs(&self) -> Vec<Message> {
            let mut d = Decoder::new(1 << 20);
            d.push(&self.0.lock().unwrap());
            std::iter::from_fn(|| d.next().unwrap()).collect()
        }
    }

    /// Feed events, then end the host link; returns what the relay did.
    fn run(evs: Vec<Ev>) -> (Vec<Message>, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut r = Relay::new(Fake(seen.clone()), 1024);
        let host = Sink::default();
        let (tx, rx): (Sender<Ev>, _) = channel();
        for e in evs {
            tx.send(e).unwrap();
        }
        tx.send(Ev::HostGone).unwrap();
        let mut hw: Writer = Box::new(host.clone());
        assert!(r.run(&mut hw, &rx).is_err());
        (host.msgs(), seen)
    }

    fn host(m: Message) -> Ev {
        Ev::Host(m.encode())
    }

    #[test]
    fn gpu3d_is_switched_by_the_service_and_answered() {
        let helper = Sink::default();
        let (out, seen) = run(vec![Ev::HelperUp(1, Box::new(helper.clone())), host(Message::Gpu3d { on: false }), host(Message::Gpu3d { on: true })]);
        assert_eq!(seen.lock().unwrap().gpu3d, vec![false, true]);
        assert_eq!(out[1..], [Message::Gpu3d { on: false }, Message::Gpu3d { on: true }]);
        assert!(helper.msgs().is_empty());
    }

    #[test]
    fn privileged_messages_stay_in_the_service() {
        let helper = Sink::default();
        let (out, seen) = run(vec![
            Ev::HelperUp(1, Box::new(helper.clone())),
            host(Message::TimeSync { unix_secs: 42, nanos: 0 }),
            host(Message::Shutdown { reboot: true }),
            host(Message::Ping(7)),
        ]);
        assert_eq!(seen.lock().unwrap().time, vec![42]);
        assert_eq!(seen.lock().unwrap().shutdown, vec![true]);
        assert!(matches!(out[0], Message::Hello { .. }));
        assert!(out.iter().any(|m| matches!(m, Message::Pong(7))));
        assert!(helper.msgs().is_empty());
    }

    #[test]
    fn desktop_messages_go_to_the_helper() {
        let helper = Sink::default();
        run(vec![
            Ev::HelperUp(1, Box::new(helper.clone())),
            host(Message::ClipboardText("hi".into())),
            host(Message::SetResolution { width: 800, height: 600 }),
        ]);
        let m = helper.msgs();
        assert!(matches!(&m[0], Message::ClipboardText(t) if t == "hi"));
        assert!(matches!(m[1], Message::SetResolution { width: 800, height: 600 }));
    }

    #[test]
    fn a_late_helper_gets_the_current_display() {
        let helper = Sink::default();
        run(vec![
            host(Message::SetResolution { width: 800, height: 600 }),
            host(Message::SetScale { percent: 150 }),
            host(Message::SetResolution { width: 1600, height: 1200 }),
            host(Message::SetScale { percent: 200 }),
            host(Message::ClipboardText("old".into())),
            Ev::HelperUp(1, Box::new(helper.clone())),
        ]);
        assert_eq!(helper.msgs(), vec![Message::SetResolution { width: 1600, height: 1200 }, Message::SetScale { percent: 200 }]);
    }

    #[test]
    fn helper_may_only_send_clipboard_text() {
        let (out, seen) = run(vec![
            Ev::HelperUp(1, Box::new(Sink::default())),
            Ev::Helper(1, Message::ClipboardText("copied".into()).encode()),
            Ev::Helper(1, Message::Shutdown { reboot: false }.encode()),
            Ev::Helper(1, Message::ClipboardText("x".repeat(2000)).encode()),
        ]);
        let clips: Vec<_> = out.iter().filter_map(|m| if let Message::ClipboardText(t) = m { Some(t.clone()) } else { None }).collect();
        assert_eq!(clips, vec!["copied".to_string()]);
        assert!(seen.lock().unwrap().shutdown.is_empty());
    }

    #[test]
    fn stale_helper_is_ignored() {
        let (old, new) = (Sink::default(), Sink::default());
        let (out, _) = run(vec![
            Ev::HelperUp(1, Box::new(old.clone())),
            Ev::HelperUp(2, Box::new(new.clone())),
            Ev::Helper(1, Message::ClipboardText("old".into()).encode()),
            Ev::HelperGone(1),
            host(Message::ClipboardText("to new".into())),
        ]);
        assert!(!out.iter().any(|m| matches!(m, Message::ClipboardText(_))));
        assert!(old.msgs().is_empty());
        assert_eq!(new.msgs().len(), 1);
    }
}
