// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Per-process file descriptors (phase 2.6), on top of the VFS.
//
// The Unix split, kept honest:
//
//   * an *open file description* (`FileDesc`) is what one `open` produced:
//     what it is connected to (a VFS slot, a device, one end of a pipe) plus
//     the flags it was opened with.  It is `Rc`-shared, so a `fork` hands the
//     child the *same* descriptions (not copies: the file offset of a VFS
//     slot is shared, exactly like after a real fork), and `dup` adds a
//     second fd number for the same one;
//   * a *fd table* (`FdTable`) is the per-process array fd -> description.
//     Every process starts with 0/1/2 on the console (three separate
//     descriptions, like any unix boot), `fork` copies the table (Rc bumps),
//     `execve` keeps it as is (POSIX), and death releases it: dropping the
//     last clone of a description is what closes the underlying thing -- the
//     VFS slot for a file, the end of a pipe for a pipe.
//
// What a description can be connected to:
//
//   * `Vfs(slot)`   a slot of the global open-file table (fs/vfs.rs): regular
//                   files and directories; the offset, flags and permission
//                   checks live there;
//   * `Console`     /dev/console: reads park until a key or serial byte
//                   arrives, writes go through the sanitising console path
//                   (the same one fd 1 / fd 2 use);
//   * `DevNull`     /dev/null: reads are an immediate EOF, writes swallowed;
//   * `DevZero`     /dev/zero: reads produce zeroes, writes swallowed;
//   * `PipeRead/Write` the two ends of an anonymous pipe: one ring buffer
//                   shared by both ends, plus the count of live descriptions
//                   per end, so a reader can tell EOF (`writers == 0`) from
//                   would-block and a writer gets -EPIPE (`readers == 0`)
//                   instead of blocking forever.
//
// Syscalls (the ABI grows by six, Linux-style numbers; see syscall.rs):
//    13 read(fd, buf, len)     14 open(path, flags)    15 close(fd)
//    16 lseek(fd, off, whence) 17 dup(fd)              18 pipe(int *fds)
//   and `write` (1) now routes through the table too (it used to special-case
//   fd 1/2 straight to the console).
//
// Blocking: a pipe read on an empty buffer (writers alive) and a console
// read with nothing typed park the process on the scheduler -- 20 ms at a
// time, re-checked on every wake -- for at most 10 s, then -EAGAIN.  The
// bound is deliberate: the shell is blocked while its children run, so
// nothing may wait forever (the same rule sleep() follows).  A pipe write
// with no readers is -EPIPE at once; there is no SIGPIPE delivery yet.
//
// Not here: O_NONBLOCK / O_CLOEXEC, dup2 / fcntl, per-process cwd (paths
// resolve against the shell's, like execve's), getdents (a directory fd
// opens but cannot be read), pipe atomicity promises beyond FIFO order,
// and anything about permissions (single user, ring 0 -- see the README).

use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::fs::devfs::{DEV_CONSOLE, DEV_NULL, DEV_ZERO};
use crate::fs::vfs::{FsError, O_APPEND, O_CREATE, O_EXCL, O_READ, O_TRUNC, O_WRITE, Opened, Whence};
use crate::mem::vmm::AddressSpace;
use crate::user::process;
use crate::user::usermode::{EBADF, EFAULT, EINVAL};

// ------------------------------------------------------------- limits

/// Descriptors per process (fd 0..63); one more open is -EMFILE.
pub const MAX_FDS: usize = 64;
/// Most one read/write call moves: the rest needs another call.
const MAX_RW: usize = 4096;
/// Kernel scratch per chunk (a syscall must not eat the 16 KiB stack).
const CHUNK: usize = 256;
/// Pipe buffer size (classic 16 KiB, a quarter of Linux's 64 KiB).
const PIPE_CAPACITY: usize = 16 * 1024;
/// Longest a blocking read parks before -EAGAIN (see the header).
const WAIT_MAX_MS: u64 = 10_000;
/// Park granularity: ~2 ticks (the one in flight plus the next).
const PARK_MS: u64 = 10;

