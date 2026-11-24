// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of the eventfd counter (sys/kern/src/user/eventfd_core.rs):
// read / write / semaphore mode / readiness / overflow.
//
//   rustc --edition 2021 --test tests/eventfd_host.rs -o /tmp/eventfd_host \
//     && /tmp/eventfd_host --test-threads=1

#![allow(dead_code, unused_imports)]

#[path = "../sys/kern/src/user/eventfd_core.rs"]
mod eventfd_core;

use eventfd_core::*;

#[test]
fn starts_at_the_initial_value() {
    assert_eq!(EventCounter::new(0, false).value(), 0);
    assert_eq!(EventCounter::new(7, false).value(), 7);
    assert_eq!(EventCounter::new(u32::MAX, false).value(), u32::MAX as u64);
}

#[test]
fn empty_read_would_block() {
    let mut c = EventCounter::new(0, false);
    assert!(!c.readable());
    assert_eq!(c.read(), Err(-EAGAIN));
}

#[test]
fn read_returns_the_sum_and_resets() {
    let mut c = EventCounter::new(0, false);
    c.write(3).unwrap();
    c.write(4).unwrap();
    assert!(c.readable());
    assert_eq!(c.read(), Ok(7));
    assert_eq!(c.value(), 0);
    assert!(!c.readable());
    assert_eq!(c.read(), Err(-EAGAIN));
}

#[test]
fn initial_value_is_readable_at_once() {
    let mut c = EventCounter::new(5, false);
    assert!(c.readable());
    assert_eq!(c.read(), Ok(5));
}

#[test]
fn semaphore_read_takes_one_at_a_time() {
    let mut c = EventCounter::new(0, true);
    c.write(3).unwrap();
    assert_eq!(c.read(), Ok(1));
    assert_eq!(c.value(), 2);
    assert_eq!(c.read(), Ok(1));
    assert_eq!(c.read(), Ok(1));
    assert!(!c.readable());
    assert_eq!(c.read(), Err(-EAGAIN));
}

#[test]
fn write_zero_is_valid_and_changes_nothing() {
    let mut c = EventCounter::new(0, false);
    assert_eq!(c.write(0), Ok(()));
    assert_eq!(c.value(), 0);
    assert!(!c.readable());
}

#[test]
fn writing_u64_max_is_invalid() {
    let mut c = EventCounter::new(0, false);
    assert_eq!(c.write(u64::MAX), Err(-EINVAL));
    assert_eq!(c.value(), 0);
}

#[test]
fn counter_max_is_accepted_and_one_more_would_block() {
    let mut c = EventCounter::new(0, false);
    assert_eq!(c.write(COUNTER_MAX), Ok(()));
    assert_eq!(c.value(), COUNTER_MAX);
    assert!(!c.writable());
    assert_eq!(c.write(1), Err(-EAGAIN));
    assert_eq!(c.write(0), Ok(())); // adding nothing never overflows
    assert_eq!(c.value(), COUNTER_MAX);
}

#[test]
fn overflowing_write_leaves_the_counter_alone() {
    let mut c = EventCounter::new(0, false);
    c.write(10).unwrap();
    assert_eq!(c.write(COUNTER_MAX), Err(-EAGAIN));
    assert_eq!(c.value(), 10);
}

#[test]
fn a_read_makes_room_again() {
    let mut c = EventCounter::new(0, false);
    c.write(COUNTER_MAX).unwrap();
    assert!(!c.writable());
    assert_eq!(c.read(), Ok(COUNTER_MAX));
    assert!(c.writable());
    assert_eq!(c.write(1), Ok(()));
}

#[test]
fn writable_until_one_below_the_ceiling() {
    let mut c = EventCounter::new(0, false);
    c.write(COUNTER_MAX - 1).unwrap();
    assert!(c.writable()); // a write of 1 still fits
    assert_eq!(c.write(1), Ok(()));
    assert!(!c.writable());
}

#[test]
fn semaphore_mode_does_not_change_write_rules() {
    let mut c = EventCounter::new(0, true);
    assert_eq!(c.write(u64::MAX), Err(-EINVAL));
    assert_eq!(c.write(COUNTER_MAX), Ok(()));
    assert_eq!(c.write(1), Err(-EAGAIN));
}
