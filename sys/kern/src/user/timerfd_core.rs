// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// timerfd, the pure part: the clock choice, the timespec checks and the timer
// state machine of timerfd(2).  No kernel types in here and no clock reads
// (every method is handed `now`), so tests/timerfd_host.rs can include this
// file as it is.  The descriptor, the clocks, the blocking and the user
// memory live in user/fd/timerfd.rs.
//
// The rules (Linux's):
//   arm       `value` is the first expiry (relative to now, or an absolute
//             time on the timer's clock with TFD_TIMER_ABSTIME); 0 disarms.
//             `interval` > 0 makes it periodic.  Arming forgets the
//             expirations nobody has read yet.
//   expire    every expiry that has gone by since the last look adds 1 to the
//             pending count: a one-shot timer ends up disarmed, a periodic
//             one counts every missed interval and moves on to the next.
//   read      returns the pending count and resets it to 0.  A count of 0
//             would block.
//   ready     readable when the count is above 0 (after counting what has
//             expired).  Never writable.
//   gettime   the time left until the next expiry (0 when disarmed) and the
//             interval, which is kept even when the timer is disarmed.
//
// Nothing here ticks by itself: an expiry is noticed when somebody looks (a
// read, a poll probe, gettime, settime), which is what makes the state
// machine testable with a plain integer for `now`.

pub const TFD_TIMER_ABSTIME: u32 = 1;
pub const TFD_TIMER_CANCEL_ON_SET: u32 = 2;

pub const EAGAIN: i64 = 11;
pub const EINVAL: i64 = 22;

pub const NSEC_PER_SEC: u64 = 1_000_000_000;

/// The clocks a timerfd can run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clock {
    Realtime,
    Monotonic,
}

impl Clock {
    /// `timerfd_create`'s `clockid`: CLOCK_REALTIME (0), CLOCK_MONOTONIC (1)
    /// and CLOCK_BOOTTIME (7, the same clock as the monotonic one here).
    /// Anything else -- the raw / coarse / cputime ids, the *_ALARM clocks --
    /// is `-EINVAL`.
    pub fn from_id(id: u64) -> Result<Clock, i64> {
        match id {
            0 => Ok(Clock::Realtime),
            1 | 7 => Ok(Clock::Monotonic),
            _ => Err(-EINVAL),
        }
    }
}

/// `timerfd_settime`'s `flags`: returns whether the first expiry is an
/// absolute time.  TFD_TIMER_CANCEL_ON_SET is accepted (the wall clock cannot
/// be stepped here, so there is nothing to cancel); any other bit is
/// `-EINVAL`.
pub fn settime_flags(flags: u64) -> Result<bool, i64> {
    let known = (TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) as u64;
    if flags & !known != 0 {
        return Err(-EINVAL);
    }
    Ok(flags & TFD_TIMER_ABSTIME as u64 != 0)
}

/// A `struct timespec` as nanoseconds.  `-EINVAL` for a negative second, a
/// nanosecond field outside 0..1e9.  A span too long for 64 bits of
/// nanoseconds (about 584 years) saturates, which for a timer means "never".
pub fn ts_to_ns(sec: i64, nsec: i64) -> Result<u64, i64> {
    if sec < 0 || nsec < 0 || nsec >= NSEC_PER_SEC as i64 {
        return Err(-EINVAL);
    }
    Ok((sec as u64).saturating_mul(NSEC_PER_SEC).saturating_add(nsec as u64))
}

/// Nanoseconds as a `struct timespec` (seconds, nanoseconds).
pub fn ns_to_ts(ns: u64) -> (i64, i64) {
    ((ns / NSEC_PER_SEC) as i64, (ns % NSEC_PER_SEC) as i64)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimerCore {
    clock: Clock,
    /// The next expiry, on `clock`, in nanoseconds; `None` = disarmed.
    next: Option<u64>,
    /// Reload span in nanoseconds; 0 = one shot.  Kept while disarmed.
    interval: u64,
    /// Expirations counted and not read yet.
    ticks: u64,
}

impl TimerCore {
    pub const fn new(clock: Clock) -> TimerCore {
        TimerCore { clock, next: None, interval: 0, ticks: 0 }
    }

    pub fn clock(&self) -> Clock {
        self.clock
    }

    /// Count what has expired by `now`.
    fn expire(&mut self, now: u64) {
        let Some(t) = self.next else { return };
        if now < t {
            return;
        }
        if self.interval == 0 {
            self.ticks = self.ticks.saturating_add(1);
            self.next = None;
        } else {
            // Every interval that has gone by, the one that just ended
            // included; `t + n * interval` is then the first expiry after now.
            let n = (now - t) / self.interval + 1;
            self.ticks = self.ticks.saturating_add(n);
            self.next = t.checked_add(n.saturating_mul(self.interval));
        }
    }

    /// A read would not block.
    pub fn readable(&mut self, now: u64) -> bool {
        self.expire(now);
        self.ticks > 0
    }

    /// Take the pending count.  `Err(-EAGAIN)` when it is 0 (the caller
    /// decides whether that means "park" or "fail").
    pub fn read(&mut self, now: u64) -> Result<u64, i64> {
        self.expire(now);
        if self.ticks == 0 {
            return Err(-EAGAIN);
        }
        let v = self.ticks;
        self.ticks = 0;
        Ok(v)
    }

    /// Put back what `read` took (the caller could not hand it over).
    pub fn give_back(&mut self, v: u64) {
        self.ticks = self.ticks.saturating_add(v);
    }

    /// How long a reader that found nothing should wait: nanoseconds until
    /// the next expiry (0 when something is pending already), `None` when the
    /// timer is disarmed and only a `settime` can change that.
    pub fn wait_ns(&self, now: u64) -> Option<u64> {
        if self.ticks > 0 {
            return Some(0);
        }
        self.next.map(|t| t.saturating_sub(now))
    }

    /// `timerfd_gettime`: (time left until the next expiry, interval), both in
    /// nanoseconds.  The time left is 0 for a disarmed timer, and never 0 for
    /// an armed one (an expiry that has come is counted first).
    pub fn gettime(&mut self, now: u64) -> (u64, u64) {
        self.expire(now);
        (self.next.map_or(0, |t| t.saturating_sub(now)), self.interval)
    }

    /// `timerfd_settime`: arm, re-arm or (`value` 0) disarm.  `value` is
    /// relative to `now`, or an absolute time on the clock when `absolute`;
    /// an absolute time already gone by expires at the next look.  The
    /// interval is stored whatever `value` is, and the unread expirations are
    /// dropped.  Returns what `gettime` would have said just before.
    pub fn settime(&mut self, now: u64, absolute: bool, value: u64, interval: u64) -> (u64, u64) {
        let old = self.gettime(now);
        self.ticks = 0;
        self.interval = interval;
        self.next = if value == 0 {
            None
        } else if absolute {
            Some(value)
        } else {
            Some(now.saturating_add(value))
        };
        old
    }
}