// ------------------------------------------------------------- errno
//
// Local ones (the shared set lives in usermode.rs); FsError -> errno below.

const ENOENT: i64 = 2;
const EAGAIN: i64 = 11;
const EEXIST: i64 = 17;
const ENODEV: i64 = 19;
const EISDIR: i64 = 21;
const EMFILE: i64 = 24;
const EFBIG: i64 = 27;
const ESPIPE: i64 = 29;
const EPIPE: i64 = 32;
const ENAMETOOLONG: i64 = 36;

/// Linux-style errno for a VFS error (mirrors the map elf.rs uses).
fn fs_errno(e: FsError) -> i64 {
    match e {
        FsError::NotFound | FsError::NotDir => ENOENT,
        FsError::Exists => EEXIST,
        FsError::IsDir => EISDIR,
        FsError::NameTooLong | FsError::Loop => ENAMETOOLONG,
        FsError::TooBig => EFBIG,
        FsError::NoSpace => 28,  // ENOSPC
        FsError::ReadOnly => 30, // EROFS
        FsError::Busy => 16,     // EBUSY
        FsError::BadFd => EBADF,
        FsError::CrossDevice => 18, // EXDEV
        FsError::Invalid => EINVAL,
        FsError::Unsupported => 38, // ENOSYS: no better code for "not here"
        _ => 5,                    // EIO
    }
}

// ------------------------------------------------------- open file tables

/// What one description is connected to (see the header).
enum Kind {
    Vfs(usize),
    Console,
    DevNull,
    DevZero,
    PipeRead(Rc<RefCell<Pipe>>),
    PipeWrite(Rc<RefCell<Pipe>>),
}

/// One open file description: Rc-shared by the fd tables that reference it.
pub struct FileDesc {
    kind: Kind,
    /// The flags `open` got (O_READ / O_WRITE / O_APPEND): readable and
    /// writable are checked here, uniformly for every kind.
    flags: u32,
}

impl FileDesc {
    fn new(kind: Kind, flags: u32) -> FileDesc {
        FileDesc { kind, flags }
    }
}

impl Drop for FileDesc {
    fn drop(&mut self) {
        // The last clone is gone: release what the description held.  A VFS
        // slot close is in-memory only (no disk I/O), so this is safe from
        // the exit paths too.  Pipe ends just keep the peer counts honest.
        match &self.kind {
            Kind::Vfs(slot) => {
                let _ = crate::fs::fsys::vfs().close(*slot);
            }
            Kind::PipeRead(p) => p.borrow_mut().readers -= 1,
            Kind::PipeWrite(p) => p.borrow_mut().writers -= 1,
            _ => {}
        }
    }
}

/// The shared state of one pipe: the ring buffer plus how many descriptions
/// are alive on each end (a `dup` / a fork clone shares a description, so
/// it does not count twice).
struct Pipe {
    buf: VecDeque<u8>,
    readers: u32,
    writers: u32,
}

/// The per-process fd table: fd numbers are indices, 0/1/2 = stdio.
pub struct FdTable {
    slots: Vec<Option<Rc<FileDesc>>>,
}

impl FdTable {
    /// A fresh table with stdin/stdout/stderr on the console -- three
    /// separate descriptions, like any unix boot (a dup of one of them
    /// would share the flags; here stdout and stderr stay independent).
    pub fn with_stdio() -> FdTable {
        FdTable {
            slots: alloc::vec![
                Some(Rc::new(FileDesc::new(Kind::Console, O_READ))),
                Some(Rc::new(FileDesc::new(Kind::Console, O_WRITE))),
                Some(Rc::new(FileDesc::new(Kind::Console, O_WRITE))),
            ],
        }
    }

    /// What `fork` hands the child: the same descriptions (Rc bumps), so
    /// the child and the parent share file offsets and pipe ends.
    pub fn fork_copy(&self) -> FdTable {
        FdTable { slots: self.slots.clone() }
    }

    /// The description behind `fd`, if the slot is occupied.
    fn get(&self, fd: usize) -> Option<Rc<FileDesc>> {
        self.slots.get(fd).and_then(|s| s.clone())
    }

