// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Futexes (phase 3.2): the wait/wake primitive every libc's mutex, condvar
// and one-shot thread sync boils down to.  A futex is one 32-bit word in
// user memory plus the kernel's ability to park a process on it until
// another calls `FUTEX_WAKE` on the same word.
//
// Model (kept single-CPU honest):
//
//   * the waiter table is global, keyed by (address space, word address) --
//     the address space identity is the PML4's physical address (vmm's
//     `AddressSpace::id`), so two processes share futex queues exactly when
//     they share tables, which after a `fork` they do not: the child waits
//     on its own COW copy, the same rule Linux's per-mm queues follow.  A
//     process parks with at most one waiter (single-threaded userspace),
//     so entries are found by pid when the waiter itself cleans up;
//
//   * FUTEX_WAIT parks the caller when the word still equals the value it
//     quoted (-EAGAIN otherwise, the classic lost-wakeup check).  Parking
//     follows the house rule every bounded blocker here follows (the pipes,
//     the console): ~20 ms naps re-checked on every wake, at most
//     MAX_WAIT_MS in total, then -ETIMEDOUT -- nothing may wait forever,
//     because the shell is blocked while its children run.  mlibc treats a
//     spurious -ETIMEDOUT like a spurious wakeup: re-check the word, retry.
//     A `struct timespec` timeout (NULL = forever) bounds the park -- relative
//     for FUTEX_WAIT, absolute for FUTEX_WAIT_BITSET (against the uptime
//     clock, the only one we have; a CLOCK_REALTIME absolute value is
//     capped like everything else);
//
//   * FUTEX_WAKE flips up to `val` matching waiters to ready and un-parks
//     them at once (the scheduler picks them up on its next pass).  The
//     bitset pair narrows the match with val3's bits; the requeue pair
//     moves waiters from one word to another after waking `val` of them --
//     what a pthread condvar broadcast is made of.  FUTEX_CMP_REQUEUE
//     checks the word first (-EAGAIN on a mismatch), the futex-word guard
//     against the thundering herd;
//
//   * a dying process drops its waiter entry on the way out (process.rs
//     calls `on_death` from the zombify path), so a wake never counts a
//     corpse and the table never leaks.
//
// What is deliberately not here: FUTEX_FD, the PI variants (no priorities
// in user space, no robust lists), cross-process shared mappings (there is
// no MAP_SHARED file mapping yet -- the table's keying is ready for the day
// there is), and FUTEX_CLOCK_REALTIME (every clock is the uptime one).
//
// Syscall 37 (six arguments, r10 / r8 / r9 carry 4..6):
//    futex(uaddr, op, val, timeout, uaddr2, val3)

use alloc::vec::Vec;
use core::ptr::addr_of_mut;

use crate::user::process;
use crate::user::usermode::{EFAULT, EINVAL};

const EAGAIN: i64 = 11;
const ENOSYS: i64 = 38;
const ETIMEDOUT: i64 = 110;

/// op numbers (the Linux values; the private / clock bits are masked off).
pub const FUTEX_WAIT: u64 = 0;
pub const FUTEX_WAKE: u64 = 1;
pub const FUTEX_REQUEUE: u64 = 3;
pub const FUTEX_CMP_REQUEUE: u64 = 4;
pub const FUTEX_WAIT_BITSET: u64 = 9;
pub const FUTEX_WAKE_BITSET: u64 = 10;
/// FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME: accepted, ignored.
const FUTEX_CMD_MASK: u64 = !(128 | 256);
/// The bitset of the plain (non-bitset) ops: every bit.
const MATCH_ANY: u32 = u32::MAX;

/// Longest a futex park runs before -ETIMEDOUT (the bounded-blocking house
/// rule; see the header).
const MAX_WAIT_MS: u64 = 10_000;
/// Nap granularity between re-checks (~2 ticks, like the pipes).
const PARK_MS: u64 = 10;

/// One parked `FUTEX_WAIT`: who (pid), where (address space + word address)
/// and with which bitset.  `woken` is what FUTEX_WAKE flips: the waiter
/// re-checks it after every nap and unwinds when it sees it.
struct Waiter {
    space: u64,
    uaddr: u64,
    pid: u64,
    bitset: u32,
    woken: bool,
}

static mut WAITERS: Vec<Waiter> = Vec::new();

// ------------------------------------------------------------- table access
//
// Single CPU: every mutation runs from a syscall (IF=0 through the entry
// stub) or a path that already masks interrupts (the exit paths), and the
// waiter never holds a reference into the table across a park (it finds
// its entry by pid again on every pass).  The cli/restore pairs below are
// the belt-and-braces the rest of the kernel wears too.

