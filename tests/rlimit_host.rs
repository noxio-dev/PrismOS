// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of the resource-limit table (sys/kern/src/user/rlimit_core.rs):
// defaults, the setrlimit(2) rules, the descriptor-table ceiling.
//
//   rustc --edition 2021 --test tests/rlimit_host.rs -o /tmp/rlimit_host \
//     && /tmp/rlimit_host --test-threads=1

#![allow(dead_code, unused_imports)]

#[path = "../sys/kern/src/user/rlimit_core.rs"]
mod rlimit_core;

use rlimit_core::*;

const STACK: u64 = 4 * 4096;
const NOFILE: u64 = 64;

fn table() -> RlimitTable {
    RlimitTable::new(STACK, NOFILE)
}

#[test]
fn defaults_report_the_real_system_limits() {
    let t = table();
    assert_eq!(t.get(RLIMIT_STACK), Ok(Rlimit { cur: STACK, max: STACK }));
    assert_eq!(t.get(RLIMIT_NOFILE), Ok(Rlimit { cur: NOFILE, max: NOFILE }));
    assert_eq!(t.get(RLIMIT_CORE), Ok(Rlimit { cur: 0, max: RLIM_INFINITY }));
    assert_eq!(t.get(RLIMIT_AS), Ok(Rlimit::INFINITE));
    assert_eq!(t.get(RLIMIT_CPU), Ok(Rlimit::INFINITE));
}

#[test]
fn every_defined_resource_answers_and_the_next_does_not() {
    let t = table();
    for r in 0..RLIM_NLIMITS as u64 {
        assert!(t.get(r).is_ok(), "resource {r}");
    }
    assert_eq!(t.get(RLIM_NLIMITS as u64), Err(-EINVAL));
    assert_eq!(t.get(u64::MAX), Err(-EINVAL));
}

#[test]
fn soft_above_hard_is_einval() {
    let mut t = table();
    let r = t.set(RLIMIT_CORE, Rlimit { cur: 10, max: 5 }, true);
    assert_eq!(r, Err(-EINVAL));
    // nothing changed
    assert_eq!(t.get(RLIMIT_CORE), Ok(Rlimit { cur: 0, max: RLIM_INFINITY }));
}

#[test]
fn bad_resource_is_einval_on_set() {
    let mut t = table();
    assert_eq!(t.set(99, Rlimit::INFINITE, true), Err(-EINVAL));
}

#[test]
fn lowering_is_always_allowed() {
    let mut t = table();
    let l = Rlimit { cur: 1000, max: 2000 };
    assert_eq!(t.set(RLIMIT_AS, l, false), Ok(()));
    assert_eq!(t.get(RLIMIT_AS), Ok(l));
    // lowering again, soft and hard
    let l2 = Rlimit { cur: 10, max: 20 };
    assert_eq!(t.set(RLIMIT_AS, l2, false), Ok(()));
    assert_eq!(t.get(RLIMIT_AS), Ok(l2));
}

#[test]
fn unprivileged_cannot_raise_the_hard_limit() {
    let mut t = table();
    t.set(RLIMIT_AS, Rlimit { cur: 10, max: 20 }, false).unwrap();
    assert_eq!(t.set(RLIMIT_AS, Rlimit { cur: 10, max: 21 }, false), Err(-EPERM));
    assert_eq!(t.set(RLIMIT_AS, Rlimit { cur: 10, max: RLIM_INFINITY }, false), Err(-EPERM));
    assert_eq!(t.get(RLIMIT_AS), Ok(Rlimit { cur: 10, max: 20 }));
}

#[test]
fn unprivileged_may_raise_the_soft_limit_up_to_the_hard_one() {
    let mut t = table();
    t.set(RLIMIT_AS, Rlimit { cur: 10, max: 20 }, false).unwrap();
    assert_eq!(t.set(RLIMIT_AS, Rlimit { cur: 20, max: 20 }, false), Ok(()));
    assert_eq!(t.get(RLIMIT_AS), Ok(Rlimit { cur: 20, max: 20 }));
}

#[test]
fn privileged_may_raise_the_hard_limit() {
    let mut t = table();
    t.set(RLIMIT_AS, Rlimit { cur: 10, max: 20 }, false).unwrap();
    assert_eq!(t.set(RLIMIT_AS, Rlimit::INFINITE, true), Ok(()));
    assert_eq!(t.get(RLIMIT_AS), Ok(Rlimit::INFINITE));
}

#[test]
fn nofile_cannot_pass_the_descriptor_table_even_for_root() {
    let mut t = table();
    assert_eq!(
        t.set(RLIMIT_NOFILE, Rlimit { cur: 64, max: 65 }, true),
        Err(-EPERM)
    );
    assert_eq!(
        t.set(RLIMIT_NOFILE, Rlimit { cur: 64, max: RLIM_INFINITY }, true),
        Err(-EPERM)
    );
    assert_eq!(t.get(RLIMIT_NOFILE), Ok(Rlimit { cur: 64, max: 64 }));
}

#[test]
fn nofile_can_be_lowered() {
    let mut t = table();
    assert_eq!(t.set(RLIMIT_NOFILE, Rlimit { cur: 16, max: 32 }, false), Ok(()));
    assert_eq!(t.get(RLIMIT_NOFILE), Ok(Rlimit { cur: 16, max: 32 }));
}

#[test]
fn tables_are_independent_copies() {
    let a = table();
    let mut b = a; // what fork does: a copy
    b.set(RLIMIT_CORE, Rlimit { cur: 1, max: 1 }, true).unwrap();
    assert_eq!(a.get(RLIMIT_CORE), Ok(Rlimit { cur: 0, max: RLIM_INFINITY }));
    assert_eq!(b.get(RLIMIT_CORE), Ok(Rlimit { cur: 1, max: 1 }));
}
