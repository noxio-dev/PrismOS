// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Signals (phase 2.5, basic; phase 3.2: the POSIX mask/disposition half a
// libc needs).  Per-process state, pending bits, delivery at the syscall
// boundary and a sigreturn trampoline.
//
// Model:
//
//   * a process installs a handler with `signal(sig, handler)` or the full
//     `sigaction(sig, act, oldact)` (phase 3.2): handler, flags, restorer
//     and a per-handler mask.  SIGKILL (9) can never be caught or blocked,
//     `execve` resets every *caught* handler to the default (SIG_IGN
//     survives, POSIX) and `fork` keeps the lot -- dispositions *and* the
//     blocked mask;
//   * `kill(pid, sig)` raises a signal: SIGKILL/SIGTERM/SIGINT terminate the
//     target at once (or the caller itself, for a self-kill); a signal with
//     a handler becomes pending instead and is delivered just before the
//     process returns to ring 3 -- which happens on every syscall return, so
//     a program making calls sees its handlers run "between instructions".
//     SIGCHLD is raised in a parent when a child dies (if it listens);
//   * delivery pushes the *whole* register frame (IsrFrame) on the user stack
//     below a 128-byte red-zone gap, prefixed with the interrupted signal
//     mask (phase 3.2), then enters the handler like a call: rdi = signal
//     number, return address = the restorer the action installed
//     (SA_RESTORER -- a libc's own code) or the `sigreturn` trampoline page
//     (vmm::USER_SIGCODE, mapped in every address space).  When the handler
//     returns, the restorer / trampoline makes the `sigreturn` syscall and
//     the kernel restores the saved frame verbatim -- rax included, so the
//     interrupted code still sees the syscall return value it was waiting
//     for -- and the saved mask with it;
//   * while a handler runs its signal is blocked together with the action's
//     sa_mask (unless SA_NODEFER; auto-mask before 3.2 was just this with an
//     empty sa_mask): a second occurrence stays pending and is delivered
//     after the return.  SA_RESETHAND restores the default disposition at
//     delivery.  Standard signals coalesce -- one bit each;
//   * `sigprocmask(how, set, oldset)` (phase 3.2) exchanges the 8-byte
//     blocked mask: SIG_BLOCK / SIG_UNBLOCK / SIG_SETMASK, SIGKILL's bit
//     dropped from whatever is installed.  The mask survives `fork` and
//     `execve` (POSIX); pending bits never do.
//
// Not here: stop/continue (SIGSTOP / SIGCONT), SA_RESTART (a wait or sleep
// interrupted by a delivery is not restarted), alternate stacks (SA_ONSTACK
// is refused), siginfo delivery (SA_SIGINFO is refused -- the handler would
// read the info/ucontext pointers we do not build), real-time queued
// signals, sigpending, and fault-signal delivery (a #PF still kills the
// program even when a SIGSEGV handler exists: resuming the faulting
// instruction would just fault again).
//
// Syscalls (see user/syscall.rs): 10 kill(pid, sig), 11 signal(sig, handler),
// 12 sigreturn(), 38 sigaction(sig, act, oldact), 39 sigprocmask(how, set,
// oldset) (phase 3.2).

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
pub const SIGPIPE: u64 = 13;
pub const SIGTERM: u64 = 15;
pub const SIGCHLD: u64 = 17;

/// `signal()` / `sigaction` dispositions beyond a plain handler (phase 3.1):
/// 0 is the default action, 1 is SIG_IGN -- "ignore".  The value is the
/// classic `((void *)1)` of every unix, and what lets a program opt out of
/// the default SIGPIPE death when writing to a readerless pipe.
pub const SIG_IGN: u64 = 1;

