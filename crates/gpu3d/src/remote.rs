//! The 3D renderer in a process of its own (the GPU process), as browsers
//! run their GPU code: virglrenderer, the GL driver, MoltenVK and DXMT
//! parse what the guest sends, so they live where a bug can reach the GPU
//! and the guest's own RAM, nothing else (no disks, no network, no
//! hypervisor). The VMM keeps the virtio-gpu device and talks to it here.
//!
//! [`Remote`] is the VMM's side, a [`Renderer3d`] like the in-process one.
//! [`serve`] is the GPU process's side, around a [`crate::Virgl`].
//!
//! The link: a stream socket for calls (one at a time, each answered),
//! and a second one the GPU process writes a byte to when fences may have
//! finished. Guest RAM is shared memory both map; buffers the guest maps
//! (blobs) are shared memory the GPU process hands over as fds. Frames
//! come back as bytes.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::raw::c_void;
use std::os::unix::net::UnixStream;

use vroom_memory::GuestMemory;
use vroom_virtio::devices::gpu3d::{Blob, Box3d, Create3d, FenceDone, HostIov, Mapping, Renderer3d, Transfer3d};

/// No call or answer is bigger (a frame of 16384² pixels is 1 GiB; GL
/// scanouts are far smaller, snapshot levels are read in pieces).
const MAX_FRAME: usize = 1 << 30;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Capset = 1,
    CtxCreate,
    CtxDestroy,
    CtxAttach,
    CtxDetach,
    Submit,
    Create3d,
    AttachBacking,
    DetachBacking,
    Transfer,
    CreateBlob,
    MapBlob,
    UnmapBlob,
    Export,
    ReadBack,
    RowBytes,
    ReadLevel,
    WriteLevel,
    ScanoutStart,
    ScanoutPoll,
    Unref,
    CreateFence,
    Poll,
    Reset,
    Remap,
    Frame,
}

impl Op {
    fn from(b: u8) -> Option<Op> {
        use Op::*;
        Some(match b {
            1 => Capset,
            2 => CtxCreate,
            3 => CtxDestroy,
            4 => CtxAttach,
            5 => CtxDetach,
            6 => Submit,
            7 => Create3d,
            8 => AttachBacking,
            9 => DetachBacking,
            10 => Transfer,
            11 => CreateBlob,
            12 => MapBlob,
            13 => UnmapBlob,
            14 => Export,
            15 => ReadBack,
            16 => RowBytes,
            17 => ReadLevel,
            18 => WriteLevel,
            19 => ScanoutStart,
            20 => ScanoutPoll,
            21 => Unref,
            22 => CreateFence,
            23 => Poll,
            24 => Reset,
            25 => Remap,
            26 => Frame,
            _ => return None,
        })
    }
}

#[derive(Default)]
struct Enc(Vec<u8>);

impl Enc {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
        self
    }
    fn bx(&mut self, b: &Box3d) -> &mut Self {
        for v in [b.x, b.y, b.z, b.w, b.h, b.d] {
            self.u32(v);
        }
        self
    }
}

struct Dec<'a>(&'a [u8]);

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    fn bx(&mut self) -> Option<Box3d> {
        Some(Box3d { x: self.u32()?, y: self.u32()?, z: self.u32()?, w: self.u32()?, h: self.u32()?, d: self.u32()? })
    }
}

fn write_frame(s: &mut UnixStream, b: &[u8]) -> std::io::Result<()> {
    s.write_all(&(b.len() as u32).to_le_bytes())?;
    s.write_all(b)
}

fn read_frame(s: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut n = [0u8; 4];
    s.read_exact(&mut n)?;
    let n = u32::from_le_bytes(n) as usize;
    if n > MAX_FRAME {
        return Err(std::io::Error::other("frame too large"));
    }
    let mut b = vec![0u8; n];
    s.read_exact(&mut b)?;
    Ok(b)
}

/// One fd, with a byte so the message isn't empty.
#[allow(unsafe_code)]
fn send_fd(s: &UnixStream, fd: RawFd) -> std::io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr() as *mut c_void, iov_len: 1 };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: zeroed msghdr is valid; the pointers below outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut c_void;
    msg.msg_controllen = space as _;
    // SAFETY: msg has room for one cmsg of one fd (CMSG_SPACE above).
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, fd);
    }
    // SAFETY: valid socket and message.
    if unsafe { libc::sendmsg(s.as_raw_fd(), &msg, 0) } != 1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[allow(unsafe_code)]
fn recv_fd(s: &UnixStream) -> std::io::Result<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr() as *mut c_void, iov_len: 1 };
    // SAFETY: pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: zeroed msghdr is valid; the pointers below outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut c_void;
    msg.msg_controllen = space as _;
    // SAFETY: valid socket and message buffers.
    let n = unsafe { libc::recvmsg(s.as_raw_fd(), &mut msg, 0) };
    if n != 1 {
        return Err(if n == 0 { std::io::ErrorKind::UnexpectedEof.into() } else { std::io::Error::last_os_error() });
    }
    // SAFETY: the kernel filled msg; we only read the first cmsg.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if c.is_null() || (*c).cmsg_level != libc::SOL_SOCKET || (*c).cmsg_type != libc::SCM_RIGHTS {
            return Err(std::io::Error::other("no fd"));
        }
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const RawFd);
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

/// Map `fd` (shared memory) `size` bytes, read-write or read-only.
#[allow(unsafe_code)]
fn map_fd(fd: RawFd, size: u64, write: bool) -> Option<*mut u8> {
    let prot = libc::PROT_READ | if write { libc::PROT_WRITE } else { 0 };
    // SAFETY: a fresh shared mapping of an object we were handed.
    let p = unsafe { libc::mmap(std::ptr::null_mut(), size as usize, prot, libc::MAP_SHARED, fd, 0) };
    (p != libc::MAP_FAILED).then_some(p as *mut u8)
}

