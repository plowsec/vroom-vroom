//! Mach ports between our own processes (macOS): what IOSurfaces cross by.
//!
//! An IOSurface can only go to another process as a mach port
//! (`IOSurfaceCreateMachPort`) or by its global id, and global lookup stays
//! off. Unix sockets can't carry mach ports, so a process that starts a
//! helper also hands it a send right to a port of its own: registered as
//! this task's port 0 for the spawn (`mach_ports_register`), which the
//! child inherits and takes with [`inherited`]. No name in the bootstrap
//! namespace, nothing another process could look up.
//!
//! Messages carry one port (a send right) and a few numbers. The receiver
//! learns the sender's pid from the kernel (audit trailer), not from the
//! message.

use std::io;
use std::sync::Mutex;
use std::time::Duration;

#[allow(non_camel_case_types)]
type mach_port_t = u32;
#[allow(non_camel_case_types)]
type kern_return_t = i32;

const KERN_SUCCESS: kern_return_t = 0;
const MACH_PORT_NULL: mach_port_t = 0;
const MACH_PORT_RIGHT_RECEIVE: u32 = 1;
const MACH_MSG_TYPE_MOVE_SEND: u32 = 17;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;
const MACH_SEND_MSG: i32 = 1;
const MACH_RCV_MSG: i32 = 2;
const MACH_SEND_TIMEOUT: i32 = 0x10;
const MACH_RCV_TIMEOUT: i32 = 0x100;
const MACH_RCV_LARGE: i32 = 0x4;
const MACH_RCV_TOO_LARGE: kern_return_t = 0x1000_4004;
const MACH_RCV_TIMED_OUT: kern_return_t = 0x1000_4003;
const MACH_SEND_TIMED_OUT: kern_return_t = 0x1000_0004;
const MACH_SEND_INTERRUPTED: kern_return_t = 0x1000_0007;
// MACH_RCV_TRAILER_TYPE(FORMAT_0) | MACH_RCV_TRAILER_ELEMENTS(AUDIT)
const MACH_RCV_TRAILER_AUDIT: i32 = 3 << 24;
const MACH_PORT_LIMITS_INFO: i32 = 1;
const MACH_PORT_QLIMIT_LARGE: u32 = 1024;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct Header {
    bits: u32,
    size: u32,
    remote: mach_port_t,
    local: mach_port_t,
    voucher: mach_port_t,
    id: i32,
}

#[repr(C, packed(4))]
#[derive(Debug, Default, Clone, Copy)]
struct PortDescriptor {
    name: mach_port_t,
    pad1: u32,
    pad2: u16,
    disposition: u8,
    kind: u8,
}

/// What a message holds besides its port.
pub const WORDS: usize = 6;

#[repr(C, packed(4))]
#[derive(Debug, Default, Clone, Copy)]
struct Wire {
    header: Header,
    descriptors: u32,
    port: PortDescriptor,
    data: [u64; WORDS],
}

#[repr(C, packed(4))]
#[derive(Debug, Default, Clone, Copy)]
struct AuditTrailer {
    kind: u32,
    size: u32,
    seqno: u32,
    sender: [u32; 2],
    audit: [u32; 8],
}

#[repr(C, packed(4))]
#[derive(Debug, Default, Clone, Copy)]
struct Received {
    wire: Wire,
    trailer: AuditTrailer,
    // room for a larger trailer than we asked for
    spare: [u32; 16],
}

extern "C" {
    static mach_task_self_: mach_port_t;
    fn mach_port_allocate(task: mach_port_t, right: u32, name: *mut mach_port_t) -> kern_return_t;
    fn mach_port_insert_right(task: mach_port_t, name: mach_port_t, right: mach_port_t, kind: u32) -> kern_return_t;
    fn mach_port_deallocate(task: mach_port_t, name: mach_port_t) -> kern_return_t;
    fn mach_port_mod_refs(task: mach_port_t, name: mach_port_t, right: u32, delta: i32) -> kern_return_t;
    fn mach_port_set_attributes(task: mach_port_t, name: mach_port_t, flavor: i32, info: *const u32, count: u32) -> kern_return_t;
    fn mach_ports_register(task: mach_port_t, ports: *const mach_port_t, count: u32) -> kern_return_t;
    fn mach_ports_lookup(task: mach_port_t, ports: *mut *mut mach_port_t, count: *mut u32) -> kern_return_t;
    fn mach_msg(msg: *mut Header, option: i32, send_size: u32, rcv_size: u32, rcv_name: mach_port_t, timeout: u32, notify: mach_port_t) -> kern_return_t;
    fn mach_msg_destroy(msg: *mut Header);
    fn vm_deallocate(task: mach_port_t, addr: usize, size: usize) -> kern_return_t;
}