/// `sigaction` flags understood here (the Linux x86-64 values, which are
/// what a mlibc port passes through).  SA_RESTORER is the one that matters:
/// the caller supplies the return address its handler will `ret` into (the
/// restorer that makes the `sigreturn` syscall); without it the kernel
/// pushes its own trampoline page.  SA_NODEFER / SA_RESETHAND are honored at
/// delivery.  SA_NOCLDSTOP / SA_NOCLDWAIT / SA_RESTART are accepted and
/// ignored (no stop/continue, no restart yet); SA_SIGINFO and SA_ONSTACK are
/// refused -- see the header.
pub const SA_NOCLDSTOP: u64 = 0x1;
pub const SA_NOCLDWAIT: u64 = 0x2;
pub const SA_SIGINFO: u64 = 0x4;
pub const SA_RESTORER: u64 = 0x0400_0000;
pub const SA_ONSTACK: u64 = 0x0800_0000;
pub const SA_RESTART: u64 = 0x1000_0000;
pub const SA_NODEFER: u64 = 0x4000_0000;
pub const SA_RESETHAND: u64 = 0x8000_0000;
const SA_KNOWN: u64 =
    SA_NOCLDSTOP | SA_NOCLDWAIT | SA_SIGINFO | SA_RESTORER | SA_ONSTACK | SA_RESTART | SA_NODEFER | SA_RESETHAND;

/// `sigprocmask` directions (the Linux values).
pub const SIG_BLOCK: u64 = 0;
pub const SIG_UNBLOCK: u64 = 1;
pub const SIG_SETMASK: u64 = 2;

const ESRCH: i64 = 3;
const EINVAL: i64 = 22;
const EPIPE: i64 = 32;

/// Signals whose default action terminates the process (SIGPIPE joined the
/// set in phase 3.1: a readerless pipe write is a death sentence by POSIX).
fn terminates_by_default(sig: u64) -> bool {
    sig == SIGKILL || sig == SIGTERM || sig == SIGINT || sig == SIGPIPE
}

/// Wait flags (the part wait() understands).
pub const WNOHANG: u64 = 1;

/// One installed disposition (phase 3.2): the handler, its flags, the
/// restorer SA_RESTORER points at, and the extra mask to wear while the
/// handler runs.  The struct `sigaction` exchanges with user space is these
/// four u64 words in this order -- the x86-64 kernel layout a mlibc port
/// copies into.
#[derive(Clone, Copy)]
pub struct SigAction {
    /// 0 = default action, 1 = SIG_IGN, otherwise a ring 3 address.
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

impl SigAction {
    pub const fn new() -> SigAction {
        SigAction { handler: 0, flags: 0, restorer: 0, mask: 0 }
    }
}

/// Per-process signal state.  `actions` are the installed dispositions;
/// `pending` / `blocked` are signal bit masks (bit n = signal n).  The
/// fields are crate-visible: process.rs walks the table to raise SIGCHLD
/// and pending bits from the exit paths.
pub struct SignalState {
    pub(crate) actions: [SigAction; NSIG],
    pub(crate) pending: u32,
    pub(crate) blocked: u32,
}

impl SignalState {
    pub const fn new() -> SignalState {
        SignalState { actions: [SigAction::new(); NSIG], pending: 0, blocked: 0 }
    }

    /// The installed handler of `sig` (0 = default, SIG_IGN = ignored) --
    /// the accessor the exit / kill paths of process.rs use.
    pub(crate) fn handler_of(&self, sig: u64) -> u64 {
        self.actions[sig as usize].handler
    }

    /// What `fork` hands to the child: the dispositions (POSIX keeps them),
    /// an empty pending set and the *blocked mask* (POSIX keeps it too --
    /// phase 3.2; the pre-3.2 code cleared it).
    pub fn fork_copy(&self) -> SignalState {
        SignalState { actions: self.actions, pending: 0, blocked: self.blocked }
    }