#[allow(unsafe_code)]
fn unmap(p: *mut u8, size: u64) {
    // SAFETY: a mapping we made with map_fd.
    unsafe { libc::munmap(p as *mut c_void, size as usize) };
}

/// Read a non-blocking fd until it is empty.
#[allow(unsafe_code)]
fn drain(fd: RawFd) {
    let mut buf = [0u8; 256];
    // SAFETY: reads into a local buffer.
    while unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) } > 0 {}
}

// ---- the VMM's side ----

/// The renderer, in the GPU process.
pub struct Remote {
    sock: UnixStream,
    events: UnixStream,
    capsets: Vec<(u32, u32, u32)>,
    mem: GuestMemory,
    /// Guest RAM generation the GPU process maps.
    mapped_gen: u64,
    /// Blobs the guest maps, mapped here too: resource -> (ptr, size).
    maps: HashMap<u32, (*mut u8, u64)>,
    /// Read-only views of blobs shown on screen.
    views: HashMap<u32, (*mut u8, u64)>,
    blob_sizes: HashMap<u32, u64>,
    granule: u64,
    /// Frames read back come through this shared memory, not the socket
    /// (one per guest frame with GL: gigabytes a second).
    frame: Option<vroom_memory::MmapRegion>,
    /// None in tests (the other side is a thread).
    child: Option<std::process::Child>,
    dead: bool,
}

// SAFETY: the pointers are our own mappings, used from the GPU worker
// thread that owns this.
#[allow(unsafe_code)]
unsafe impl Send for Remote {}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote").field("pid", &self.pid()).field("capsets", &self.capsets).finish()
    }
}

/// How the GPU process starts.
#[derive(Debug)]
pub struct HelperArgs<'a> {
    /// `vroom` (it runs `vroom gpu-helper`).
    pub exe: &'a std::path::Path,
    /// "apple", "angle", or None (no GL).
    pub gl: Option<&'a str>,
    pub gl_renderer: Option<&'a str>,
    pub map_granule: u64,
    /// Where DXMT keeps the Metal versions of guest shaders (the only
    /// place the GPU process may write).
    pub shaders: &'a std::path::Path,
}

impl Remote {
    /// Start the GPU process and wait until its renderer is up.
    #[allow(unsafe_code)]
    pub fn spawn(a: &HelperArgs, mem: GuestMemory) -> std::io::Result<Remote> {
        let (fds, _) = mem.shared_fds().ok_or_else(|| std::io::Error::other("guest RAM isn't shared memory"))?;
        let (sock, theirs) = UnixStream::pair()?;
        let (events, their_events) = UnixStream::pair()?;
        let mut cmd = std::process::Command::new(a.exe);
        cmd.arg("gpu-helper")
            .arg("--sock")
            .arg(theirs.as_raw_fd().to_string())
            .arg("--events")
            .arg(their_events.as_raw_fd().to_string())
            .arg("--granule")
            .arg(a.map_granule.to_string())
            .arg("--shaders")
            .arg(a.shaders);
        if let Some(g) = a.gl {
            cmd.arg("--gl").arg(g);
        }
        if let Some(r) = a.gl_renderer {
            cmd.arg("--gl-renderer").arg(r);
        }
        for (fd, size) in &fds {
            cmd.arg("--ram").arg(format!("{fd}:{size}"));
        }
        let keep: Vec<RawFd> = [theirs.as_raw_fd(), their_events.as_raw_fd()].into_iter().chain(fds.iter().map(|f| f.0)).collect();
        // SAFETY: only clears close-on-exec on fds we own, in the child.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut cmd, move || {
                for &fd in &keep {
                    libc::fcntl(fd, libc::F_SETFD, 0);
                }
                Ok(())
            });
        }
        let mut child = cmd.stdin(std::process::Stdio::null()).spawn()?;
        drop((theirs, their_events));
        let pid = child.id();
        // Hello: the capsets, once its renderer is up.
        if let Err(e) = sock.set_read_timeout(Some(std::time::Duration::from_secs(30))) {
            let _ = child.kill();
            return Err(e);
        }
        match Remote::connect(sock, events, mem, a.map_granule) {
            Ok(mut r) => {
                r.child = Some(child);
                log::info!("gpu: GPU process {pid} up, capsets {:?}", r.capsets);
                Ok(r)
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }

    /// The VMM's end of a GPU process already started: reads its hello.
    pub fn connect(sock: UnixStream, events: UnixStream, mem: GuestMemory, map_granule: u64) -> std::io::Result<Remote> {
        let (_, gen) = mem.shared_fds().ok_or_else(|| std::io::Error::other("guest RAM isn't shared memory"))?;
        let mut r = Remote {
            sock,
            events,
            capsets: Vec::new(),
            mem,
            mapped_gen: gen,
            maps: HashMap::new(),
            views: HashMap::new(),
            blob_sizes: HashMap::new(),
            granule: map_granule.max(4096),
            frame: None,
            child: None,
            dead: false,
        };
        let hello = read_frame(&mut r.sock)?;
        r.sock.set_read_timeout(None)?;
        let mut d = Dec(&hello);
        let n = d.u32().ok_or_else(|| std::io::Error::other("bad hello"))?;
        for _ in 0..n.min(64) {
            let (Some(id), Some(v), Some(s)) = (d.u32(), d.u32(), d.u32()) else { break };
            r.capsets.push((id, v, s));
        }
        r.events.set_nonblocking(true)?;
        Ok(r)
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().map_or(0, |c| c.id())
    }

    /// One call and its answer. None if the GPU process is gone.
    fn call(&mut self, op: Op, f: impl FnOnce(&mut Enc)) -> Option<Vec<u8>> {
        if self.dead {
            return None;
        }
        let mut e = Enc::default();
        e.u8(op as u8);
        f(&mut e);
        let r = write_frame(&mut self.sock, &e.0).and_then(|_| read_frame(&mut self.sock));
        match r {
            Ok(b) => Some(b),
            Err(e) => {
                log::error!("gpu: the GPU process is gone ({e}): no more 3D for this VM until it restarts");
                self.dead = true;
                None
            }
        }
    }

    fn ok(&mut self, op: Op, f: impl FnOnce(&mut Enc)) -> bool {
        self.call(op, f).is_some_and(|b| b.first() == Some(&1))
    }

    /// Guest RAM ranges as (region, offset, length), which the GPU process
    /// finds in its own mapping. Sends the RAM again first if a reset
    /// replaced it.
    fn ranges(&mut self, iov: &HostIov) -> Option<Vec<(u32, u64, u64)>> {
        if self.mem.generation() != self.mapped_gen {
            self.remap()?;
        }
        let regions: Vec<(usize, usize)> = self.mem.regions().map(|r| (r.host_ptr() as usize, r.len() as usize)).collect();
        iov.iter()
            .map(|&(p, len)| {
                let a = p as usize;
                let (i, (base, size)) = regions.iter().enumerate().find(|(_, (b, s))| a >= *b && a < b + s)?;
                (a + len <= base + size).then_some((i as u32, (a - base) as u64, len as u64))
            })
            .collect()
    }

    /// After a reset gave the guest new RAM: the GPU process maps the new
    /// objects (the call, then one fd per region, then the answer).
    fn remap(&mut self) -> Option<()> {
        let (fds, gen) = self.mem.shared_fds()?;
        let mut e = Enc::default();
        e.u8(Op::Remap as u8).u32(fds.len() as u32);
        for (_, size) in &fds {
            e.u64(*size);
        }
        let r = write_frame(&mut self.sock, &e.0)
            .and_then(|_| fds.iter().try_for_each(|(fd, _)| send_fd(&self.sock, *fd)))
            .and_then(|_| read_frame(&mut self.sock));
        match r {
            Ok(b) if b.first() == Some(&1) => {
                self.mapped_gen = gen;
                Some(())
            }
            Ok(_) => None,
            Err(e) => {
                log::error!("gpu: the GPU process is gone ({e}): no more 3D for this VM until it restarts");
                self.dead = true;
                None
            }
        }
    }

    fn drop_view(&mut self, resource: u32) {
        if let Some((p, n)) = self.views.remove(&resource) {
            unmap(p, n);
        }
    }

    fn export(&mut self, resource: u32) -> Option<(OwnedFd, u64)> {
        let b = self.call(Op::Export, |e| {
            e.u32(resource);
        })?;
        let mut d = Dec(&b);
        if d.u8()? != 1 {
            return None;
        }
        let size = d.u64()?;
        let fd = recv_fd(&self.sock).ok()?;
        Some((fd, size))
    }
}

