// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// epoll, the pure half.
//
// This file knows nothing about descriptors, processes or user memory: it is
// the interest list of one epoll instance, generic over `T`, the handle of a
// watched target (the kernel instantiates it with `Weak<FileDesc>`;
// tests/epoll_host.rs with a plain integer).  The glue -- syscalls, parking,
// user-memory copies -- lives in user/fd/epoll.rs.
//
// Model (the Linux contract, minus what a single non-preemptible CPU makes
// moot):
//
//   * an entry is keyed by (fd number, target), like Linux's (fd, file):
//     the same open description may be watched under two numbers, the same
//     number twice is EEXIST;
//   * level-triggered by default: every wait reports what is ready *now*;
//   * EPOLLET reports only the bits that turned on since the last scan of
//     the entry.  Readiness is sampled when somebody waits, so a rise and
//     fall between two waits is not seen (documented in docs/SYSCALLS.md);
//   * EPOLLONESHOT disarms the entry after it reported; EPOLL_CTL_MOD
//     re-arms it;
//   * a target that no longer exists (its probe answers `None`) is dropped
//     silently, like Linux does when the last reference to a file goes;
//   * a scan starts where the previous one stopped, so a small `maxevents`
//     cannot starve the entries at the end of the list.

use alloc::vec::Vec;

pub const EPOLLIN: u32 = 0x001;
pub const EPOLLPRI: u32 = 0x002;
pub const EPOLLOUT: u32 = 0x004;
pub const EPOLLERR: u32 = 0x008;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLRDNORM: u32 = 0x040;
pub const EPOLLRDBAND: u32 = 0x080;
pub const EPOLLWRNORM: u32 = 0x100;
pub const EPOLLWRBAND: u32 = 0x200;
pub const EPOLLRDHUP: u32 = 0x2000;
pub const EPOLLONESHOT: u32 = 1 << 30;
pub const EPOLLET: u32 = 1 << 31;

/// Entries one instance may watch (ENOSPC beyond).
pub const MAX_WATCHES: usize = 256;

const EEXIST: i64 = 17;
const ENOENT: i64 = 2;
const ENOSPC: i64 = 28;

/// The bits an entry can ever report: ERR and HUP always, IN / OUT when any
/// of their aliases was asked for, PRI and RDHUP as asked.
pub fn interest(events: u32) -> u32 {
    let mut m = EPOLLERR | EPOLLHUP;
    if events & (EPOLLIN | EPOLLRDNORM | EPOLLRDBAND) != 0 {
        m |= EPOLLIN;
    }
    if events & (EPOLLOUT | EPOLLWRNORM | EPOLLWRBAND) != 0 {
        m |= EPOLLOUT;
    }
    m | (events & (EPOLLPRI | EPOLLRDHUP))
}

struct Watch<T> {
    fd: i32,
    target: T,
    events: u32,
    data: u64,
    armed: bool,
    /// What the last scan saw (masked by `interest`): the edge reference.
    last: u32,
}

pub struct EpollSet<T> {
    watches: Vec<Watch<T>>,
    cursor: usize,
}

impl<T> EpollSet<T> {
    pub fn new() -> Self {
        EpollSet { watches: Vec::new(), cursor: 0 }
    }

    pub fn len(&self) -> usize {
        self.watches.len()
    }

    /// The watched targets (the kernel's loop check walks nested epolls).
    pub fn targets(&self) -> impl Iterator<Item = &T> + '_ {
        self.watches.iter().map(|w| &w.target)
    }

    fn find(&self, fd: i32, target: &T, same: &impl Fn(&T, &T) -> bool) -> Option<usize> {
        self.watches.iter().position(|w| w.fd == fd && same(&w.target, target))
    }

    /// EPOLL_CTL_ADD.  Errors are positive errno values.
    pub fn add(&mut self, fd: i32, target: T, events: u32, data: u64, same: impl Fn(&T, &T) -> bool) -> Result<(), i64> {
        if self.find(fd, &target, &same).is_some() {
            return Err(EEXIST);
        }
        if self.watches.len() >= MAX_WATCHES {
            return Err(ENOSPC);
        }
        self.watches.push(Watch { fd, target, events, data, armed: true, last: 0 });
        Ok(())
    }

    /// EPOLL_CTL_MOD: new mask and data, re-armed, edge reference reset (so
    /// something already ready reports again, as on Linux).
    pub fn modify(&mut self, fd: i32, target: &T, events: u32, data: u64, same: impl Fn(&T, &T) -> bool) -> Result<(), i64> {
        let i = self.find(fd, target, &same).ok_or(ENOENT)?;
        let w = &mut self.watches[i];
        w.events = events;
        w.data = data;
        w.armed = true;
        w.last = 0;
        Ok(())
    }

    /// EPOLL_CTL_DEL.
    pub fn remove(&mut self, fd: i32, target: &T, same: impl Fn(&T, &T) -> bool) -> Result<(), i64> {
        let i = self.find(fd, target, &same).ok_or(ENOENT)?;
        self.watches.remove(i);
        if i < self.cursor {
            self.cursor -= 1;
        }
        Ok(())
    }

    /// Whether any armed entry is ready right now (level semantics): what a
    /// nested epoll descriptor answers to poll / select / epoll.
    pub fn any_ready(&self, mut probe: impl FnMut(&T) -> Option<u32>) -> bool {
        self.watches
            .iter()
            .any(|w| w.armed && probe(&w.target).map_or(false, |st| st & interest(w.events) != 0))
    }

    /// One scan: up to `max` (events, data) pairs.  `probe` answers the
    /// readiness bits of a target (EPOLLIN / OUT / ERR / HUP / RDHUP), or
    /// `None` when it is gone -- the entry is then dropped.
    pub fn collect(&mut self, max: usize, mut probe: impl FnMut(&T) -> Option<u32>) -> Vec<(u32, u64)> {
        let n = self.watches.len();
        let states: Vec<Option<u32>> = self.watches.iter().map(|w| probe(&w.target)).collect();
        let start = if n == 0 { 0 } else { self.cursor % n };
        let mut out = Vec::new();
        let mut next = start;
        for k in 0..n {
            if out.len() >= max {
                break;
            }
            let i = (start + k) % n;
            let Some(st) = states[i] else { continue };
            let w = &mut self.watches[i];
            if !w.armed {
                continue;
            }
            let cur = st & interest(w.events);
            let rep = if w.events & EPOLLET != 0 { cur & !w.last } else { cur };
            w.last = cur;
            if rep != 0 {
                out.push((rep, w.data));
                if w.events & EPOLLONESHOT != 0 {
                    w.armed = false;
                }
                next = i + 1;
            }
        }
        // Drop the dead, keeping the cursor on the same entry.
        let removed_before = states.iter().take(next.min(n)).filter(|s| s.is_none()).count();
        let mut it = states.iter();
        self.watches.retain(|_| it.next().map_or(true, |s| s.is_some()));
        self.cursor = next.saturating_sub(removed_before);
        out
    }
}
