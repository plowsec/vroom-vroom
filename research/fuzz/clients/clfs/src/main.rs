//! A CLFS client for sequence fuzzing: runs a program of CLFS calls (the
//! fuzzer's input) against clfs.sys, so state carries from call to call
//! (a log, its containers, marshalling areas, records, cursors).
//!
//!   clfs-client wait          park for the snapshot (addresses in C:\clfs\client.txt)
//!   clfs-client run FILE      run a program once, printing each call's result
//!
//! A program is calls, each `u32 op | u16 length | payload`. Handles,
//! marshalling areas and LSNs live in slots the payloads name, so any
//! byte string is a program; unknown ops and missing slots do nothing.
//!
//! In `wait`, the snapshot stands at `park`; the fuzzer writes the program
//! to the input page and its length to `LEN`, which ends the wait. The
//! input ends at `done` (the harness's `stop_pc`).

use std::ffi::c_void;
use std::ptr::{null, null_mut};

type Handle = *mut c_void;
type Bool = i32;
type Lsn = u64;

const INVALID: Handle = -1isize as Handle;

#[repr(C)]
struct WriteEntry {
    buffer: *const c_void,
    length: u32,
}

#[link(name = "clfsw32")]
extern "system" {
    fn CreateLogFile(name: *const u16, access: u32, share: u32, security: *const c_void, disposition: u32, flags: u32) -> Handle;
    fn AddLogContainer(log: Handle, size: *mut u64, path: *const u16, reserved: *mut c_void) -> Bool;
    fn RemoveLogContainer(log: Handle, path: *const u16, force: Bool, reserved: *mut c_void) -> Bool;
    fn CreateLogMarshallingArea(log: Handle, alloc: *const c_void, free: *const c_void, context: *mut c_void, size: u32, writes: u32, reads: u32, marshal: *mut *mut c_void) -> Bool;
    fn DeleteLogMarshallingArea(marshal: *mut c_void) -> Bool;
    fn ReserveAndAppendLog(marshal: *mut c_void, entries: *const WriteEntry, count: u32, undo: *const Lsn, previous: *const Lsn, reserve: u32, reservations: *mut i64, flags: u32, lsn: *mut Lsn, overlapped: *mut c_void) -> Bool;
    fn FlushLogBuffers(marshal: *mut c_void, overlapped: *mut c_void) -> Bool;
    fn ReadLogRecord(marshal: *mut c_void, first: *const Lsn, mode: u32, buffer: *mut *mut c_void, size: *mut u32, kind: *mut u8, undo: *mut Lsn, previous: *mut Lsn, context: *mut *mut c_void, overlapped: *mut c_void) -> Bool;
    fn ReadNextLogRecord(context: *mut c_void, buffer: *mut *mut c_void, size: *mut u32, kind: *mut u8, user: *const Lsn, undo: *mut Lsn, previous: *mut Lsn, record: *mut Lsn, overlapped: *mut c_void) -> Bool;
    fn TerminateReadLog(context: *mut c_void) -> Bool;
    fn SetLogArchiveTail(log: Handle, tail: *const Lsn, reserved: *mut c_void) -> Bool;
    fn AdvanceLogBase(marshal: *mut c_void, base: *const Lsn, flags: u32, overlapped: *mut c_void) -> Bool;
    fn TruncateLog(marshal: *mut c_void, end: *const Lsn, overlapped: *mut c_void) -> Bool;
    fn GetLogFileInformation(log: Handle, info: *mut c_void, size: *mut u32) -> Bool;
    fn DeleteLogFile(name: *const u16, reserved: *mut c_void) -> Bool;
    fn AllocReservedLog(marshal: *mut c_void, count: u32, adjustment: *mut i64) -> Bool;
    fn FreeReservedLog(marshal: *mut c_void, count: u32, adjustment: *mut i64) -> Bool;
    fn CreateLogContainerScanContext(log: Handle, from: u32, count: u32, mode: u8, scan: *mut c_void, overlapped: *mut c_void) -> Bool;
    fn ScanLogContainers(scan: *mut c_void, mode: u8, reserved: *mut c_void) -> Bool;
    fn WriteLogRestartArea(marshal: *mut c_void, buffer: *const c_void, size: u32, base: *const Lsn, flags: u32, written: *mut u32, next: *mut Lsn, overlapped: *mut c_void) -> Bool;
    fn ReadLogRestartArea(marshal: *mut c_void, buffer: *mut *mut c_void, size: *mut u32, lsn: *mut Lsn, context: *mut *mut c_void, overlapped: *mut c_void) -> Bool;
    fn ReadPreviousLogRestartArea(context: *mut c_void, buffer: *mut *mut c_void, size: *mut u32, lsn: *mut Lsn, overlapped: *mut c_void) -> Bool;
    fn PrepareLogArchive(log: Handle, name: *mut u16, len: u32, low: *const Lsn, high: *const Lsn, actual: *mut u32, offset: *mut u64, length: *mut u64, base: *mut Lsn, last: *mut Lsn, tail: *mut Lsn, context: *mut *mut c_void) -> Bool;
    fn GetNextLogArchiveExtent(context: *mut c_void, extents: *mut c_void, count: u32, returned: *mut u32) -> Bool;
    fn ReadLogArchiveMetadata(context: *mut c_void, offset: u32, size: u32, buffer: *mut u8, read: *mut u32) -> Bool;
    fn TerminateLogArchive(context: *mut c_void) -> Bool;
    fn AddLogContainerSet(log: Handle, count: u16, size: *mut u64, paths: *const *const u16, reserved: *mut c_void) -> Bool;
    fn RemoveLogContainerSet(log: Handle, count: u16, paths: *const *const u16, force: Bool, reserved: *mut c_void) -> Bool;
    fn GetLogContainerName(log: Handle, container: u32, name: *mut u16, len: u32, actual: *mut u32) -> Bool;
    fn AlignReservedLog(marshal: *mut c_void, count: u32, sizes: *const i64, aligned: *mut i64) -> Bool;
    fn ReserveAndAppendLogAligned(marshal: *mut c_void, entries: *const WriteEntry, count: u32, alignment: u32, undo: *const Lsn, previous: *const Lsn, reserve: u32, reservations: *mut i64, flags: u32, lsn: *mut Lsn, overlapped: *mut c_void) -> Bool;
    fn SetEndOfLog(log: Handle, end: *const Lsn, overlapped: *mut c_void) -> Bool;
    fn InstallLogPolicy(log: Handle, policy: *const c_void) -> Bool;
    fn RemoveLogPolicy(log: Handle, kind: u32) -> Bool;
    fn QueryLogPolicy(log: Handle, kind: u32, policy: *mut c_void, size: *mut u32) -> Bool;
    fn SetLogFileSizeWithPolicy(log: Handle, desired: *const u64, resulting: *mut u64) -> Bool;
    fn HandleLogFull(log: Handle) -> Bool;
    fn SetLogArchiveMode(log: Handle, mode: u32) -> Bool;
}