impl Remote {
    /// The frame buffer, at least `n` bytes (made or grown here).
    fn frame(&mut self, n: usize) -> Option<*const u8> {
        if !self.frame.as_ref().is_some_and(|f| f.size() >= n) {
            if n > MAX_FRAME {
                return None;
            }
            let size = n.next_power_of_two().max(8 << 20);
            let f = vroom_memory::MmapRegion::new_shared(size).ok()?;
            let fd = f.shared_fd()?;
            let mut e = Enc::default();
            e.u8(Op::Frame as u8).u64(size as u64);
            let r = write_frame(&mut self.sock, &e.0).and_then(|_| send_fd(&self.sock, fd)).and_then(|_| read_frame(&mut self.sock));
            match r {
                Ok(b) if b.first() == Some(&1) => self.frame = Some(f),
                Ok(_) => return None,
                Err(e) => {
                    log::error!("gpu: the GPU process is gone ({e}): no more 3D for this VM until it restarts");
                    self.dead = true;
                    return None;
                }
            }
        }
        self.frame.as_ref().map(|f| f.as_ptr() as *const u8)
    }

    /// `n` bytes the GPU process left in the frame buffer, into `out`.
    #[allow(unsafe_code)]
    fn take_frame(&self, out: &mut [u8]) {
        if let Some(f) = &self.frame {
            // SAFETY: frame() made it at least out.len() bytes; the GPU
            // process is done writing (it answered).
            unsafe { std::ptr::copy_nonoverlapping(f.as_ptr(), out.as_mut_ptr(), out.len().min(f.size())) };
        }
    }
}

fn iov_enc(e: &mut Enc, r: &[(u32, u64, u64)]) {
    e.u32(r.len() as u32);
    for &(i, off, len) in r {
        e.u32(i).u64(off).u64(len);
    }
}

impl Renderer3d for Remote {
    fn capsets(&self) -> Vec<(u32, u32, u32)> {
        self.capsets.clone()
    }

    fn capset(&mut self, id: u32, version: u32) -> Option<Vec<u8>> {
        let b = self.call(Op::Capset, |e| {
            e.u32(id).u32(version);
        })?;
        let mut d = Dec(&b);
        (d.u8()? == 1).then(|| d.bytes().map(<[u8]>::to_vec)).flatten()
    }

    fn context_create(&mut self, ctx: u32, capset: u32, name: &[u8]) -> bool {
        self.ok(Op::CtxCreate, |e| {
            e.u32(ctx).u32(capset).bytes(name);
        })
    }