    /// Install at the lowest free number (like every unix); -EMFILE's worth
    /// of a full table is the caller's problem.
    fn install(&mut self, d: Rc<FileDesc>) -> Option<usize> {
        if let Some(i) = self.slots.iter().position(|s| s.is_none()) {
            self.slots[i] = Some(d);
            return Some(i);
        }
        if self.slots.len() >= MAX_FDS {
            return None;
        }
        self.slots.push(Some(d));
        Some(self.slots.len() - 1)
    }

    /// Release `fd`'s slot; false (and untouched) when it was not open.
    fn close(&mut self, fd: usize) -> bool {
        match self.slots.get_mut(fd) {
            Some(slot @ Some(_)) => {
                *slot = None;
                true
            }
            _ => false,
        }
    }

    /// Drop every description the table holds (death closes all
    /// descriptors: the last clone of each is what closes the file / pipe
    /// end).  Leaves the table empty; a zombie holds nothing open.
    pub fn release(&mut self) {
        self.slots.clear();
    }
}

// ---------------------------------------------------- user memory helpers

/// Copy `data` to user memory at `buf + done` (the read() direction):
/// copy-on-write pages are resolved first, then the plain write.
fn copy_out(buf: u64, done: usize, data: &[u8]) -> bool {
    let Some(cur) = process::current() else { return false };
    let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return false };
    let at = buf + done as u64;
    aspace.ensure_writable(at) && aspace.write(at, data, false)
}

/// Copy `n` bytes from user memory at `buf + done` into `into` (the write()
/// direction).  Returns false when the page is not readable.
fn copy_in(buf: u64, done: usize, into: &mut [u8]) -> bool {
    let Some(cur) = process::current() else { return false };
    let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return false };
    aspace.read(buf + done as u64, into)
}

/// Next input byte from the keyboard or the serial line, if either has one
/// (the same merge the shell's line reader does; non-blocking).
fn get_input_byte() -> Option<u8> {
    if let Some(c) = crate::hw::kbd::pop_char() {
        return Some(c);
    }
    crate::hw::serial::pop_rx()
}

fn uptime_ms() -> u64 {
    crate::hw::pit::uptime_ms()
}

/// Park ~20 ms (see the header): refs into the process table must be gone
/// before the call -- the switch may move the table (another process fork).
fn park_once() {
    process::sleep_current(PARK_MS);
}

// -------------------------------------------------------------- read

/// `read(fd, buf, len)` (syscall 13): up to 4096 bytes per call, chunked
/// through a kernel scratch buffer.  A short count is returned as is; 0 on
/// a VFS EOF or the /dev/null EOF; -EAGAIN after 10 s of nothing to read.
pub fn sys_read(fd: u64, buf: u64, len: u64) -> i64 {
    let len = len.min(MAX_RW as u64) as usize;
    let desc = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(d) = cur.fds.get(fd as usize) else { return -EBADF };
        d
    };
    if desc.flags & O_READ == 0 {
        return -EBADF; // opened write-only: not a descriptor to read from
    }
    if len == 0 {
        return 0;
    }
    match &desc.kind {
        Kind::Vfs(slot) => read_vfs(*slot, buf, len),
        Kind::Console => read_console(buf, len),
        Kind::DevNull => 0, // EOF: the buffer is not even looked at
        Kind::DevZero => read_zero(buf, len),
        Kind::PipeRead(p) => read_pipe(p, buf, len),
        Kind::PipeWrite(_) => -EBADF,
    }
}

fn read_vfs(slot: usize, buf: u64, len: usize) -> i64 {
    let mut chunk = [0u8; CHUNK];
    let mut done = 0usize;
    while done < len {
        let n = (len - done).min(CHUNK);
        let got = match crate::fs::fsys::vfs().read(slot, &mut chunk[..n]) {
            Ok(0) => break, // EOF
            Ok(k) => k,
            Err(e) => return if done > 0 { done as i64 } else { -fs_errno(e) },
        };
        if !copy_out(buf, done, &chunk[..got]) {
            return if done > 0 { done as i64 } else { -EFAULT };
        }
        done += got;
    }
    done as i64
}

