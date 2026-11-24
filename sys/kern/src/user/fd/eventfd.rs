// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// eventfd: the syscall half (the counter and its rules are
// user/eventfd_core.rs, host tested by tests/eventfd_host.rs).
//
//   124 eventfd2(initval, flags)     EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC
//
// One more kind of open file description (`Kind::EventFd`), so a child module
// of user/fd.rs like unix.rs and epoll.rs: read and write go through the
// shared `sys_read` / `sys_write` dispatch, readiness through `desc_ready`
// (so poll, select and epoll see it).
//
// Blocking follows the house rule of pipes: re-probe, park, bounded by
// WAIT_MAX_MS, interruptible (-EINTR), O_NONBLOCK answers -EAGAIN at once.
// A blocked reader or writer is woken by `unix::notify` (the shared "something
// became ready" kick) instead of waiting for the next quantum.

use alloc::rc::Rc;
use core::cell::RefCell;

use super::{copy_in, copy_out, park_once, uptime_ms, unix, FileDesc, Kind};
use super::{EAGAIN, EINTR, EMFILE, WAIT_MAX_MS};
use crate::fs::vfs::{O_NONBLOCK, O_READ, O_WRITE};
use crate::user::eventfd_core::*;
use crate::user::process;
use crate::user::signal;
use crate::user::usermode::{EFAULT, EINVAL};

/// EFD_NONBLOCK / EFD_CLOEXEC are O_NONBLOCK / O_CLOEXEC of whichever ABI the
/// libc was built for.  This port's headers are not consistent about it
/// (abi-bits/fcntl.h has O_NONBLOCK 0100 and O_CLOEXEC 0200, while
/// abi-bits/epoll.h keeps Linux's 02000000), and the mlibc sources that define
/// EFD_* are not in the tree, so both spellings are accepted.  They share no
/// bit with each other or with EFD_SEMAPHORE (1), so nothing is ambiguous.
const EFD_NONBLOCK_LINUX: u64 = 0o4000;
const EFD_CLOEXEC_LINUX: u64 = 0o2000000;
const EFD_NONBLOCK_PORT: u64 = 0o100;
const EFD_CLOEXEC_PORT: u64 = 0o200;
const EFD_NONBLOCK: u64 = EFD_NONBLOCK_LINUX | EFD_NONBLOCK_PORT;
const EFD_CLOEXEC: u64 = EFD_CLOEXEC_LINUX | EFD_CLOEXEC_PORT;

pub(crate) type Counter = Rc<RefCell<EventCounter>>;

/// What an eventfd answers to poll / select / epoll: (readable, writable).
pub(super) fn ready(c: &Counter) -> (bool, bool) {
    let c = c.borrow();
    (c.readable(), c.writable())
}

/// `eventfd2(initval, flags)` (124).
pub fn sys_eventfd2(initval: u64, flags: u64) -> i64 {
    if flags & !(EFD_NONBLOCK | EFD_CLOEXEC | EFD_SEMAPHORE as u64) != 0 {
        return -EINVAL;
    }
    // initval is an unsigned int: only the low 32 bits count.
    let init = initval as u32;
    let sem = flags & EFD_SEMAPHORE as u64 != 0;
    let mut f = O_READ | O_WRITE;
    if flags & EFD_NONBLOCK != 0 {
        f |= O_NONBLOCK;
    }
    let cloexec = flags & EFD_CLOEXEC != 0;
    let d = Rc::new(FileDesc::new(
        Kind::EventFd(Rc::new(RefCell::new(EventCounter::new(init, sem)))),
        f,
    ));
    let Some(cur) = process::current() else { return -EFAULT };
    match cur.fds.install(d, cloexec) {
        Some(fd) => fd as i64,
        None => -EMFILE,
    }
}

/// `read(2)` on an eventfd: exactly 8 bytes out (the counter), `-EINVAL` for
/// a shorter buffer.  Parks while the counter is 0, unless O_NONBLOCK.
pub(super) fn read(c: &Counter, desc: &Rc<FileDesc>, buf: u64, len: usize) -> i64 {
    if len < 8 {
        return -EINVAL;
    }
    let nonblock = desc.flags() & O_NONBLOCK != 0;
    let start = uptime_ms();
    loop {
        let got = c.borrow_mut().read();
        match got {
            Ok(v) => {
                if !copy_out(buf, 0, &v.to_le_bytes()) {
                    // Do not lose the events to a bad pointer: put back what
                    // the read took (the whole counter, or 1 in semaphore
                    // mode -- `v` is exactly that in both).
                    let _ = c.borrow_mut().write(v);
                    return -EFAULT;
                }
                unix::notify(); // a writer blocked on a full counter may go on
                return 8;
            }
            Err(_) => {
                if signal::interrupt_pending() {
                    return -EINTR;
                }
                if nonblock || uptime_ms() - start >= WAIT_MAX_MS {
                    return -EAGAIN;
                }
                park_once();
            }
        }
    }
}

/// `write(2)` on an eventfd: exactly 8 bytes in (the value to add),
/// `-EINVAL` for a shorter buffer or the value u64::MAX.  Parks while the sum
/// would overflow, unless O_NONBLOCK.
pub(super) fn write(c: &Counter, desc: &Rc<FileDesc>, buf: u64, len: usize) -> i64 {
    if len < 8 {
        return -EINVAL;
    }
    let mut raw = [0u8; 8];
    if !copy_in(buf, 0, &mut raw) {
        return -EFAULT;
    }
    let v = u64::from_le_bytes(raw);
    let nonblock = desc.flags() & O_NONBLOCK != 0;
    let start = uptime_ms();
    loop {
        let r = c.borrow_mut().write(v);
        match r {
            Ok(()) => {
                unix::notify(); // a parked reader / epoll_wait may go on
                return 8;
            }
            Err(e) if e == -EAGAIN => {
                if signal::interrupt_pending() {
                    return -EINTR;
                }
                if nonblock || uptime_ms() - start >= WAIT_MAX_MS {
                    return -EAGAIN;
                }
                park_once();
            }
            Err(e) => return e,
        }
    }
}
