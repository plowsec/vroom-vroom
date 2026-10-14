//! Pass one fd over a unix stream (SCM_RIGHTS).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::{Error, Result};

pub fn send_fd(sock: &UnixStream, fd: RawFd) -> Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr() as *mut _, iov_len: 1 };
    // SAFETY: CMSG_SPACE is a pure computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: zeroed msghdr is valid; fields set below.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut _;
    msg.msg_controllen = space as _;
    // SAFETY: control buffer is CMSG_SPACE(int) bytes, so the first header fits.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, fd);
    }
    // SAFETY: msg describes valid buffers.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, 0) };
    if n != 1 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

pub fn recv_fd(sock: &UnixStream) -> Result<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr() as *mut _, iov_len: 1 };
    // SAFETY: pure computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: zeroed msghdr is valid.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut _;
    msg.msg_controllen = space as _;
    // SAFETY: msg describes valid buffers.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) };
    if n != 1 {
        return Err(Error::Proto("no fd message"));
    }
    // SAFETY: walking the control buffer recvmsg filled.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if c.is_null() || (*c).cmsg_level != libc::SOL_SOCKET || (*c).cmsg_type != libc::SCM_RIGHTS {
            return Err(Error::Proto("expected SCM_RIGHTS"));
        }
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const RawFd);
        if fd < 0 {
            return Err(Error::Proto("bad fd"));
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn pass_pipe_end() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut x, y) = UnixStream::pair().unwrap();
        send_fd(&a, y.as_raw_fd()).unwrap();
        let got = recv_fd(&b).unwrap();
        let mut y2 = UnixStream::from(got);
        x.write_all(b"ok").unwrap();
        let mut buf = [0u8; 2];
        y2.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ok");
    }
}