/// Append a waiter.  Out-of-memory aborts like every Vec push here (the
/// kernel heap is bounded, a futex waiter is 32 bytes).
fn push_waiter(w: Waiter) {
    let flags = crate::arch::io::cli_save();
    // SAFETY: interrupts masked; the push may reallocate, nobody else is
    // walking (single CPU).
    unsafe { (*addr_of_mut!(WAITERS)).push(w) };
    // SAFETY: flags came from cli_save on this stack.
    unsafe { crate::arch::io::restore_flags(flags) };
}

/// The `woken` flag of `pid`'s waiter, if the entry still exists.
fn own_woken(pid: u64) -> Option<bool> {
    let flags = crate::arch::io::cli_save();
    // SAFETY: read-only walk with interrupts masked.
    let r = unsafe {
        (*addr_of_mut!(WAITERS)).iter().find(|w| w.pid == pid).map(|w| w.woken)
    };
    // SAFETY: as cli_save.
    unsafe { crate::arch::io::restore_flags(flags) };
    r
}

/// Drop `pid`'s waiter (a single-threaded process parks on one word).
fn remove_waiter(pid: u64) {
    let flags = crate::arch::io::cli_save();
    // SAFETY: structure change with interrupts masked.
    unsafe {
        let t = &mut *addr_of_mut!(WAITERS);
        if let Some(i) = t.iter().position(|w| w.pid == pid) {
            t.remove(i);
        }
    }
    // SAFETY: as cli_save.
    unsafe { crate::arch::io::restore_flags(flags) };
}

/// A process left the table: drop its waiter (a dead process waits on
/// nothing, and FUTEX_WAKE must not count it).  Called from the zombify
/// path with interrupts already masked; the guard here is idempotent.
pub fn on_death(pid: u64) {
    remove_waiter(pid);
}

// ----------------------------------------------------------- user memory

/// The 32-bit futex word at `uaddr` (read through the running process's own
/// tables, like every user pointer).
fn read_word(uaddr: u64, out: &mut u32) -> bool {
    let Some(cur) = process::current() else { return false };
    let Some(aspace) = cur.aspace.as_ref() else { return false };
    let mut b = [0u8; 4];
    if !aspace.read(uaddr, &mut b) {
        return false;
    }
    *out = u32::from_le_bytes(b);
    true
}