    fn context_destroy(&mut self, ctx: u32) {
        self.call(Op::CtxDestroy, |e| {
            e.u32(ctx);
        });
    }

    fn context_attach(&mut self, ctx: u32, resource: u32) {
        self.call(Op::CtxAttach, |e| {
            e.u32(ctx).u32(resource);
        });
    }

    fn context_detach(&mut self, ctx: u32, resource: u32) {
        self.call(Op::CtxDetach, |e| {
            e.u32(ctx).u32(resource);
        });
    }

    fn submit(&mut self, ctx: u32, cmds: &mut [u8]) -> bool {
        self.ok(Op::Submit, |e| {
            e.u32(ctx).bytes(cmds);
        })
    }

    fn create_3d(&mut self, resource: u32, a: &Create3d) -> bool {
        self.ok(Op::Create3d, |e| {
            e.u32(resource);
            for v in [a.target, a.format, a.bind, a.width, a.height, a.depth, a.array_size, a.last_level, a.nr_samples, a.flags] {
                e.u32(v);
            }
        })
    }

    fn attach_backing(&mut self, resource: u32, iov: HostIov) -> bool {
        let Some(r) = self.ranges(&iov) else { return false };
        self.ok(Op::AttachBacking, |e| {
            e.u32(resource);
            iov_enc(e, &r);
        })
    }

    fn detach_backing(&mut self, resource: u32) {
        self.call(Op::DetachBacking, |e| {
            e.u32(resource);
        });
    }

    fn transfer(&mut self, to_host: bool, ctx: u32, t: &Transfer3d) -> bool {
        self.ok(Op::Transfer, |e| {
            e.u8(to_host as u8).u32(ctx).u32(t.resource).u32(t.level).u32(t.stride).u32(t.layer_stride).bx(&t.bx).u64(t.offset);
        })
    }

    fn create_blob(&mut self, blob: &Blob, iov: HostIov) -> bool {
        let Some(r) = self.ranges(&iov) else { return false };
        let ok = self.ok(Op::CreateBlob, |e| {
            e.u32(blob.resource).u32(blob.ctx).u32(blob.mem).u32(blob.flags).u64(blob.id).u64(blob.size);
            iov_enc(e, &r);
        });
        if ok {
            self.blob_sizes.insert(blob.resource, blob.size);
        }
        ok
    }

    fn map_blob(&mut self, resource: u32) -> Option<Mapping> {
        if self.maps.contains_key(&resource) {
            return None;
        }
        let b = self.call(Op::MapBlob, |e| {
            e.u32(resource);
        })?;
        let mut d = Dec(&b);
        if d.u8()? != 1 {
            return None;
        }
        let (size, map_info) = (d.u64()?, d.u32()?);
        let fd = recv_fd(&self.sock).ok()?;
        let Some(p) = map_fd(fd.as_raw_fd(), size, true) else {
            self.unmap_blob(resource);
            return None;
        };
        self.maps.insert(resource, (p, size));
        // The guest's place for it is the blob's size, rounded to the
        // mapping step at most: never more of the object than that.
        let blob = self.blob_sizes.get(&resource).copied().unwrap_or(size).min(size);
        let rounded = blob.next_multiple_of(self.granule);
        Some(Mapping { ptr: p, size: if rounded <= size { rounded } else { blob }, map_info })
    }

    fn unmap_blob(&mut self, resource: u32) {
        if let Some((p, n)) = self.maps.remove(&resource) {
            unmap(p, n);
        }
        self.call(Op::UnmapBlob, |e| {
            e.u32(resource);
        });
    }

    fn read_mapped(&mut self, resource: u32, offset: u64, out: &mut [u8]) -> bool {
        let (p, size) = match self.maps.get(&resource).or(self.views.get(&resource)) {
            Some(&v) => v,
            None => {
                let Some((fd, size)) = self.export(resource) else { return false };
                let Some(p) = map_fd(fd.as_raw_fd(), size, false) else { return false };
                self.views.insert(resource, (p, size));
                (p, size)
            }
        };
        let Some(end) = offset.checked_add(out.len() as u64) else { return false };
        if end > size {
            return false;
        }
        // SAFETY: offset..end lies inside our mapping (checked above).
        #[allow(unsafe_code)]
        unsafe {
            std::ptr::copy_nonoverlapping(p.add(offset as usize), out.as_mut_ptr(), out.len())
        };
        true
    }

    fn read_back(&mut self, resource: u32, bx: Box3d, stride: u32, out: &mut [u8]) -> bool {
        let n = out.len() as u64;
        if self.frame(out.len()).is_none() {
            return false;
        }
        let ok = self.ok(Op::ReadBack, |e| {
            e.u32(resource).bx(&bx).u32(stride).u64(n);
        });
        if ok {
            self.take_frame(out);
        }
        ok
    }

    fn row_bytes(&mut self, resource: u32) -> Option<u32> {
        let b = self.call(Op::RowBytes, |e| {
            e.u32(resource);
        })?;
        let mut d = Dec(&b);
        (d.u8()? == 1).then(|| d.u32()).flatten()
    }

    fn read_level(&mut self, resource: u32, level: u32, bx: Box3d, stride: u32, layer_stride: u32, out: &mut [u8]) -> bool {
        let n = out.len() as u64;
        let Some(b) = self.call(Op::ReadLevel, |e| {
            e.u32(resource).u32(level).bx(&bx).u32(stride).u32(layer_stride).u64(n);
        }) else {
            return false;
        };
        let mut d = Dec(&b);
        match (d.u8(), d.bytes()) {
            (Some(1), Some(px)) if px.len() == out.len() => {
                out.copy_from_slice(px);
                true
            }
            _ => false,
        }
    }

