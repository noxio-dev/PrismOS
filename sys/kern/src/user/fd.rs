// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Per-process file descriptors (phase 2.6, extended in 3.1), on top of the VFS.
//
// The Unix split, kept honest:
//
//   * an *open file description* (`FileDesc`) is what one `open` produced:
//     what it is connected to (a VFS slot, a device, one end of a pipe) plus
//     the flags it was opened with.  It is `Rc`-shared, so a `fork` hands the
//     child the *same* descriptions (not copies: the file offset of a VFS
//     slot is shared, exactly like after a real fork), and `dup` / `dup2`
//     add a second fd number for the same one.  The flags are a `Cell`:
//     `fcntl(F_SETFL)` mutates the description every number shares (POSIX);
//   * a *fd table* (`FdTable`) is the per-process array fd -> description.
//     Every process starts with 0/1/2 on the console (three separate
//     descriptions, like any unix boot), `fork` copies the table (Rc bumps),
//     `execve` keeps it minus the O_CLOEXEC numbers (3.1), and death
//     releases it: dropping the last clone of a description is what closes
//     the underlying thing.  The FD_CLOEXEC bit (3.1) lives on the *number*,
//     in this table -- a dup of the number does not inherit it.
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
//                   would-block and a writer meets the POSIX SIGPIPE rule
//                   (`readers == 0`: terminate by default, -EPIPE when the
//                   signal is ignored or caught -- phase 3.1).
//
// Syscalls (the ABI grows Linux-style; see syscall.rs):
//    13 read(fd, buf, len)     14 open(path, flags)    15 close(fd)
//    16 lseek(fd, off, whence) 17 dup(fd)              18 pipe(int *fds)
//   and `write` (1) routes through the table too.  Phase 3.1 adds the
//   descriptor-half of the newlib set: 20 fstat, 26 dup2, 27 fcntl,
//   28 getdents, 29 ioctl, 30 unlink, 31 mkdir, 32 rmdir, 33 rename,
//   34 link (19 stat lives in user/sysc.rs, next to brk / mmap).
//
// Blocking: a pipe read on an empty buffer (writers alive) and a console
// read with nothing typed park the process -- 20 ms at a time, re-checked
// on every wake -- for at most 10 s, then -EAGAIN.  The bound is
// deliberate: the shell is blocked while its children run, so nothing may
// wait forever (the same rule sleep() follows).  O_NONBLOCK (3.1) turns
// both would-block paths into an immediate -EAGAIN, and a full pipe write
// ditto.
//
// Paths resolve against the *calling process's* cwd (phase 3.1; before
// that they went to the shell's).  Phase 3.2 adds the multiplex pair on
// top of the table: poll / select watch the descriptors through a
// readiness probe (files always ready, console input when a byte is
// pending, pipes by their buffer state), parking the bounded way.  Still
// not here: O_NONBLOCK on the console *output* (writes never block
// anyway), and anything about permissions (single user, ring 0 -- see the
// README).

use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use crate::fs::devfs::{DEV_CONSOLE, DEV_NULL, DEV_ZERO};
use crate::fs::vfs::{
    DirEntry, FsError, FileType, O_APPEND, O_CLOEXEC, O_CREATE, O_EXCL, O_NONBLOCK, O_READ,
    O_TRUNC, O_WRITE, Opened, Whence,
};
use crate::mem::vmm::AddressSpace;
use crate::user::process;
use crate::user::signal;
use crate::user::usermode::{EBADF, EFAULT, EINVAL};

// ------------------------------------------------------------- limits

/// Descriptors per process (fd 0..63); one more open is -EMFILE.
pub const MAX_FDS: usize = 64;
/// Most one read/write/getdents call moves: the rest needs another call.
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
const EIO: i64 = 5;
const EAGAIN: i64 = 11;
const EEXIST: i64 = 17;
const EXDEV: i64 = 18;
const ENODEV: i64 = 19;
const ENOTDIR: i64 = 20;
const EISDIR: i64 = 21;
const EMFILE: i64 = 24;
const ENOTTY: i64 = 25;
const EFBIG: i64 = 27;
const ESPIPE: i64 = 29;
const ENAMETOOLONG: i64 = 36;
const ENOSYS: i64 = 38;
const ENOTEMPTY: i64 = 39;
const EMLINK: i64 = 31;