fn read_zero(buf: u64, len: usize) -> i64 {
    let zeros = [0u8; CHUNK];
    let mut done = 0usize;
    while done < len {
        let n = (len - done).min(CHUNK);
        if !copy_out(buf, done, &zeros[..n]) {
            return if done > 0 { done as i64 } else { -EFAULT };
        }
        done += n;
    }
    done as i64
}

fn read_console(buf: u64, len: usize) -> i64 {
    let start = uptime_ms();
    let mut chunk = [0u8; CHUNK];
    let mut got = 0usize;
    loop {
        // Take whatever is pending right now, at most one chunk: a short
        // count is a valid read, so the caller loops for the rest.
        while got < len.min(CHUNK) {
            let Some(b) = get_input_byte() else { break };
            chunk[got] = b;
            got += 1;
        }
        if got > 0 {
            return if copy_out(buf, 0, &chunk[..got]) { got as i64 } else { -EFAULT };
        }
        if uptime_ms() - start >= WAIT_MAX_MS {
            return -EAGAIN; // nothing typed: a way out, not a wedge
        }
        park_once();
    }
}

fn read_pipe(pipe: &Rc<RefCell<Pipe>>, buf: u64, len: usize) -> i64 {
    let start = uptime_ms();
    let mut chunk = [0u8; CHUNK];
    let mut done = 0usize;
    loop {
        // FIFO: bytes leave in the order they were written.  Take at most
        // one chunk out per pass, then deliver it to user memory.
        let buffered = pipe.borrow().buf.len();
        if buffered > 0 {
            let take = buffered.min(len - done).min(CHUNK);
            {
                let mut p = pipe.borrow_mut();
                for i in 0..take {
                    chunk[i] = p.buf.pop_front().unwrap_or(0);
                }
            }
            if !copy_out(buf, done, &chunk[..take]) {
                return if done > 0 { done as i64 } else { -EFAULT };
            }
            done += take;
            if done == len {
                return done as i64;
            }
            continue; // the buffer may hold more than a chunk
        }
        // Empty: EOF when no writer is left, otherwise wait for one.
        if pipe.borrow().writers == 0 {
            return done as i64; // 0 when nothing was read: the EOF itself
        }
        if uptime_ms() - start >= WAIT_MAX_MS {
            return if done > 0 { done as i64 } else { -EAGAIN };
        }
        park_once();
    }
}

// -------------------------------------------------------------- write

/// `write(fd, buf, len)` (syscall 1): same shape as read.  Console writes
/// are sanitised (no terminal escapes from ring 3); /dev/null and /dev/zero
/// swallow after validating the buffer; a pipe write blocks (bounded) when
/// the buffer is full and gets -EPIPE with no reader left.
pub fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    let len = len.min(MAX_RW as u64) as usize;
    let desc = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(d) = cur.fds.get(fd as usize) else { return -EBADF };
        d
    };
    if desc.flags & O_WRITE == 0 {
        return -EBADF;
    }
    if len == 0 {
        return 0;
    }
    match &desc.kind {
        Kind::Vfs(slot) => write_vfs(*slot, buf, len),
        Kind::Console => write_console(buf, len),
        // The sinks still validate the buffer (an unreadable pointer is
        // -EFAULT, like on any unix) before reporting success.
        Kind::DevNull | Kind::DevZero => write_sink(buf, len),
        Kind::PipeWrite(p) => write_pipe(p, buf, len),
        Kind::PipeRead(_) => -EBADF,
    }
}

fn write_vfs(slot: usize, buf: u64, len: usize) -> i64 {
    let mut chunk = [0u8; CHUNK];
    let mut done = 0usize;
    while done < len {
        let n = (len - done).min(CHUNK);
        if !copy_in(buf, done, &mut chunk[..n]) {
            return if done > 0 { done as i64 } else { -EFAULT };
        }
        // O_APPEND: the VFS slot re-reads the size on every write.
        let put = match crate::fs::fsys::vfs().write(slot, &chunk[..n]) {
            Ok(0) => return if done > 0 { done as i64 } else { -fs_errno(FsError::NoSpace) },
            Ok(k) => k,
            Err(e) => return if done > 0 { done as i64 } else { -fs_errno(e) },
        };
        done += put;
    }
    done as i64
}

