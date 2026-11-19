// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of the thread decisions (sys/kern/src/user/thread_core.rs):
// which clone(2) flag words the kernel serves, what thread_exit does to its
// group, which thread takes a process-directed signal.  The module is
// compiled unchanged.
//
//   rustc --edition 2021 --test tests/thread_host.rs -o /tmp/thread_host \
//     && /tmp/thread_host --test-threads=1

#![allow(dead_code, unused_imports)]

#[path = "../sys/kern/src/user/thread_core.rs"]
mod thread_core;

use thread_core::*;

/// What mlibc's Sysdeps<Clone> passes (sysdeps/linux/generic/sysdeps.cpp).
const MLIBC: u64 = CLONE_VM
    | CLONE_FS
    | CLONE_FILES
    | CLONE_SIGHAND
    | CLONE_THREAD
    | CLONE_SYSVSEM
    | CLONE_SETTLS
    | CLONE_PARENT_SETTID;

#[test]
fn mlibc_flag_word_is_served() {
    let s = parse_clone(MLIBC).unwrap();
    assert!(s.share_files && s.set_tls && s.parent_settid);
    assert!(!s.child_settid && !s.child_cleartid);
}

#[test]
fn glibc_style_flag_word_is_served() {
    let s = parse_clone(MLIBC | CLONE_CHILD_CLEARTID | CLONE_CHILD_SETTID).unwrap();
    assert!(s.child_cleartid && s.child_settid);
}

#[test]
fn fork_style_clone_is_enosys() {
    assert_eq!(parse_clone(17), Err(-38)); // SIGCHLD alone
    assert_eq!(parse_clone(0), Err(-38));
}

#[test]
fn partial_thread_flags_are_einval() {
    // THREAD without SIGHAND, SIGHAND without VM, VM alone with SIGHAND.
    assert_eq!(parse_clone(CLONE_VM | CLONE_THREAD), Err(-22));
    assert_eq!(parse_clone(CLONE_SIGHAND | CLONE_THREAD), Err(-22));
    assert_eq!(parse_clone(CLONE_VM | CLONE_SIGHAND), Err(-22));
}

#[test]
fn unknown_and_exit_signal_bits_are_einval() {
    assert_eq!(parse_clone(MLIBC | 0x0002_0000), Err(-22)); // CLONE_NEWNS
    assert_eq!(parse_clone(MLIBC | 0x4000_0000), Err(-22)); // CLONE_NEWNET
    assert_eq!(parse_clone(MLIBC | 17), Err(-22)); // exit signal on a thread
}

#[test]
fn files_not_shared_is_a_copy() {
    let flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
    assert!(!parse_clone(flags).unwrap().share_files);
}

#[test]
fn thread_exit_table() {
    use ThreadExit::*;
    assert_eq!(thread_exit(true, 0, false), ExitProcess);
    assert_eq!(thread_exit(true, 3, false), LeaderParks);
    assert_eq!(thread_exit(false, 2, false), Die);
    assert_eq!(thread_exit(false, 2, true), Die);
    assert_eq!(thread_exit(false, 0, false), Die); // leader still runs
    assert_eq!(thread_exit(false, 0, true), DieAndExitProcess);
}

fn c(is_leader: bool, blocked: u32, parked: bool) -> Candidate {
    Candidate { is_leader, blocked, parked }
}

#[test]
fn signal_goes_to_the_leader_when_it_can_take_it() {
    let g = [c(true, 0, false), c(false, 0, false)];
    assert_eq!(pick_signal_thread(&g, 15), Some(0));
}

#[test]
fn signal_skips_a_leader_that_blocks_it() {
    let g = [c(true, 1 << 15, false), c(false, 0, false), c(false, 0, false)];
    assert_eq!(pick_signal_thread(&g, 15), Some(1));
    // a different signal still goes to the leader
    assert_eq!(pick_signal_thread(&g, 2), Some(0));
}

#[test]
fn signal_skips_a_parked_leader() {
    let g = [c(true, 0, true), c(false, 0, false)];
    assert_eq!(pick_signal_thread(&g, 15), Some(1));
}

#[test]
fn signal_blocked_everywhere_stays_with_the_leader() {
    let g = [c(true, 1 << 10, false), c(false, 1 << 10, false)];
    assert_eq!(pick_signal_thread(&g, 10), Some(0));
}

#[test]
fn signal_blocked_everywhere_with_parked_leader_uses_a_worker() {
    let g = [c(true, 0, true), c(false, 1 << 10, false)];
    assert_eq!(pick_signal_thread(&g, 10), Some(1));
}

#[test]
fn empty_group_has_no_target() {
    assert_eq!(pick_signal_thread(&[], 15), None);
}

#[test]
fn signal_zero_and_out_of_range_do_not_panic() {
    let g = [c(true, 0, false)];
    assert_eq!(pick_signal_thread(&g, 0), Some(0));
    assert_eq!(pick_signal_thread(&g, 40), Some(0));
}
