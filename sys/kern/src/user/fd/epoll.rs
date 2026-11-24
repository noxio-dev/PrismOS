// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// epoll: the syscall half (the interest list is user/epoll_core.rs, host
// tested by tests/epoll_host.rs).
//
//   109 epoll_create1(flags)                 EPOLL_CLOEXEC only
//   110 epoll_ctl(epfd, op, fd, event *)     ADD / DEL / MOD
//   111 epoll_wait(epfd, events *, max, timeout_ms)
//
// An epoll instance is one more kind of open file description
// (`Kind::Epoll`), so a child module of user/fd.rs, like unix.rs: it reuses
// the readiness probe `desc_ready` that poll and select answer from.  The
// interest list holds `Weak<FileDesc>`: an epoll never keeps a watched file
// open, and an entry whose description is gone vanishes by itself.
//
// Waiting follows the house rule of poll: re-probe, park (a socket event
// wakes the parked process at once), bounded by WAIT_MAX_MS and
// interruptible (-EINTR).  A negative timeout means "until an event" and
// re-arms the cap.
//
// Not here (answered honestly): epoll_pwait with a signal mask, EPOLLEXCLUSIVE
// and EPOLLWAKEUP (accepted, no effect), regular files and directories
// (-EPERM, like Linux).  Edge-triggered readiness is sampled when somebody
// waits: a rise and fall between two waits is not seen.

use alloc::rc::{Rc, Weak};
use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use super::{copy_in, copy_out, desc_of_or_errno, desc_ready, park_once, uptime_ms, FileDesc, Kind};
use super::{EINTR, EMFILE, EPERM, WAIT_MAX_MS};
use crate::fs::vfs::{O_READ, O_WRITE};
use crate::user::epoll_core::*;
use crate::user::process;
use crate::user::signal;
use crate::user::usermode::{EFAULT, EINVAL};

pub(crate) type Set = EpollSet<Weak<FileDesc>>;

const EPOLL_CLOEXEC: u64 = 0o2000000;
const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_DEL: u64 = 2;
const EPOLL_CTL_MOD: u64 = 3;
/// struct epoll_event on x86-64 is packed: u32 events + u64 data.
const EVENT_SIZE: usize = 12;
/// Linux's EP_MAX_EVENTS ceiling for one wait.
const MAX_EVENTS: u64 = 1024;
const ELOOP: i64 = 40;
const EBUSY: i64 = 16;
/// How deep epoll-in-epoll readiness / loop checks may nest.
const MAX_NEST: u32 = 4;

static DEPTH: AtomicU32 = AtomicU32::new(0);

pub(super) fn new_set() -> Rc<RefCell<Set>> {
    Rc::new(RefCell::new(Set::new()))
}

/// Readiness of one target as epoll bits; `None` once it is gone.
fn probe(w: &Weak<FileDesc>) -> Option<u32> {
    let d = w.upgrade()?;
    let r = desc_ready(&d);
    let mut b = 0;
    if r.read {
        b |= EPOLLIN;
    }
    if r.write {
        b |= EPOLLOUT;
    }
    if r.err {
        b |= EPOLLERR;
    }
    if r.hup {
        b |= EPOLLHUP | EPOLLRDHUP;
    }
    Some(b)
}

/// What an epoll descriptor itself answers to poll / select / epoll: readable
/// when some watched entry is.  Depth-bounded and never panics on a cycle
/// (a set being scanned is mutably borrowed, so it reads as "not ready").
pub(super) fn nested_ready(set: &Rc<RefCell<Set>>) -> bool {
    if DEPTH.fetch_add(1, Ordering::Relaxed) >= MAX_NEST {
        DEPTH.fetch_sub(1, Ordering::Relaxed);
        return false;
    }
    let ready = match set.try_borrow() {
        Ok(s) => s.any_ready(probe),
        Err(_) => false,
    };
    DEPTH.fetch_sub(1, Ordering::Relaxed);
    ready
}

/// Whether `target`'s epoll (transitively) already watches `ep`.
fn watches_transitively(target: &Rc<FileDesc>, ep: &Rc<FileDesc>, depth: u32) -> bool {
    if depth > MAX_NEST {
        return true; // too deep: refuse, like Linux's nesting limit
    }
    let Kind::Epoll(set) = &target.kind else { return false };
    let Ok(s) = set.try_borrow() else { return true };
    let kids: alloc::vec::Vec<Rc<FileDesc>> = s.targets().filter_map(|w| w.upgrade()).collect();
    drop(s);
    kids.iter().any(|k| Rc::ptr_eq(k, ep) || watches_transitively(k, ep, depth + 1))
}

/// `epoll_create1(flags)` (109).
pub fn sys_epoll_create1(flags: u64) -> i64 {
    if flags & !EPOLL_CLOEXEC != 0 {
        return -EINVAL;
    }
    let d = Rc::new(FileDesc::new(Kind::Epoll(new_set()), O_READ | O_WRITE));
    let Some(cur) = process::current() else { return -EFAULT };
    match cur.fds.install(d, flags & EPOLL_CLOEXEC != 0) {
        Some(fd) => fd as i64,
        None => -EMFILE,
    }
}

