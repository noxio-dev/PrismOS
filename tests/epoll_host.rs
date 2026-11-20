// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of the epoll interest list (sys/kern/src/user/epoll_core.rs):
// level / edge / oneshot semantics, key rules, dead targets, fair scanning.
//
//   rustc --edition 2021 --test tests/epoll_host.rs -o /tmp/epoll_host \
//     && /tmp/epoll_host --test-threads=1

#![allow(dead_code, unused_imports)]

extern crate alloc;

#[path = "../sys/kern/src/user/epoll_core.rs"]
mod epoll_core;

use epoll_core::*;
use std::collections::HashMap;

fn same(a: &u32, b: &u32) -> bool {
    a == b
}

/// A probe over a table of target -> bits (absent = dead).
fn probe(t: &HashMap<u32, u32>) -> impl FnMut(&u32) -> Option<u32> + '_ {
    move |k| t.get(k).copied()
}

#[test]
fn level_reports_until_drained() {
    let mut s = EpollSet::new();
    s.add(3, 1u32, EPOLLIN, 77, same).unwrap();
    let mut t = HashMap::from([(1, EPOLLIN | EPOLLOUT)]);
    // only IN was asked: OUT is not reported
    assert_eq!(s.collect(8, probe(&t)), vec![(EPOLLIN, 77)]);
    assert_eq!(s.collect(8, probe(&t)), vec![(EPOLLIN, 77)]); // level: again
    t.insert(1, 0);
    assert!(s.collect(8, probe(&t)).is_empty());
}

#[test]
fn err_and_hup_always_reported() {
    let mut s = EpollSet::new();
    s.add(3, 1u32, EPOLLIN, 0, same).unwrap();
    let t = HashMap::from([(1, EPOLLHUP | EPOLLERR)]);
    assert_eq!(s.collect(8, probe(&t)), vec![(EPOLLHUP | EPOLLERR, 0)]);
}

#[test]
fn aliases_fold_onto_in_and_out() {
    let mut s = EpollSet::new();
    s.add(3, 1u32, EPOLLRDNORM | EPOLLWRBAND, 5, same).unwrap();
    let t = HashMap::from([(1, EPOLLIN | EPOLLOUT)]);
    assert_eq!(s.collect(8, probe(&t)), vec![(EPOLLIN | EPOLLOUT, 5)]);
}

#[test]
fn edge_reports_only_new_bits() {
    let mut s = EpollSet::new();
    s.add(3, 1u32, EPOLLIN | EPOLLET, 9, same).unwrap();
    let mut t = HashMap::from([(1, EPOLLIN)]);
    assert_eq!(s.collect(8, probe(&t)), vec![(EPOLLIN, 9)]); // already ready: reports
    assert!(s.collect(8, probe(&t)).is_empty()); // no new edge
    t.insert(1, 0);
    assert!(s.collect(8, probe(&t)).is_empty()); // falling edge: silent
    t.insert(1, EPOLLIN);
    assert_eq!(s.collect(8, probe(&t)), vec![(EPOLLIN, 9)]); // rising again
}

#[test]
fn oneshot_disarms_until_modified() {
    let mut s = EpollSet::new();
    s.add(3, 1u32, EPOLLIN | EPOLLONESHOT, 1, same).unwrap();
    let t = HashMap::from([(1, EPOLLIN)]);
    assert_eq!(s.collect(8, probe(&t)).len(), 1);
    assert!(s.collect(8, probe(&t)).is_empty());
    s.modify(3, &1, EPOLLIN | EPOLLONESHOT, 2, same).unwrap();
    assert_eq!(s.collect(8, probe(&t)), vec![(EPOLLIN, 2)]);
}

#[test]
fn key_is_fd_and_target() {
    let mut s = EpollSet::new();
    s.add(3, 1u32, EPOLLIN, 0, same).unwrap();
    assert_eq!(s.add(3, 1u32, EPOLLIN, 0, same), Err(17)); // EEXIST
    s.add(4, 1u32, EPOLLIN, 0, same).unwrap(); // same target, other number
    s.add(3, 2u32, EPOLLIN, 0, same).unwrap(); // same number, other target
    assert_eq!(s.remove(9, &1, same), Err(2)); // ENOENT
    assert_eq!(s.modify(9, &1, 0, 0, same), Err(2));
    s.remove(3, &1, same).unwrap();
    assert_eq!(s.len(), 2);
}

#[test]
fn watch_limit_is_enospc() {
    let mut s = EpollSet::new();
    for i in 0..MAX_WATCHES as u32 {
        s.add(i as i32, i, EPOLLIN, 0, same).unwrap();
    }
    assert_eq!(s.add(999, 999u32, EPOLLIN, 0, same), Err(28));
}

#[test]
fn dead_targets_are_dropped() {
    let mut s = EpollSet::new();
    s.add(3, 1u32, EPOLLIN, 1, same).unwrap();
    s.add(4, 2u32, EPOLLIN, 2, same).unwrap();
    let t = HashMap::from([(2, EPOLLIN)]); // 1 is gone
    assert_eq!(s.collect(8, probe(&t)), vec![(EPOLLIN, 2)]);
    assert_eq!(s.len(), 1);
}

#[test]
fn small_max_does_not_starve_the_tail() {
    let mut s = EpollSet::new();
    for i in 0..4u32 {
        s.add(i as i32, i, EPOLLIN, i as u64, same).unwrap();
    }
    let t: HashMap<u32, u32> = (0..4).map(|i| (i, EPOLLIN)).collect();
    let mut seen = Vec::new();
    for _ in 0..4 {
        for (_, d) in s.collect(1, probe(&t)) {
            seen.push(d);
        }
    }
    assert_eq!(seen, vec![0, 1, 2, 3]);
}

#[test]
fn max_caps_the_batch() {
    let mut s = EpollSet::new();
    for i in 0..5u32 {
        s.add(i as i32, i, EPOLLIN, 0, same).unwrap();
    }
    let t: HashMap<u32, u32> = (0..5).map(|i| (i, EPOLLIN)).collect();
    assert_eq!(s.collect(3, probe(&t)).len(), 3);
}

#[test]
fn any_ready_is_level_and_ignores_disarmed() {
    let mut s = EpollSet::new();
    s.add(3, 1u32, EPOLLIN | EPOLLONESHOT, 0, same).unwrap();
    let t = HashMap::from([(1, EPOLLIN)]);
    assert!(s.any_ready(probe(&t)));
    s.collect(8, probe(&t));
    assert!(!s.any_ready(probe(&t))); // disarmed by the oneshot
}