#[link(name = "kernel32")]
extern "system" {
    fn CloseHandle(handle: Handle) -> Bool;
    fn DeviceIoControl(handle: Handle, code: u32, input: *const c_void, input_size: u32, output: *mut c_void, output_size: u32, returned: *mut u32, overlapped: *mut c_void) -> Bool;
    fn CreateFileW(name: *const u16, access: u32, share: u32, security: *const c_void, disposition: u32, flags: u32, template: Handle) -> Handle;
    fn SetFilePointerEx(file: Handle, distance: i64, new: *mut i64, method: u32) -> Bool;
    fn WriteFile(file: Handle, buffer: *const c_void, size: u32, written: *mut u32, overlapped: *mut c_void) -> Bool;
    fn GetLastError() -> u32;
    fn SetLastError(error: u32);
    fn VirtualAlloc(address: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
    fn VirtualLock(address: *mut c_void, size: usize) -> Bool;
    fn CreateDirectoryW(path: *const u16, security: *const c_void) -> Bool;
    fn CreateEventW(security: *const c_void, manual: Bool, initial: Bool, name: *const u16) -> Handle;
    fn WaitForSingleObject(handle: Handle, ms: u32) -> u32;
}

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const SHARE_ALL: u32 = 7;
const CLFS_SCAN_FORWARD: u8 = 0x02;
const CLFS_SCAN_BACKWARD: u8 = 0x04;
const CLFS_SCAN_CLOSE: u8 = 0x08;

/// The input page: one page, so the fuzzer writes it at one physical
/// address.
const PAGE: usize = 4096;

/// The fuzzer writes the program's length here; non-zero ends the wait.
#[no_mangle]
static mut LEN: u32 = 0;
/// Where the program is (the input page), for the harness's
/// `input_pointer_va`.
#[no_mangle]
static mut INPUT: *mut u8 = null_mut();

/// The snapshot's PC: a call per turn of the wait.
#[inline(never)]
#[no_mangle]
extern "C" fn park() {
    std::hint::spin_loop();
}

/// The end of a program (the harness's `stop_pc`).
#[inline(never)]
#[no_mangle]
extern "C" fn done(results: *const u32) {
    unsafe { std::ptr::read_volatile(results) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Slots the calls share.
struct State {
    logs: [Handle; 8],
    marshals: [*mut c_void; 8],
    /// Logs opened for overlapped I/O, and areas on them: their calls go
    /// asynchronous (completion paths).
    async_logs: [bool; 8],
    async_areas: [bool; 8],
    /// OVERLAPPEDs handed out (Internal, InternalHigh, Offset, hEvent):
    /// alive for good, a late completion writes into them.
    overlapped: Vec<Box<[u64; 4]>>,
    lsns: [Lsn; 16],
    /// Each call's result: 0, or the Win32 error (op in the top byte).
    results: Vec<u32>,
    verbose: bool,
}

/// A payload read a byte, a word at a time; past its end, zeros.
struct Args<'a>(&'a [u8], usize);

impl Args<'_> {
    fn u8(&mut self) -> u8 {
        let v = self.0.get(self.1).copied().unwrap_or(0);
        self.1 += 1;
        v
    }
    fn u16(&mut self) -> u16 {
        u16::from(self.u8()) | u16::from(self.u8()) << 8
    }
    fn u32(&mut self) -> u32 {
        u32::from(self.u16()) | u32::from(self.u16()) << 16
    }
    fn u64(&mut self) -> u64 {
        u64::from(self.u32()) | u64::from(self.u32()) << 32
    }
    fn rest(&self) -> &[u8] {
        self.0.get(self.1..).unwrap_or(&[])
    }
}

impl State {
    /// An OVERLAPPED with its own event, when `on`; else none.
    unsafe fn ov(&mut self, on: bool) -> *mut c_void {
        if !on {
            return null_mut();
        }
        let event = CreateEventW(null(), 1, 0, null());
        self.overlapped.push(Box::new([0, 0, 0, event as u64]));
        self.overlapped.last_mut().expect("just pushed").as_mut_ptr().cast()
    }

    /// A call's result, waiting (100 ms at most) for one that went
    /// asynchronous: its status then.
    unsafe fn finish(&self, ok: Bool, ov: *mut c_void) -> Bool {
        if ok != 0 || ov.is_null() || GetLastError() != 997 {
            return ok;
        }
        let o = ov.cast::<u64>();
        if WaitForSingleObject(*o.add(3) as Handle, 100) != 0 {
            return 0;
        }
        i32::from(*o == 0)
    }

    fn area_async(&self, slot: u8) -> bool {
        self.async_areas[slot as usize % 8]
    }

    fn log(&self, slot: u8) -> Handle {
        self.logs[slot as usize % 8]
    }
    fn marshal(&self, slot: u8) -> *mut c_void {
        self.marshals[slot as usize % 8]
    }
    /// An LSN: a slot's (high bit clear), or the next 8 payload bytes.
    fn lsn(&self, a: &mut Args) -> Lsn {
        let which = a.u8();
        if which & 0x80 != 0 {
            a.u64()
        } else {
            self.lsns[which as usize % 16]
        }
    }

    unsafe fn call(&mut self, op: u32, payload: &[u8]) -> Bool {
        let mut a = Args(payload, 0);
        match op {
            // Open or create a log (a stream of one, or a dedicated log).
            0 => {
                let slot = a.u8();
                let name = a.u8() % 4;
                let disposition = 1 + a.u8() as u32 % 5;
                let stream = a.u8();
                let overlapped = a.u8() & 1 != 0;
                let path = if stream == 0 { format!("LOG:C:\\clfs\\l{name}") } else { format!("LOG:C:\\clfs\\l{name}::s{}", stream % 4) };
                let flags = if overlapped { 0x4000_0000 } else { 0 };
                let h = CreateLogFile(wide(&path).as_ptr(), GENERIC_READ | GENERIC_WRITE, SHARE_ALL, null(), disposition, flags);
                if h == INVALID {
                    return 0;
                }
                self.logs[slot as usize % 8] = h;
                self.async_logs[slot as usize % 8] = overlapped;
                1
            }
            1 => {
                let log = self.log(a.u8());
                let name = a.u8() % 8;
                // Up to 8 MiB: a 4 GiB container is gigabytes of disk
                // writes every input.
                let mut size = u64::from(a.u32() % (8 << 20));
                AddLogContainer(log, &mut size, wide(&format!("C:\\clfs\\c{name}")).as_ptr(), null_mut())
            }
            2 => {
                let log = self.log(a.u8());
                let name = a.u8() % 8;
                RemoveLogContainer(log, wide(&format!("C:\\clfs\\c{name}")).as_ptr(), i32::from(a.u8() & 1), null_mut())
            }
            3 => {
                let log_slot = a.u8();
                let log = self.log(log_slot);
                let slot = a.u8() as usize % 8;
                self.async_areas[slot] = self.async_logs[log_slot as usize % 8];
                let size = a.u32() % (1 << 20);
                let (writes, reads) = (a.u8() as u32, a.u8() as u32);
                CreateLogMarshallingArea(log, null(), null(), null_mut(), size, writes, reads, &mut self.marshals[slot])
            }
            // Append a record: flags, where its LSN goes, its undo-next and
            // previous LSNs, its bytes.
            4 => {
                let area = a.u8();
                let marshal = self.marshal(area);
                let flags = a.u8() as u32;
                let into = a.u8() as usize % 16;
                let undo = self.lsn(&mut a);
                let previous = self.lsn(&mut a);
                let data = a.rest();
                let entry = WriteEntry { buffer: data.as_ptr().cast(), length: data.len() as u32 };
                let mut lsn = 0;
                let o = self.ov(self.area_async(area));
                let ok = self.finish(ReserveAndAppendLog(marshal, &entry, 1, &undo, &previous, 0, null_mut(), flags, &mut lsn, o), o);
                self.lsns[into] = lsn;
                ok
            }
            5 => {
                let area = a.u8();
                let o = self.ov(self.area_async(area));
                self.finish(FlushLogBuffers(self.marshal(area), o), o)
            }
            // Read from an LSN in a mode, then the next records.
            6 => {
                let area = a.u8();
                let marshal = self.marshal(area);
                let mode = 1 + a.u8() as u32 % 3;
                let first = self.lsn(&mut a);
                let more = a.u8() % 8;
                let (mut buffer, mut size, mut kind, mut undo, mut previous, mut context) = (null_mut(), 0, 0u8, 0, 0, null_mut());
                let o = self.ov(self.area_async(area));
                if self.finish(ReadLogRecord(marshal, &first, mode, &mut buffer, &mut size, &mut kind, &mut undo, &mut previous, &mut context, o), o) == 0 {
                    return 0;
                }
                self.lsns[15] = undo;
                for _ in 0..more {
                    let mut record = 0;
                    let o = self.ov(self.area_async(area));
                    if self.finish(ReadNextLogRecord(context, &mut buffer, &mut size, &mut kind, null(), &mut undo, &mut previous, &mut record, o), o) == 0 {
                        break;
                    }
                    self.lsns[14] = record;
                }
                TerminateReadLog(context)
            }
            7 => {
                let slot = a.u8();
                if slot & 0x80 != 0 {
                    let m = std::mem::replace(&mut self.marshals[slot as usize % 8], null_mut());
                    DeleteLogMarshallingArea(m)
                } else {
                    let h = std::mem::replace(&mut self.logs[slot as usize % 8], null_mut());
                    CloseHandle(h)
                }
            }
            // Move the log's ends: archive tail, base, end.
            8 => {
                let slot = a.u8();
                let which = a.u8() % 3;
                let lsn = self.lsn(&mut a);
                match which {
                    0 => SetLogArchiveTail(self.log(slot), &lsn, null_mut()),
                    1 => {
                        let o = self.ov(self.area_async(slot));
                        self.finish(AdvanceLogBase(self.marshal(slot), &lsn, 0, o), o)
                    }
                    _ => {
                        let o = self.ov(self.area_async(slot));
                        self.finish(TruncateLog(self.marshal(slot), &lsn, o), o)
                    }
                }
            }
            9 => {
                let mut info = [0u8; 512];
                let mut size = a.u16() as u32 % 513;
                GetLogFileInformation(self.log(a.u8()), info.as_mut_ptr().cast(), &mut size)
            }
            // Any IOCTL on a log handle: code, output size, input bytes.
            10 => {
                let log = self.log(a.u8());
                let code = a.u32();
                let mut out = vec![0u8; a.u16() as usize % 4097];
                let input = a.rest();
                let mut returned = 0;
                DeviceIoControl(log, code, input.as_ptr().cast(), input.len() as u32, out.as_mut_ptr().cast(), out.len() as u32, &mut returned, null_mut())
            }
            // Bytes into a base log file (closed: read again when opened).
            11 => {
                let name = a.u8() % 4;
                let offset = a.u32() % 0x10_0000;
                let data = a.rest();
                let f = CreateFileW(wide(&format!("C:\\clfs\\l{name}.blf")).as_ptr(), GENERIC_WRITE, SHARE_ALL, null(), 3, 0, null_mut());
                if f == INVALID {
                    return 0;
                }
                let mut written = 0;
                let ok = SetFilePointerEx(f, offset as i64, null_mut(), 0) != 0 && WriteFile(f, data.as_ptr().cast(), data.len() as u32, &mut written, null_mut()) != 0;
                CloseHandle(f);
                i32::from(ok)
            }
            12 => DeleteLogFile(wide(&format!("LOG:C:\\clfs\\l{}", a.u8() % 4)).as_ptr(), null_mut()),
            13 => {
                let marshal = self.marshal(a.u8());
                let count = a.u8() as u32 % 8;
                let mut adjustment = (a.u32() % (8 << 20)) as i64;
                if a.u8() & 1 == 0 {
                    AllocReservedLog(marshal, count, &mut adjustment)
                } else {
                    FreeReservedLog(marshal, count, &mut adjustment)
                }
            }
            // Scan containers forward or backward (mode bit 0): the
            // context comes with the first ones; the next, then close.
            14 => {
                let log = self.log(a.u8());
                let from = a.u8() as u32;
                let count = 1 + a.u8() as u32 % 8;
                let direction = if a.u8() & 1 == 0 { CLFS_SCAN_FORWARD } else { CLFS_SCAN_BACKWARD };
                let mut scan = [0u64; 16];
                if CreateLogContainerScanContext(log, from, count, direction, scan.as_mut_ptr().cast(), null_mut()) == 0 {
                    return 0;
                }
                let ok = ScanLogContainers(scan.as_mut_ptr().cast(), direction, null_mut());
                let error = GetLastError();
                ScanLogContainers(scan.as_mut_ptr().cast(), CLFS_SCAN_CLOSE, null_mut());
                SetLastError(error);
                ok
            }
            // Restart areas: write one (flags, base LSN, bytes), or read the
            // latest and the ones before it.
            15 => {
                let area = a.u8();
                let marshal = self.marshal(area);
                let flags = a.u8() as u32;
                let base = self.lsn(&mut a);
                let data = a.rest();
                let (mut written, mut next) = (0, 0);
                let o = self.ov(self.area_async(area));
                let ok = self.finish(WriteLogRestartArea(marshal, data.as_ptr().cast(), data.len() as u32, &base, flags, &mut written, &mut next, o), o);
                self.lsns[13] = next;
                ok
            }
            16 => {
                let area = a.u8();
                let marshal = self.marshal(area);
                let more = a.u8() % 8;
                let (mut buffer, mut size, mut lsn, mut context) = (null_mut(), 0, 0, null_mut());
                let o = self.ov(self.area_async(area));
                if self.finish(ReadLogRestartArea(marshal, &mut buffer, &mut size, &mut lsn, &mut context, o), o) == 0 {
                    return 0;
                }
                self.lsns[12] = lsn;
                for _ in 0..more {
                    let o = self.ov(self.area_async(area));
                    if self.finish(ReadPreviousLogRestartArea(context, &mut buffer, &mut size, &mut lsn, o), o) == 0 {
                        break;
                    }
                }
                TerminateReadLog(context)
            }
            // Archive: between two LSNs, extents, base log metadata.
            17 => {
                let log = self.log(a.u8());
                let (low, high) = (self.lsn(&mut a), self.lsn(&mut a));
                let extents = a.u8() as u32 % 5;
                let (offset, length) = (a.u32() % 0x10_0000, a.u16() as u32);
                let mut name = [0u16; 260];
                let (mut actual, mut off, mut len, mut base, mut last, mut tail, mut context) = (0, 0, 0, 0, 0, 0, null_mut());
                let bounds = a.u8();
                let (pl, ph) = (if bounds & 1 != 0 { &low as *const Lsn } else { null() }, if bounds & 2 != 0 { &high as *const Lsn } else { null() });
                if PrepareLogArchive(log, name.as_mut_ptr(), name.len() as u32, pl, ph, &mut actual, &mut off, &mut len, &mut base, &mut last, &mut tail, &mut context) == 0 {
                    return 0;
                }
                let mut descriptors = vec![0u8; 1024 * 4];
                let mut returned = 0;
                GetNextLogArchiveExtent(context, descriptors.as_mut_ptr().cast(), extents, &mut returned);
                let mut buffer = vec![0u8; length as usize];
                let mut read = 0;
                ReadLogArchiveMetadata(context, offset, length, buffer.as_mut_ptr(), &mut read);
                TerminateLogArchive(context)
            }
            // Container sets: add (size) or remove (force) up to 4 at once.
            18 => {
                let log = self.log(a.u8());
                let how = a.u8();
                let count = 1 + a.u8() % 4;
                let mut size = u64::from(a.u32() % (8 << 20));
                let names: Vec<Vec<u16>> = (0..count).map(|_| wide(&format!("C:\\clfs\\c{}", a.u8() % 8))).collect();
                let paths: Vec<*const u16> = names.iter().map(|n| n.as_ptr()).collect();
                if how & 1 == 0 {
                    AddLogContainerSet(log, count as u16, &mut size, paths.as_ptr(), null_mut())
                } else {
                    RemoveLogContainerSet(log, count as u16, paths.as_ptr(), i32::from(how & 2 != 0), null_mut())
                }
            }
            19 => {
                let log = self.log(a.u8());
                let container = a.u32();
                let mut name = vec![0u16; a.u16() as usize % 1025];
                let mut actual = 0;
                GetLogContainerName(log, container, name.as_mut_ptr(), name.len() as u32, &mut actual)
            }
            // Aligned reservations, then an aligned append with them.
            20 => {
                let area = a.u8();
                let marshal = self.marshal(area);
                let count = 1 + a.u8() as usize % 4;
                let mut sizes: Vec<i64> = (0..count).map(|_| (a.u32() % (8 << 20)) as i64).collect();
                let mut aligned = 0;
                if AlignReservedLog(marshal, count as u32, sizes.as_ptr(), &mut aligned) == 0 {
                    return 0;
                }
                let alignment = a.u32() % 0x1_0001;
                let flags = a.u8() as u32;
                let into = a.u8() as usize % 16;
                let undo = self.lsn(&mut a);
                let previous = self.lsn(&mut a);
                let data = a.rest();
                let entry = WriteEntry { buffer: data.as_ptr().cast(), length: data.len() as u32 };
                let mut lsn = 0;
                let o = self.ov(self.area_async(area));
                let ok = self.finish(ReserveAndAppendLogAligned(marshal, &entry, 1, alignment, &undo, &previous, count as u32, sizes.as_mut_ptr(), flags, &mut lsn, o), o);
                self.lsns[into] = lsn;
                ok
            }
            21 => {
                let slot = a.u8();
                let log = self.log(slot);
                let end = self.lsn(&mut a);
                let o = self.ov(self.async_logs[slot as usize % 8]);
                self.finish(SetEndOfLog(log, &end, o), o)
            }
            // Management policies: install one from raw bytes (version,
            // length, flags, type, parameters, as the payload says), remove
            // or query one by type.
            22 => {
                let log = self.log(a.u8());
                let mut policy = vec![0u8; 1024];
                let raw = a.rest();
                policy[..raw.len().min(1024)].copy_from_slice(&raw[..raw.len().min(1024)]);
                InstallLogPolicy(log, policy.as_ptr().cast())
            }
            23 => {
                let log = self.log(a.u8());
                let kind = a.u8() as u32 % 11;
                if a.u8() & 1 == 0 {
                    RemoveLogPolicy(log, kind)
                } else {
                    let mut policy = vec![0u8; 1024];
                    let mut size = a.u16() as u32 % 1025;
                    QueryLogPolicy(log, kind, policy.as_mut_ptr().cast(), &mut size)
                }
            }
            24 => {
                let log = self.log(a.u8());
                if a.u8() & 1 == 0 {
                    let desired = u64::from(a.u32() % 64);
                    let mut resulting = 0;
                    SetLogFileSizeWithPolicy(log, &desired, &mut resulting)
                } else {
                    HandleLogFull(log)
                }
            }
            25 => {
                let log = self.log(a.u8());
                SetLogArchiveMode(log, a.u8() as u32 % 4)
            }
            _ => 1,
        }
    }

    fn run(&mut self, program: &[u8]) {
        let mut at = 0;
        while at + 6 <= program.len() {
            let op = u32::from_le_bytes(program[at..at + 4].try_into().expect("four bytes"));
            let length = u16::from_le_bytes(program[at + 4..at + 6].try_into().expect("two bytes")) as usize;
            let payload = &program[at + 6..(at + 6 + length).min(program.len())];
            at += 6 + length;
            let ok = unsafe { self.call(op, payload) };
            let error = if ok != 0 { 0 } else { unsafe { GetLastError() } };
            if self.verbose {
                println!("op {op} ({} bytes): {}", payload.len(), if ok != 0 { "ok".to_string() } else { format!("error {error}") });
            }
            self.results.push((op & 0xff) << 24 | (error & 0xff_ffff));
        }
    }
}

fn state(verbose: bool) -> State {
    State {
        logs: [null_mut(); 8],
        marshals: [null_mut(); 8],
        async_logs: [false; 8],
        async_areas: [false; 8],
        overlapped: Vec::new(),
        lsns: [0; 16],
        results: Vec::with_capacity(1024),
        verbose,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    unsafe { CreateDirectoryW(wide("C:\\clfs").as_ptr(), null()) };
    match args.get(1).map(String::as_str) {
        Some("run") => {
            let program = std::fs::read(args.get(2).expect("clfs-client run FILE")).expect("read the program");
            let mut s = state(true);
            s.run(&program);
            done(s.results.as_ptr());
        }
        Some("wait") => unsafe {
            let page = VirtualAlloc(null_mut(), PAGE, 0x3000, 4).cast::<u8>();
            assert!(!page.is_null(), "VirtualAlloc");
            VirtualLock(page.cast(), PAGE);
            page.write_bytes(0, PAGE);
            INPUT = page;
            let mut s = state(false);
            let report = format!(
                "input_va={:#x} input_pointer_va={:#x} input_length_va={:#x} entry_pc={:#x} stop_pc={:#x} capacity={PAGE}\n",
                page as usize,
                std::ptr::addr_of!(INPUT) as usize,
                std::ptr::addr_of!(LEN) as usize,
                park as *const () as usize,
                done as *const () as usize,
            );
            std::fs::write("C:\\clfs\\client.txt", &report).expect("write C:\\clfs\\client.txt");
            print!("{report}");
            loop {
                while std::ptr::read_volatile(std::ptr::addr_of!(LEN)) == 0 {
                    park();
                }
                let len = (std::ptr::read_volatile(std::ptr::addr_of!(LEN)) as usize).min(PAGE);
                let program = std::slice::from_raw_parts(INPUT, len);
                s.run(program);
                done(s.results.as_ptr());
                std::ptr::write_volatile(std::ptr::addr_of_mut!(LEN), 0);
            }
        },
        _ => eprintln!("clfs-client wait | run FILE"),
    }
}
