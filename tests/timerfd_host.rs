// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of the timerfd state machine
// (sys/kern/src/user/timerfd_core.rs): clock ids, timespec checks, one-shot
// and periodic expiry, absolute and relative arming, gettime / settime.
//
//   rustc --edition 2021 --test tests/timerfd_host.rs -o /tmp/timerfd_host \
//     && /tmp/timerfd_host --test-threads=1

#![allow(dead_code, unused_imports)]

#[path = "../sys/kern/src/user/timerfd_core.rs"]
mod timerfd_core;

use timerfd_core::*;

const MS: u64 = 1_000_000;

fn mono() -> TimerCore {
    TimerCore::new(Clock::Monotonic)
}

// ------------------------------------------------------------ clocks, flags

#[test]
fn clock_ids() {
    assert_eq!(Clock::from_id(0), Ok(Clock::Realtime));
    assert_eq!(Clock::from_id(1), Ok(Clock::Monotonic));
    assert_eq!(Clock::from_id(7), Ok(Clock::Monotonic)); // BOOTTIME
    for bad in [2, 3, 4, 5, 6, 8, 9, 10, 11, 1000, u64::MAX] {
        assert_eq!(Clock::from_id(bad), Err(-EINVAL), "clock id {bad}");
    }
}

#[test]
fn settime_flag_bits() {
    assert_eq!(settime_flags(0), Ok(false));
    assert_eq!(settime_flags(TFD_TIMER_ABSTIME as u64), Ok(true));
    assert_eq!(settime_flags(TFD_TIMER_CANCEL_ON_SET as u64), Ok(false));
    assert_eq!(
        settime_flags((TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) as u64),
        Ok(true)
    );
    assert_eq!(settime_flags(4), Err(-EINVAL));
    assert_eq!(settime_flags(1 << 32), Err(-EINVAL));
}

// -------------------------------------------------------------- timespecs

#[test]
fn timespec_conversion() {
    assert_eq!(ts_to_ns(0, 0), Ok(0));
    assert_eq!(ts_to_ns(1, 500), Ok(1_000_000_500));
    assert_eq!(ts_to_ns(0, 999_999_999), Ok(999_999_999));
    assert_eq!(ns_to_ts(1_000_000_500), (1, 500));
    assert_eq!(ns_to_ts(0), (0, 0));
    for (s, n) in [(0, 123), (7, 999_999_999), (86_400, 1)] {
        let ns = ts_to_ns(s, n).unwrap();
        assert_eq!(ns_to_ts(ns), (s, n));
    }
}

#[test]
fn invalid_timespecs() {
    assert_eq!(ts_to_ns(-1, 0), Err(-EINVAL));
    assert_eq!(ts_to_ns(0, -1), Err(-EINVAL));
    assert_eq!(ts_to_ns(0, 1_000_000_000), Err(-EINVAL));
    assert_eq!(ts_to_ns(0, i64::MAX), Err(-EINVAL));
    assert_eq!(ts_to_ns(i64::MIN, 0), Err(-EINVAL));
}

#[test]
fn a_huge_timespec_saturates() {
    assert_eq!(ts_to_ns(i64::MAX, 999_999_999), Ok(u64::MAX));
}

// ------------------------------------------------------------ a fresh timer

#[test]
fn starts_disarmed() {
    let mut t = mono();
    assert_eq!(t.clock(), Clock::Monotonic);
    assert!(!t.readable(0));
    assert!(!t.readable(u64::MAX));
    assert_eq!(t.read(1_000), Err(-EAGAIN));
    assert_eq!(t.gettime(1_000), (0, 0));
    assert_eq!(t.wait_ns(1_000), None);
}

// ----------------------------------------------------------------- one shot

#[test]
fn one_shot_fires_once() {
    let mut t = mono();
    t.settime(100, false, 50, 0);
    assert!(!t.readable(100));
    assert!(!t.readable(149));
    assert_eq!(t.read(149), Err(-EAGAIN));
    assert!(t.readable(150)); // exactly at the expiry
    assert_eq!(t.read(150), Ok(1));
    assert!(!t.readable(150));
    assert!(!t.readable(10_000)); // a one shot does not come back
    assert_eq!(t.read(10_000), Err(-EAGAIN));
}