fn write_console(buf: u64, len: usize) -> i64 {
    let mut chunk = [0u8; CHUNK];
    let mut done = 0usize;
    while done < len {
        let n = (len - done).min(CHUNK);
        if !copy_in(buf, done, &mut chunk[..n]) {
            return if done > 0 { done as i64 } else { -EFAULT };
        }
        crate::console::write_bytes(&chunk[..n]);
        done += n;
    }
    done as i64
}

/// /dev/null and /dev/zero: validate the whole buffer, report it written.
fn write_sink(buf: u64, len: usize) -> i64 {
    let mut chunk = [0u8; CHUNK];
    let mut done = 0usize;
    while done < len {
        let n = (len - done).min(CHUNK);
        if !copy_in(buf, done, &mut chunk[..n]) {
            return if done > 0 { done as i64 } else { -EFAULT };
        }
        done += n;
    }
    done as i64
}

fn write_pipe(pipe: &Rc<RefCell<Pipe>>, buf: u64, len: usize) -> i64 {
    let start = uptime_ms();
    let mut chunk = [0u8; CHUNK];
    let mut done = 0usize;
    loop {
        // A readerless pipe refuses the write no matter the space (POSIX
        // EPIPE beats a wedged writer); checked before the space, or a
        // short buffer would swallow it.
        if pipe.borrow().readers == 0 {
            return if done > 0 { done as i64 } else { -EPIPE };
        }
        let space = PIPE_CAPACITY - pipe.borrow().buf.len();
        if space > 0 {
            let take = space.min(len - done).min(CHUNK);
            if !copy_in(buf, done, &mut chunk[..take]) {
                return if done > 0 { done as i64 } else { -EFAULT };
            }
            {
                let mut p = pipe.borrow_mut();
                for i in 0..take {
                    p.buf.push_back(chunk[i]);
                }
            }
            done += take;
            if done == len {
                return done as i64;
            }
            continue;
        }
        if uptime_ms() - start >= WAIT_MAX_MS {
            return if done > 0 { done as i64 } else { -EAGAIN };
        }
        park_once();
    }
}

// -------------------------------------------------------------- open/close

/// `open(path, flags)` (syscall 14).  Flags are the kernel's own O_ values
/// (see fs/vfs.rs); relative paths resolve against the current directory of
/// the shell, like execve's.  Devices resolve through /dev (devfs): the
/// node gives the id, the id gives the descriptor.
pub fn sys_open(path_p: u64, flags: u64) -> i64 {
    const KNOWN: u32 = O_READ | O_WRITE | O_CREATE | O_TRUNC | O_APPEND | O_EXCL;
    if flags > KNOWN as u64 {
        return -EINVAL; // unknown bits: refuse rather than guess
    }
    let flagbits = flags as u32 & KNOWN;
    // Read the path out of user memory (chunk-wise, page-aware).
    let got: Result<String, i64> = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return -EFAULT };
        match crate::user::syscall::read_user_cstr(aspace, path_p, 255) {
            Ok(v) => match String::from_utf8(v) {
                Ok(s) => Ok(s),
                Err(_) => Err(-ENOENT), // like execve: a non-utf8 path is a no-such-file
            },
            Err(-7) => Err(-ENAMETOOLONG), // E2BIG from the reader = path too long
            Err(e) => Err(e),
        }
    };
    let path = match got {
        Ok(p) => p,
        Err(e) => return e,
    };
    if path.is_empty() {
        return -ENOENT;
    }
    let abs = crate::fs::fsys::abs(&path);
    let opened = match crate::fs::fsys::vfs().open_desc(&abs, flagbits) {
        Ok(o) => o,
        Err(e) => return -fs_errno(e),
    };
    let kind = match opened {
        Opened::File(slot) => Kind::Vfs(slot),
        Opened::Dev(id) => match id {
            DEV_CONSOLE => Kind::Console,
            DEV_NULL => Kind::DevNull,
            DEV_ZERO => Kind::DevZero,
            _ => return -ENODEV, // a node without a device behind it
        },
    };
    let desc = Rc::new(FileDesc::new(kind, flagbits));
    let Some(cur) = process::current() else { return -EFAULT };
    match cur.fds.install(desc) {
        Some(fd) => fd as i64,
        None => -EMFILE,
    }
}

