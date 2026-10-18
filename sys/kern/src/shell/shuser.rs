// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// User-space shell commands (phase 2):
//   `user <name>...`  spawn built-in ring 3 programs and run them to
//                     completion (several at once share the CPU through
//                     cooperative round-robin -- try `user ping pong`)
//   `usertest`        run every program, check outcome and page accounting,
//                     plus the context-switch and kernel-thread scenarios
//   `ps`              list the process table
//   `kill <pid>`      terminate a process from the shell

use super::{bad_number, parse_u64, Args};
use crate::mem::pmm;
use crate::user::process::{self, State};
use crate::user::usermode::{self, Outcome, PROGRAMS};
use alloc::vec::Vec;

/// Set by the kernel-thread body of `usertest`, read after the queue
/// drained (single CPU: strictly ordered).
static mut KTHREAD_RAN: bool = false;

fn kthread_body() {
    // SAFETY: written once from the thread, read from the main context
    // after run_until_idle drained the queue.
    unsafe { KTHREAD_RAN = true };
}

pub(super) fn cmd_user(args: &mut Args) {
    // No argument: list the built-in programs.
    let Some(first) = args.next() else {
        crate::kprintln!("ring 3 programs (spawned as processes, cooperative round-robin):");
        for p in PROGRAMS.iter() {
            crate::kprintln!("  {:8} {}", p.name, p.desc);
        }
        crate::kprintln!("several at once: 'user ping pong'");
        return;
    };

    // Collect the names, spawn them all, then run the queue.
    let mut names = Vec::<&'static str>::new();
    let mut tok = Some(first);
    while let Some(t) = tok {
        match usermode::find(t) {
            Some(p) => names.push(p.name),
            None => {
                crate::kprintln!("user: no program '{}' (run 'user' to list them)", t);
                return;
            }
        }
        tok = args.next();
    }

    let mut pids = Vec::<u64>::new();
    for name in &names {
        let p = usermode::find(name).unwrap();
        match process::spawn_user(p) {
            Ok(pid) => pids.push(pid),
            Err(e) => {
                crate::kprintln!("user: cannot spawn '{}': {}", name, e);
                // Reap what was already spawned so the table stays clean.
                process::drain_and_reap();
                return;
            }
        }
    }
    crate::kprint!("user: {} process(es) spawned --", pids.len());
    for pid in &pids {
        crate::kprint!(" [{}]", pid);
    }
    crate::kprintln!();
    process::run_until_idle();

    // Outcomes (they stay in the table until the next reap).
    let mut ok = 0;
    for pid in &pids {
        let mut found = false;
        process::for_each(|p| {
            if p.pid == *pid && !found {
                found = true;
                crate::kprint!("  pid {} {:8} -- ", p.pid, p.name);
                usermode::describe(p.outcome);
                if p.state == State::Zombie {
                    ok += 1;
                }
            }
        });
    }
    crate::kprintln!("user: {}/{} terminated", ok, pids.len());
    process::drain_and_reap();
}