    fn write_level(&mut self, resource: u32, level: u32, bx: Box3d, stride: u32, layer_stride: u32, data: &[u8]) -> bool {
        self.ok(Op::WriteLevel, |e| {
            e.u32(resource).u32(level).bx(&bx).u32(stride).u32(layer_stride).bytes(data);
        })
    }

    fn scanout_start(&mut self, resource: u32, bx: Box3d) -> bool {
        self.ok(Op::ScanoutStart, |e| {
            e.u32(resource).bx(&bx);
        })
    }

    fn scanout_poll(&mut self, out: &mut [u8], wait: bool) -> Option<bool> {
        let n = out.len() as u64;
        self.frame(out.len())?;
        let b = self.call(Op::ScanoutPoll, |e| {
            e.u64(n).u8(wait as u8);
        })?;
        match *b.first()? {
            0 => None,
            1 => {
                self.take_frame(out);
                Some(true)
            }
            _ => Some(false),
        }
    }

    fn unref(&mut self, resource: u32) {
        if let Some((p, n)) = self.maps.remove(&resource) {
            unmap(p, n);
        }
        self.drop_view(resource);
        self.blob_sizes.remove(&resource);
        self.call(Op::Unref, |e| {
            e.u32(resource);
        });
    }

    fn create_fence(&mut self, ctx: u32, ring: Option<u8>, fence: u64) -> bool {
        self.ok(Op::CreateFence, |e| {
            e.u32(ctx).u8(ring.map_or(0, |r| r.wrapping_add(1))).u64(fence);
        })
    }

    fn poll_fd(&self) -> Option<RawFd> {
        Some(self.events.as_raw_fd())
    }

    fn poll(&mut self) -> Vec<FenceDone> {
        drain(self.events.as_raw_fd());
        let Some(b) = self.call(Op::Poll, |_| {}) else { return Vec::new() };
        let mut d = Dec(&b);
        let n = d.u32().unwrap_or(0).min(1 << 16);
        let mut out = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let (Some(ctx), Some(ring), Some(fence)) = (d.u32(), d.u8(), d.u64()) else { break };
            out.push(FenceDone { ctx, ring: ring.checked_sub(1), fence });
        }
        out
    }

    fn reset(&mut self) {
        for (_, (p, n)) in self.maps.drain().chain(self.views.drain()) {
            unmap(p, n);
        }
        self.blob_sizes.clear();
        self.call(Op::Reset, |_| {});
    }

    fn release_thread(&mut self) {}
}