fn task() -> mach_port_t {
    // SAFETY: a variable libSystem sets up before main.
    unsafe { mach_task_self_ }
}

fn err(what: &str, kr: kern_return_t) -> io::Error {
    io::Error::other(format!("{what}: mach error {kr:#x}"))
}

/// One right to a mach port, given back when dropped.
#[derive(Debug)]
pub struct Port {
    name: mach_port_t,
    receive: bool,
}

impl Port {
    /// A new port we receive on, with room for many queued messages.
    pub fn receiver() -> io::Result<Port> {
        let mut name = MACH_PORT_NULL;
        // SAFETY: out pointer to a local.
        let kr = unsafe { mach_port_allocate(task(), MACH_PORT_RIGHT_RECEIVE, &mut name) };
        if kr != KERN_SUCCESS {
            return Err(err("mach_port_allocate", kr));
        }
        let p = Port { name, receive: true };
        let limit = [MACH_PORT_QLIMIT_LARGE];
        // SAFETY: a port we own; one u32 of MACH_PORT_LIMITS_INFO.
        let kr = unsafe { mach_port_set_attributes(task(), name, MACH_PORT_LIMITS_INFO, limit.as_ptr(), 1) };
        if kr != KERN_SUCCESS {
            log::debug!("mach: queue limit: {kr:#x}");
        }
        Ok(p)
    }

    /// A send right to this receive port.
    pub fn sender(&self) -> io::Result<Port> {
        if !self.receive {
            return Err(io::Error::other("not a receive right"));
        }
        // SAFETY: our receive right; adds a send right under the same name.
        let kr = unsafe { mach_port_insert_right(task(), self.name, self.name, MACH_MSG_TYPE_MAKE_SEND) };
        if kr != KERN_SUCCESS {
            return Err(err("mach_port_insert_right", kr));
        }
        Ok(Port { name: self.name, receive: false })
    }

    /// Take ownership of a send right named `name` (one reference).
    ///
    /// # Safety
    /// `name` must be a send right this task holds and nothing else
    /// gives back.
    pub unsafe fn from_send_right(name: u32) -> Port {
        Port { name, receive: false }
    }

    pub fn name(&self) -> u32 {
        self.name
    }

    /// The right, no longer given back on drop.
    pub fn into_raw(self) -> u32 {
        let n = self.name;
        std::mem::forget(self);
        n
    }

    /// Send `data` and a copy of the send right `port` (if any). Fails
    /// rather than wait when the receiver's queue is full.
    pub fn send(&self, id: i32, port: Option<&Port>, data: [u64; WORDS]) -> io::Result<()> {
        self.send_right(id, port.map(|p| (p.name, MACH_MSG_TYPE_COPY_SEND)), data)
    }

    /// Like [`Port::send`], giving the send right away with the message
    /// (it is gone even if the send fails).
    pub fn send_moving(&self, id: i32, port: Port, data: [u64; WORDS]) -> io::Result<()> {
        let name = port.into_raw();
        self.send_right(id, Some((name, MACH_MSG_TYPE_MOVE_SEND)), data)
    }

