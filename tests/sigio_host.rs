// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of the SIGIO rules (sys/kern/src/user/sigio_core.rs):
// F_SETOWN decoding, F_SETSIG validation, the readiness edge.
//
//   rustc --edition 2021 --test tests/sigio_host.rs -o /tmp/sigio_host \
//     && /tmp/sigio_host --test-threads=1

#![allow(dead_code, unused_imports)]

#[path = "../sys/kern/src/user/sigio_core.rs"]
mod sigio_core;

use sigio_core::*;

#[test]
fn owner_zero_is_nobody() {
    assert_eq!(decode_owner(0), Some(Owner::None));
}

#[test]
fn positive_owner_is_a_process() {
    assert_eq!(decode_owner(42), Some(Owner::Pid(42)));
}

#[test]
fn negative_owner_is_a_group() {
    assert_eq!(decode_owner(-7), Some(Owner::Pgrp(7)));
}

#[test]
fn int_min_is_refused() {
    assert_eq!(decode_owner(i32::MIN), None);
}

#[test]
fn owner_round_trips() {
    for v in [0, 1, 99, -1, -300, i32::MAX, -i32::MAX] {
        assert_eq!(encode_owner(decode_owner(v).unwrap()), v);
    }
}

#[test]
fn setsig_zero_means_sigio() {
    assert_eq!(signal_for(0), SIGIO);
    assert_eq!(SIGIO, 29);
}

#[test]
fn setsig_picks_the_chosen_signal() {
    assert_eq!(signal_for(10), 10);
}

#[test]
fn setsig_range() {
    assert!(valid_setsig(0));
    assert!(valid_setsig(31));
    assert!(!valid_setsig(32));
    assert!(!valid_setsig(64));
}

#[test]
fn fires_on_the_rising_edge() {
    assert!(should_fire(true, Owner::Pid(3), false, true));
}

#[test]
fn silent_while_staying_ready() {
    assert!(!should_fire(true, Owner::Pid(3), true, true));
}

#[test]
fn silent_while_not_ready() {
    assert!(!should_fire(true, Owner::Pid(3), false, false));
}

#[test]
fn silent_on_the_falling_edge() {
    assert!(!should_fire(true, Owner::Pid(3), true, false));
}

#[test]
fn silent_without_o_async() {
    assert!(!should_fire(false, Owner::Pid(3), false, true));
}

#[test]
fn silent_without_an_owner() {
    assert!(!should_fire(true, Owner::None, false, true));
}

#[test]
fn group_owner_fires_too() {
    assert!(should_fire(true, Owner::Pgrp(3), false, true));
}
