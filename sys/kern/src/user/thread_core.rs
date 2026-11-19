// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Threads (phase 4.2), the pure half.
//
// A thread is a `Process` table entry that shares its group leader's address
// space (CLONE_VM) and, by default, its descriptor table (CLONE_FILES).  This
// file knows nothing about page tables, kernel stacks or user memory: it
// holds the three decisions the kernel glue in process.rs and signal.rs
// takes, so they can be tested on the host (tests/thread_host.rs):
//
//   * `parse_clone`   -- which clone(2) flag combinations the kernel serves,
//                        and what each bit asks of the glue;
//   * `thread_exit`   -- what ending one thread does to its group;
//   * `pick_signal_thread` -- which thread of a group takes a signal aimed at
//                        the process (POSIX: any thread that does not block it).
//
// The model, kept honest for one CPU:
//
//   * the table key (`Process::pid`) is the thread id; `tgid` is the pid of
//     the group leader, which is what getpid(), wait() and kill() speak;
//   * exit(2) and every fatal path (a fault, a default-action signal, the
//     watchdog) end the whole group; thread_exit(2) ends the caller only;
//   * a leader that calls thread_exit while workers live keeps its table
//     entry (the parent's wait() must not see the process die) and parks
//     until the last worker is gone, then the process exits with status 0 --
//     the POSIX pthread_exit-from-main rule.

/// clone(2) flag bits (the Linux x86-64 values, abi-bits/clone-flags.h).
pub const CLONE_VM: u64 = 0x0000_0100;
pub const CLONE_FS: u64 = 0x0000_0200;
pub const CLONE_FILES: u64 = 0x0000_0400;
pub const CLONE_SIGHAND: u64 = 0x0000_0800;
pub const CLONE_THREAD: u64 = 0x0001_0000;
pub const CLONE_SYSVSEM: u64 = 0x0004_0000;
pub const CLONE_SETTLS: u64 = 0x0008_0000;
pub const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
pub const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
pub const CLONE_DETACHED: u64 = 0x0040_0000;
pub const CLONE_UNTRACED: u64 = 0x0080_0000;
pub const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
pub const CLONE_IO: u64 = 0x8000_0000;

/// The bits a thread clone may carry besides the three it must.
const ACCEPTED: u64 = CLONE_FS
    | CLONE_FILES
    | CLONE_SYSVSEM
    | CLONE_SETTLS
    | CLONE_PARENT_SETTID
    | CLONE_CHILD_CLEARTID
    | CLONE_DETACHED
    | CLONE_UNTRACED
    | CLONE_CHILD_SETTID
    | CLONE_IO;

/// The three bits that make a clone a thread (Linux: THREAD needs SIGHAND,
/// SIGHAND needs VM).
const REQUIRED: u64 = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;

const EINVAL: i64 = 22;
const ENOSYS: i64 = 38;

/// What a validated clone asks the glue to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CloneSpec {
    /// Share the parent's descriptor table (CLONE_FILES); otherwise the
    /// child gets a copy that shares the open descriptions, like fork.
    pub share_files: bool,
    /// Install `tls` as the child's FS base (CLONE_SETTLS).
    pub set_tls: bool,
    /// Store the new tid at `ptid` in the shared memory before the child can
    /// run (CLONE_PARENT_SETTID).
    pub parent_settid: bool,
    /// Store the new tid at `ctid` (CLONE_CHILD_SETTID).
    pub child_settid: bool,
    /// Remember `ctid`: at the child's death store 0 there and wake its
    /// futex waiters -- what a join is built on (CLONE_CHILD_CLEARTID).
    pub child_cleartid: bool,
}

/// Validate a clone(2) flag word.  `-ENOSYS` for the fork-style clone (no
/// CLONE_VM / CLONE_THREAD: use fork), `-EINVAL` for a partial or unknown
/// combination.  The exit-signal byte must be 0: a thread does not signal
/// its parent when it ends.
pub fn parse_clone(flags: u64) -> Result<CloneSpec, i64> {
    if flags & (CLONE_VM | CLONE_THREAD) == 0 {
        return Err(-ENOSYS);
    }
    if flags & 0xff != 0 {
        return Err(-EINVAL);
    }
    if flags & REQUIRED != REQUIRED {
        return Err(-EINVAL);
    }
    if flags & !(REQUIRED | ACCEPTED) != 0 {
        return Err(-EINVAL);
    }
    Ok(CloneSpec {
        share_files: flags & CLONE_FILES != 0,
        set_tls: flags & CLONE_SETTLS != 0,
        parent_settid: flags & CLONE_PARENT_SETTID != 0,
        child_settid: flags & CLONE_CHILD_SETTID != 0,
        child_cleartid: flags & CLONE_CHILD_CLEARTID != 0,
    })
}

/// What ending one thread (thread_exit) does to its group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadExit {
    /// The caller was the last thread of the group: the whole process
    /// exits with status 0.
    ExitProcess,
    /// A worker ends; others (or a parked leader that is not alone) remain.
    Die,
    /// The last worker ends and the leader had already called thread_exit:
    /// the worker dies and the process exits with status 0.
    DieAndExitProcess,
    /// The leader ends while workers live: it keeps its entry and parks.
    LeaderParks,
}

/// Decide a thread_exit.  `workers_left` counts the live non-leader threads
/// *other than the caller*; `leader_exited` is true once the leader has
/// parked after its own thread_exit.
pub fn thread_exit(is_leader: bool, workers_left: usize, leader_exited: bool) -> ThreadExit {
    if is_leader {
        if workers_left == 0 {
            ThreadExit::ExitProcess
        } else {
            ThreadExit::LeaderParks
        }
    } else if workers_left == 0 && leader_exited {
        ThreadExit::DieAndExitProcess
    } else {
        ThreadExit::Die
    }
}

/// One live thread of a group, as the signal code sees it.
#[derive(Clone, Copy, Debug)]
pub struct Candidate {
    pub is_leader: bool,
    /// The thread's blocked mask (bit N = signal N).
    pub blocked: u32,
    /// A leader parked by thread_exit: it will never run user code again.
    pub parked: bool,
}

/// The thread of a group that takes a signal aimed at the process: the
/// leader when it can (the single-thread case stays as it always was), else
/// the first worker that does not block `sig`, else -- every thread blocks
/// it -- the leader (or the first candidate), where it stays pending until
/// somebody unblocks it.  `None` for an empty group.
pub fn pick_signal_thread(group: &[Candidate], sig: u64) -> Option<usize> {
    let bit = 1u32.checked_shl(sig as u32).unwrap_or(0);
    let takes = |c: &Candidate| !c.parked && c.blocked & bit == 0;
    if let Some(i) = group.iter().position(|c| c.is_leader && takes(c)) {
        return Some(i);
    }
    if let Some(i) = group.iter().position(|c| takes(c)) {
        return Some(i);
    }
    group
        .iter()
        .position(|c| c.is_leader && !c.parked)
        .or_else(|| group.iter().position(|c| !c.parked))
        .or(if group.is_empty() { None } else { Some(0) })
}