/// `epoll_ctl(epfd, op, fd, event)` (110).
pub fn sys_epoll_ctl(epfd: u64, op: u64, fd: u64, ev_p: u64) -> i64 {
    let ep = match desc_of_or_errno(epfd) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let Kind::Epoll(set) = &ep.kind else { return -EINVAL };
    let tgt = match desc_of_or_errno(fd) {
        Ok(d) => d,
        Err(e) => return e,
    };
    if Rc::ptr_eq(&ep, &tgt) {
        return -EINVAL;
    }
    // Plain files and directories are always "ready": Linux refuses them.
    if matches!(tgt.kind, Kind::Vfs(_)) {
        return -EPERM;
    }
    if !matches!(op, EPOLL_CTL_ADD | EPOLL_CTL_DEL | EPOLL_CTL_MOD) {
        return -EINVAL;
    }
    let (mut events, mut data) = (0u32, 0u64);
    if op != EPOLL_CTL_DEL {
        let mut raw = [0u8; EVENT_SIZE];
        if !copy_in(ev_p, 0, &mut raw) {
            return -EFAULT;
        }
        events = u32::from_le_bytes(raw[0..4].try_into().unwrap());
        data = u64::from_le_bytes(raw[4..12].try_into().unwrap());
    }
    if op == EPOLL_CTL_ADD && watches_transitively(&tgt, &ep, 0) {
        return -ELOOP;
    }
    let me = Rc::downgrade(&tgt);
    let same = |a: &Weak<FileDesc>, b: &Weak<FileDesc>| a.ptr_eq(b);
    let Ok(mut s) = set.try_borrow_mut() else { return -EBUSY };
    let r = match op {
        EPOLL_CTL_ADD => s.add(fd as i32, me, events, data, same),
        EPOLL_CTL_MOD => s.modify(fd as i32, &me, events, data, same),
        _ => s.remove(fd as i32, &me, same),
    };
    match r {
        Ok(()) => 0,
        Err(e) => -e,
    }
}

/// `epoll_pwait(epfd, events, maxevents, timeout_ms, sigmask, sigsetsize)`
/// (125): `epoll_wait` with the blocked mask swapped for `sigmask` for the
/// duration of the call (NULL = plain `epoll_wait`).  `sigsetsize` must be 8,
/// the kernel's mask width.  When a signal interrupts the wait the temporary
/// mask stays until the handler has run, and `sigreturn` then restores the
/// caller's own (see `signal::restore_mask_after_delivery`); in every other
/// outcome the caller's mask is back before the call returns.
pub fn sys_epoll_pwait(epfd: u64, ev_p: u64, max: u64, timeout: u64, mask_p: u64, size: u64) -> i64 {
    if mask_p == 0 {
        return sys_epoll_wait(epfd, ev_p, max, timeout);
    }
    if size != 8 {
        return -EINVAL;
    }
    let mut raw = [0u8; 8];
    if !copy_in(mask_p, 0, &mut raw) {
        return -EFAULT;
    }
    let Some(old) = signal::swap_mask(u64::from_le_bytes(raw)) else { return -EFAULT };
    let r = sys_epoll_wait(epfd, ev_p, max, timeout);
    if r == -EINTR {
        signal::restore_mask_after_delivery(old);
    } else {
        signal::restore_mask(old);
    }
    r
}

/// `epoll_wait(epfd, events, maxevents, timeout_ms)` (111).
pub fn sys_epoll_wait(epfd: u64, ev_p: u64, max: u64, timeout: u64) -> i64 {
    let ep = match desc_of_or_errno(epfd) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let Kind::Epoll(set) = &ep.kind else { return -EINVAL };
    if max == 0 || max > MAX_EVENTS || (max as i64) < 0 {
        return -EINVAL;
    }
    let max = max as usize;
    // Fail on a bad buffer before a scan consumes any edge / oneshot state.
    {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(aspace) = cur.aspace.as_ref() else { return -EFAULT };
        if !aspace.ensure_writable(ev_p) || !aspace.ensure_writable(ev_p + (max * EVENT_SIZE) as u64 - 1) {
            return -EFAULT;
        }
    }
    // int timeout: sign-extended or not, the low 32 bits decide.
    let t = timeout as i32 as i64;
    let start = uptime_ms();
    let mut deadline = start + if t < 0 { WAIT_MAX_MS } else { (t as u64).min(WAIT_MAX_MS) };
    loop {
        let got = match set.try_borrow_mut() {
            Ok(mut s) => s.collect(max, probe),
            Err(_) => return -EBUSY,
        };
        if !got.is_empty() {
            let mut out = alloc::vec::Vec::with_capacity(got.len() * EVENT_SIZE);
            for (ev, d) in &got {
                out.extend_from_slice(&ev.to_le_bytes());
                out.extend_from_slice(&d.to_le_bytes());
            }
            return if copy_out(ev_p, 0, &out) { got.len() as i64 } else { -EFAULT };
        }
        if t >= 0 && uptime_ms() >= deadline {
            return 0;
        }
        if signal::interrupt_pending() {
            return -EINTR;
        }
        if uptime_ms() >= deadline {
            // Forever, by POSIX: the house cap only bounds one park at a time.
            deadline = uptime_ms() + WAIT_MAX_MS;
        }
        park_once();
    }
}
