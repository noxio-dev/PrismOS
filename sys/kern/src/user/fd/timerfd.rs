// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// timerfd: the syscall half (the state machine and its rules are
// user/timerfd_core.rs, host tested by tests/timerfd_host.rs).
//
//   126 timerfd_create(clockid, flags)            TFD_NONBLOCK | TFD_CLOEXEC
//   127 timerfd_settime(fd, flags, new, old)      TFD_TIMER_ABSTIME | _CANCEL_ON_SET
//   128 timerfd_gettime(fd, cur)
//
// One more kind of open file description (`Kind::TimerFd`), so a child module
// of user/fd.rs like unix.rs, epoll.rs and eventfd.rs: read goes through the
// shared `sys_read` dispatch, readiness through `desc_ready` (so poll, select
// and epoll see it).  A timerfd is not writable (`write` is -EINVAL).
//
// There is no kernel timer behind the descriptor: it is a deadline on a
// clock, and whoever looks -- a read, a poll / epoll probe, gettime, settime
// -- counts what has expired.  Closing it has nothing to cancel.
//
// A blocked `read` sleeps to the exact deadline (the one-shot timer of
// process::sleep_current_ns), so it wakes within microseconds of the expiry.
// A poll / select / epoll waiter notices an expiry at its next probe, which is
// the usual 10 ms park (PARK_MS), because those waits are not told about a
// deadline.  Both are bounded by WAIT_MAX_MS and interruptible (-EINTR), and
// O_NONBLOCK answers -EAGAIN at once, like every blocker here.  `settime` kicks
// the shared "something became ready" notice (`unix::notify`) so a parked
// reader re-checks against the new setting.

use alloc::rc::Rc;
use core::cell::RefCell;

use super::{copy_in, copy_out, desc_of_or_errno, uptime_ms, unix, FileDesc, Kind};
use super::{EAGAIN, EINTR, EMFILE, PARK_MS, WAIT_MAX_MS};
use crate::fs::vfs::{O_NONBLOCK, O_READ, O_WRITE};
use crate::user::process;
use crate::user::signal;
use crate::user::timerfd_core::*;
use crate::user::usermode::{EFAULT, EINVAL};

/// TFD_NONBLOCK / TFD_CLOEXEC are O_NONBLOCK / O_CLOEXEC of whichever ABI the
/// libc was built for; both spellings are accepted, exactly like eventfd's
/// (see user/fd/eventfd.rs for why).  They share no bit, so nothing is
/// ambiguous.
const TFD_NONBLOCK_LINUX: u64 = 0o4000;
const TFD_CLOEXEC_LINUX: u64 = 0o2000000;
const TFD_NONBLOCK_PORT: u64 = 0o100;
const TFD_CLOEXEC_PORT: u64 = 0o200;
const TFD_NONBLOCK: u64 = TFD_NONBLOCK_LINUX | TFD_NONBLOCK_PORT;
const TFD_CLOEXEC: u64 = TFD_CLOEXEC_LINUX | TFD_CLOEXEC_PORT;

/// `struct itimerspec`: it_interval then it_value, two `struct timespec`
/// (i64 seconds, i64 nanoseconds) each.
const ITIMERSPEC_SIZE: usize = 32;

pub(crate) type Timer = Rc<RefCell<TimerCore>>;

/// The clock a timer runs on, read now, in nanoseconds.
fn now_on(clock: Clock) -> u64 {
    match clock {
        Clock::Monotonic => crate::hw::clock::now_ns(),
        Clock::Realtime => crate::user::sysc::realtime_ns(),
    }
}

/// `read` would not block: what a timerfd answers to poll / select / epoll
/// (it is never writable).  Counts what has expired as a side effect, which
/// is how a waiter that only probes finds out.
pub(super) fn ready(t: &Timer) -> bool {
    let mut t = t.borrow_mut();
    let now = now_on(t.clock());
    t.readable(now)
}

/// Park for up to `ns` nanoseconds, registered with `unix::notify` so a
/// `settime` can cut the nap short.  No `RefCell` guard may be alive here.
fn nap(ns: u64) {
    let pid = process::getpid();
    unix::parked_enter(pid);
    process::sleep_current_ns(ns);
    unix::parked_leave(pid);
}

/// `timerfd_create(clockid, flags)` (126).
pub fn sys_timerfd_create(clockid: u64, flags: u64) -> i64 {
    if flags & !(TFD_NONBLOCK | TFD_CLOEXEC) != 0 {
        return -EINVAL;
    }
    let clock = match Clock::from_id(clockid) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let mut f = O_READ | O_WRITE;
    if flags & TFD_NONBLOCK != 0 {
        f |= O_NONBLOCK;
    }
    let cloexec = flags & TFD_CLOEXEC != 0;
    let d = Rc::new(FileDesc::new(
        Kind::TimerFd(Rc::new(RefCell::new(TimerCore::new(clock)))),
        f,
    ));
    let Some(cur) = process::current() else { return -EFAULT };
    match cur.fds.install(d, cloexec) {
        Some(fd) => fd as i64,
        None => -EMFILE,
    }
}

