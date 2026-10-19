// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Signals (phase 2.5, basic): per-process handlers, pending bits, delivery
// at the syscall boundary and a sigreturn trampoline.
//
// Model (kept deliberately small, like early Unix):
//
//   * a process installs a handler with `signal(sig, handler)`; SIGKILL (9)
//     can never be caught or blocked, `execve` resets every handler to the
//     default and `fork` keeps them;
//   * `kill(pid, sig)` raises a signal: SIGKILL/SIGTERM/SIGINT terminate the
//     target at once (or the caller itself, for a self-kill); a signal with
//     a handler becomes pending instead and is delivered just before the
//     process returns to ring 3 -- which happens on every syscall return, so
//     a program making calls sees its handlers run "between instructions".
//     SIGCHLD is raised in a parent when a child dies (if it listens);
//   * delivery pushes the *whole* register frame (IsrFrame) on the user stack
//     below a 128-byte red-zone gap, then enters the handler like a call:
//     rdi = signal number, return address = the `sigreturn` trampoline page
//     (vmm::USER_SIGCODE, mapped in every address space).  When the handler
//     returns, the trampoline makes the `sigreturn` syscall and the kernel
//     restores the saved frame verbatim -- rax included, so the interrupted
//     code still sees the syscall return value it was waiting for;
//   * while a handler runs the signal is blocked (auto-mask, cleared by
//     sigreturn): a second occurrence stays pending and is delivered after
//     the return.  Standard signals coalesce -- one bit each.
//
// Not here: signal masks from user space (sigprocmask), stop/continue
// (SIGSTOP / SIGCONT), SA_RESTART (a wait or sleep interrupted by a delivery
// is not restarted), alternate stacks, real-time queued signals, and
// fault-signal delivery (a #PF still kills the program even when a SIGSEGV
// handler exists: resuming the faulting instruction would just fault again).
//
// Syscalls (see user/syscall.rs): 10 kill(pid, sig), 11 signal(sig, handler),
// 12 sigreturn().

use crate::arch::intr::IsrFrame;
use crate::mem::vmm::{USER_BASE, USER_END, USER_SIGCODE};
use crate::user::process;
use crate::user::usermode::EFAULT;

/// Number of signals the state tracks (bits 1..31; 0 is not a signal).
pub const NSIG: usize = 32;

pub const SIGINT: u64 = 2;
pub const SIGILL: u64 = 4;
pub const SIGFPE: u64 = 8;
pub const SIGKILL: u64 = 9;
pub const SIGUSR1: u64 = 10;
pub const SIGSEGV: u64 = 11;
pub const SIGTERM: u64 = 15;
pub const SIGCHLD: u64 = 17;

const ESRCH: i64 = 3;
const EINVAL: i64 = 22;

/// Wait flags (the part wait() understands).
pub const WNOHANG: u64 = 1;

/// Per-process signal state.  `handlers` are ring 3 addresses (0 = default
/// action); `pending` / `blocked` are signal bit masks (bit n = signal n).
/// The fields are crate-visible: process.rs walks the table to raise
/// SIGCHLD and pending bits from the exit paths.
pub struct SignalState {
    pub(crate) handlers: [u64; NSIG],
    pub(crate) pending: u32,
    pub(crate) blocked: u32,
}

impl SignalState {
    pub const fn new() -> SignalState {
        SignalState { handlers: [0; NSIG], pending: 0, blocked: 0 }
    }

    /// What `fork` hands to the child: the handlers (POSIX keeps them), a
    /// clean pending / blocked state.
    pub fn fork_copy(&self) -> SignalState {
        SignalState { handlers: self.handlers, pending: 0, blocked: 0 }
    }

    /// `execve` starts with a clean slate (POSIX resets caught signals).
    pub fn reset(&mut self) {
        *self = SignalState::new();
    }
}

// ------------------------------------------------------------ syscall side

/// Run `f` on the running process's signal state (syscall context).
fn with_current<R>(f: impl FnOnce(&mut SignalState) -> R) -> Option<R> {
    process::current().map(|p| f(&mut p.signals))
}

/// `signal(sig, handler)`: install (0 = default) and return the old one.
/// The handler must live in user space: a kernel address there would either
/// fault on `sysretq` or hand ring 3 a path it must not have.
pub fn sys_signal(sig: u64, handler: u64) -> i64 {
    if sig == 0 || sig >= NSIG as u64 {
        return -EINVAL;
    }
    if sig == SIGKILL {
        return -EINVAL; // catching SIGKILL is refused, like everywhere
    }
    if handler != 0 && !(USER_BASE..USER_END).contains(&handler) {
        return -EINVAL;
    }
    match with_current(|s| core::mem::replace(&mut s.handlers[sig as usize], handler)) {
        Some(old) => old as i64,
        None => -EFAULT,
    }
}