impl Drop for Remote {
    fn drop(&mut self) {
        for (_, (p, n)) in self.maps.drain().chain(self.views.drain()) {
            unmap(p, n);
        }
        // Closing the socket ends it; don't leave a zombie behind. The
        // sandbox lets us signal no one, so give it a moment, no more (a
        // GPU process stuck in the driver is reaped by launchd later).
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
        if let Some(c) = &mut self.child {
            for _ in 0..40 {
                if !matches!(c.try_wait(), Ok(None)) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

// ---- the GPU process's side ----

/// Guest RAM as mapped in the GPU process: (pointer, size) per region.
struct Ram(Vec<(*mut u8, u64)>);

impl Ram {
    fn map(fds: &[(RawFd, u64)]) -> Option<Ram> {
        fds.iter().map(|&(fd, size)| map_fd(fd, size, true).map(|p| (p, size))).collect::<Option<Vec<_>>>().map(Ram)
    }

    fn iov(&self, d: &mut Dec) -> Option<HostIov> {
        let n = d.u32()? as usize;
        if n > 1 << 20 {
            return None;
        }
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let (i, off, len) = (d.u32()? as usize, d.u64()?, d.u64()?);
            let &(p, size) = self.0.get(i)?;
            if off.checked_add(len)? > size {
                return None;
            }
            // SAFETY-free: pointer arithmetic inside our own mapping.
            v.push((p.wrapping_add(off as usize), len as usize));
        }
        Some(v)
    }
}

impl Drop for Ram {
    fn drop(&mut self) {
        for &(p, n) in &self.0 {
            unmap(p, n);
        }
    }
}

/// The first `n` bytes of the frame buffer, if it has them.
#[allow(unsafe_code)]
fn frame_slice<'a>(frame: Option<(*mut u8, u64)>, n: u64) -> Option<&'a mut [u8]> {
    let (p, size) = frame?;
    // SAFETY: our own mapping of `size` bytes, alive until the next Frame
    // call; the slice is used within this request only.
    (n <= size).then(|| unsafe { std::slice::from_raw_parts_mut(p, n as usize) })
}

/// A renderer whose buffers can go to another process.
pub trait Export {
    /// A blob's shared memory and its size.
    fn export(&mut self, resource: u32) -> Option<(OwnedFd, u64)>;
}

impl Export for crate::Virgl {
    fn export(&mut self, resource: u32) -> Option<(OwnedFd, u64)> {
        crate::Virgl::export(self, resource)
    }
}

/// Serve the VMM until it goes away. `ram`: guest RAM's shared memory.
/// Run with the sandbox up: everything here works on what is open.
pub fn serve<R: Renderer3d + Export>(mut r: R, mut sock: UnixStream, mut events: UnixStream, ram: &[(RawFd, u64)]) -> std::io::Result<()> {
    let mut ram = Ram::map(ram).ok_or_else(|| std::io::Error::other("can't map guest RAM"))?;
    let mut hello = Enc::default();
    let caps = r.capsets();
    hello.u32(caps.len() as u32);
    for (id, v, s) in caps {
        hello.u32(id).u32(v).u32(s);
    }
    write_frame(&mut sock, &hello.0)?;
    events.set_nonblocking(true)?;
    let wake = r.poll_fd();
    // The frame buffer (see Remote::frame): pointer and size.
    let mut frame: Option<(*mut u8, u64)> = None;
    loop {
        // A request, or fences the library finished on its own threads.
        let mut fds = [libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 }, libc::pollfd { fd: wake.unwrap_or(-1), events: libc::POLLIN, revents: 0 }];
        #[allow(unsafe_code)]
        // SAFETY: two valid pollfd entries.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), if wake.is_some() { 2 } else { 1 }, -1) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if fds[1].revents != 0 {
            // Tell the VMM to come and collect them (Poll); a full socket
            // means it already has a wakeup pending.
            let _ = events.write(&[1]);
            drain(fds[1].fd);
        }
        if fds[0].revents == 0 {
            continue;
        }
        let req = match read_frame(&mut sock) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut d = Dec(&req);
        let Some(op) = d.u8().and_then(Op::from) else {
            write_frame(&mut sock, &[0])?;
            continue;
        };
        let mut out = Enc::default();
        let mut fd_after: Option<OwnedFd> = None;
        let ok = |o: &mut Enc, b: bool| {
            o.u8(b as u8);
        };
        match op {
            Op::Capset => match (d.u32(), d.u32()) {
                (Some(id), Some(v)) => match r.capset(id, v) {
                    Some(c) => {
                        out.u8(1).bytes(&c);
                    }
                    None => ok(&mut out, false),
                },
                _ => ok(&mut out, false),
            },
            Op::CtxCreate => {
                let b = match (d.u32(), d.u32(), d.bytes()) {
                    (Some(ctx), Some(cap), Some(name)) => r.context_create(ctx, cap, name),
                    _ => false,
                };
                ok(&mut out, b)
            }
            Op::CtxDestroy => {
                if let Some(ctx) = d.u32() {
                    r.context_destroy(ctx);
                }
                ok(&mut out, true)
            }
            Op::CtxAttach | Op::CtxDetach => {
                if let (Some(ctx), Some(res)) = (d.u32(), d.u32()) {
                    if op == Op::CtxAttach {
                        r.context_attach(ctx, res)
                    } else {
                        r.context_detach(ctx, res)
                    }
                }
                ok(&mut out, true)
            }
            Op::Submit => {
                let b = match (d.u32(), d.bytes()) {
                    (Some(ctx), Some(cmds)) => r.submit(ctx, &mut cmds.to_vec()),
                    _ => false,
                };
                ok(&mut out, b)
            }
            Op::Create3d => {
                let mut v = [0u32; 11];
                let b = v.iter_mut().all(|x| d.u32().map(|n| *x = n).is_some())
                    && r.create_3d(
                        v[0],
                        &Create3d {
                            target: v[1],
                            format: v[2],
                            bind: v[3],
                            width: v[4],
                            height: v[5],
                            depth: v[6],
                            array_size: v[7],
                            last_level: v[8],
                            nr_samples: v[9],
                            flags: v[10],
                        },
                    );
                ok(&mut out, b)
            }
            Op::AttachBacking => {
                let b = match (d.u32(), ram.iov(&mut d)) {
                    (Some(res), Some(iov)) => r.attach_backing(res, iov),
                    _ => false,
                };
                ok(&mut out, b)
            }
            Op::DetachBacking => {
                if let Some(res) = d.u32() {
                    r.detach_backing(res);
                }
                ok(&mut out, true)
            }
            Op::Transfer => {
                let b = (|| {
                    let to_host = d.u8()? != 0;
                    let ctx = d.u32()?;
                    let t = Transfer3d { resource: d.u32()?, level: d.u32()?, stride: d.u32()?, layer_stride: d.u32()?, bx: d.bx()?, offset: d.u64()? };
                    Some(r.transfer(to_host, ctx, &t))
                })()
                .unwrap_or(false);
                ok(&mut out, b)
            }
            Op::CreateBlob => {
                let b = (|| {
                    let blob = Blob { resource: d.u32()?, ctx: d.u32()?, mem: d.u32()?, flags: d.u32()?, id: d.u64()?, size: d.u64()? };
                    let iov = ram.iov(&mut d)?;
                    Some(r.create_blob(&blob, iov))
                })()
                .unwrap_or(false);
                ok(&mut out, b)
            }
            Op::MapBlob => {
                let m = d.u32().and_then(|res| r.map_blob(res).map(|m| (res, m)));
                match m.and_then(|(res, m)| r.export(res).map(|(fd, _)| (m, fd))) {
                    Some((m, fd)) => {
                        out.u8(1).u64(m.size).u32(m.map_info);
                        fd_after = Some(fd);
                    }
                    None => {
                        if let Some(res) = Dec(&req[1..]).u32() {
                            r.unmap_blob(res);
                        }
                        ok(&mut out, false)
                    }
                }
            }
            Op::UnmapBlob => {
                if let Some(res) = d.u32() {
                    r.unmap_blob(res);
                }
                ok(&mut out, true)
            }
            Op::Export => match d.u32().and_then(|res| r.export(res)) {
                Some((fd, size)) => {
                    out.u8(1).u64(size);
                    fd_after = Some(fd);
                }
                None => ok(&mut out, false),
            },
            Op::ReadBack => {
                let b = (|| {
                    let (res, bx, stride, n) = (d.u32()?, d.bx()?, d.u32()?, d.u64()?);
                    let px = frame_slice(frame, n)?;
                    Some(r.read_back(res, bx, stride, px))
                })()
                .unwrap_or(false);
                ok(&mut out, b)
            }
            Op::RowBytes => match d.u32().and_then(|res| r.row_bytes(res)) {
                Some(n) => {
                    out.u8(1).u32(n);
                }
                None => ok(&mut out, false),
            },
            Op::ReadLevel => {
                let px = (|| {
                    let (res, level, bx, stride, ls, n) = (d.u32()?, d.u32()?, d.bx()?, d.u32()?, d.u32()?, d.u64()? as usize);
                    if n > MAX_FRAME / 2 {
                        return None;
                    }
                    let mut px = vec![0u8; n];
                    r.read_level(res, level, bx, stride, ls, &mut px).then_some(px)
                })();
                match px {
                    Some(px) => {
                        out.u8(1).bytes(&px);
                    }
                    None => ok(&mut out, false),
                }
            }
            Op::WriteLevel => {
                let b = (|| {
                    let (res, level, bx, stride, ls, data) = (d.u32()?, d.u32()?, d.bx()?, d.u32()?, d.u32()?, d.bytes()?);
                    Some(r.write_level(res, level, bx, stride, ls, data))
                })()
                .unwrap_or(false);
                ok(&mut out, b)
            }
            Op::ScanoutStart => {
                let b = match (d.u32(), d.bx()) {
                    (Some(res), Some(bx)) => r.scanout_start(res, bx),
                    _ => false,
                };
                ok(&mut out, b)
            }
            Op::ScanoutPoll => {
                let (n, wait) = (d.u64().unwrap_or(0), d.u8().unwrap_or(0) != 0);
                match frame_slice(frame, n).map(|px| r.scanout_poll(px, wait)) {
                    Some(None) => out.u8(0),
                    Some(Some(true)) => out.u8(1),
                    _ => out.u8(2),
                };
            }
            Op::Unref => {
                if let Some(res) = d.u32() {
                    r.unref(res);
                }
                ok(&mut out, true)
            }
            Op::CreateFence => {
                let b = match (d.u32(), d.u8(), d.u64()) {
                    (Some(ctx), Some(ring), Some(fence)) => r.create_fence(ctx, ring.checked_sub(1), fence),
                    _ => false,
                };
                ok(&mut out, b)
            }
            Op::Poll => {
                let done = r.poll();
                out.u32(done.len() as u32);
                for f in done {
                    out.u32(f.ctx).u8(f.ring.map_or(0, |r| r.wrapping_add(1))).u64(f.fence);
                }
            }
            Op::Reset => {
                r.reset();
                ok(&mut out, true)
            }
            Op::Frame => {
                let size = d.u64().unwrap_or(0);
                let fd = recv_fd(&sock)?;
                let b = size > 0 && size as usize <= MAX_FRAME && {
                    let new = map_fd(fd.as_raw_fd(), size, true);
                    if let Some(p) = new {
                        if let Some((old, n)) = frame.replace((p, size)) {
                            unmap(old, n);
                        }
                    }
                    new.is_some()
                };
                ok(&mut out, b)
            }
            Op::Remap => {
                let n = d.u32().unwrap_or(0).min(64) as usize;
                let sizes: Vec<u64> = (0..n).map_while(|_| d.u64()).collect();
                let mut fds = Vec::new();
                for &size in &sizes {
                    fds.push((recv_fd(&sock)?, size));
                }
                let raw: Vec<(RawFd, u64)> = fds.iter().map(|(f, s)| (f.as_raw_fd(), *s)).collect();
                let b = match Ram::map(&raw) {
                    Some(new) if sizes.len() == n => {
                        ram = new;
                        true
                    }
                    _ => false,
                };
                ok(&mut out, b)
            }
        }
        write_frame(&mut sock, &out.0)?;
        if let Some(fd) = fd_after {
            send_fd(&sock, fd.as_raw_fd())?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use vroom_memory::{GuestAddress, MmapRegion};

    /// Records what reaches it, as the GPU process's renderer would see it.
    struct Fake {
        seen: Arc<Mutex<Vec<u8>>>,
        blob: MmapRegion,
        wake: (UnixStream, UnixStream),
        done: Vec<FenceDone>,
    }

    impl Export for Fake {
        fn export(&mut self, _: u32) -> Option<(OwnedFd, u64)> {
            // SAFETY: dup of an fd we hold.
            #[allow(unsafe_code)]
            let fd = unsafe { OwnedFd::from_raw_fd(libc::dup(self.blob.shared_fd()?)) };
            Some((fd, self.blob.size() as u64))
        }
    }

    impl Renderer3d for Fake {
        fn capsets(&self) -> Vec<(u32, u32, u32)> {
            vec![(4, 0, 160), (7, 0, 60)]
        }
        fn capset(&mut self, _: u32, _: u32) -> Option<Vec<u8>> {
            Some(vec![7; 60])
        }
        fn context_create(&mut self, _: u32, _: u32, _: &[u8]) -> bool {
            true
        }
        fn context_destroy(&mut self, _: u32) {}
        fn context_attach(&mut self, _: u32, _: u32) {}
        fn context_detach(&mut self, _: u32, _: u32) {}
        fn submit(&mut self, _: u32, cmds: &mut [u8]) -> bool {
            *self.seen.lock().unwrap() = cmds.to_vec();
            true
        }
        fn create_3d(&mut self, _: u32, _: &Create3d) -> bool {
            true
        }
        #[allow(unsafe_code)]
        fn attach_backing(&mut self, _: u32, iov: HostIov) -> bool {
            let mut v = Vec::new();
            for (p, n) in iov {
                // SAFETY: the iovec points into our mapping of guest RAM.
                v.extend_from_slice(unsafe { std::slice::from_raw_parts(p, n) });
            }
            *self.seen.lock().unwrap() = v;
            true
        }
        fn detach_backing(&mut self, _: u32) {}
        fn transfer(&mut self, _: bool, _: u32, _: &Transfer3d) -> bool {
            true
        }
        fn create_blob(&mut self, _: &Blob, _: HostIov) -> bool {
            true
        }
        fn map_blob(&mut self, _: u32) -> Option<Mapping> {
            Some(Mapping { ptr: self.blob.as_ptr(), size: self.blob.size() as u64, map_info: 2 })
        }
        fn unmap_blob(&mut self, _: u32) {}
        fn read_mapped(&mut self, _: u32, _: u64, _: &mut [u8]) -> bool {
            false
        }
        fn read_back(&mut self, _: u32, _: Box3d, _: u32, _: &mut [u8]) -> bool {
            false
        }
        fn scanout_poll(&mut self, out: &mut [u8], _: bool) -> Option<bool> {
            out.fill(0xab);
            Some(true)
        }
        fn unref(&mut self, _: u32) {}
        fn create_fence(&mut self, ctx: u32, ring: Option<u8>, fence: u64) -> bool {
            self.done.push(FenceDone { ctx, ring, fence });
            (&self.wake.1).write_all(&[1]).is_ok()
        }
        fn poll_fd(&self) -> Option<RawFd> {
            Some(self.wake.0.as_raw_fd())
        }
        fn poll(&mut self) -> Vec<FenceDone> {
            std::mem::take(&mut self.done)
        }
        fn reset(&mut self) {}
    }

    fn readable(fd: RawFd) -> bool {
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        #[allow(unsafe_code)]
        let n = unsafe { libc::poll(&mut p, 1, 2000) };
        n == 1
    }

    #[test]
    fn calls_ram_blobs_frames_and_fences_cross_over() {
        let base = GuestAddress(0x4000_0000);
        let mem = GuestMemory::new_shared(&[(base, 1 << 20)]).unwrap();
        mem.write(GuestAddress(0x4000_1000), b"guest data").unwrap();
        let (fds, _) = mem.shared_fds().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let blob = MmapRegion::new_shared(16384).unwrap();
        // SAFETY: our own fresh mapping.
        #[allow(unsafe_code)]
        unsafe {
            std::ptr::copy_nonoverlapping(b"blob!".as_ptr(), blob.as_ptr(), 5)
        };
        let blob_ptr = blob.as_ptr() as usize;
        let fake = Fake { seen: seen.clone(), blob, wake: UnixStream::pair().unwrap(), done: Vec::new() };
        fake.wake.0.set_nonblocking(true).unwrap();
        let ((a, b), (ea, eb)) = (UnixStream::pair().unwrap(), UnixStream::pair().unwrap());
        let server = std::thread::spawn(move || serve(fake, b, eb, &fds));
        let mut r = Remote::connect(a, ea, mem.clone(), 4096).unwrap();
        assert_eq!(r.capsets(), vec![(4, 0, 160), (7, 0, 60)]);
        assert_eq!(r.capset(7, 0), Some(vec![7; 60]));

        // Guest RAM: the GPU process reads what the guest wrote.
        assert!(r.create_3d(1, &Create3d { target: 2, format: 1, bind: 2, width: 4, height: 4, depth: 1, array_size: 1, last_level: 0, nr_samples: 0, flags: 0 }));
        assert!(r.attach_backing(1, mem.host_ranges(GuestAddress(0x4000_1000), 10).unwrap()));
        assert_eq!(&*seen.lock().unwrap(), b"guest data");
        // Outside guest RAM: refused before it goes over.
        assert!(!r.attach_backing(2, vec![(0x1000 as *mut u8, 16)]));
        assert!(r.submit(1, &mut b"cmds".to_vec()));
        assert_eq!(&*seen.lock().unwrap(), b"cmds");

        // A mapped blob: our own mapping of its memory, at the blob's
        // size rounded to the granule (not the whole 16 KiB object).
        let bl = Blob { resource: 3, ctx: 1, mem: 2, flags: 1, id: 9, size: 100 };
        assert!(r.create_blob(&bl, Vec::new()));
        let m = r.map_blob(3).unwrap();
        assert_ne!(m.ptr as usize, blob_ptr);
        assert_eq!((m.size, m.map_info), (4096, 2));
        // SAFETY: the mapping Remote made, 4096 bytes.
        #[allow(unsafe_code)]
        let got = unsafe { std::slice::from_raw_parts(m.ptr, 5) }.to_vec();
        assert_eq!(got, b"blob!");
        let mut out = [0u8; 5];
        assert!(r.read_mapped(3, 0, &mut out));
        assert!(!r.read_mapped(3, 16380, &mut out));
        r.unmap_blob(3);

        // A frame comes back through the frame buffer.
        let mut px = vec![0u8; 1280 * 800 * 4];
        assert_eq!(r.scanout_poll(&mut px, true), Some(true));
        assert!(px.iter().all(|&b| b == 0xab));

        // Fences: a byte on the event socket, then the list.
        assert!(r.create_fence(1, Some(0), 42));
        assert!(readable(r.poll_fd().unwrap()));
        assert_eq!(r.poll(), vec![FenceDone { ctx: 1, ring: Some(0), fence: 42 }]);

        // A reset gives the guest new RAM; the GPU process maps it.
        let gen = mem.generation();
        mem.discard_all().unwrap();
        assert_ne!(mem.generation(), gen);
        mem.write(GuestAddress(0x4000_1000), b"after".to_vec().as_slice()).unwrap();
        assert!(r.attach_backing(1, mem.host_ranges(GuestAddress(0x4000_1000), 5).unwrap()));
        assert_eq!(&*seen.lock().unwrap(), b"after");
        assert_eq!(r.mapped_gen, mem.generation());

        drop(r);
        server.join().unwrap().unwrap();
    }
}