    /// `execve` (POSIX, phase 3.2): caught handlers go back to the default,
    /// SIG_IGN stays ignored (a daemon's opt-out survives the exec), the
    /// flags / restorer / per-handler masks drop with the image, pending is
    /// discarded -- and the blocked mask is preserved.
    pub fn exec_reset(&mut self) {
        for a in self.actions.iter_mut() {
            if a.handler != SIG_IGN {
                *a = SigAction::new();
            } else {
                a.flags = 0;
                a.restorer = 0;
                a.mask = 0;
            }
        }
        self.pending = 0;
    }
}

// ------------------------------------------------------------ syscall side

/// Run `f` on the running process's signal state (syscall context).
fn with_current<R>(f: impl FnOnce(&mut SignalState) -> R) -> Option<R> {
    process::current().map(|p| f(&mut p.signals))
}

/// `signal(sig, handler)`: install a handler (0 = default action, 1 =
/// SIG_IGN) and return the old disposition.  A handler must live in user
/// space: a kernel address there would either fault on `sysretq` or hand
/// ring 3 a path it must not have.  The phase 3.2 `sigaction` is the full
/// form; this is its plain wrapper (empty mask, no restorer: the kernel
/// trampoline page brings the handler home).
pub fn sys_signal(sig: u64, handler: u64) -> i64 {
    if sig == 0 || sig >= NSIG as u64 {
        return -EINVAL;
    }
    if sig == SIGKILL {
        return -EINVAL; // catching SIGKILL is refused, like everywhere
    }
    if handler != 0 && handler != SIG_IGN && !(USER_BASE..USER_END).contains(&handler) {
        return -EINVAL;
    }
    match with_current(|s| core::mem::replace(&mut s.actions[sig as usize].handler, handler)) {
        Some(old) => old as i64,
        None => -EFAULT,
    }
}

/// `sigaction(sig, act, oldact)` (syscall 38, phase 3.2): the full install.
/// `act` (32 bytes: handler, flags, restorer, mask) replaces the
/// disposition; `oldact` receives the previous one first.  Either pointer
/// may be NULL -- a query, or a fire-and-forget install.  Validation is
/// up front, so a rejected request leaves nothing behind.
pub fn sys_sigaction(sig: u64, act_p: u64, oldact_p: u64) -> i64 {
    if sig == 0 || sig >= NSIG as u64 || sig == SIGKILL {
        return -EINVAL;
    }
    // Read the new action first (a bad pointer changes nothing).
    let mut newact: Option<SigAction> = None;
    if act_p != 0 {
        let got: Result<SigAction, i64> = {
            let Some(cur) = process::current() else { return -EFAULT };
            let Some(aspace) = cur.aspace.as_ref() else { return -EFAULT };
            let mut buf = [0u8; 32];
            if !aspace.read(act_p, &mut buf) {
                return -EFAULT;
            }
            let mut w = [0u64; 4];
            for i in 0..4 {
                w[i] = u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap());
            }
            let a = SigAction { handler: w[0], flags: w[1], restorer: w[2], mask: w[3] };
            if a.handler != 0 && a.handler != SIG_IGN && !(USER_BASE..USER_END).contains(&a.handler) {
                return -EINVAL; // a kernel address as handler: no
            }
            if a.flags & !SA_KNOWN != 0 {
                return -EINVAL; // unknown bits: refuse rather than guess
            }
            if a.flags & (SA_SIGINFO | SA_ONSTACK) != 0 {
                return -EINVAL; // not here (yet): no siginfo, no alt stack
            }
            if a.flags & SA_RESTORER != 0 && !(USER_BASE..USER_END).contains(&a.restorer) {
                return -EINVAL; // the restorer must live in ring 3 too
            }
            Ok(a)
        };
        match got {
            Ok(a) => newact = Some(a),
            Err(e) => return e,
        }
    }
    // The old action out (first, like POSIX orders the side effects).
    if oldact_p != 0 {
        let old = match with_current(|s| s.actions[sig as usize]) {
            Some(a) => a,
            None => return -EFAULT,
        };
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(aspace) = cur.aspace.as_ref() else { return -EFAULT };
        let mut buf = [0u8; 32];
        for (i, w) in [old.handler, old.flags, old.restorer, old.mask].iter().enumerate() {
            buf[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        if !aspace.ensure_writable(oldact_p) || !aspace.write(oldact_p, &buf, false) {
            return -EFAULT;
        }
    }
    if let Some(a) = newact {
        let _ = with_current(|s| s.actions[sig as usize] = a);
    }
    0
}

/// `sigprocmask(how, set, oldset)` (syscall 39, phase 3.2): exchange the
/// 8-byte blocked mask.  `set` = NULL queries through `oldset` (POSIX: the
/// `how` argument is then ignored); otherwise SIG_BLOCK / SIG_UNBLOCK /
/// SIG_SETMASK apply it.  SIGKILL's bit is dropped from whatever is
/// installed -- it cannot be blocked, only delivered.
pub fn sys_sigprocmask(how: u64, set_p: u64, oldset_p: u64) -> i64 {
    // Read the new mask first; compute against the current one.
    let new_mask: Option<u64> = if set_p != 0 {
        let got: Result<u64, i64> = {
            let Some(cur) = process::current() else { return -EFAULT };
            let Some(aspace) = cur.aspace.as_ref() else { return -EFAULT };
            let mut buf = [0u8; 8];
            if !aspace.read(set_p, &mut buf) {
                return -EFAULT;
            }
            Ok(u64::from_le_bytes(buf))
        };
        let v = match got {
            Ok(v) => v,
            Err(e) => return e,
        };
        let old = match with_current(|s| s.blocked as u64) {
            Some(m) => m,
            None => return -EFAULT,
        };
        match how {
            SIG_BLOCK => Some(old | v),
            SIG_UNBLOCK => Some(old & !v),
            SIG_SETMASK => Some(v),
            _ => return -EINVAL,
        }
    } else {
        None
    };
    if oldset_p != 0 {
        let old = match with_current(|s| s.blocked as u64) {
            Some(m) => m,
            None => return -EFAULT,
        };
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(aspace) = cur.aspace.as_ref() else { return -EFAULT };
        if !aspace.ensure_writable(oldset_p) || !aspace.write(oldset_p, &old.to_le_bytes(), false) {
            return -EFAULT;
        }
    }
    if let Some(v) = new_mask {
        // Bits past the signals we track (and SIGKILL) never stick.
        let v = v & ((1u64 << NSIG) - 1) & !(1 << SIGKILL);
        let _ = with_current(|s| s.blocked = v as u32);
    }
    0
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
            if sig != 0 && s.handler_of(sig) != 0 {
                s.pending |= 1 << sig;
                true
            } else {
                false
            }
        }) {
            Some(true) => 0,
            Some(false) => {
                if terminates_by_default(sig) {
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
    if terminates_by_default(sig) {
        if !process::signal_target(pid, sig) {
            return -ESRCH; // it died in between: not an error worth hiding
        }
    }
    0 // everything else is ignored by default (no core dumps)
}

// ---------------------------------------------------------------- sigreturn

/// `sigreturn()`: restore what delivery pushed.At the restorer /
/// trampoline the user stack holds, from `frame.rsp` up: the interrupted
/// mask word, then the saved IsrFrame (the handler's `ret` popped the
/// return address right below them).  The frame is copied back wholesale --
/// registers, rip, rflags, rsp -- the saved mask is reinstalled minus
/// SIGKILL's bit, and the syscall itself "never returns" a value: the
/// rewritten trap frame is what `sysretq` consumes.
pub fn sys_sigreturn(frame: &mut IsrFrame) {
    let Some(cur) = process::current() else { return };
    let Some(aspace) = cur.aspace.as_ref() else { return };

    let mut buf = [0u8; 8 + core::mem::size_of::<IsrFrame>()];
    if !aspace.read(frame.rsp, &mut buf) {
        bad_sigreturn(frame);
    }
    let mut words = [0u64; 1 + 23];
    for i in 0..words.len() {
        words[i] = u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap());
    }
    let saved_mask = words[0] as u32;
    let frame_words: [u64; 23] = words[1..24].try_into().unwrap();
    let mut saved = frame_from_words(&frame_words);
    if !sanitize_user_frame(&mut saved) {
        // The handler smashed its own context (or crafted one): over.
        bad_sigreturn(frame);
    }
    with_current(|s| s.blocked = saved_mask & !(1 << SIGKILL));
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

/// The write side of a readerless pipe met the POSIX SIGPIPE rule (phase
/// 3.1, called from user/fd.rs): with the default disposition the process
/// dies by signal 13 and this never returns; with SIG_IGN -- or a handler
/// installed, in which case the signal goes pending for the syscall return
/// -- the caller reports -EPIPE and carries on.
pub fn pipe_sigpipe() -> i64 {
    let handler = with_current(|s| s.handler_of(SIGPIPE));
    match handler {
        Some(SIG_IGN) => -EPIPE,
        Some(h) if h != 0 => {
            let _ = with_current(|s| s.pending |= 1 << SIGPIPE);
            -EPIPE
        }
        _ => process::exit_current(process::Outcome::Signaled(SIGPIPE)),
    }
}

/// Deliver one pending signal to the running process by rewriting its trap
/// frame: the current user context is pushed on the user stack (below the
/// red zone) with the interrupted mask in front of it, the handler becomes
/// the new return target, and the return address planted for it is the
/// action's restorer (SA_RESTORER) or the `sigreturn` trampoline.  Called
/// from the syscall dispatch, just before `sysretq`.
/// Signal frame block written on the user stack: the interrupted mask
/// (one u64 slot, sigset_t-sized like Linux) followed by the saved
/// IsrFrame.  sigreturn reads this exact layout back -- keep both in sync.
pub fn deliver(frame: &mut IsrFrame) {
    // Pick a signal: lowest number first, handlers only (SIG_IGN counts as
    // "installed" -- its pending bit is simply dropped), nothing blocked.
    let pick = with_current(|s| {
        let mut pick = None;
        for sig in 1..NSIG as u64 {
            let bit = 1u32 << sig;
            if s.pending & bit == 0 || s.blocked & bit != 0 {
                continue;
            }
            let a = s.actions[sig as usize];
            if a.handler == SIG_IGN {
                s.pending &= !bit; // ignored: consume and move on
                continue;
            }
            if a.handler != 0 {
                pick = Some((sig, a, bit));
                break;
            }
        }
        pick
    });
    let Some(Some((sig, action, bit))) = pick else { return };
    let Some(cur) = process::current() else { return };
    let Some(aspace) = cur.aspace.as_ref() else { return };
    // The interrupted mask, saved on the stack and restored by sigreturn.
    let saved_mask = cur.signals.blocked;

    // Where the handler returns into: the libc restorer (SA_RESTORER) or
    // the kernel trampoline page.
    let ret_addr = if action.flags & SA_RESTORER != 0 { action.restorer } else { USER_SIGCODE };

    // Build the handler's stack, below the interrupted one.  The saved
    // block is one mask word (8 bytes, what sigreturn reads as the
    // sigset_t) followed by the IsrFrame, 16-byte aligned; the handler
    // entry rsp ends 8 below it, so rsp % 16 == 8 at entry -- exactly the
    // state after a `call`.  128 = red zone, below the interrupted rsp.
    let block = 8 + core::mem::size_of::<IsrFrame>() as u64;
    let Some(frame_sp) = frame.rsp.checked_sub(128 + block) else { return };
    let frame_sp = frame_sp & !0xF;
    let sp = frame_sp - 8;

    // The pages we touch may be copy-on-write after a fork: resolve EVERY
    // page of [sp, frame_sp + block) first, then write (a failure leaves
    // the signal pending, not fatal -- unless the stack itself is gone,
    // which ends the process).  The region can straddle up to three pages,
    // so the loop covers the middle page too, not just the two ends.
    let lo = sp;
    let hi = frame_sp + block - 1;
    let mut ok = true;
    let mut page = lo & !0xFFF;
    while page <= hi {
        ok &= aspace.ensure_writable(page);
        page += 0x1000;
    }

    // The mask goes out as one u64 word (the fix: a u32 mask into an
    // 8-byte slot, no more copy_from_slice length mismatch), then the
    // frame through the same layout-safe serialization sigreturn decodes
    // on the way back -- no repr(C) assumption on either side.
    let mut bytes = [0u8; 8 + core::mem::size_of::<IsrFrame>()];
    bytes[0..8].copy_from_slice(&u64::from(saved_mask).to_le_bytes());
    bytes[8..].copy_from_slice(&frame_bytes(frame));
    ok &= aspace.write(frame_sp, &bytes, false);
    let ret_slot = ret_addr.to_le_bytes();
    ok &= aspace.write(sp, &ret_slot, false);
    if !ok {
        process::exit_current(process::Outcome::Fault {
            vector: 14,
            err: 0x6, // user-mode write, not present
            rip: frame.rip,
            cr2: sp,
        });
    }

    // Consume the pending bit, run the POSIX mask rules: the interrupted
    // mask | sa_mask | the signal itself (unless SA_NODEFER), SIGKILL
    // never blocked.  SA_RESETHAND puts the default disposition back.
    with_current(|s| {
        s.pending &= !bit;
        if action.flags & SA_RESETHAND != 0 {
            s.actions[sig as usize] = SigAction::new();
        }
        let mut m = saved_mask | action.mask as u32 | bit;
        if action.flags & SA_NODEFER != 0 {
            m &= !bit;
        }
        m &= !(1 << SIGKILL);
        s.blocked = m;
    });

    // Enter the handler like a call: rdi = signal, rax = 0 (clobbered by
    // design), return address = restorer / sigreturn trampoline.
    frame.rdi = sig;
    frame.rax = 0;
    frame.rip = action.handler;
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