#[test]
fn one_shot_read_late_still_counts_one() {
    let mut t = mono();
    t.settime(0, false, 10, 0);
    assert_eq!(t.read(1_000_000), Ok(1));
    assert_eq!(t.gettime(1_000_000), (0, 0)); // disarmed again
}

#[test]
fn gettime_reports_the_time_left() {
    let mut t = mono();
    t.settime(100, false, 50, 0);
    assert_eq!(t.gettime(100), (50, 0));
    assert_eq!(t.gettime(120), (30, 0));
    assert_eq!(t.gettime(149), (1, 0));
    assert_eq!(t.gettime(150), (0, 0)); // expired: disarmed, one pending
    assert!(t.readable(150));
}

#[test]
fn readable_is_stable_until_read() {
    let mut t = mono();
    t.settime(0, false, 10, 0);
    assert!(t.readable(10));
    assert!(t.readable(11));
    assert!(t.readable(500));
    assert_eq!(t.read(500), Ok(1));
}

// ----------------------------------------------------------------- periodic

#[test]
fn periodic_counts_every_missed_interval() {
    let mut t = mono();
    t.settime(0, false, 10, 10);
    assert!(!t.readable(9));
    // expiries at 10, 20, 30 have gone by at 35
    assert!(t.readable(35));
    assert_eq!(t.gettime(35), (5, 10)); // next one at 40
    assert_eq!(t.read(35), Ok(3));
    assert!(!t.readable(39));
    assert!(t.readable(40));
    assert_eq!(t.read(40), Ok(1));
}

#[test]
fn periodic_exact_boundary_counts_one() {
    let mut t = mono();
    t.settime(0, false, 10, 10);
    assert_eq!(t.read(10), Ok(1));
    assert_eq!(t.gettime(10), (10, 10));
    assert_eq!(t.read(20), Ok(1));
}

#[test]
fn periodic_first_expiry_may_differ_from_the_interval() {
    let mut t = mono();
    t.settime(0, false, 5, 100);
    assert_eq!(t.read(5), Ok(1));
    assert!(!t.readable(104));
    assert_eq!(t.read(105), Ok(1));
}

#[test]
fn periodic_does_not_drift_when_read_late() {
    let mut t = mono();
    t.settime(0, false, 10 * MS, 10 * MS);
    // read at 25 ms: two have expired, the next is still at 30 ms
    assert_eq!(t.read(25 * MS), Ok(2));
    assert_eq!(t.gettime(25 * MS), (5 * MS, 10 * MS));
    assert_eq!(t.read(30 * MS), Ok(1));
}

#[test]
fn periodic_after_a_long_sleep() {
    let mut t = mono();
    t.settime(0, false, 1, 1);
    assert_eq!(t.read(1_000_000), Ok(1_000_000));
    assert_eq!(t.gettime(1_000_000), (1, 1));
}

#[test]
fn pending_counts_add_up_across_looks() {
    let mut t = mono();
    t.settime(0, false, 10, 10);
    assert!(t.readable(10));
    assert!(t.readable(20)); // looked at twice without a read
    assert_eq!(t.read(30), Ok(3));
}

// ------------------------------------------------------------------ settime

#[test]
fn settime_returns_the_previous_setting() {
    let mut t = mono();
    assert_eq!(t.settime(0, false, 100, 40), (0, 0));
    assert_eq!(t.settime(30, false, 500, 7), (70, 40));
    assert_eq!(t.gettime(30), (500, 7));
}

#[test]
fn settime_forgets_unread_expirations() {
    let mut t = mono();
    t.settime(0, false, 10, 10);
    assert!(t.readable(50)); // 5 pending
    t.settime(50, false, 100, 0);
    assert!(!t.readable(50));
    assert_eq!(t.read(50), Err(-EAGAIN));
}

#[test]
fn settime_old_value_of_an_expired_one_shot_is_zero() {
    let mut t = mono();
    t.settime(0, false, 10, 0);
    assert_eq!(t.settime(20, false, 10, 0), (0, 0));
}

#[test]
fn settime_zero_disarms() {
    let mut t = mono();
    t.settime(0, false, 100, 100);
    t.settime(10, false, 0, 0);
    assert!(!t.readable(10_000));
    assert_eq!(t.gettime(10_000), (0, 0));
    assert_eq!(t.wait_ns(10_000), None);
}