    fn send_right(&self, id: i32, port: Option<(mach_port_t, u32)>, data: [u64; WORDS]) -> io::Result<()> {
        let (name, disposition) = port.unwrap_or((MACH_PORT_NULL, MACH_MSG_TYPE_COPY_SEND));
        let mut w = Wire {
            header: Header {
                // remote: COPY_SEND, local: none
                bits: MACH_MSG_TYPE_COPY_SEND | MACH_MSGH_BITS_COMPLEX,
                size: std::mem::size_of::<Wire>() as u32,
                remote: self.name,
                id,
                ..Default::default()
            },
            descriptors: 1,
            port: PortDescriptor { name, disposition: disposition as u8, kind: MACH_MSG_PORT_DESCRIPTOR, ..Default::default() },
            data,
        };
        // SAFETY: a complete message in `w`, sized as it says.
        let kr = unsafe { mach_msg(&mut w.header, MACH_SEND_MSG | MACH_SEND_TIMEOUT, w.header.size, 0, MACH_PORT_NULL, 0, MACH_PORT_NULL) };
        if kr == KERN_SUCCESS {
            return Ok(());
        }
        if kr == MACH_SEND_TIMED_OUT || kr == MACH_SEND_INTERRUPTED {
            // Not sent: the rights came back in the buffer.
            // SAFETY: the message as the kernel handed it back.
            unsafe { mach_msg_destroy(&mut w.header) };
        } else if disposition == MACH_MSG_TYPE_MOVE_SEND && name != MACH_PORT_NULL {
            // SAFETY: the right we meant to move.
            unsafe { mach_port_deallocate(task(), name) };
        }
        Err(err("mach_msg send", kr))
    }

    /// The next message, waiting up to `timeout` (None: forever, zero:
    /// only if one is there). Ok(None) when nothing came in time.
    /// Malformed messages are thrown away (their rights given back).
    pub fn recv(&self, timeout: Option<Duration>) -> io::Result<Option<Message>> {
        if !self.receive {
            return Err(io::Error::other("not a receive right"));
        }
        loop {
            let mut r = Received::default();
            let (opt, ms) = match timeout {
                None => (0, 0),
                Some(d) => (MACH_RCV_TIMEOUT, d.as_millis().min(u32::MAX as u128) as u32),
            };
            let size = std::mem::size_of::<Received>() as u32;
            // SAFETY: `r` is `size` bytes, room for our message and trailer.
            let kr = unsafe { mach_msg(&mut r.wire.header, MACH_RCV_MSG | MACH_RCV_LARGE | MACH_RCV_TRAILER_AUDIT | opt, 0, size, self.name, ms, MACH_PORT_NULL) };
            if kr == MACH_RCV_TIMED_OUT {
                return Ok(None);
            }
            if kr == MACH_RCV_TOO_LARGE {
                // Still queued (MACH_RCV_LARGE): take it without room, which
                // destroys it.
                let mut h = Header::default();
                // SAFETY: a zero-size receive into a header we own.
                unsafe { mach_msg(&mut h, MACH_RCV_MSG | MACH_RCV_TIMEOUT, 0, 0, self.name, 0, MACH_PORT_NULL) };
                continue;
            }
            if kr != KERN_SUCCESS {
                return Err(err("mach_msg receive", kr));
            }
            let w = r.wire;
            let p = w.port;
            let ok = w.header.size as usize == std::mem::size_of::<Wire>()
                && w.header.bits & MACH_MSGH_BITS_COMPLEX != 0
                && w.descriptors == 1
                && p.kind == MACH_MSG_PORT_DESCRIPTOR
                && (p.name == MACH_PORT_NULL || p.disposition as u32 == MACH_MSG_TYPE_MOVE_SEND);
            if !ok {
                // SAFETY: the message as received, rights and all.
                unsafe { mach_msg_destroy(&mut r.wire.header) };
                continue;
            }
            // audit_token_t: val[5] is the pid.
            let pid = r.trailer.audit[5] as i32;
            let port = (p.name != MACH_PORT_NULL).then_some(Port { name: p.name, receive: false });
            return Ok(Some(Message { id: w.header.id, pid, port, data: w.data }));
        }
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        // SAFETY: the one right we own under this name.
        unsafe {
            if self.receive {
                mach_port_mod_refs(task(), self.name, MACH_PORT_RIGHT_RECEIVE, -1);
            } else {
                mach_port_deallocate(task(), self.name);
            }
        }
    }
}

/// A message [`Port::recv`] got.
#[derive(Debug)]
pub struct Message {
    pub id: i32,
    /// The sender's pid, from the kernel.
    pub pid: i32,
    /// The send right it carried, now ours.
    pub port: Option<Port>,
    pub data: [u64; WORDS],
}

/// Registered ports are per task: one spawn with ours at a time.
static BEQUEATH: Mutex<()> = Mutex::new(());

