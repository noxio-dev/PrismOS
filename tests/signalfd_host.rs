// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of the signalfd rules (sys/kern/src/user/signalfd_core.rs):
// mask normalisation, which pending signal a read takes, the record layout.
//
//   rustc --edition 2021 --test tests/signalfd_host.rs -o /tmp/signalfd_host \
//     && /tmp/signalfd_host --test-threads=1

#![allow(dead_code, unused_imports)]

#[path = "../sys/kern/src/user/signalfd_core.rs"]
mod signalfd_core;

use signalfd_core::*;

fn bit(n: u32) -> u64 {
    1u64 << n
}

// ------------------------------------------------------------------- mask

#[test]
fn mask_keeps_ordinary_signals() {
    let raw = bit(2) | bit(15) | bit(17);
    assert_eq!(normalize_mask(raw), raw as u32);
}

#[test]
fn mask_drops_sigkill_and_sigstop() {
    assert_eq!(normalize_mask(bit(SIGKILL)), 0);
    assert_eq!(normalize_mask(bit(SIGSTOP)), 0);
    assert_eq!(normalize_mask(bit(SIGKILL) | bit(SIGSTOP) | bit(15)), bit(15) as u32);
}

#[test]
fn mask_drops_bit_zero_and_signals_past_nsig() {
    assert_eq!(normalize_mask(1), 0);
    assert_eq!(normalize_mask(bit(32)), 0);
    assert_eq!(normalize_mask(bit(63)), 0);
    assert_eq!(normalize_mask(bit(31)), bit(31) as u32);
}

#[test]
fn mask_all_ones() {
    let m = normalize_mask(u64::MAX);
    assert_eq!(m & 1, 0);
    assert_eq!(m & (1 << SIGKILL), 0);
    assert_eq!(m & (1 << SIGSTOP), 0);
    assert_eq!(m.count_ones(), 32 - 3);
}

#[test]
fn mask_empty() {
    assert_eq!(normalize_mask(0), 0);
}

#[test]
fn sigsetsize_must_be_eight() {
    assert_eq!(check_sigsetsize(8), Ok(()));
    for bad in [0, 1, 4, 7, 9, 16, 128, u64::MAX] {
        assert_eq!(check_sigsetsize(bad), Err(-EINVAL), "size {bad}");
    }
}

// --------------------------------------------------------- next_signal

#[test]
fn nothing_pending_nothing_taken() {
    assert_eq!(next_signal(0, u32::MAX), None);
    assert_eq!(next_signal(1 << 15, 0), None);
}

#[test]
fn only_masked_signals_are_taken() {
    let pending = (1 << 2) | (1 << 15);
    assert_eq!(next_signal(pending, 1 << 15), Some(15));
    assert_eq!(next_signal(pending, 1 << 2), Some(2));
    assert_eq!(next_signal(pending, 1 << 10), None);
}

#[test]
fn lowest_signal_first() {
    let pending = (1 << 17) | (1 << 15) | (1 << 2);
    let mask = (1 << 17) | (1 << 15) | (1 << 2);
    assert_eq!(next_signal(pending, mask), Some(2));
    assert_eq!(next_signal(pending & !(1 << 2), mask), Some(15));
    assert_eq!(next_signal(pending & !(1 << 2) & !(1 << 15), mask), Some(17));
}

#[test]
fn bit_zero_is_never_a_signal() {
    assert_eq!(next_signal(1, u32::MAX), None);
}

#[test]
fn signal_31_is_reachable() {
    assert_eq!(next_signal(1 << 31, 1 << 31), Some(31));
}

#[test]
fn draining_takes_each_once() {
    let mut pending: u32 = (1 << 1) | (1 << 10) | (1 << 17);
    let mask = pending;
    let mut got = Vec::new();
    while let Some(s) = next_signal(pending, mask) {
        pending &= !(1 << s);
        got.push(s);
    }
    assert_eq!(got, vec![1, 10, 17]);
    assert_eq!(pending, 0);
}

// ----------------------------------------------------------- records

#[test]
fn records_per_buffer() {
    assert_eq!(records_that_fit(0), 0);
    assert_eq!(records_that_fit(127), 0);
    assert_eq!(records_that_fit(128), 1);
    assert_eq!(records_that_fit(255), 1);
    assert_eq!(records_that_fit(256), 2);
    assert_eq!(records_that_fit(128 * 5 + 3), 5);
}

#[test]
fn record_is_128_bytes() {
    assert_eq!(SIGINFO_SIZE, 128);
    assert_eq!(record(15, 0, 0, 0).len(), 128);
}

#[test]
fn record_layout_matches_linux() {
    let r = record(15, -6, 1234, 1000);
    assert_eq!(u32::from_le_bytes(r[0..4].try_into().unwrap()), 15); // ssi_signo
    assert_eq!(i32::from_le_bytes(r[4..8].try_into().unwrap()), 0); // ssi_errno
    assert_eq!(i32::from_le_bytes(r[8..12].try_into().unwrap()), -6); // ssi_code
    assert_eq!(u32::from_le_bytes(r[12..16].try_into().unwrap()), 1234); // ssi_pid
    assert_eq!(u32::from_le_bytes(r[16..20].try_into().unwrap()), 1000); // ssi_uid
}

#[test]
fn record_rest_is_zero() {
    let r = record(u32::MAX, i32::MIN, u32::MAX, u32::MAX);
    assert!(r[20..].iter().all(|&b| b == 0));
    assert!(r[4..8].iter().all(|&b| b == 0));
}

#[test]
fn kernel_raised_record() {
    // SI_KERNEL = 0x80, no sender
    let r = record(17, 0x80, 0, 0);
    assert_eq!(i32::from_le_bytes(r[8..12].try_into().unwrap()), 0x80);
    assert_eq!(u32::from_le_bytes(r[12..16].try_into().unwrap()), 0);
}