#[test]
fn disarming_drops_what_was_pending() {
    let mut t = mono();
    t.settime(0, false, 10, 0);
    assert!(t.readable(20));
    t.settime(20, false, 0, 0);
    assert!(!t.readable(20));
}

#[test]
fn zero_value_keeps_the_interval_like_linux() {
    let mut t = mono();
    t.settime(0, false, 0, 5_000);
    assert!(!t.readable(1_000_000)); // disarmed: never fires
    assert_eq!(t.gettime(1_000_000), (0, 5_000)); // yet the interval is reported
}

#[test]
fn rearming_restarts_from_now_not_from_the_old_phase() {
    let mut t = mono();
    t.settime(0, false, 10, 10);
    t.settime(33, false, 10, 10); // the old schedule (40) is gone
    assert!(!t.readable(42));
    assert!(t.readable(43));
}

// ----------------------------------------------------------------- absolute

#[test]
fn absolute_expiry_is_on_the_clock_not_relative() {
    let mut t = mono();
    t.settime(1_000, true, 1_500, 0);
    assert_eq!(t.gettime(1_000), (500, 0));
    assert!(!t.readable(1_499));
    assert!(t.readable(1_500));
    assert_eq!(t.read(1_500), Ok(1));
}

#[test]
fn absolute_in_the_past_fires_at_once() {
    let mut t = mono();
    t.settime(1_000, true, 400, 0);
    assert!(t.readable(1_000));
    assert_eq!(t.read(1_000), Ok(1));
    assert!(!t.readable(1_000));
}

#[test]
fn periodic_absolute_in_the_past_counts_the_missed_intervals() {
    let mut t = mono();
    // first expiry at 100, every 100, armed at 1_050: 100, 200, ..., 1000 = 10
    t.settime(1_050, true, 100, 100);
    assert_eq!(t.read(1_050), Ok(10));
    assert_eq!(t.gettime(1_050), (50, 100)); // next at 1_100
}

#[test]
fn absolute_zero_disarms_too() {
    let mut t = mono();
    t.settime(100, true, 0, 0);
    assert!(!t.readable(1_000_000));
}

// -------------------------------------------------------------- give_back

#[test]
fn give_back_restores_a_read() {
    let mut t = mono();
    t.settime(0, false, 10, 10);
    let v = t.read(35).unwrap();
    assert_eq!(v, 3);
    assert!(!t.readable(36));
    t.give_back(v);
    assert!(t.readable(36));
    assert_eq!(t.read(36), Ok(3));
}

// ----------------------------------------------------------------- wait_ns

#[test]
fn wait_ns_is_the_time_to_the_next_expiry() {
    let mut t = mono();
    t.settime(100, false, 50, 0);
    assert_eq!(t.wait_ns(100), Some(50));
    assert_eq!(t.wait_ns(130), Some(20));
    assert_eq!(t.wait_ns(150), Some(0)); // due, not counted yet
    assert!(t.readable(150));
    assert_eq!(t.wait_ns(150), Some(0)); // pending
    assert_eq!(t.read(150), Ok(1));
    assert_eq!(t.wait_ns(150), None); // one shot done: only settime helps
}

// ---------------------------------------------------------------- overflow

#[test]
fn relative_value_near_the_end_of_time_saturates() {
    let mut t = mono();
    t.settime(u64::MAX - 5, false, 100, 0);
    assert!(!t.readable(u64::MAX - 5));
    assert_eq!(t.gettime(u64::MAX - 5), (5, 0));
}

#[test]
fn a_huge_interval_does_not_overflow() {
    let mut t = mono();
    t.settime(0, false, 10, u64::MAX / 2 + 7);
    assert_eq!(t.read(10), Ok(1));
    // the next expiry would be past u64::MAX / 2 + ...: still representable
    assert!(!t.readable(11));
    let mut u = mono();
    u.settime(0, false, u64::MAX - 1, u64::MAX - 1);
    assert_eq!(u.read(u64::MAX - 1), Ok(1)); // the next one is out of range: disarmed
    assert_eq!(u.gettime(u64::MAX - 1), (0, u64::MAX - 1));
}

#[test]
fn the_clock_is_kept() {
    assert_eq!(TimerCore::new(Clock::Realtime).clock(), Clock::Realtime);
}
