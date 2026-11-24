// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// eventfd, the pure part: the 64-bit counter and the rules of eventfd(2) for
// read, write and readiness.  No kernel types in here, so tests/eventfd_host.rs
// can include this file as it is.  The descriptor, the blocking and the user
// memory live in user/fd/eventfd.rs.
//
// The rules (Linux's):
//   read   returns the whole counter and resets it to 0; in semaphore mode it
//          returns 1 and subtracts 1.  A counter at 0 would block.
//   write  adds a value; it would block if the sum would pass COUNTER_MAX
//          (u64::MAX - 1); writing u64::MAX is invalid.
//   ready  readable when the counter is above 0, writable when a write of 1
//          would not block.

pub const EFD_SEMAPHORE: u32 = 1;

/// The largest value the counter may hold (`ULLONG_MAX - 1`).
pub const COUNTER_MAX: u64 = u64::MAX - 1;

pub const EAGAIN: i64 = 11;
pub const EINVAL: i64 = 22;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventCounter {
    count: u64,
    semaphore: bool,
}

impl EventCounter {
    pub const fn new(initval: u32, semaphore: bool) -> EventCounter {
        EventCounter { count: initval as u64, semaphore }
    }

    /// The counter, for tests and /proc-style reporting.
    pub fn value(&self) -> u64 {
        self.count
    }

    /// A read would not block.
    pub fn readable(&self) -> bool {
        self.count > 0
    }

    /// A write of 1 would not block.
    pub fn writable(&self) -> bool {
        self.count < COUNTER_MAX
    }

    /// Take the counter.  `Err(-EAGAIN)` when it is 0 (the caller decides
    /// whether that means "park" or "fail").
    pub fn read(&mut self) -> Result<u64, i64> {
        if self.count == 0 {
            return Err(-EAGAIN);
        }
        if self.semaphore {
            self.count -= 1;
            Ok(1)
        } else {
            let v = self.count;
            self.count = 0;
            Ok(v)
        }
    }

    /// Add `v`.  `Err(-EINVAL)` for u64::MAX (never valid); `Err(-EAGAIN)`
    /// when the sum would pass COUNTER_MAX (the caller decides whether that
    /// means "park" or "fail").  Writing 0 is valid and changes nothing.
    pub fn write(&mut self, v: u64) -> Result<(), i64> {
        if v == u64::MAX {
            return Err(-EINVAL);
        }
        if v > COUNTER_MAX - self.count {
            return Err(-EAGAIN);
        }
        self.count += v;
        Ok(())
    }
}