/// The timer behind `fd`: `-EBADF` for a number that is not open, `-EINVAL`
/// for a descriptor that is not a timerfd.
fn timer_of(fd: u64) -> Result<Timer, i64> {
    let desc = desc_of_or_errno(fd)?;
    match &desc.kind {
        Kind::TimerFd(t) => Ok(t.clone()),
        _ => Err(-EINVAL),
    }
}

/// Write one (value, interval) pair as `struct itimerspec` at `p`.
fn put_itimerspec(p: u64, value: u64, interval: u64) -> bool {
    let (isec, insec) = ns_to_ts(interval);
    let (vsec, vnsec) = ns_to_ts(value);
    let mut b = [0u8; ITIMERSPEC_SIZE];
    b[0..8].copy_from_slice(&isec.to_le_bytes());
    b[8..16].copy_from_slice(&insec.to_le_bytes());
    b[16..24].copy_from_slice(&vsec.to_le_bytes());
    b[24..32].copy_from_slice(&vnsec.to_le_bytes());
    copy_out(p, 0, &b)
}

/// `timerfd_settime(fd, flags, new_value, old_value)` (127).  `old_value` may
/// be NULL.  The new setting is checked as a whole (`-EINVAL` for a bad
/// timespec) before the timer is touched; a bad `old_value` pointer is
/// reported after the timer was re-armed, like Linux.
pub fn sys_timerfd_settime(fd: u64, flags: u64, new_p: u64, old_p: u64) -> i64 {
    let absolute = match settime_flags(flags) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let t = match timer_of(fd) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let mut raw = [0u8; ITIMERSPEC_SIZE];
    if !copy_in(new_p, 0, &mut raw) {
        return -EFAULT;
    }
    let word = |i: usize| {
        let mut w = [0u8; 8];
        w.copy_from_slice(&raw[i * 8..i * 8 + 8]);
        i64::from_le_bytes(w)
    };
    let interval = match ts_to_ns(word(0), word(1)) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let value = match ts_to_ns(word(2), word(3)) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let (old_value, old_interval) = {
        let mut s = t.borrow_mut();
        let now = now_on(s.clock());
        s.settime(now, absolute, value, interval)
    };
    // A reader parked against the old setting, or an epoll_wait watching an
    // absolute time that has already gone by, must look again now.
    unix::notify();
    if old_p != 0 && !put_itimerspec(old_p, old_value, old_interval) {
        return -EFAULT;
    }
    0
}

/// `timerfd_gettime(fd, curr_value)` (128): the time left until the next
/// expiry (0 when disarmed) and the interval.
pub fn sys_timerfd_gettime(fd: u64, cur_p: u64) -> i64 {
    let t = match timer_of(fd) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let (value, interval) = {
        let mut s = t.borrow_mut();
        let now = now_on(s.clock());
        s.gettime(now)
    };
    if !put_itimerspec(cur_p, value, interval) {
        return -EFAULT;
    }
    0
}

/// `read(2)` on a timerfd: exactly 8 bytes out (the number of expirations
/// since the last read, or since the timer was armed), `-EINVAL` for a
/// shorter buffer.  Parks until the next expiry while there is none, unless
/// O_NONBLOCK.
pub(super) fn read(t: &Timer, desc: &Rc<FileDesc>, buf: u64, len: usize) -> i64 {
    if len < 8 {
        return -EINVAL;
    }
    let nonblock = desc.flags() & O_NONBLOCK != 0;
    let start = uptime_ms();
    loop {
        // Statements, not a `match` on the borrow: no guard may live across
        // the nap below (another process may need the same timer).
        let (clock, got) = {
            let mut s = t.borrow_mut();
            let clock = s.clock();
            (clock, s.read(now_on(clock)))
        };
        match got {
            Ok(n) => {
                if !copy_out(buf, 0, &n.to_le_bytes()) {
                    // Do not lose the expirations to a bad pointer.
                    t.borrow_mut().give_back(n);
                    return -EFAULT;
                }
                return 8;
            }
            Err(_) => {
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
                let left_ns = (WAIT_MAX_MS - waited) * 1_000_000;
                let until = t.borrow().wait_ns(now_on(clock));
                // To the deadline when there is one; a short nap when the
                // timer is disarmed (only a settime, which notifies, or a
                // signal can change that).
                let ns = match until {
                    Some(ns) => ns.min(left_ns),
                    None => (PARK_MS * 1_000_000).min(left_ns),
                };
                nap(ns.max(1));
            }
        }
    }
}