pub(super) fn cmd_usertest(_args: &mut Args) {
    use core::sync::atomic::Ordering;
    let mut ok = 0;
    let mut total = 0;
    let free = || pmm::stats_snapshot().free_pages.load(Ordering::Relaxed);

    // Address-space sanity: a fresh space shares the kernel half.
    total += 1;
    match crate::mem::vmm::AddressSpace::new() {
        Some(a) if a.shares_kernel() => {
            crate::kprintln!("  {:8} ok (kernel half shared)", "aspace");
            ok += 1;
        }
        _ => crate::kprintln!("  {:8} FAILED (kernel half not shared)", "aspace"),
    }

    // Every program, alone: outcome + page-leak check.
    for p in PROGRAMS.iter() {
        total += 1;
        let before = free();
        let res = process::spawn_user(p).map(|pid| {
            process::run_until_idle();
            let mut outcome = Outcome::None;
            process::for_each(|q| {
                if q.pid == pid {
                    outcome = q.outcome;
                }
            });
            outcome
        });
        let after = free();
        process::drain_and_reap();
        match res {
            Ok(o) if p.check(o) && before == after => {
                crate::kprint!("  {:8} ok -- ", p.name);
                usermode::describe(o);
                ok += 1;
            }
            Ok(o) => {
                crate::kprint!("  {:8} FAILED -- ", p.name);
                usermode::describe(o);
                if before != after {
                    crate::kprintln!("           leaked {} page(s)", before.wrapping_sub(after) as i64);
                }
            }
            Err(e) => crate::kprintln!("  {:8} FAILED -- {}", p.name, e),
        }
    }

    // Phase 2.2 core: the context switch, two processes at once.  The
    // ping-pong programs print and yield in a loop; round-robin must
    // interleave them and both must exit with their own status (0 and 3).
    total += 1;
    {
        let before = free();
        let ping = usermode::find("ping").unwrap();
        let pong = usermode::find("pong").unwrap();
        let spawned =
            process::spawn_user(ping).and_then(|a| process::spawn_user(pong).map(|b| (a, b)));
        let res = spawned.map(|(pa, pb)| {
            process::run_until_idle();
            let mut oa = Outcome::None;
            let mut ob = Outcome::None;
            process::for_each(|q| {
                if q.pid == pa {
                    oa = q.outcome;
                }
                if q.pid == pb {
                    ob = q.outcome;
                }
            });
            (oa, ob)
        });
        let after = free();
        process::drain_and_reap();
        let expect = (Outcome::Exited(0), Outcome::Exited(3));
        match res {
            Ok(r) if r == expect && before == after => {
                crate::kprintln!("  {:8} ok -- two processes interleaved, exited 0 and 3", "switch");
                ok += 1;
            }
            Ok((a, b)) => {
                crate::kprintln!("  {:8} FAILED -- ping: {:?}", "switch", a);
                crate::kprintln!("           pong: {:?}", b);
                if before != after {
                    crate::kprintln!("           leaked {} page(s)", before.wrapping_sub(after) as i64);
                }
            }
            Err(e) => crate::kprintln!("  {:8} FAILED -- {}", "switch", e),
        }
    }

    // A kernel thread: a pure ring 0 context switch, no ring 3 involved.
    total += 1;
    {
        let before = free();
        // SAFETY: single CPU, written below only after the previous drain.
        unsafe { KTHREAD_RAN = false };
        let res = process::spawn_kernel("kthread", kthread_body);
        if let Ok(pid) = res {
            process::run_until_idle();
            let mut z = false;
            process::for_each(|q| {
                if q.pid == pid && q.state == State::Zombie {
                    z = true;
                }
            });
            let after = free();
            process::drain_and_reap();
            // SAFETY: read after the queue drained, no concurrency since.
            let ran = unsafe { KTHREAD_RAN };
            if ran && z && before == after {
                crate::kprintln!("  {:8} ok -- kernel thread ran and exited", "kthread");
                ok += 1;
            } else {
                crate::kprintln!(
                    "  {:8} FAILED (ran={}, zombie={}, leaked {})",
                    "kthread",
                    ran,
                    z,
                    before.wrapping_sub(after) as i64
                );
            }
        } else {
            process::drain_and_reap();
            crate::kprintln!("  {:8} FAILED -- {}", "kthread", res.unwrap_err());
        }
    }

    if ok == total {
        crate::kprintln!("usertest: all {} passed", total);
    } else {
        crate::kprintln!("usertest: {}/{} passed, FAILURES", ok, total);
    }
}

pub(super) fn cmd_ps(_args: &mut Args) {
    let n = process::count();
    if n == 0 {
        crate::kprintln!("process table empty (the kernel itself runs as pid 0)");
        return;
    }
    crate::kprintln!("  {:>4} {:10} {:8} {}", "pid", "name", "state", "outcome");
    process::for_each(|p| {
        let state = match p.state {
            State::Ready => "ready",
            State::Running => "running",
            State::Zombie => "zombie",
        };
        crate::kprint!("  {:>4} {:10} {:8} ", p.pid, p.name, state);
        match p.outcome {
            Outcome::None => crate::kprintln!("-"),
            Outcome::Exited(c) => crate::kprintln!("exit {}", c),
            Outcome::Timeout => crate::kprintln!("timeout"),
            Outcome::Killed => crate::kprintln!("killed"),
            Outcome::Fault { vector, rip, .. } => {
                crate::kprintln!("fault {} at {:#x}", vector, rip)
            }
        };
    });
}

pub(super) fn cmd_kill(args: &mut Args) {
    let Some(tok) = args.next() else { return super::usage("kill") };
    let Some(pid) = parse_u64(tok) else { return bad_number("kill", tok) };
    if pid == 0 {
        crate::kprintln!("kill: pid 0 is the kernel itself, refusing");
        return;
    }
    if process::kill(pid) {
        crate::kprintln!("kill: pid {} terminated", pid);
        process::drain_and_reap();
    } else {
        crate::kprintln!("kill: no live process {}", pid);
    }
}