/// Run `spawn` with `send` as this task's registered port 0, so the
/// process it starts inherits a send right to it (and passes it on to its
/// own children, unless it takes it with [`inherited`]). Taken off again
/// before returning. The child must be started with posix_spawn (a std
/// `Command` without `pre_exec`): one forked then exec'd ends up with a
/// right to some other port (seen on macOS 15).
pub fn bequeath<R>(send: &Port, spawn: impl FnOnce() -> R) -> R {
    let _g = BEQUEATH.lock().unwrap_or_else(|e| e.into_inner());
    let ports = [send.name];
    // SAFETY: one valid send right; the kernel copies it.
    let kr = unsafe { mach_ports_register(task(), ports.as_ptr(), 1) };
    if kr != KERN_SUCCESS {
        log::warn!("mach: can't register a port for a child: {kr:#x}");
    }
    let r = spawn();
    // SAFETY: an empty list clears the registration.
    unsafe { mach_ports_register(task(), std::ptr::null(), 0) };
    r
}

/// The send right our parent left as registered port 0, if any. Unless
/// `keep`, it is also taken off our own registration, so processes we
/// start don't get it. Call early, before starting anything.
pub fn inherited(keep: bool) -> Option<Port> {
    let mut ports: *mut mach_port_t = std::ptr::null_mut();
    let mut n = 0u32;
    // SAFETY: out pointers to locals; the kernel allocates the array.
    let kr = unsafe { mach_ports_lookup(task(), &mut ports, &mut n) };
    if kr != KERN_SUCCESS || ports.is_null() {
        return None;
    }
    // SAFETY: `n` names the kernel put in the array it allocated.
    let names = unsafe { std::slice::from_raw_parts(ports, n as usize) }.to_vec();
    // SAFETY: the array mach_ports_lookup allocated, no longer used.
    unsafe { vm_deallocate(task(), ports as usize, n as usize * std::mem::size_of::<mach_port_t>()) };
    let mut first = None;
    for (i, &name) in names.iter().enumerate() {
        if name == MACH_PORT_NULL {
            continue;
        }
        if i == 0 {
            first = Some(Port { name, receive: false });
        } else {
            // SAFETY: a send right the lookup gave us; we don't use it.
            unsafe { mach_port_deallocate(task(), name) };
        }
    }
    if !keep {
        // SAFETY: an empty list clears the registration.
        unsafe { mach_ports_register(task(), std::ptr::null(), 0) };
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_and_a_port_cross() {
        let rx = Port::receiver().unwrap();
        let tx = rx.sender().unwrap();
        let other = Port::receiver().unwrap();
        let other_tx = other.sender().unwrap();
        tx.send(7, Some(&other_tx), [1, 2, 3, 4, 5, 6]).unwrap();
        let m = rx.recv(Some(Duration::from_secs(1))).unwrap().unwrap();
        assert_eq!((m.id, m.pid, m.data), (7, std::process::id() as i32, [1, 2, 3, 4, 5, 6]));
        // The right that came works.
        m.port.as_ref().unwrap().send(8, None, [9; WORDS]).unwrap();
        let m2 = other.recv(Some(Duration::ZERO)).unwrap().unwrap();
        assert!(m2.id == 8 && m2.port.is_none() && m2.data == [9; WORDS]);
        assert!(rx.recv(Some(Duration::ZERO)).unwrap().is_none());
    }

    /// A child gets the port at spawn and sends on it; its pid comes with
    /// the message. Run as a child of itself.
    #[test]
    fn a_child_inherits_the_port() {
        const ME: &str = "mach::tests::a_child_inherits_the_port";
        if std::env::var_os("VROOM_MACH_CHILD").is_some() {
            let p = inherited(false).expect("no inherited port");
            assert!(inherited(false).is_none(), "still registered");
            p.send(42, None, [0; WORDS]).unwrap();
            return;
        }
        let rx = Port::receiver().unwrap();
        let tx = rx.sender().unwrap();
        let mut child = bequeath(&tx, || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", ME, "--nocapture", "--test-threads=1"])
                .env("VROOM_MACH_CHILD", "1")
                .spawn()
                .unwrap()
        });
        assert!(inherited(true).is_none(), "registration left behind");
        let m = rx.recv(Some(Duration::from_secs(20))).unwrap().expect("nothing from the child");
        assert_eq!((m.id, m.pid), (42, child.id() as i32));
        assert!(child.wait().unwrap().success());
    }
}