/// A `struct timespec` at `p` into milliseconds (truncated).  Ok(None) for
/// NULL (wait forever); Err(-EFAULT) / Err(-EINVAL) for a bad pointer or a
/// nsec out of range.
fn read_timespec_ms(p: u64) -> Result<Option<u64>, i64> {
    if p == 0 {
        return Ok(None);
    }
    let Some(cur) = process::current() else { return Err(-EFAULT) };
    let Some(aspace) = cur.aspace.as_ref() else { return Err(-EFAULT) };
    let mut b = [0u8; 16];
    if !aspace.read(p, &mut b) {
        return Err(-EFAULT);
    }
    let sec = i64::from_le_bytes(b[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(b[8..16].try_into().unwrap());
    if !(0..1_000_000_000).contains(&nsec) {
        return Err(-EINVAL);
    }
    // Negative / zero values park nothing: the deadline check below fires
    // on the first pass (-ETIMEDOUT, the caller's notion of "already past").
    let ms = sec.saturating_mul(1000).saturating_add(nsec / 1_000_000);
    Ok(Some(ms.max(0) as u64))
}

// -------------------------------------------------------------- the syscall

/// `futex(uaddr, op, val, timeout, uaddr2, val3)` (syscall 37).  See the
/// header for the model and the blocking contract.
pub fn sys_futex(a: [u64; 6]) -> i64 {
    let (uaddr, cmd, val, timeout_p, uaddr2, val3) = (a[0], a[1], a[2], a[3], a[4], a[5]);
    let op = cmd & FUTEX_CMD_MASK;
    if uaddr & 3 != 0 {
        return -EINVAL; // the word must be a 32-bit aligned one
    }
    let Some(cur) = process::current() else { return -EFAULT };
    let me = cur.pid;
    let space = match cur.aspace.as_ref() {
        Some(a) => a.id(),
        None => return -EFAULT, // a kernel thread has no user words
    };
    match op {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => futex_wait(space, me, uaddr, op, val, timeout_p, val3),
        FUTEX_WAKE => futex_wake(space, uaddr, val, MATCH_ANY),
        FUTEX_WAKE_BITSET => {
            if val3 == 0 {
                return -EINVAL; // matches nothing: refuse like Linux
            }
            futex_wake(space, uaddr, val, val3 as u32)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            futex_requeue(space, uaddr, val, timeout_p, uaddr2, val3, op == FUTEX_CMP_REQUEUE)
        }
        _ => -ENOSYS, // FUTEX_FD, the PI variants: not here
    }
}

/// The WAIT side: value check, deadline, then the bounded park loop.
fn futex_wait(space: u64, me: u64, uaddr: u64, op: u64, val: u64, timeout_p: u64, val3: u64) -> i64 {
    // The bitset variant carries its interest mask in val3.
    let bitset = if op == FUTEX_WAIT_BITSET {
        if val3 == 0 {
            return -EINVAL;
        }
        val3 as u32
    } else {
        MATCH_ANY
    };
    // The lost-wakeup check: quote the value, let the kernel judge.  We are
    // the only process running, so read-then-park cannot interleave.
    let mut word = 0u32;
    if !read_word(uaddr, &mut word) {
        return -EFAULT;
    }
    if word as u64 != val {
        return -EAGAIN;
    }
    // Deadline in uptime milliseconds: FUTEX_WAIT's timespec is relative,
    // FUTEX_WAIT_BITSET's absolute (against this clock; a realtime-epoch
    // value just reads as far future and gets capped like everything).
    let ts = match read_timespec_ms(timeout_p) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let now = crate::hw::pit::uptime_ms();
    let deadline = match ts {
        None => now + MAX_WAIT_MS, // "forever", house-capped
        Some(ms) if op == FUTEX_WAIT => now + ms.min(MAX_WAIT_MS),
        Some(abs) => now + abs.saturating_sub(now).min(MAX_WAIT_MS),
    };

    push_waiter(Waiter { space, uaddr, pid: me, bitset, woken: false });
    loop {
        match own_woken(me) {
            // Woken (or requeued then woken): the entry is gone either way.
            Some(true) => {
                remove_waiter(me);
                return 0;
            }
            // Our entry vanished (the death path never runs for the living;
            // a REQUEUE moved somebody else's): treat as a wakeup, re-check
            // the word upstream by returning 0 -- a spurious success is the
            // safe direction for every futex caller.
            None => return 0,
            Some(false) => {}
        }
        if crate::hw::pit::uptime_ms() >= deadline {
            remove_waiter(me);
            return -ETIMEDOUT;
        }
        // The bounded nap: parks us, another process (or the tick) brings
        // us back, we re-check the flag.
        process::sleep_current(PARK_MS);
    }
}

/// The WAKE side: flip matching waiters to woken, un-park them, answer how
/// many.  The word itself is not read -- Linux does not either; the value
/// check belonged to the waiters.
fn futex_wake(space: u64, uaddr: u64, count: u64, mask: u32) -> i64 {
    if count == 0 {
        return 0;
    }
    let flags = crate::arch::io::cli_save();
    let mut pids: Vec<u64> = Vec::new();
    // SAFETY: walk with interrupts masked; the pids leave the table's
    // scope before anybody can move it.
    unsafe {
        let t = &mut *addr_of_mut!(WAITERS);
        for w in t.iter_mut() {
            if pids.len() as u64 >= count {
                break;
            }
            if w.space == space && w.uaddr == uaddr && !w.woken && w.bitset & mask != 0 {
                w.woken = true;
                pids.push(w.pid);
            }
        }
    }
    // SAFETY: as cli_save.
    unsafe { crate::arch::io::restore_flags(flags) };
    let n_woken = pids.len();
    for pid in pids {
        process::wake_parked(pid);
    }
    // What Linux answers: the waiters this call woke.
    n_woken as i64
}

/// The REQUEUE pair: wake up to `wake_count` waiters on `uaddr`, move up to
/// `val2` more onto `uaddr2` (their wait continues against the new word --
/// a condvar broadcast's second half).  FUTEX_CMP_REQUEUE checks the word
/// against `val3` first (-EAGAIN on a mismatch).  Answers woken + requeued,
/// like Linux.
fn futex_requeue(
    space: u64,
    uaddr: u64,
    wake_count: u64,
    val2: u64,
    uaddr2: u64,
    val3: u64,
    cmp: bool,
) -> i64 {
    if uaddr2 & 3 != 0 {
        return -EINVAL;
    }
    if cmp {
        let mut word = 0u32;
        if !read_word(uaddr, &mut word) {
            return -EFAULT;
        }
        if word as u64 != val3 {
            return -EAGAIN;
        }
    }
    let flags = crate::arch::io::cli_save();
    let mut pids: Vec<u64> = Vec::new();
    let mut woken = 0u64;
    let mut requeued = 0u64;
    // SAFETY: walk with interrupts masked; the pids leave the table's scope
    // before anybody can move it.
    unsafe {
        let t = &mut *addr_of_mut!(WAITERS);
        for w in t.iter_mut() {
            if w.space != space || w.uaddr != uaddr || w.woken {
                continue;
            }
            if woken < wake_count {
                w.woken = true;
                woken += 1;
                pids.push(w.pid);
            } else if requeued < val2 {
                w.uaddr = uaddr2; // the wait continues against the new word
                requeued += 1;
            }
        }
    }
    // SAFETY: as cli_save.
    unsafe { crate::arch::io::restore_flags(flags) };
    for pid in pids {
        process::wake_parked(pid);
    }
    (woken + requeued) as i64
}
