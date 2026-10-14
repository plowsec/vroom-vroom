//! The host channel: a virtio-serial port, split into reader and writer.
//!
//! Windows opens it overlapped. With a plain (synchronous) handle, Windows
//! serializes I/O per file object: the reader thread waiting for the host
//! would block every write (clipboard, pong) until the host spoke.

use std::io::{Read, Write};

pub type Reader = Box<dyn Read + Send>;
pub type Writer = Box<dyn Write + Send>;

#[cfg(unix)]
pub fn open(path: &str) -> std::io::Result<(Reader, Writer)> {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    Ok((Box::new(f.try_clone()?), Box::new(f)))
}

#[cfg(windows)]
pub fn open(path: &str) -> std::io::Result<(Reader, Writer)> {
    win::open(path)
}

#[cfg(windows)]
mod win {
    use std::io;
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, OPEN_EXISTING};
    use windows_sys::Win32::System::Threading::CreateEventW;
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

    use super::{Reader, Writer};

    struct Handle(HANDLE);
    // SAFETY: a kernel handle; Windows allows using it from any thread.
    unsafe impl Send for Handle {}
    // SAFETY: as above; each side has its own OVERLAPPED and event.
    unsafe impl Sync for Handle {}
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: we own the handle.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// One direction: shared port handle, private event.
    struct Side {
        port: Arc<Handle>,
        event: Handle,
    }

    impl Side {
        fn new(port: Arc<Handle>) -> io::Result<Side> {
            // SAFETY: anonymous manual-reset event.
            let e = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if e.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Side { port, event: Handle(e) })
        }

        /// Start an overlapped op with `f` and wait for it.
        fn io(&self, f: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32) -> io::Result<usize> {
            // SAFETY: zeroed OVERLAPPED is valid; it outlives the op because
            // we wait for completion below.
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = self.event.0;
            if f(self.port.0, &mut ov) == 0 {
                // SAFETY: plain call.
                let err = unsafe { GetLastError() };
                if err != ERROR_IO_PENDING {
                    return Err(io::Error::from_raw_os_error(err as i32));
                }
            }
            let mut n = 0u32;
            // SAFETY: same handle and OVERLAPPED; wait = TRUE.
            if unsafe { GetOverlappedResult(self.port.0, &ov, &mut n, 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(n as usize)
        }
    }

    impl io::Read for Side {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let len = buf.len().min(u32::MAX as usize) as u32;
            // SAFETY: buf is valid for len bytes until the op completes.
            self.io(|h, ov| unsafe { ReadFile(h, buf.as_mut_ptr(), len, std::ptr::null_mut(), ov) })
        }
    }

    impl io::Write for Side {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let len = buf.len().min(u32::MAX as usize) as u32;
            // SAFETY: buf is valid for len bytes until the op completes.
            self.io(|h, ov| unsafe { WriteFile(h, buf.as_ptr(), len, std::ptr::null_mut(), ov) })
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub fn open(path: &str) -> io::Result<(Reader, Writer)> {
        let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
        // SAFETY: valid NUL-terminated path; no sharing, no template.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let port = Arc::new(Handle(h));
        Ok((Box::new(Side::new(port.clone())?), Box::new(Side::new(port)?)))
    }
}
