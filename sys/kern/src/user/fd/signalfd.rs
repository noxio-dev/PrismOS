// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// signalfd: the syscall half (the mask rules, the choice of signal and the
// record layout are user/signalfd_core.rs, host tested by
// tests/signalfd_host.rs).
//
//   129 signalfd4(fd, mask, sizemask, flags)     SFD_NONBLOCK | SFD_CLOEXEC
//
// `fd` -1 makes a new descriptor, any other `fd` must be a signalfd and gets
// a new mask.  `flags` travels in r10 (4th argument).  One more kind of open
// file description (`Kind::SignalFd`), so a child module of user/fd.rs like
// eventfd.rs and timerfd.rs: read goes through the shared `sys_read`
// dispatch, readiness through `desc_ready` (so poll, select and epoll see
// it).  A signalfd is not writable (`write` is -EINVAL).
//
// The descriptor holds only its mask.  What a read takes is the *calling
// process's* pending set (`signals.pending`), which signal.rs fills for
// blocked signals too (`SignalState::queues`) -- that is what makes the
// usual "block SIGTERM / SIGCHLD, read them from an fd" pattern work.  A
// signal taken here is consumed: it is not delivered afterwards.
//
// A blocked `read` naps PARK_MS at a time; raising a signal wakes a sleeping
// process (process.rs), so it re-checks at once.  Bounded by WAIT_MAX_MS and
// interruptible (-EINTR) by a signal that is *not* in the mask and is
// deliverable, and O_NONBLOCK answers -EAGAIN at once, like every blocker.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use super::{copy_in, copy_out, desc_of_or_errno, uptime_ms, unix, FileDesc, Kind};
use super::{EAGAIN, EINTR, EMFILE, PARK_MS, WAIT_MAX_MS};
use crate::fs::vfs::{O_NONBLOCK, O_READ, O_WRITE};
use crate::user::process;
use crate::user::signal;
use crate::user::signalfd_core::*;
use crate::user::usermode::{EFAULT, EINVAL};

/// SFD_NONBLOCK / SFD_CLOEXEC are O_NONBLOCK / O_CLOEXEC of whichever ABI the
/// libc was built for; both spellings are accepted, exactly like eventfd's and
/// timerfd's (see user/fd/eventfd.rs for why).
const SFD_NONBLOCK_LINUX: u64 = 0o4000;
const SFD_CLOEXEC_LINUX: u64 = 0o2000000;
const SFD_NONBLOCK_PORT: u64 = 0o100;
const SFD_CLOEXEC_PORT: u64 = 0o200;
const SFD_NONBLOCK: u64 = SFD_NONBLOCK_LINUX | SFD_NONBLOCK_PORT;
const SFD_CLOEXEC: u64 = SFD_CLOEXEC_LINUX | SFD_CLOEXEC_PORT;

pub(crate) type Mask = Rc<RefCell<u32>>;

/// `read` would not block: what a signalfd answers to poll / select / epoll
/// (it is never writable).  Looks at the calling process's pending set.
pub(super) fn ready(m: &Mask) -> bool {
    let mask = *m.borrow();
    match process::current() {
        Some(cur) => cur.signals.pending & mask != 0,
        None => false,
    }
}

/// Park for up to `ms` milliseconds, registered with `unix::notify` machinery
/// like timerfd's nap.  No `RefCell` guard may be alive here.
fn nap(ms: u64) {
    let pid = process::getpid();
    unix::parked_enter(pid);
    process::sleep_current_ns(ms * 1_000_000);
    unix::parked_leave(pid);
}

/// `signalfd4(fd, mask, sizemask, flags)` (129).
pub fn sys_signalfd4(fd: u64, mask_p: u64, sizemask: u64, flags: u64) -> i64 {
    if flags & !(SFD_NONBLOCK | SFD_CLOEXEC) != 0 {
        return -EINVAL;
    }
    if let Err(e) = check_sigsetsize(sizemask) {
        return e;
    }
    let mut raw = [0u8; 8];
    if !copy_in(mask_p, 0, &mut raw) {
        return -EFAULT;
    }
    let mask = normalize_mask(u64::from_le_bytes(raw));

    // fd -1 (as an int, so the upper half may or may not be set): a new one.
    if fd as u32 != u32::MAX {
        // An existing signalfd: only its mask changes.
        let desc = match desc_of_or_errno(fd) {
            Ok(d) => d,
            Err(e) => return e,
        };
        return match &desc.kind {
            Kind::SignalFd(m) => {
                *m.borrow_mut() = mask;
                fd as i64
            }
            _ => -EINVAL,
        };
    }

    let mut f = O_READ | O_WRITE;
    if flags & SFD_NONBLOCK != 0 {
        f |= O_NONBLOCK;
    }
    let cloexec = flags & SFD_CLOEXEC != 0;
    let d = Rc::new(FileDesc::new(Kind::SignalFd(Rc::new(RefCell::new(mask))), f));
    let Some(cur) = process::current() else { return -EFAULT };
    match cur.fds.install(d, cloexec) {
        Some(n) => n as i64,
        None => -EMFILE,
    }
}

/// Take up to `max` pending signals of `mask` out of the calling process:
/// each one's record, and the (signal, saved info) pairs to put back if the
/// copy to user space fails.
fn dequeue(mask: u32, max: usize) -> (Vec<u8>, Vec<(u32, signal::SigInfoRec)>) {
    let mut out = Vec::new();
    let mut taken = Vec::new();
    let Some(cur) = process::current() else { return (out, taken) };
    while taken.len() < max {
        let Some(sig) = next_signal(cur.signals.pending, mask) else { break };
        let info = cur.signals.info[sig as usize];
        cur.signals.pending &= !(1u32 << sig);
        cur.signals.info[sig as usize] = signal::SigInfoRec::KERNEL; // as `deliver` does
        out.extend_from_slice(&record(sig, info.code, info.pid, info.uid));
        taken.push((sig, info));
    }
    (out, taken)
}

/// `read(2)` on a signalfd: whole 128-byte `signalfd_siginfo` records, as many
/// as are pending and fit (at least one), `-EINVAL` for a buffer under 128
/// bytes.  Parks until a signal of the mask is pending, unless O_NONBLOCK.
pub(super) fn read(m: &Mask, desc: &Rc<FileDesc>, buf: u64, len: usize) -> i64 {
    let fit = records_that_fit(len);
    if fit == 0 {
        return -EINVAL;
    }
    let nonblock = desc.flags() & O_NONBLOCK != 0;
    let start = uptime_ms();
    loop {
        let mask = *m.borrow();
        let (bytes, taken) = dequeue(mask, fit);
        if !taken.is_empty() {
            if !copy_out(buf, 0, &bytes) {
                // Do not lose the signals to a bad pointer.
                if let Some(cur) = process::current() {
                    for (sig, info) in taken {
                        cur.signals.pending |= 1u32 << sig;
                        cur.signals.info[sig as usize] = info;
                    }
                }
                return -EFAULT;
            }
            return bytes.len() as i64;
        }
        if nonblock {
            return -EAGAIN;
        }
        if signal::interrupt_pending() {
            return -EINTR;
        }
        let waited = uptime_ms().saturating_sub(start);
        if waited >= WAIT_MAX_MS {
            return -EAGAIN;
        }
        nap(PARK_MS.min(WAIT_MAX_MS - waited).max(1));
    }
}