/// Linux-style errno for a VFS error (mirrors the map elf.rs uses; the
/// phase 3.1 entries -- ENOTDIR, ENOTEMPTY, EMLINK -- make the new syscalls
/// answer what a POSIX program expects).
pub(crate) fn fs_errno(e: FsError) -> i64 {
    match e {
        FsError::NotFound => ENOENT,
        FsError::NotDir => ENOTDIR,
        FsError::Exists => EEXIST,
        FsError::IsDir => EISDIR,
        FsError::NotEmpty => ENOTEMPTY,
        FsError::NameTooLong | FsError::Loop => ENAMETOOLONG,
        FsError::TooBig => EFBIG,
        FsError::NoSpace => 28,  // ENOSPC
        FsError::ReadOnly => 30, // EROFS
        FsError::Busy => 16,     // EBUSY
        FsError::BadFd => EBADF,
        FsError::CrossDevice => EXDEV,
        FsError::TooManyLinks => EMLINK,
        FsError::Invalid => EINVAL,
        FsError::Unsupported => ENOSYS, // no better code for "not here"
        _ => EIO,
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
    /// The flags `open` got (O_READ / O_WRITE / O_APPEND / O_NONBLOCK):
    /// readable and writable are checked here, uniformly for every kind.
    /// A `Cell` because fcntl(F_SETFL) rewrites what every sharer sees.
    flags: Cell<u32>,
}

impl FileDesc {
    fn new(kind: Kind, flags: u32) -> FileDesc {
        FileDesc { kind, flags: Cell::new(flags) }
    }

    pub(crate) fn flags(&self) -> u32 {
        self.flags.get()
    }
}

/// The VFS open-file slot behind a description, when it is a file (the
/// mmap of user/sysc.rs needs it to fill a private mapping through
/// `pread`); devices and pipes answer None.
pub fn vfs_slot_of(desc: &FileDesc) -> Option<usize> {
    match &desc.kind {
        Kind::Vfs(slot) => Some(*slot),
        _ => None,
    }
}

/// The description behind `fd`, if the slot is occupied (sysc.rs's mmap
/// checks its readability itself).
pub fn desc_of(fd: usize) -> Option<Rc<FileDesc>> {
    process::current().and_then(|cur| cur.fds.get(fd))
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

/// One slot of the per-process table: the shared description plus the one
/// property that belongs to the *number* -- FD_CLOEXEC (phase 3.1).
struct Slot {
    desc: Rc<FileDesc>,
    cloexec: bool,
}

/// The per-process fd table: fd numbers are indices, 0/1/2 = stdio.
pub struct FdTable {
    slots: Vec<Option<Slot>>,
}

impl FdTable {
    /// A fresh table with stdin/stdout/stderr on the console -- three
    /// separate descriptions, like any unix boot (a dup of one of them
    /// would share the flags; here stdout and stderr stay independent).
    pub fn with_stdio() -> FdTable {
        let mk = |f| Some(Slot { desc: Rc::new(FileDesc::new(Kind::Console, f)), cloexec: false });
        FdTable { slots: alloc::vec![mk(O_READ), mk(O_WRITE), mk(O_WRITE)] }
    }

    /// What `fork` hands the child: the same descriptions (Rc bumps), so
    /// the child and the parent share file offsets and pipe ends -- and the
    /// same FD_CLOEXEC marks on the numbers (POSIX keeps them).
    pub fn fork_copy(&self) -> FdTable {
        FdTable {
            slots: self
                .slots
                .iter()
                .map(|s| s.as_ref().map(|x| Slot { desc: x.desc.clone(), cloexec: x.cloexec }))
                .collect(),
        }
    }

    /// The description behind `fd`, if the slot is occupied.
    fn get(&self, fd: usize) -> Option<Rc<FileDesc>> {
        self.slots.get(fd).and_then(|s| s.as_ref().map(|x| x.desc.clone()))
    }

    /// Install at the lowest free number (like every unix); -EMFILE's worth
    /// of a full table is the caller's problem.
    fn install(&mut self, d: Rc<FileDesc>, cloexec: bool) -> Option<usize> {
        if let Some(i) = self.slots.iter().position(|s| s.is_none()) {
            self.slots[i] = Some(Slot { desc: d, cloexec });
            return Some(i);
        }
        if self.slots.len() >= MAX_FDS {
            return None;
        }
        self.slots.push(Some(Slot { desc: d, cloexec }));
        Some(self.slots.len() - 1)
    }

    /// Install `d` at exactly `new` (the dup2 / F_DUPFD contract): the
    /// table grows with empty slots when needed, an occupied target is
    /// *replaced* (its old description drops a reference).  False past
    /// MAX_FDS.  The new number starts without FD_CLOEXEC (POSIX).
    fn install_at(&mut self, new: usize, d: Rc<FileDesc>) -> bool {
        if new >= MAX_FDS {
            return false;
        }
        while self.slots.len() <= new {
            self.slots.push(None);
        }
        self.slots[new] = Some(Slot { desc: d, cloexec: false });
        true
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

    /// The FD_CLOEXEC mark of `fd` (fcntl F_GETFD).
    fn cloexec_of(&self, fd: usize) -> Option<bool> {
        self.slots.get(fd).and_then(|s| s.as_ref().map(|x| x.cloexec))
    }

    /// Set / clear it (fcntl F_SETFD).
    fn set_cloexec(&mut self, fd: usize, on: bool) -> bool {
        match self.slots.get_mut(fd) {
            Some(Some(s)) => {
                s.cloexec = on;
                true
            }
            _ => false,
        }
    }

    /// `execve` (phase 3.1): close every number marked FD_CLOEXEC, keep the
    /// rest as POSIX says.  Returns how many were closed.
    pub fn close_exec(&mut self) -> usize {
        let mut n = 0;
        for s in self.slots.iter_mut() {
            if matches!(s, Some(x) if x.cloexec) {
                *s = None;
                n += 1;
            }
        }
        n
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

/// Read the NUL-terminated string at user address `ptr` and make it the
/// absolute path the *process* sees (its own cwd, phase 3.1).  Shared by
/// every path-taking syscall of this file.
fn user_path(ptr: u64) -> Result<String, i64> {
    let got: Result<String, i64> = {
        let Some(cur) = process::current() else { return Err(-EFAULT) };
        let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return Err(-EFAULT) };
        match crate::user::syscall::read_user_cstr(aspace, ptr, 255) {
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
        Err(e) => return Err(e),
    };
    if path.is_empty() {
        return Err(-ENOENT);
    }
    Ok(process::abs_current(&path))
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
/// a VFS EOF or the /dev/null EOF; -EAGAIN after 10 s of nothing to read,
/// or at once with O_NONBLOCK.
pub fn sys_read(fd: u64, buf: u64, len: u64) -> i64 {
    let len = len.min(MAX_RW as u64) as usize;
    let desc = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(d) = cur.fds.get(fd as usize) else { return -EBADF };
        d
    };
    if desc.flags() & O_READ == 0 {
        return -EBADF; // opened write-only: not a descriptor to read from
    }
    if len == 0 {
        return 0;
    }
    match &desc.kind {
        Kind::Vfs(slot) => read_vfs(*slot, buf, len),
        Kind::Console => read_console(&desc, buf, len),
        Kind::DevNull => 0, // EOF: the buffer is not even looked at
        Kind::DevZero => read_zero(buf, len),
        Kind::PipeRead(p) => read_pipe(p, &desc, buf, len),
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

fn read_console(desc: &Rc<FileDesc>, buf: u64, len: usize) -> i64 {
    let nonblock = desc.flags() & O_NONBLOCK != 0;
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
        if nonblock {
            return -EAGAIN; // nothing typed and the caller asked not to wait
        }
        if uptime_ms() - start >= WAIT_MAX_MS {
            return -EAGAIN; // nothing typed: a way out, not a wedge
        }
        park_once();
    }
}

fn read_pipe(pipe: &Rc<RefCell<Pipe>>, desc: &Rc<FileDesc>, buf: u64, len: usize) -> i64 {
    let nonblock = desc.flags() & O_NONBLOCK != 0;
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
        if nonblock {
            return if done > 0 { done as i64 } else { -EAGAIN };
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
/// the buffer is full, answers -EAGAIN at once with O_NONBLOCK, and meets
/// the SIGPIPE rule when no reader is left (phase 3.1: terminate by
/// default, -EPIPE with the signal ignored or caught).
pub fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    let len = len.min(MAX_RW as u64) as usize;
    let desc = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(d) = cur.fds.get(fd as usize) else { return -EBADF };
        d
    };
    if desc.flags() & O_WRITE == 0 {
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
        Kind::PipeWrite(p) => write_pipe(p, &desc, buf, len),
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

fn write_pipe(pipe: &Rc<RefCell<Pipe>>, desc: &Rc<FileDesc>, buf: u64, len: usize) -> i64 {
    let nonblock = desc.flags() & O_NONBLOCK != 0;
    let start = uptime_ms();
    let mut chunk = [0u8; CHUNK];
    let mut done = 0usize;
    loop {
        // A readerless pipe refuses the write no matter the space (POSIX
        // EPIPE beats a wedged writer); checked before the space, or a
        // short buffer would swallow it.  Phase 3.1: the refusal is the
        // SIGPIPE rule -- a short count already delivered goes back first,
        // then the signal side (terminate / -EPIPE) plays out.
        if pipe.borrow().readers == 0 {
            return if done > 0 {
                done as i64
            } else {
                signal::pipe_sigpipe() // -EPIPE, or death by signal 13
            };
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
        if nonblock {
            return if done > 0 { done as i64 } else { -EAGAIN };
        }
        if uptime_ms() - start >= WAIT_MAX_MS {
            return if done > 0 { done as i64 } else { -EAGAIN };
        }
        park_once();
    }
}

// -------------------------------------------------------------- open/close

/// `open(path, flags)` (syscall 14).  Flags are the kernel's own O_ values
/// (see fs/vfs.rs), now including O_NONBLOCK (kept on the description) and
/// O_CLOEXEC (phase 3.1, kept on the fd number).  Relative paths resolve
/// against the *process's* cwd.  Devices resolve through /dev (devfs): the
/// node gives the id, the id gives the descriptor.
pub fn sys_open(path_p: u64, flags: u64) -> i64 {
    const KNOWN: u32 = O_READ | O_WRITE | O_CREATE | O_TRUNC | O_APPEND | O_EXCL | O_NONBLOCK | O_CLOEXEC;
    if flags > KNOWN as u64 {
        return -EINVAL; // unknown bits: refuse rather than guess
    }
    let asked = flags as u32 & KNOWN;
    let cloexec = asked & O_CLOEXEC != 0;
    let flagbits = asked & !O_CLOEXEC; // description-level flags only
    let abs = match user_path(path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
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
    match cur.fds.install(desc, cloexec) {
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
/// offset to move; pipes and devices answer -ESPIPE.  On a directory the
/// offset is the getdents cursor, so `lseek(fd, 0, SET)` rewinds it (3.1).
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
/// working (that is what dup means on a unix).  The new number carries no
/// FD_CLOEXEC (POSIX; that mark belongs to a number, not a description).
pub fn sys_dup(fd: u64) -> i64 {
    let desc = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(d) = cur.fds.get(fd as usize) else { return -EBADF };
        d
    };
    let Some(cur) = process::current() else { return -EFAULT };
    match cur.fds.install(desc, false) {
        Some(nfd) => nfd as i64,
        None => -EMFILE,
    }
}

/// `dup2(old, new)` (syscall 26, phase 3.1): the description of `old`
/// lands on exactly `new`, whatever occupied it (closed first -- closing
/// 1 and dup2-ing a file onto it is the classic shell redirection trick).
/// `old == new` is a no-op that only checks the number.  The new number
/// starts without FD_CLOEXEC.
pub fn sys_dup2(old: u64, new: u64) -> i64 {
    if old >= MAX_FDS as u64 || new >= MAX_FDS as u64 {
        return -EBADF;
    }
    let Some(cur) = process::current() else { return -EFAULT };
    if old == new {
        return if cur.fds.get(old as usize).is_some() { new as i64 } else { -EBADF };
    }
    let desc = match cur.fds.get(old as usize) {
        Some(d) => d,
        None => return -EBADF,
    };
    let _ = cur.fds.close(new as usize); // an occupied target goes first
    if !cur.fds.install_at(new as usize, desc) {
        return -EBADF; // past MAX_FDS: guarded above, belt and braces
    }
    new as i64
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
        let rfd = match cur.fds.install(r, false) {
            Some(f) => f,
            None => return -EMFILE,
        };
        match cur.fds.install(w, false) {
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

// ------------------------------------------------- stat / fstat (phase 3.1)

/// The x86-64 Linux `struct stat` layout (144 bytes) -- the ABI the future
/// newlib port (phase 3.2) compiles against.  `dev` is 0 (no device
/// numbering yet), `rdev` carries the devfs id of a character device, and
/// the file type shows up in the mode's S_IFMT bits like everywhere.
pub const STAT_SIZE: usize = 144;

/// Serialize one VFS `Stat` (plus a device number for the pipe / device
/// fstat synthetics) into the user layout at `buf`.
fn put_stat(buf: u64, ino: u64, mode_perm: u16, ftype: &FileType, size: u64, nlink: u32, rdev: u64, blocks: u64, times: (u32, u32, u32)) -> bool {
    let ifmt: u32 = match ftype {
        FileType::Regular => 0o100000,
        FileType::Dir => 0o040000,
        FileType::Symlink => 0o120000,
        FileType::Dev(_) => 0o020000, // character device
        _ => 0o010000,                // fifo / other
    };
    let mode = ifmt | (mode_perm as u32 & 0o7777);
    let mut w = Vec::with_capacity(STAT_SIZE / 8);
    let mut push = |v: u64| w.extend_from_slice(&v.to_le_bytes());
    push(0);                       // st_dev
    push(ino);                     // st_ino
    push(nlink as u64);            // st_nlink
    push(mode as u64);             // st_mode (u32) + st_uid (u32)
    push(0);                       // st_gid + __pad0
    push(rdev);                    // st_rdev
    push(size);                    // st_size
    push(4096);                    // st_blksize
    push(blocks);                  // st_blocks (512-byte units)
    push(times.0 as u64);          // st_atime
    push(times.1 as u64);          // st_mtime
    push(times.2 as u64);          // st_ctime
    push(0);
    push(0);
    push(0);                       // __reserved[3]
    debug_assert_eq!(w.len(), STAT_SIZE);
    copy_out(buf, 0, &w)
}

/// `stat(path, statbuf)` (syscall 19, phase 3.1 -- dispatched from
/// user/sysc.rs's table but living here with the rest of the fs syscalls).
pub fn sys_stat(path_p: u64, buf: u64) -> i64 {
    let abs = match user_path(path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::fs::fsys::vfs().stat(&abs) {
        Ok(st) => {
            let times = (st.atime, st.mtime, st.ctime);
            if put_stat(buf, st.ino, st.mode, &st.ftype, st.size, st.nlink, dev_rdev(&st.ftype), st.blocks, times) {
                0
            } else {
                -EFAULT
            }
        }
        Err(e) => -fs_errno(e),
    }
}

/// `rdev` of a devfs node (major 1, like Linux's mem devices: /dev/zero
/// and friends -- the id is the minor).
fn dev_rdev(ftype: &FileType) -> u64 {
    match ftype {
        FileType::Dev(id) => (1 << 8) | *id as u64,
        _ => 0,
    }
}

/// `fstat(fd, statbuf)` (syscall 20, phase 3.1): the description decides --
/// a VFS slot asks the file system, a device synthesizes its node's stat
/// (what /dev/<name> would answer), a pipe end answers a fifo.
pub fn sys_fstat(fd: u64, buf: u64) -> i64 {
    let desc = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(d) = cur.fds.get(fd as usize) else { return -EBADF };
        d
    };
    match &desc.kind {
        Kind::Vfs(slot) => match crate::fs::fsys::vfs().fstat(*slot) {
            Ok(st) => {
                let times = (st.atime, st.mtime, st.ctime);
                if put_stat(buf, st.ino, st.mode, &st.ftype, st.size, st.nlink, dev_rdev(&st.ftype), st.blocks, times) {
                    0
                } else {
                    -EFAULT
                }
            }
            Err(e) => -fs_errno(e),
        },
        Kind::Console => put_dev_stat(buf, DEV_CONSOLE, 0o620),
        Kind::DevNull => put_dev_stat(buf, DEV_NULL, 0o666),
        Kind::DevZero => put_dev_stat(buf, DEV_ZERO, 0o666),
        // A pipe end fstats as a fifo: no inode, no size (the buffered
        // byte count is a snapshot nobody promises).
        Kind::PipeRead(_) | Kind::PipeWrite(_) => {
            if put_stat(buf, 0, 0o600, &FileType::Other, 0, 1, 0, 0, (0, 0, 0)) {
                0
            } else {
                -EFAULT
            }
        }
    }
}

fn put_dev_stat(buf: u64, id: u32, mode: u16) -> i64 {
    let now = crate::fs::vfs::now();
    let ftype = FileType::Dev(id);
    if put_stat(buf, id as u64, mode, &ftype, 0, 1, (1 << 8) | id as u64, 0, (now, now, now)) {
        0
    } else {
        -EFAULT
    }
}

// ------------------------------------------------- fcntl (phase 3.1)

/// `fcntl(fd, cmd, arg)` (syscall 27): F_DUPFD (dup at the lowest number
/// >= arg), the FD_CLOEXEC side of a number (F_GETFD / F_SETFD) and the
/// O_NONBLOCK / O_APPEND side of the *description* (F_GETFL / F_SETFL --
/// what every dup of the number shares).  Anything else is -EINVAL.
pub fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> i64 {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    let Some(cur) = process::current() else { return -EFAULT };
    match cmd {
        F_DUPFD => {
            if arg >= MAX_FDS as u64 {
                return -EINVAL;
            }
            let desc = match cur.fds.get(fd as usize) {
                Some(d) => d,
                None => return -EBADF,
            };
            // Lowest free number at or past arg (POSIX wording).
            let mut n = arg as usize;
            while n < MAX_FDS && cur.fds.cloexec_of(n).is_some() {
                n += 1;
            }
            if n >= MAX_FDS {
                return -EMFILE;
            }
            if cur.fds.install_at(n, desc) {
                n as i64
            } else {
                -EMFILE
            }
        }
        F_GETFD => match cur.fds.cloexec_of(fd as usize) {
            Some(on) => on as i64, // the only fd flag there is
            None => -EBADF,
        },
        F_SETFD => {
            if cur.fds.set_cloexec(fd as usize, arg & 1 != 0) {
                0
            } else {
                -EBADF
            }
        }
        F_GETFL => match cur.fds.get(fd as usize) {
            Some(d) => d.flags() as i64,
            None => -EBADF,
        },
        F_SETFL => {
            // Only O_NONBLOCK / O_APPEND may change after the open (the
            // access mode never does); the description is shared, so every
            // dup sees the new flags -- exactly POSIX.
            let desc = match cur.fds.get(fd as usize) {
                Some(d) => d,
                None => return -EBADF,
            };
            let mut f = desc.flags();
            f &= !(O_NONBLOCK | O_APPEND);
            f |= (arg as u32) & (O_NONBLOCK | O_APPEND);
            desc.flags.set(f);
            0
        }
        _ => -EINVAL,
    }
}

// ------------------------------------------------- getdents (phase 3.1)

/// One `getdents` record: the x86-64 Linux `struct linux_dirent64` -- the
/// layout opendir/readdir wrappers (newlib, phase 3.2) walk.
///
///   u64 d_ino; s64 d_off; u16 d_reclen; u8 d_type; char d_name[];
///
/// `d_off` is the entry index + 1 (the VFS slot's offset doubles as the
/// cursor), `d_type` the DT_* code.
pub fn sys_getdents(fd: u64, buf: u64, len: u64) -> i64 {
    let len = len.min(MAX_RW as u64) as usize;
    if len < 20 {
        // Not even the shortest record (19 header bytes + 1 name char)
        // fits; -EINVAL is what Linux answers a too-small buffer.
        return -EINVAL;
    }
    let desc = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(d) = cur.fds.get(fd as usize) else { return -EBADF };
        d
    };
    let Kind::Vfs(slot) = &desc.kind else {
        return -ENOTDIR; // a pipe / device has no entries
    };
    let vfs = crate::fs::fsys::vfs();
    // Where the cursor starts, so an EFAULT can put it back.
    let start = match vfs.seek(*slot, Whence::Cur(0)) {
        Ok(o) => o,
        Err(e) => return -fs_errno(e),
    };
    let mut out: Vec<u8> = Vec::new();
    let mut accepted = 0usize; // records that made it into `out`
    let mut refused = false;   // an entry was skipped: the buffer is full
    let mut fill = |e: &DirEntry| -> bool {
        let dtype: u8 = match e.ftype {
            FileType::Regular => 8,  // DT_REG
            FileType::Dir => 4,      // DT_DIR
            FileType::Symlink => 10, // DT_LNK
            FileType::Dev(_) => 2,   // DT_CHR
            _ => 0,                  // DT_UNKNOWN
        };
        let namelen = e.name.len() + 1;
        let reclen = 19 + namelen;
        // Stop before overflowing the user's buffer: a record goes back
        // whole or not at all, and the cursor stays put for the next call.
        if out.len() + reclen > len {
            refused = true;
            return false;
        }
        out.extend_from_slice(&e.ino.to_le_bytes());
        out.extend_from_slice(&0i64.to_le_bytes()); // d_off: patched below
        out.extend_from_slice(&(reclen as u16).to_le_bytes());
        out.push(dtype);
        out.extend_from_slice(e.name.as_bytes());
        out.push(0);
        accepted += 1;
        true
    };
    let consumed = match vfs.getdents_fd(*slot, &mut fill) {
        Ok(c) => c,
        Err(e) => return -fs_errno(e),
    };
    if out.is_empty() {
        if refused {
            return -EINVAL; // the very first record did not fit
        }
        debug_assert_eq!(consumed, start);
        return 0; // the directory is exhausted: the EOF itself
    }
    // The accepted records cover entries [consumed - accepted, consumed):
    // give each its d_off (entry index + 1) so a caller can seek back.
    let base = (consumed as usize).saturating_sub(accepted);
    let mut off = 0usize;
    let mut k = 0usize;
    while off < out.len() {
        let reclen = u16::from_le_bytes([out[off + 16], out[off + 17]]) as usize;
        let d_off = (base + k + 1) as i64;
        out[off + 8..off + 16].copy_from_slice(&d_off.to_le_bytes());
        off += reclen;
        k += 1;
    }
    if !copy_out(buf, 0, &out) {
        // Put the cursor back so the caller can retry the same batch.
        let _ = vfs.seek(*slot, Whence::Set(start));
        return -EFAULT;
    }
    out.len() as i64
}

// ------------------------------------------------- ioctl (phase 3.1)

/// `ioctl(fd, req, arg)` (syscall 29): just enough of the tty side for a
/// libc's `isatty` / `tcgetattr` -- TCGETS succeeds (with a zeroed
/// termios, nothing to configure yet) and TIOCGWINSZ reports the 80x25
/// text grid on the console.  Everything else, and every non-console
/// descriptor, is -ENOTTY.
pub fn sys_ioctl(fd: u64, req: u64, arg: u64) -> i64 {
    const TCGETS: u64 = 0x5401;
    const TIOCGWINSZ: u64 = 0x5413;
    let desc = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(d) = cur.fds.get(fd as usize) else { return -EBADF };
        d
    };
    if !matches!(desc.kind, Kind::Console) {
        return -ENOTTY; // files, pipes and sinks are not terminals
    }
    match req {
        TCGETS => {
            // struct termios, all zero: c_iflag..c_cc + ispeed/ospeed.
            let zeros = [0u8; 60];
            if arg == 0 || copy_out(arg, 0, &zeros) {
                0
            } else {
                -EFAULT
            }
        }
        TIOCGWINSZ => {
            // struct winsize { u16 row, col, xpixel, ypixel; } -- the text
            // grid is 80x25, the framebuffer behind it 640x400.
            let ws: [u8; 8] = [25, 0, 80, 0, 0x80, 0x02, 0x90, 0x01];
            if arg == 0 || copy_out(arg, 0, &ws) {
                0
            } else {
                -EFAULT
            }
        }
        _ => -ENOTTY,
    }
}

// ------------------------------------------------- name space (phase 3.1)

/// `unlink(path)` (syscall 30): remove a name.  A directory is refused
/// (-EISDIR, that is rmdir's job); devfs and other read-only mounts answer
/// -EROFS through the VFS.
pub fn sys_unlink(path_p: u64) -> i64 {
    let abs = match user_path(path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::fs::fsys::vfs().unlink(&abs).map_or_else(|e| -fs_errno(e), |_| 0)
}

/// `mkdir(path, mode)` (syscall 31): the low 12 bits of `mode` reach the
/// file system (the permission checks come with phase 3.5).
pub fn sys_mkdir(path_p: u64, mode: u64) -> i64 {
    let abs = match user_path(path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::fs::fsys::vfs().mkdir(&abs, (mode & 0o7777) as u16).map_or_else(|e| -fs_errno(e), |_| 0)
}

/// `rmdir(path)` (syscall 32): remove an empty directory (-ENOTEMPTY
/// otherwise, -ENOTDIR when it is not a directory at all).
pub fn sys_rmdir(path_p: u64) -> i64 {
    let abs = match user_path(path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::fs::fsys::vfs().rmdir(&abs).map_or_else(|e| -fs_errno(e), |_| 0)
}

/// `rename(old, new)` (syscall 33): move / rename, -EXDEV across mounts.
pub fn sys_rename(old_p: u64, new_p: u64) -> i64 {
    let old = match user_path(old_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let new = match user_path(new_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::fs::fsys::vfs().rename(&old, &new).map_or_else(|e| -fs_errno(e), |_| 0)
}

/// `link(old, new)` (syscall 34): a second name for the same inode
/// (-EISDIR on directories, -EXDEV across mounts, -EMLINK at the limit).
pub fn sys_link(old_p: u64, new_p: u64) -> i64 {
    let old = match user_path(old_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let new = match user_path(new_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::fs::fsys::vfs().link(&old, &new).map_or_else(|e| -fs_errno(e), |_| 0)
}

// ------------------------------------------------------- poll / select (3.2)

/// poll(2) event bits (the Linux values).
const POLLIN: i32 = 0x001;
const POLLOUT: i32 = 0x004;
const POLLERR: i32 = 0x008;
const POLLHUP: i32 = 0x010;
const POLLNVAL: i32 = 0x020;
/// The readable / writable questions an entry may ask: POLLIN |
/// POLLRDNORM | POLLRDBAND (0x001 | 0x040 | 0x080) and POLLOUT |
/// POLLWRNORM | POLLWRBAND (0x004 | 0x100 | 0x200) -- everything a caller
/// can mean maps onto the two directions.
const READ_ASKED: i32 = POLLIN | 0x040 | 0x080;
const WRITE_ASKED: i32 = POLLOUT | 0x100 | 0x200;

/// Most descriptors one poll / select call may watch (an fd_set's worth;
/// our tables are 64 deep, the libc's sets 1024).
const MAX_POLLFDS: usize = 1024;
/// fd_set: 1024 bits, 128 bytes, 16 words.
const FDSET_WORDS: usize = 16;

/// What poll / select report about one description: whether a `read` /
/// `write` would not park, and the exceptional bits (a readerless pipe).
/// Files, /dev/null and /dev/zero are always ready in the directions they
/// were opened for (a read returns data or EOF -- EOF is readiness, a read
/// answers 0 -- and file writes never park); the console is readable
/// exactly when a decoded byte is pending (the same merge the line reader
/// polls); a pipe read end is readable with buffered bytes or at EOF (no
/// writer left -- POLLHUP), a pipe write end writable with buffer space,
/// or POLLERR when nobody reads the other end.
struct Ready {
    read: bool,
    write: bool,
    err: bool,
    hup: bool,
}

/// The readiness probe behind both multiplex syscalls.
fn desc_ready(desc: &Rc<FileDesc>) -> Ready {
    let f = desc.flags();
    match &desc.kind {
        Kind::Vfs(_) => Ready { read: f & O_READ != 0, write: f & O_WRITE != 0, err: false, hup: false },
        Kind::Console => Ready { read: input_pending(), write: f & O_WRITE != 0, err: false, hup: false },
        Kind::DevNull | Kind::DevZero => {
            Ready { read: f & O_READ != 0, write: f & O_WRITE != 0, err: false, hup: false }
        }
        Kind::PipeRead(p) => {
            let p = p.borrow();
            let eof = p.writers == 0;
            Ready { read: !p.buf.is_empty() || eof, write: false, err: false, hup: eof }
        }
        Kind::PipeWrite(p) => {
            let p = p.borrow();
            let broken = p.readers == 0;
            Ready { read: false, write: PIPE_CAPACITY - p.buf.len() > 0 || broken, err: broken, hup: false }
        }
    }
}

/// Whether the keyboard or the serial line holds an input byte (the
/// non-destructive twin of `get_input_byte`).
fn input_pending() -> bool {
    crate::hw::kbd::has_char() || crate::hw::serial::has_byte()
}

/// The common deadline: `timeout_ms` (poll's int; negative = forever) or a
/// `struct timeval` at `tv_p` (select; NULL = forever), house-capped like
/// every bounded blocker here (WAIT_MAX_MS): the shell is blocked while
/// its children run, so nothing may wait forever.
fn wait_deadline(timeout_ms: u64, start: u64) -> u64 {
    if timeout_ms as i64 >= 0 {
        start + timeout_ms.min(WAIT_MAX_MS)
    } else {
        start + WAIT_MAX_MS // negative = "forever", by the house rule
    }
}

/// `poll(fds, nfds, timeout_ms)` (syscall 41, phase 3.2): the Linux pollfd
/// array -- 8 bytes per entry (int fd, short events, short revents).
/// POLLIN / POLLOUT follow the readiness probe; POLLERR / POLLHUP /
/// POLLNVAL are always reported (they are output-only bits).  The call
/// parks (bounded, like every blocking wait here) until some descriptor is
/// ready or the timeout expires, then answers the number of entries with a
/// non-zero revents (0 on timeout), revents written back for every entry.
/// A negative fd is skipped (POSIX: ignored, revents 0); a closed number
/// polls POLLNVAL.  Unknown event bits are dropped on the floor.
pub fn sys_poll(fds_p: u64, nfds: u64, timeout_ms: u64) -> i64 {
    let n = nfds as usize;
    if n > MAX_POLLFDS {
        return -EINVAL;
    }
    // The array in kernel memory (8 KB worst case -- the heap, not the
    // 16 KiB kernel stack).
    let mut buf: Vec<u8> = alloc::vec![0u8; n * 8];
    if n > 0 {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return -EFAULT };
        if !aspace.read(fds_p, &mut buf) {
            return -EFAULT;
        }
    }
    let start = uptime_ms();
    let deadline = wait_deadline(timeout_ms, start);
    let mut revents: Vec<i32> = alloc::vec![0; n];
    loop {
        let mut ready = 0i64;
        for i in 0..n {
            let fd = i32::from_le_bytes(buf[i * 8..i * 8 + 4].try_into().unwrap());
            let events = i16::from_le_bytes(buf[i * 8 + 4..i * 8 + 6].try_into().unwrap()) as i32;
            let mut re = 0i32;
            if fd >= 0 {
                match desc_of(fd as usize) {
                    None => re = POLLNVAL, // a closed number is not a watchable one
                    Some(d) => {
                        let r = desc_ready(&d);
                        if r.err {
                            re |= POLLERR;
                        }
                        if r.hup {
                            re |= POLLHUP;
                        }
                        // The directions asked about, answered.
                        if r.read && events & READ_ASKED != 0 {
                            re |= POLLIN;
                        }
                        if r.write && events & WRITE_ASKED != 0 {
                            re |= POLLOUT;
                        }
                    }
                }
            }
            revents[i] = re;
            if re != 0 {
                ready += 1;
            }
        }
        if ready > 0 || uptime_ms() >= deadline {
            // Write the revents back into the array and hand it to the user
            // (fd / events go back as they came -- the ABI leaves them
            // alone, and so do we).
            for i in 0..n {
                let re = revents[i] as i16;
                buf[i * 8 + 6..i * 8 + 8].copy_from_slice(&re.to_le_bytes());
            }
            if n > 0 && !copy_out(fds_p, 0, &buf) {
                return -EFAULT;
            }
            return ready;
        }
        park_once();
    }
}

/// `select(nfds, readfds, writefds, exceptfds, timeout)` (syscall 42, phase
/// 3.2; five arguments -- r10 / r8 carry 4 and 5): the classic three
/// bitsets (fd_set: 1024 bits) plus a `struct timeval`.  Readiness follows
/// the same probe as poll; exceptfds is accepted and always comes back
/// empty (nothing exceptional ever happens here).  The call parks (bounded)
/// until a watched descriptor is ready or the timeout (NULL = forever,
/// capped; a zero timeval is a single probe) expires; it answers the number
/// of bits set in the returned sets (0 on timeout), -EBADF when a watched
/// number is not open (POSIX), -EINVAL for a bad nfds, -EFAULT for bad
/// pointers.  Bits above nfds are ignored, like everywhere.
pub fn sys_select(nfds: u64, r_p: u64, w_p: u64, e_p: u64, tv_p: u64) -> i64 {
    if nfds as usize > MAX_POLLFDS {
        return -EINVAL;
    }
    // The three sets in, 384 bytes of kernel stack.
    let mut r = [0u64; FDSET_WORDS];
    let mut w = [0u64; FDSET_WORDS];
    let mut e = [0u64; FDSET_WORDS];
    {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return -EFAULT };
        for (ptr, set) in [(r_p, &mut r), (w_p, &mut w), (e_p, &mut e)] {
            if ptr == 0 {
                continue;
            }
            let mut bytes = [0u8; FDSET_WORDS * 8];
            if !aspace.read(ptr, &mut bytes) {
                return -EFAULT;
            }
            for (i, word) in set.iter_mut().enumerate() {
                *word = u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
            }
        }
    }
    // Bits past nfds never existed (a libc zeroes them; so do we).
    if nfds < 1024 {
        let last = (nfds / 64) as usize;
        let mask = (1u64 << (nfds % 64)).wrapping_sub(1);
        for set in [&mut r, &mut w, &mut e] {
            for (i, word) in set.iter_mut().enumerate() {
                if i > last {
                    *word = 0;
                } else if i == last {
                    *word &= mask;
                }
            }
        }
    }
    // The deadline: a timeval (NULL = forever), house-capped like above.
    let start = uptime_ms();
    let deadline = if tv_p == 0 {
        start + WAIT_MAX_MS
    } else {
        let got: Result<u64, i64> = {
            let Some(cur) = process::current() else { return -EFAULT };
            let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return -EFAULT };
            let mut bytes = [0u8; 16];
            if !aspace.read(tv_p, &mut bytes) {
                return -EFAULT;
            }
            let sec = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
            let usec = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
            if !(0..1_000_000).contains(&usec) {
                return -EINVAL;
            }
            let ms = sec.saturating_mul(1000).saturating_add(usec / 1000);
            Ok(ms.max(0) as u64)
        };
        match got {
            Ok(ms) => start + ms.min(WAIT_MAX_MS),
            Err(e) => return e,
        }
    };
    loop {
        let mut outr = [0u64; FDSET_WORDS];
        let mut outw = [0u64; FDSET_WORDS];
        let mut count = 0i64;
        for fd in 0..nfds {
            let (word, bit) = ((fd / 64) as usize, 1u64 << (fd % 64));
            let want_r = r[word] & bit != 0;
            let want_w = w[word] & bit != 0;
            let want_e = e[word] & bit != 0;
            if !want_r && !want_w && !want_e {
                continue;
            }
            let Some(d) = desc_of(fd as usize) else {
                return -EBADF; // POSIX: a watched number that is not open
            };
            let ready = desc_ready(&d);
            if want_r && ready.read {
                outr[word] |= bit;
                count += 1;
            }
            if want_w && ready.write {
                outw[word] |= bit;
                count += 1;
            }
            // exceptfds: nothing exceptional ever happens here -- the set
            // comes back empty, which POSIX lets us choose.
        }
        if count > 0 || uptime_ms() >= deadline {
            // The modified sets go back (a NULL set pointer stays NULL --
            // nothing to answer into).
            let Some(cur) = process::current() else { return -EFAULT };
            let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return -EFAULT };
            for (ptr, set) in [(r_p, &outr), (w_p, &outw)] {
                if ptr == 0 {
                    continue;
                }
                let mut bytes = [0u8; FDSET_WORDS * 8];
                for (i, word) in set.iter().enumerate() {
                    bytes[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
                }
                if !aspace.ensure_writable(ptr) || !aspace.write(ptr, &bytes, false) {
                    return -EFAULT;
                }
            }
            return count;
        }
        park_once();
    }
}
