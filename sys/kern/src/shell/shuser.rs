// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// User-space shell commands (phase 2):
//   `user <name>[:prio]...`  spawn built-in ring 3 programs and run them to
//                     completion (several at once share the CPU: the timer
//                     preempts them, strict priority 0 = best .. 3, default
//                     2 -- try `user ping pong` or `user spin:3 spin:1`)
//   `usertest`        run every program, check outcome and page accounting,
//                     plus the context-switch, kernel-thread, preemption,
//                     priority and wake-up scenarios
//   `exec [-p prio] <program> [args...] [-- <program> [args...]]...`
//                     load ELF executables from the VFS (a bare name is
//                     looked up in /bin), run them as processes and report
//                     how they ended (phase 2.4)
//   `ps`              list the process table
//   `kill <pid>`      terminate a process from the shell

use super::{bad_number, parse_u64, Args};
use crate::mem::pmm;
use crate::user::process::{self, State, PRIO_DEFAULT, PRIO_LEVELS};
use crate::user::usermode::{self, Outcome, PROGRAMS};
use alloc::vec::Vec;

/// What a finished process left behind in the table.
struct Report {
    outcome: Outcome,
    preempts: u64,
    exit_seq: u64,
}

/// Spawn the given (program, priority) pairs, run the queue dry and collect
/// one report per process, in spawn order.  The table is reaped afterwards.
fn run_batch(specs: &[(&str, u8)]) -> Result<Vec<Report>, &'static str> {
    let mut pids = Vec::<u64>::new();
    for (name, prio) in specs {
        let Some(p) = usermode::find(name) else {
            process::drain_and_reap();
            return Err("no such program");
        };
        match process::spawn_user_prio(p, *prio) {
            Ok(pid) => pids.push(pid),
            Err(e) => {
                process::drain_and_reap();
                return Err(e);
            }
        }
    }
    process::run_until_idle();
    let mut out = Vec::<Report>::new();
    for pid in &pids {
        let mut r = Report { outcome: Outcome::None, preempts: 0, exit_seq: 0 };
        process::for_each(|q| {
            if q.pid == *pid {
                r = Report { outcome: q.outcome, preempts: q.preempts, exit_seq: q.exit_seq };
            }
        });
        out.push(r);
    }
    process::drain_and_reap();
    Ok(out)
}

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
        crate::kprintln!("ring 3 programs (spawned as processes, preemptive scheduler):");
        for p in PROGRAMS.iter() {
            crate::kprintln!("  {:8} {}", p.name, p.desc);
        }
        crate::kprintln!("several at once: 'user ping pong'");
        crate::kprintln!(
            "priority 0 (best) .. {}, default {}: 'user spin:3 spin:1'",
            PRIO_LEVELS - 1,
            PRIO_DEFAULT
        );
        return;
    };

    // Collect the names (with their optional ":prio"), spawn them all, then
    // run the queue.
    let mut names = Vec::<(&'static str, u8)>::new();
    let mut tok = Some(first);
    while let Some(t) = tok {
        let (pname, prio) = match t.split_once(':') {
            Some((n, p)) => match parse_u64(p) {
                Some(v) if v < PRIO_LEVELS as u64 => (n, v as u8),
                _ => {
                    crate::kprintln!("user: bad priority in '{}' (0..{})", t, PRIO_LEVELS - 1);
                    return;
                }
            },
            None => (t, PRIO_DEFAULT),
        };
        match usermode::find(pname) {
            Some(p) => names.push((p.name, prio)),
            None => {
                crate::kprintln!("user: no program '{}' (run 'user' to list them)", pname);
                return;
            }
        }
        tok = args.next();
    }

    let mut pids = Vec::<u64>::new();
    for (name, prio) in &names {
        let p = usermode::find(name).unwrap();
        match process::spawn_user_prio(p, *prio) {
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

    // Phase 2.3 scenarios: preemption, strict priority, sleep/wakeup.  Each
    // runs a batch of processes, checks the outcomes and the page accounting.
    // (name, specs, check on the reports, description of the success)
    type Check = fn(&[Report]) -> Option<&'static str>;
    let scenarios: [(&str, &[(&str, u8)], Check, &str); 3] = [
        // Two spinners that never yield: only the tick can interleave them,
        // so each one must have been preempted at least once.
        (
            "preempt",
            &[("spin", 2), ("spin", 2)],
            |r| {
                if !r.iter().all(|x| x.outcome == Outcome::Exited(0)) {
                    Some("a spinner did not exit 0")
                } else if r.iter().any(|x| x.preempts == 0) {
                    Some("a spinner was never preempted")
                } else {
                    None
                }
            },
            "two spinners interleaved by the timer tick",
        ),
        // The low priority spinner is spawned first but the good one must
        // finish first: strict priority.
        (
            "prio",
            &[("spin", 3), ("spin", 1)],
            |r| {
                if !r.iter().all(|x| x.outcome == Outcome::Exited(0)) {
                    Some("a spinner did not exit 0")
                } else if r[1].exit_seq > r[0].exit_seq {
                    Some("the low priority process finished first")
                } else {
                    None
                }
            },
            "priority 1 finished before priority 3",
        ),
        // The sleeper (best level) wakes in the middle of the spinner's run
        // (worst level) and must take the CPU back at once: it ends first
        // although the spinner still had ~70 ms to go.
        (
            "wakeup",
            &[("spin", 3), ("sleep", 0)],
            |r| {
                if !r.iter().all(|x| x.outcome == Outcome::Exited(0)) {
                    Some("a process did not exit 0")
                } else if r[1].exit_seq > r[0].exit_seq {
                    Some("the woken process did not preempt the spinner")
                } else {
                    None
                }
            },
            "woken priority 0 sleeper preempted the priority 3 spinner",
        ),
    ];
    for (name, specs, check, good) in scenarios.iter() {
        total += 1;
        let before = free();
        let res = run_batch(specs);
        let after = free();
        match res {
            Ok(r) => match check(&r) {
                None if before == after => {
                    crate::kprintln!("  {:8} ok -- {}", name, good);
                    ok += 1;
                }
                None => crate::kprintln!(
                    "  {:8} FAILED -- leaked {} page(s)",
                    name,
                    before.wrapping_sub(after) as i64
                ),
                Some(why) => {
                    crate::kprintln!("  {:8} FAILED -- {}", name, why);
                    for (i, x) in r.iter().enumerate() {
                        crate::kprint!("           #{} ({} preempts, died {}th): ", i, x.preempts, x.exit_seq);
                        usermode::describe(x.outcome);
                    }
                }
            },
            Err(e) => crate::kprintln!("  {:8} FAILED -- {}", name, e),
        }
    }

    if ok == total {
        crate::kprintln!("usertest: all {} passed", total);
    } else {
        crate::kprintln!("usertest: {}/{} passed, FAILURES", ok, total);
    }
}

/// Default environment of programs started from the shell.
const DEFAULT_ENV: [&str; 3] = ["PATH=/bin", "HOME=/", "PRISMOS=2.5"];

pub(super) fn cmd_exec(args: &mut Args) {
    use crate::user::elf;
    use core::sync::atomic::Ordering;

    // Split the command line into one argv per program ("--" separates).
    let mut prio = PRIO_DEFAULT;
    let mut cmds: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut first = true;
    while let Some(t) = args.next() {
        if first && t == "-p" {
            match args.next().and_then(parse_u64) {
                Some(v) if v < PRIO_LEVELS as u64 => prio = v as u8,
                _ => {
                    crate::kprintln!("exec: -p needs a priority 0..{}", PRIO_LEVELS - 1);
                    return;
                }
            }
            continue;
        }
        first = false;
        if t == "--" {
            if !cur.is_empty() {
                cmds.push(core::mem::take(&mut cur));
            }
        } else {
            cur.push(t);
        }
    }
    if !cur.is_empty() {
        cmds.push(cur);
    }
    if cmds.is_empty() {
        crate::kprintln!("usage: exec [-p prio] <program> [args...] [-- <program> [args...]]...");
        crate::kprintln!("       a bare name is looked up in /bin; try 'ls /bin'");
        return;
    }

    let free = || pmm::stats_snapshot().free_pages.load(Ordering::Relaxed);
    let before = free();
    let env: Vec<Vec<u8>> = DEFAULT_ENV.iter().map(|e| e.as_bytes().to_vec()).collect();
    let mut pids = Vec::<u64>::new();
    for c in &cmds {
        let prog = c[0];
        let path = if prog.contains('/') {
            crate::fs::fsys::abs(prog)
        } else {
            alloc::format!("/bin/{}", prog)
        };
        let argv: Vec<Vec<u8>> = c.iter().map(|a| a.as_bytes().to_vec()).collect();
        let spawned = elf::load_path(&path, &argv, &env)
            .map_err(|e| e.what())
            .and_then(|l| process::spawn_loaded(elf::base_name(&path), l, prio));
        match spawned {
            Ok(pid) => pids.push(pid),
            Err(e) => {
                crate::kprintln!("exec: {}: {}", path, e);
                process::drain_and_reap();
                return;
            }
        }
    }
    process::run_until_idle();
    for pid in &pids {
        process::for_each(|p| {
            if p.pid == *pid {
                crate::kprint!("exec: pid {} {} -- ", p.pid, p.name);
                usermode::describe(p.outcome);
            }
        });
    }
    process::drain_and_reap();
    let after = free();
    if before != after {
        crate::kprintln!("exec: WARNING {} page(s) not given back", before.wrapping_sub(after) as i64);
    }
}

pub(super) fn cmd_ps(_args: &mut Args) {
    let n = process::count();
    if n == 0 {
        crate::kprintln!("process table empty (the kernel itself runs as pid 0)");
        return;
    }
    crate::kprintln!(
        "  {:>4} {:>4} {:10} {:8} {:>4} {:>6} {:>8} {}",
        "pid", "ppid", "name", "state", "prio", "cpu", "preempt", "outcome"
    );
    process::for_each(|p| {
        let state = match p.state {
            State::Ready => "ready",
            State::Running => "running",
            State::Sleeping => "sleeping",
            State::Waiting => "waiting",
            State::Zombie => "zombie",
        };
        crate::kprint!(
            "  {:>4} {:>4} {:10} {:8} {:>4} {:>6} {:>8} ",
            p.pid, p.ppid, p.name, state, p.prio, p.used, p.preempts
        );
        match p.outcome {
            Outcome::None => crate::kprintln!("-"),
            Outcome::Exited(c) => crate::kprintln!("exit {}", c),
            Outcome::Timeout => crate::kprintln!("timeout"),
            Outcome::Killed => crate::kprintln!("killed"),
            Outcome::Signaled(s) => crate::kprintln!("signal {}", s),
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