/// `close(fd)` (syscall 15): free the number; when that was the last
/// reference, the description itself is what closes the file / pipe end.
pub fn sys_close(fd: u64) -> i64 {
    let Some(cur) = process::current() else { return -EFAULT };
    if cur.fds.close(fd as usize) {
        0
    } else {
        -EBADF
    }
}

// -------------------------------------------------------------- lseek / dup

/// `lseek(fd, off, whence)` (syscall 16): only a VFS description has an
/// offset to move; pipes and devices answer -ESPIPE.
pub fn sys_lseek(fd: u64, off: u64, whence: u64) -> i64 {
    let desc = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(d) = cur.fds.get(fd as usize) else { return -EBADF };
        d
    };
    let Kind::Vfs(slot) = &desc.kind else {
        return -ESPIPE;
    };
    // `off` arrives as raw bits: -4 comes through as 2^64-4, so the signed
    // view (CUR / END) is the one the arithmetic wants.  SET keeps it raw.
    let w = match whence {
        0 => Whence::Set(off),
        1 => Whence::Cur(off as i64),
        2 => Whence::End(off as i64),
        _ => return -EINVAL,
    };
    match crate::fs::fsys::vfs().seek(*slot, w) {
        Ok(pos) => pos as i64,
        Err(e) => -fs_errno(e),
    }
}

/// `dup(fd)` (syscall 17): a second number for the *same* description --
/// shared offset, shared flags, the close of one number leaves the other
/// working (that is what dup means on a unix).
pub fn sys_dup(fd: u64) -> i64 {
    let desc = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(d) = cur.fds.get(fd as usize) else { return -EBADF };
        d
    };
    let Some(cur) = process::current() else { return -EFAULT };
    match cur.fds.install(desc.clone()) {
        Some(nfd) => nfd as i64,
        None => -EMFILE,
    }
}

// -------------------------------------------------------------- pipe

/// `pipe(int *fds)` (syscall 18): two descriptions on one buffer, installed
/// as `fds[0] = read end, fds[1] = write end` (the POSIX order).  On any
/// failure both are closed again, so the pair is all-or-nothing.
pub fn sys_pipe(ptr: u64) -> i64 {
    let pipe = Rc::new(RefCell::new(Pipe { buf: VecDeque::new(), readers: 1, writers: 1 }));
    let r = Rc::new(FileDesc::new(Kind::PipeRead(pipe.clone()), O_READ));
    let w = Rc::new(FileDesc::new(Kind::PipeWrite(pipe), O_WRITE));
    let installed = {
        let Some(cur) = process::current() else { return -EFAULT };
        let rfd = match cur.fds.install(r) {
            Some(f) => f,
            None => return -EMFILE,
        };
        match cur.fds.install(w) {
            Some(f) => (rfd, f),
            None => {
                cur.fds.close(rfd); // releases the read end (readers -> 0)
                return -EMFILE;
            }
        }
    };
    let (rfd, wfd) = installed;
    let pair = [(rfd as i32).to_le_bytes(), (wfd as i32).to_le_bytes()];
    let ok = copy_out(ptr, 0, &pair[0]) && copy_out(ptr, 4, &pair[1]);
    if !ok {
        // Undo: both ends closed, the peer counts drop together.
        let Some(cur) = process::current() else { return -EFAULT };
        cur.fds.close(rfd);
        cur.fds.close(wfd);
        return -EFAULT;
    }
    0
}