/// `kill(pid, sig)`: raise a signal in `pid` (any process may signal any
/// other -- there are no users or sessions yet, only pid checks).  `sig` 0
/// is the classic existence probe.  Returns 0, or -ESRCH / -EINVAL.
pub fn sys_kill(pid: u64, sig: u64) -> i64 {
    if sig >= NSIG as u64 {
        return -EINVAL;
    }
    let me = process::getpid();
    if pid == me {
        // A self-kill with a default disposition acts right away; with a
        // handler it goes the pending route and the delivery happens at the
        // end of this very syscall.
        if sig == SIGKILL {
            process::exit_current(process::Outcome::Signaled(SIGKILL));
        }
        return match with_current(|s| {
            if sig != 0 && s.handlers[sig as usize] != 0 {
                s.pending |= 1 << sig;
                true
            } else {
                false
            }
        }) {
            Some(true) => 0,
            Some(false) => {
                if sig == SIGTERM || sig == SIGINT {
                    process::exit_current(process::Outcome::Signaled(sig));
                }
                0 // SIGCHLD and the rest are ignored by default
            }
            None => -EFAULT,
        };
    }

    // Another process: it cannot be running (single CPU, we are), so the
    // default action can be applied synchronously.
    if !process::has_live(pid) {
        return -ESRCH;
    }
    if sig == 0 {
        return 0; // just checking it exists
    }
    if process::post_signal(pid, sig) {
        return 0; // pending: delivered at its next syscall return
    }
    // Default action on the target.
    if sig == SIGKILL || sig == SIGTERM || sig == SIGINT {
        if !process::signal_target(pid, sig) {
            return -ESRCH; // it died in between: not an error worth hiding
        }
    }
    0 // everything else is ignored by default (no core dumps)
}

// ---------------------------------------------------------------- sigreturn

/// `sigreturn()`: restore the frame pushed at delivery.  At the trampoline
/// the user stack holds, from `frame.rsp` up, the saved IsrFrame (the
/// handler's `ret` popped the trampoline address right below it).  The
/// frame is copied back wholesale -- registers, rip, rflags, rsp -- the
/// delivery-time auto-mask is cleared, and the syscall itself "never
/// returns" a value: the rewritten trap frame is what `sysretq` consumes.
pub fn sys_sigreturn(frame: &mut IsrFrame) {
    let Some(cur) = process::current() else { return };
    let Some(aspace) = cur.aspace.as_ref() else { return };

    let mut buf = [0u8; core::mem::size_of::<IsrFrame>()];
    if !aspace.read(frame.rsp, &mut buf) {
        bad_sigreturn(frame);
    }
    let mut words = [0u64; 23];
    for i in 0..23 {
        words[i] = u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap());
    }
    let mut saved = frame_from_words(&words);
    if !sanitize_user_frame(&mut saved) {
        // The handler smashed its own context (or crafted one): over.
        bad_sigreturn(frame);
    }
    with_current(|s| s.blocked = 0);
    *frame = saved;
}

fn bad_sigreturn(frame: &mut IsrFrame) -> ! {
    process::exit_current(process::Outcome::Fault {
        vector: 14,
        err: 0,
        rip: frame.rip,
        cr2: frame.rsp,
    })
}

/// Keep a user-supplied frame from smuggling privileged state in (it lives
/// in plain user memory: the handler could have rewritten it).  Ring 3
/// selectors, a user-space rip/rsp, IF forced on, sane flag bits only.
fn sanitize_user_frame(f: &mut IsrFrame) -> bool {
    if f.rip < USER_BASE || f.rip >= USER_END || f.rsp < USER_BASE || f.rsp >= USER_END {
        return false;
    }
    f.cs = crate::arch::gdt::USER_CS as u64 + 3;
    f.ss = crate::arch::gdt::USER_DS as u64 + 3;
    // Keep the arithmetic / status flags the code may care about, force IF
    // (a process hiding from the tick would starve the watchdog), drop TF,
    // NT, IOPL, AC, VM and the rest.
    const KEEP: u64 = 0xCD5; // CF PF AF ZF SF DF OF
    f.rflags = (f.rflags & KEEP) | 0x202;
    true
}

// ---------------------------------------------------------------- delivery

/// Deliver one pending signal to the running process by rewriting its trap
/// frame: the current user context is pushed on the user stack (below the
/// red zone), the handler becomes the new return target, and the return
/// address planted for it is the `sigreturn` trampoline.  Called from the
/// syscall dispatch, just before `sysretq`.
pub fn deliver(frame: &mut IsrFrame) {
    // Pick a signal: lowest number first, handlers only, nothing blocked.
    let pick = with_current(|s| {
        let mut pick = None;
        for sig in 1..NSIG as u64 {
            let bit = 1u32 << sig;
            if s.pending & bit == 0 || s.blocked & bit != 0 {
                continue;
            }
            if s.handlers[sig as usize] != 0 {
                pick = Some((sig, s.handlers[sig as usize], bit));
                break;
            }
        }
        pick
    });
    let Some(Some((sig, handler, bit))) = pick else { return };
    let Some(cur) = process::current() else { return };
    let Some(aspace) = cur.aspace.as_ref() else { return };

    // Build the handler's stack, below the interrupted one.  The saved frame
    // block is 16-byte aligned; the handler entry rsp ends 8 below it, so
    // rsp % 16 == 8 at entry -- exactly the state after a `call`.
    let Some(frame_sp) = frame.rsp.checked_sub(128 + core::mem::size_of::<IsrFrame>() as u64)
    else { return };
    let frame_sp = frame_sp & !0xF;
    let sp = frame_sp - 8;

    // The pages we touch may be copy-on-write after a fork: resolve first,
    // then write (a failure leaves the signal pending, not fatal -- unless
    // the stack itself is gone, which ends the process).  The block can
    // straddle a page boundary, so both ends are resolved.
    let bytes = frame_bytes(frame);
    let mut ok = aspace.ensure_writable(sp) && aspace.ensure_writable(frame_sp + bytes.len() as u64 - 1);
    ok &= aspace.write(frame_sp, &bytes, false);
    let mut ret_slot = [0u8; 8];
    ret_slot.copy_from_slice(&USER_SIGCODE.to_le_bytes());
    ok &= aspace.write(sp, &ret_slot, false);
    if !ok {
        process::exit_current(process::Outcome::Fault {
            vector: 14,
            err: 0x6, // user-mode write, not present
            rip: frame.rip,
            cr2: sp,
        });
    }

    // Consume the pending bit and auto-block the signal until sigreturn.
    with_current(|s| {
        s.pending &= !bit;
        s.blocked |= bit;
    });

    // Enter the handler like a call: rdi = signal, rax = 0 (clobbered by
    // design), return address = sigreturn trampoline.
    frame.rdi = sig;
    frame.rax = 0;
    frame.rip = handler;
    frame.rsp = sp;
}

/// The frame as a little-endian byte array, `dummy` word included, in
/// memory order (what a plain push sequence would produce).
fn frame_bytes(f: &IsrFrame) -> [u8; core::mem::size_of::<IsrFrame>()] {
    let mut out = [0u8; core::mem::size_of::<IsrFrame>()];
    for (i, w) in frame_words(f).iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// All 23 words of the frame in memory order (dummy first): exactly the
/// struct layout, so the pushed block reads back with `frame_from_words`.
fn frame_words(f: &IsrFrame) -> [u64; 23] {
    [
        f.dummy, f.rax, f.rbx, f.rcx, f.rdx, f.rsi, f.rdi, f.rbp, f.r8, f.r9, f.r10, f.r11,
        f.r12, f.r13, f.r14, f.r15, f.vector, f.errcode, f.rip, f.cs, f.rflags, f.rsp, f.ss,
    ]
}

/// The inverse of `frame_words`: the words back into a frame.
fn frame_from_words(w: &[u64; 23]) -> IsrFrame {
    IsrFrame {
        dummy: w[0],
        rax: w[1],
        rbx: w[2],
        rcx: w[3],
        rdx: w[4],
        rsi: w[5],
        rdi: w[6],
        rbp: w[7],
        r8: w[8],
        r9: w[9],
        r10: w[10],
        r11: w[11],
        r12: w[12],
        r13: w[13],
        r14: w[14],
        r15: w[15],
        vector: w[16],
        errcode: w[17],
        rip: w[18],
        cs: w[19],
        rflags: w[20],
        rsp: w[21],
        ss: w[22],
    }
}
