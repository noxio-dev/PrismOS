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
// Frame layout (Xorg step, phase 2.1 of docs/XORG.md): every delivery now
// builds the Linux rt frame -- not only for SA_SIGINFO handlers -- so there
// is a single format for sigreturn to read.  From the handler's entry rsp
// (rsp % 16 == 8, as after a `call`) upward:
//
//   sp            return address: the restorer (SA_RESTORER) or the
//                 trampoline page
//   uc_addr       ucontext_t, exactly the layout of mlibc's x86-64 one
//                 (abi-bits/signal.h, UC_* below) -- 968 bytes; `rdx` for
//                 an SA_SIGINFO handler, and where `rsp` points when the
//                 handler's `ret` lands in the restorer
//   info_addr     siginfo_t, 128 bytes; `rsi` for an SA_SIGINFO handler
//
// The registers sigreturn reinstalls are the ones in uc_mcontext.gregs, so
// a handler that edits them (skip an instruction, redirect rip) is honoured
// -- cs / ss / rflags are forced back to safe values.  The x87 / SSE state
// travels in `__fpregs_mem`, and the handler itself starts from a pristine
// FPU (Linux does the same).  SA_ONSTACK + `sigaltstack` move the frame to
// the alternate stack unless the program already runs on it.
//
// Not here: stop/continue (SIGSTOP / SIGCONT), SA_RESTART (a wait or sleep
// interrupted by a delivery is not restarted), real-time queued signals,
// sigpending, and fault-signal delivery (a #PF still kills the program even
// when a SIGSEGV handler exists: resuming the faulting instruction would
// just fault again).  siginfo carries signo / code / pid / uid for the
// kill family and SI_KERNEL for everything the kernel raises on its own
// (timers, the tty, SIGCHLD -- use wait4 for a child's status); the FP
// state is the 512-byte FXSAVE image, no AVX (CR4.OSXSAVE is clear).
//
// Syscalls (see user/syscall.rs): 10 kill(pid, sig), 11 signal(sig, handler),
// 12 sigreturn(), 38 sigaction(sig, act, oldact), 39 sigprocmask(how, set,
// oldset) (phase 3.2), 112 sigaltstack(ss, old_ss).

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::arch::intr::IsrFrame;
use crate::mem::vmm::{USER_BASE, USER_END, USER_SIGCODE};
use crate::user::fpu::FpuState;
use crate::user::process;
use crate::user::usermode::EFAULT;

/// Number of signals the state tracks (bits 1..31; 0 is not a signal).
pub const NSIG: usize = 32;

pub const SIGHUP: u64 = 1;
pub const SIGINT: u64 = 2;
pub const SIGILL: u64 = 4;
pub const SIGFPE: u64 = 8;
pub const SIGKILL: u64 = 9;
pub const SIGUSR1: u64 = 10;
pub const SIGSEGV: u64 = 11;
pub const SIGPIPE: u64 = 13;
pub const SIGALRM: u64 = 14;
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
/// ignored (no stop/continue, no restart yet); SA_SIGINFO (rsi / rdx carry
/// the siginfo / ucontext pointers) and SA_ONSTACK (the frame goes to the
/// `sigaltstack` area) are honoured at delivery -- see the header.
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

/// `si_code` values (the Linux ones): who raised the signal.
pub const SI_USER: i32 = 0;
pub const SI_KERNEL: i32 = 0x80;
pub const SI_TKILL: i32 = -6;

/// `sigaltstack` flags and the smallest area it accepts (Linux values;
/// `MINSIGSTKSZ` is 2048 in mlibc's x86-64 `abi-bits/signal.h` too).
pub const SS_ONSTACK: i32 = 1;
pub const SS_DISABLE: i32 = 2;
pub const MINSIGSTKSZ: u64 = 2048;

// The frame pushed at delivery (see the header).  The ucontext_t layout
// follows mlibc's x86-64 abi-bits/signal.h, field by field:
//
//   0    uc_flags      u64
//   8    uc_link       u64
//   16   uc_stack      { ss_sp u64, ss_flags i32 (+4 pad), ss_size u64 }
//   40   uc_mcontext   { gregs[23] u64, fpregs ptr u64, reserved[8] u64 }
//   296  uc_sigmask    128 bytes (1024-bit sigset_t; the first word is ours)
//   424  __fpregs_mem  512-byte FXSAVE image
//   936  __ssp         4 x u64
//   968  (end)
const UC_SIZE: usize = 968;
const UC_STACK: usize = 16;
const UC_GREGS: usize = 40;
const UC_FPREGS_PTR: usize = UC_GREGS + 23 * 8; // 224
const UC_SIGMASK: usize = 296;
const UC_FPREGS_MEM: usize = 424;
/// `siginfo_t` is 128 bytes on every Linux ABI.
const SI_SIZE: usize = 128;

// gregs indices (mlibc's REG_* order).
const REG_R8: usize = 0;
const REG_R9: usize = 1;
const REG_R10: usize = 2;
const REG_R11: usize = 3;
const REG_R12: usize = 4;
const REG_R13: usize = 5;
const REG_R14: usize = 6;
const REG_R15: usize = 7;
const REG_RDI: usize = 8;
const REG_RSI: usize = 9;
const REG_RBP: usize = 10;
const REG_RBX: usize = 11;
const REG_RDX: usize = 12;
const REG_RAX: usize = 13;
const REG_RCX: usize = 14;
const REG_RSP: usize = 15;
const REG_RIP: usize = 16;
const REG_EFL: usize = 17;
const REG_CSGSFS: usize = 18;
const REG_ERR: usize = 19;
const REG_TRAPNO: usize = 20;
const REG_OLDMASK: usize = 21;
const REG_CR2: usize = 22;

const ESRCH: i64 = 3;
const EPERM: i64 = 1;
const ENOMEM: i64 = 12;
const EINVAL: i64 = 22;
const EPIPE: i64 = 32;

/// Signals whose default action terminates the process (SIGPIPE joined the
/// set in phase 3.1: a readerless pipe write is a death sentence by POSIX;
/// SIGHUP in 3.3.4: a session leader's death reaches its foreground group
/// through it).
fn terminates_by_default(sig: u64) -> bool {
    sig == SIGHUP || sig == SIGKILL || sig == SIGTERM || sig == SIGINT || sig == SIGPIPE
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

/// Who raised a pending signal -- what `siginfo_t` reports for it.  One
/// record per signal number, overwritten by the latest raiser (standard
/// signals coalesce anyway) and reset to "the kernel did it" once delivered.
#[derive(Clone, Copy)]
pub struct SigInfoRec {
    pub code: i32,
    pub pid: u32,
    pub uid: u32,
}

impl SigInfoRec {
    /// Raised by the kernel itself: timers, the tty, the exit path.
    pub const KERNEL: SigInfoRec = SigInfoRec { code: SI_KERNEL, pid: 0, uid: 0 };

    /// `kill()`-family from the running process: its thread-group id and
    /// real uid (what Linux reports in si_pid / si_uid).
    pub fn from_current(code: i32) -> SigInfoRec {
        match process::current() {
            Some(p) => SigInfoRec { code, pid: p.tgid as u32, uid: p.cred.uid },
            None => SigInfoRec::KERNEL,
        }
    }
}

/// The `sigaltstack` area of a thread (disabled when `size` is 0, which is
/// also its initial state, like Linux).
#[derive(Clone, Copy)]
pub struct AltStack {
    pub sp: u64,
    pub size: u64,
}

impl AltStack {
    pub const fn disabled() -> AltStack {
        AltStack { sp: 0, size: 0 }
    }

    /// Whether `rsp` lies inside the area (the top counts: an empty
    /// alternate stack has rsp == sp + size), Linux's `on_sig_stack`.
    pub fn contains(&self, rsp: u64) -> bool {
        self.size != 0 && rsp > self.sp && rsp - self.sp <= self.size
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
    /// Signals raised asynchronously (the interval timer) whose disposition
    /// is the default *terminate*: acted on in process context, at the
    /// next syscall return or user-mode tick, never from the IRQ itself.
    pub(crate) dflt_pending: u32,
    /// The siginfo of each pending signal (see `SigInfoRec`).
    pub(crate) info: [SigInfoRec; NSIG],
    /// The alternate signal stack: per thread, copied by `fork`, dropped by
    /// `execve` and left disabled in a new thread (the man page's rules).
    pub(crate) altstack: AltStack,
    /// The mask `epoll_pwait` was called with, set only while the call is
    /// being interrupted by a signal: `blocked` then still holds the temporary
    /// mask so the handler can run, and `deliver` records *this* mask (not the
    /// temporary one) in the ucontext, so `sigreturn` puts the original back.
    /// `deliver` clears it on every pass; never copied by fork / exec.
    pub(crate) saved_mask: Option<u32>,
}

impl SignalState {
    pub const fn new() -> SignalState {
        SignalState {
            actions: [SigAction::new(); NSIG],
            pending: 0,
            blocked: 0,
            dflt_pending: 0,
            info: [SigInfoRec::KERNEL; NSIG],
            altstack: AltStack::disabled(),
            saved_mask: None,
        }
    }

    /// A new thread starts without an alternate stack (clone shares the
    /// address space, so the parent's area would be shared too).
    pub fn disable_altstack(&mut self) {
        self.altstack = AltStack::disabled();
    }

    /// The installed handler of `sig` (0 = default, SIG_IGN = ignored) --
    /// the accessor the exit / kill paths of process.rs use.
    pub(crate) fn handler_of(&self, sig: u64) -> u64 {
        self.actions[sig as usize].handler
    }

    /// Whether a signal raised now must be *queued* (pending bit) instead of
    /// acted on at once: a handler (or SIG_IGN) is installed, or the signal
    /// is blocked -- POSIX keeps a blocked signal pending whatever its
    /// disposition, which is what `sigtimedwait` and `signalfd` consume.  The
    /// default action of a queued one is applied by `deliver` the moment it
    /// is unblocked.  Before signalfd a blocked signal with the default
    /// disposition killed the process anyway.
    pub(crate) fn queues(&self, sig: u64) -> bool {
        self.actions[sig as usize].handler != 0 || self.blocked & (1u32 << sig) != 0
    }

    /// What `fork` hands to the child: the dispositions (POSIX keeps them),
    /// an empty pending set and the *blocked mask* (POSIX keeps it too --
    /// phase 3.2; the pre-3.2 code cleared it).
    pub fn fork_copy(&self) -> SignalState {
        SignalState {
            actions: self.actions,
            pending: 0,
            blocked: self.blocked,
            dflt_pending: 0,
            info: [SigInfoRec::KERNEL; NSIG],
            altstack: self.altstack,
            saved_mask: None,
        }
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
        self.dflt_pending = 0;
        self.info = [SigInfoRec::KERNEL; NSIG];
        self.altstack = AltStack::disabled(); // the old image's memory is gone
        self.saved_mask = None;
    }
}

// ------------------------------------------------------------ syscall side

/// Swap in a temporary blocked mask for the running process (the atomic half
/// of `epoll_pwait`) and return the previous one.  SIGKILL's bit and bits
/// past the signals tracked never stick, as in `sigprocmask`.  `None` without
/// a current process.
pub fn swap_mask(new: u64) -> Option<u32> {
    with_current(|s| {
        let old = s.blocked;
        s.blocked = (new & ((1u64 << NSIG) - 1) & !(1 << SIGKILL)) as u32;
        old
    })
}

/// The call that swapped the mask in is over without a signal: put `old` back.
pub fn restore_mask(old: u32) {
    let _ = with_current(|s| s.blocked = old);
}

/// The call that swapped the mask in was interrupted by a signal: keep the
/// temporary mask so the handler runs, and have `deliver` restore `old` when
/// the handler returns (or at once, if no handler ends up running).
pub fn restore_mask_after_delivery(old: u32) {
    let _ = with_current(|s| s.saved_mask = Some(old));
}

/// Run `f` on the running process's signal state (syscall context).
fn with_current<R>(f: impl FnOnce(&mut SignalState) -> R) -> Option<R> {
    process::current().map(|p| f(&mut p.signals))
}

/// Whether the running process has a deliverable signal pending (phase
/// 3.3): an unblocked bit that carries a handler.  The blocking read paths
/// check this after every park and answer -EINTR -- the unix rule for an
/// interruptible wait.  Pending bits only ever exist for handler-installed
/// signals (see `post_signal`), so "pending and not blocked" is exactly
/// "a handler is about to run".
pub fn interrupt_pending() -> bool {
    with_current(|s| s.pending & !s.blocked != 0).unwrap_or(false)
}

/// Whether the running process has a signal that must end a sleep (phase
/// 3.5.1, `nanosleep`): a default action that terminates it (`dflt_pending`,
/// which dies at the syscall return), or an unblocked pending signal with a
/// real handler.  Unlike `interrupt_pending`, a pending bit whose
/// disposition is SIG_IGN does not count: POSIX has an ignored signal leave
/// a sleep alone, and `post_signal` flips the sleeper to Ready for it all
/// the same, so the caller must be able to tell and go back to sleep.
pub fn sleep_should_end() -> bool {
    with_current(|s| {
        if s.dflt_pending & !s.blocked != 0 {
            return true;
        }
        let live = s.pending & !s.blocked;
        (1..NSIG).any(|sig| live & (1u32 << sig) != 0 && s.actions[sig].handler != SIG_IGN)
    })
    .unwrap_or(false)
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
        Some(old) => {
            propagate_action(sig);
            old as i64
        }
        None => -EFAULT,
    }
}

/// Dispositions belong to the process, not to the thread (phase 4.2): after
/// one thread installs one, its siblings get the same -- the effect of the
/// shared handler table CLONE_SIGHAND names.  The pending set and the
/// blocked mask stay each thread's own.
fn propagate_action(sig: u64) {
    let Some(act) = with_current(|s| s.actions[sig as usize]) else { return };
    process::for_siblings(|q| q.signals.actions[sig as usize] = act);
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
        propagate_action(sig);
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

/// `sigaltstack(ss, old_ss)` (syscall 112): install and/or query the
/// alternate signal stack of the calling thread.  `stack_t` is 24 bytes:
/// `ss_sp` u64, `ss_flags` i32 (+ 4 bytes of padding), `ss_size` u64.  The
/// old value goes out first (a bad `old_ss` changes nothing).  Linux's
/// rules: -EPERM while running on the stack being replaced, -EINVAL for
/// flags other than 0 / SS_DISABLE (SS_ONSTACK is tolerated as 0 on input),
/// -ENOMEM below MINSIGSTKSZ.  The area must sit inside user space (Linux
/// would only fault at delivery; refusing up front is kinder).  `rsp` is
/// the caller's user stack pointer, which tells whether it is on the area.
pub fn sys_sigaltstack(ss_p: u64, old_p: u64, rsp: u64) -> i64 {
    let Some(cur) = process::current() else { return -EFAULT };
    let Some(aspace) = cur.aspace.as_ref() else { return -EFAULT };
    let alt = cur.signals.altstack;
    let on = alt.contains(rsp);

    let mut new: Option<AltStack> = None;
    if ss_p != 0 {
        let mut b = [0u8; 24];
        if !aspace.read(ss_p, &mut b) {
            return -EFAULT;
        }
        let sp = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let flags = i32::from_le_bytes(b[8..12].try_into().unwrap());
        let size = u64::from_le_bytes(b[16..24].try_into().unwrap());
        if on {
            return -EPERM; // cannot pull the stack from under a running handler
        }
        new = Some(match flags {
            SS_DISABLE => AltStack::disabled(),
            0 | SS_ONSTACK => {
                if size < MINSIGSTKSZ {
                    return -ENOMEM;
                }
                if !(USER_BASE..USER_END).contains(&sp) || size > USER_END - sp {
                    return -EINVAL;
                }
                AltStack { sp, size }
            }
            _ => return -EINVAL,
        });
    }

    if old_p != 0 {
        let flags = if alt.size == 0 {
            SS_DISABLE
        } else if on {
            SS_ONSTACK
        } else {
            0
        };
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&alt.sp.to_le_bytes());
        b[8..12].copy_from_slice(&flags.to_le_bytes());
        b[16..24].copy_from_slice(&alt.size.to_le_bytes());
        if !aspace.ensure_writable(old_p)
            || !aspace.ensure_writable(old_p + 23)
            || !aspace.write(old_p, &b, false)
        {
            return -EFAULT;
        }
    }
    if let Some(n) = new {
        cur.signals.altstack = n;
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
    // The group and broadcast spellings (phase 3.3.4): a negative `pid`
    // names the process group |pid|, -1 everybody the caller may signal.
    // The bit pattern arrives unsigned -- the syscall ABI's i64 in a u64.
    if pid > i64::MAX as u64 {
        let target = (pid as i64).unsigned_abs();
        if target == 0 {
            // kill(-1): every live process but the caller and pid 1.
            let me = process::tgid();
            let n = broadcast_but_self(me, sig);
            return if n > 0 { 0 } else { -ESRCH };
        }
        let n = signal_pgrp(target, sig);
        return if n > 0 { 0 } else { -ESRCH };
    }
    // "me" is the caller's process (phase 4.2): kill(getpid()) from any
    // thread is a self-kill.
    let me = process::tgid();
    if pid == me {
        // A self-kill with a default disposition acts right away; with a
        // handler it goes the pending route and the delivery happens at the
        // end of this very syscall.
        return self_signal(sig);
    }

    // Another process: it cannot be running (single CPU, we are), so the
    // default action can be applied synchronously.
    if !process::has_live(pid) {
        return -ESRCH;
    }
    if sig == 0 {
        return 0; // just checking it exists
    }
    if process::post_signal_from(pid, sig, SigInfoRec::from_current(SI_USER)) {
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

pub fn sys_sigtimedwait(set_p: u64, info_p: u64, ts_p: u64, sz: u64) -> i64 {
    if sz != 8 {
        return -EINVAL;
    }

    let Some(cur) = process::current() else { return -EFAULT };
    let Some(aspace) = cur.aspace.as_ref() else { return -EFAULT };

    let mut w = [0u8; 8];
    if !aspace.read(set_p, &mut w) {
        return -EFAULT;
    }

    let set64 = u64::from_le_bytes(w);
    let set = (set64 & ((1u64 << NSIG) - 1) & !(1u64 << SIGKILL)) as u32;

    if cur.signals.blocked & set != set {
        crate::kprintln!("[trace] sigtimedwait pid={} EINVAL set={:#x} blocked={:#x}", cur.pid, set, cur.signals.blocked);
        return -EINVAL;
    }

    let deadline = if ts_p == 0 {
        None
    } else {
        let mut t = [0u8; 16];
        if !aspace.read(ts_p, &mut t) {
            return -EFAULT;
        }

        let sec = u64::from_le_bytes(t[0..8].try_into().unwrap());
        let nsec = u64::from_le_bytes(t[8..16].try_into().unwrap());

        if nsec >= 1_000_000_000 {
            return -EINVAL;
        }

        Some(
            crate::hw::pit::uptime_ms()
                .saturating_add(sec.saturating_mul(1000))
                .saturating_add(nsec / 1_000_000)
        )
    };

    loop {
        let hit = cur.signals.pending & set;

        if hit != 0 {
            let sig = hit.trailing_zeros() as u64;
            cur.signals.pending &= !(1u32 << sig);
            let rec = core::mem::replace(&mut cur.signals.info[sig as usize], SigInfoRec::KERNEL);

            if info_p != 0 {
                let si = build_siginfo(sig, rec);

                if !aspace.ensure_writable(info_p)
                    || !aspace.ensure_writable(info_p + SI_SIZE as u64 - 1)
                    || !aspace.write(info_p, &si, false)
                {
                    return -EFAULT;
                }
            }

            return sig as i64;
        }

        if cur.signals.pending & !cur.signals.blocked != 0 {
            return -4; // EINTR
        }

        if let Some(d) = deadline {
            if crate::hw::pit::uptime_ms() >= d {
                return -11; // EAGAIN
            }
        }

        process::sleep_current(1);
    }
}

/// The `kill(getpid(), sig)` path, shared with the group delivery below
/// (the caller may be a member of its own target group).  A default
/// disposition acts right away (this call never returns then); a handler
/// takes the pending route; the rest is ignored.
fn self_signal(sig: u64) -> i64 {
    if sig == SIGKILL {
        process::exit_current(process::Outcome::Signaled(SIGKILL));
    }
    let rec = SigInfoRec::from_current(SI_USER);
    match with_current(|s| {
        if sig != 0 && s.queues(sig) {
            s.pending |= 1 << sig;
            s.info[sig as usize] = rec;
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
    }
}

/// Deliver a signal to one *other* live process (a group member): the
/// same ladder `sys_kill` walks -- sig 0 checks, a handler takes the
/// pending bit, the default action applies at once.  Returns true when
/// the process heard it (alive).
fn deliver_other(pid: u64, sig: u64) -> bool {
    if !process::has_live(pid) {
        return false;
    }
    if sig == 0 {
        return true;
    }
    if process::post_signal_from(pid, sig, SigInfoRec::from_current(SI_USER)) {
        return true;
    }
    if terminates_by_default(sig) && !process::signal_target(pid, sig) {
        return false; // gone between the look and the hit
    }
    true
}

/// The live pids of process group `pgrp` (phase 3.3.4): what a ^C through
/// the terminal's foreground claim reaches, and what `kill(-pgrp)` spells.
fn pgrp_members(pgrp: u64) -> alloc::vec::Vec<u64> {
    process::group_pids(pgrp)
}

/// Deliver `sig` to every live process of group `pgrp` (phase 3.3.4).  The
/// caller may itself be a member -- a self-delivery follows the
/// `kill(getpid())` rules (which never returns when the default action is
/// termination).  Returns how many processes heard it.
pub fn signal_pgrp(pgrp: u64, sig: u64) -> usize {
    let me = process::tgid();
    let mut n = 0;
    for pid in pgrp_members(pgrp) {
        if pid == me {
            n += 1;
            let _ = self_signal(sig); // may not return
        } else if deliver_other(pid, sig) {
            n += 1;
        }
    }
    n
}

/// `tgkill(tgid, tid, sig)` (syscall 108, phase 4.2): raise `sig` in ONE
/// thread -- `tid` must be a live thread of group `tgid`, the guard against
/// a recycled id.  A handler takes the pending route in that thread; a
/// terminating default ends the whole process (it is a process-wide action),
/// at once when the target is the caller's own group.
pub fn sys_tgkill(tgid: u64, tid: u64, sig: u64) -> i64 {
    const ESRCH: i64 = 3;
    if sig >= NSIG as u64 {
        return -EINVAL;
    }
    let Some(cur) = process::current() else { return -EFAULT };
    let (my_tid, my_tgid) = (cur.pid, cur.tgid);
    match process::with_pid_mut(tid, |p| p.tgid == tgid && p.state != process::State::Zombie) {
        Some(true) => {}
        _ => return -ESRCH,
    }
    if sig == 0 {
        return 0; // just checking it exists
    }
    if tid == my_tid {
        return self_signal(sig);
    }
    if process::post_signal_thread(tid, sig, SigInfoRec::from_current(SI_TKILL)) {
        return 0;
    }
    if terminates_by_default(sig) {
        if tgid == my_tgid {
            // A sibling of the caller: the caller is a member of the group
            // that dies, so it ends the process itself.
            process::exit_current(process::Outcome::Signaled(sig));
        }
        if !process::signal_target(tgid, sig) {
            return -ESRCH;
        }
    }
    0
}

/// `kill(-1)`: every live process but the caller and init (pid 1), the
/// same ladder member by member.
fn broadcast_but_self(me: u64, sig: u64) -> usize {
    process::all_pids_but(me)
        .iter()
        .filter(|&&pid| deliver_other(pid, sig))
        .count()
}

// ---------------------------------------------------------------- sigreturn

/// `sigreturn()`: restore what delivery pushed.  At the restorer /
/// trampoline `frame.rsp` points at the `ucontext_t` (the handler's `ret`
/// popped the return address right below it).  The general registers, rip,
/// rflags and rsp come from `uc_mcontext.gregs` -- the handler may have
/// edited them -- the mask from `uc_sigmask` (minus SIGKILL's bit) and the
/// x87 / SSE state from `__fpregs_mem`.  The syscall itself "never returns"
/// a value: the rewritten trap frame is what `sysretq` consumes.
pub fn sys_sigreturn(frame: &mut IsrFrame) {
    let Some(cur) = process::current() else { return };
    let Some(aspace) = cur.aspace.as_ref() else { return };

    let mut uc = [0u8; UC_SIZE];
    if !aspace.read(frame.rsp, &mut uc) {
        bad_sigreturn(frame);
    }
    let rd = |off: usize| u64::from_le_bytes(uc[off..off + 8].try_into().unwrap());
    let g = |i: usize| rd(UC_GREGS + i * 8);

    // Start from the trap frame we are in (its vector / errcode bookkeeping
    // is as good as any) and overwrite what the context carries.
    let mut saved = *frame;
    saved.r8 = g(REG_R8);
    saved.r9 = g(REG_R9);
    saved.r10 = g(REG_R10);
    saved.r11 = g(REG_R11);
    saved.r12 = g(REG_R12);
    saved.r13 = g(REG_R13);
    saved.r14 = g(REG_R14);
    saved.r15 = g(REG_R15);
    saved.rdi = g(REG_RDI);
    saved.rsi = g(REG_RSI);
    saved.rbp = g(REG_RBP);
    saved.rbx = g(REG_RBX);
    saved.rdx = g(REG_RDX);
    saved.rax = g(REG_RAX);
    saved.rcx = g(REG_RCX);
    saved.rsp = g(REG_RSP);
    saved.rip = g(REG_RIP);
    saved.rflags = g(REG_EFL);
    if !sanitize_user_frame(&mut saved) {
        // The handler smashed its own context (or crafted one): over.
        bad_sigreturn(frame);
    }
    let saved_mask = rd(UC_SIGMASK) as u32;
    with_current(|s| s.blocked = saved_mask & !(1 << SIGKILL));

    // The FP state last, once nothing can fail any more: FXRSTOR of the
    // image the handler left (its MXCSR masked by `from_user`).
    let fp: &[u8; 512] = uc[UC_FPREGS_MEM..UC_FPREGS_MEM + 512].try_into().unwrap();
    // SAFETY: the FPU is enabled for user processes; the image is sanitized.
    unsafe { Box::new(FpuState::from_user(fp)).restore() };

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
    let blocked = with_current(|s| s.blocked & (1u32 << SIGPIPE) != 0).unwrap_or(false);
    match handler {
        Some(SIG_IGN) => -EPIPE,
        Some(h) if h != 0 || blocked => {
            let _ = with_current(|s| s.pending |= 1 << SIGPIPE);
            -EPIPE
        }
        _ => process::exit_current(process::Outcome::Signaled(SIGPIPE)),
    }
}

/// Deliver one pending signal to the running process by rewriting its trap
/// frame: the Linux rt frame (return address, `ucontext_t`, `siginfo_t` --
/// see the header) is pushed on the interrupted stack below the red zone,
/// or on the alternate stack for an SA_ONSTACK action, the handler becomes
/// the new return target (rdi = signal, rsi = siginfo, rdx = ucontext) and
/// the return address planted for it is the action's restorer (SA_RESTORER)
/// or the `sigreturn` trampoline.  Called from the syscall dispatch, just
/// before `sysretq`.  sigreturn reads this exact layout back -- keep both
/// in sync.
pub fn deliver(frame: &mut IsrFrame) {
    // A default-action signal the timer raised (SIGALRM with no handler):
    // the process dies of it, like any other terminating signal.
    if let Some(Some(sig)) = with_current(|s| {
        let m = s.dflt_pending & !s.blocked;
        if m == 0 {
            return None;
        }
        let sig = m.trailing_zeros() as u64;
        s.dflt_pending &= !(1u32 << sig);
        Some(sig)
    }) {
        process::exit_current(process::Outcome::Signaled(sig));
    }
    // Pick a signal: lowest number first, nothing blocked.  SIG_IGN counts as
    // "installed" -- its pending bit is simply dropped.  A pending bit with
    // the default disposition is one that sat blocked (see `queues`) and has
    // just been unblocked: its default action applies now, a terminating one
    // ends the process, the others are discarded.
    let mut dies = None;
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
            if a.handler == 0 {
                s.pending &= !bit;
                s.info[sig as usize] = SigInfoRec::KERNEL;
                if terminates_by_default(sig) {
                    dies = Some(sig);
                    break;
                }
                continue;
            }
            pick = Some((sig, a, bit));
            break;
        }
        pick
    });
    if let Some(sig) = dies {
        process::exit_current(process::Outcome::Signaled(sig));
    }
    let Some(Some((sig, action, bit))) = pick else {
        // No handler runs: an `epoll_pwait` that was interrupted by a signal
        // that turned out to be ignored gets its own mask back right here.
        with_current(|s| {
            if let Some(m) = s.saved_mask.take() {
                s.blocked = m;
            }
        });
        return;
    };
    let Some(cur) = process::current() else { return };
    let Some(aspace) = cur.aspace.as_ref() else { return };
    // The mask the handler starts from (the temporary one, for epoll_pwait)...
    let interrupted_mask = cur.signals.blocked;
    // ...and the one saved in the ucontext, restored by sigreturn: the
    // caller's own mask when a mask-swapping call was interrupted.
    let saved_mask = cur.signals.saved_mask.unwrap_or(interrupted_mask);
    let alt = cur.signals.altstack;
    let rec = cur.signals.info[sig as usize];

    // Where the handler returns into: the libc restorer (SA_RESTORER) or
    // the kernel trampoline page.
    let ret_addr = if action.flags & SA_RESTORER != 0 { action.restorer } else { USER_SIGCODE };

    // Which stack: the alternate one when the action asks for it and the
    // program is not already running on it, else the interrupted stack
    // below the 128-byte red zone.
    let on_alt = alt.contains(frame.rsp);
    let base = if action.flags & SA_ONSTACK != 0 && alt.size != 0 && !on_alt {
        alt.sp + alt.size
    } else {
        let Some(b) = frame.rsp.checked_sub(128) else { return };
        b
    };

    // Lay the frame out downward from `base`: siginfo, then the ucontext,
    // then the return address, so that the handler's entry rsp is 8 mod 16
    // (the state right after a `call`) and the ucontext 16-aligned.
    let Some(info_addr) = base.checked_sub(SI_SIZE as u64).map(|a| a & !0xF) else { return };
    let Some(uc_addr) = info_addr.checked_sub(UC_SIZE as u64).map(|a| a & !0xF) else { return };
    let Some(sp) = uc_addr.checked_sub(8) else { return };
    let end = info_addr + SI_SIZE as u64;

    // The live x87 / SSE registers still hold the interrupted program's
    // state (the kernel is soft-float): snapshot them for the frame.
    // On the heap, not the stack: FXSAVE / FXRSTOR #GP on an address that is
    // not 16-byte aligned, and the syscall entry path does not hand us the
    // ABI-aligned kernel stack a `repr(align(16))` local would rely on.
    let mut fp = Box::new(FpuState::new());
    // SAFETY: the FPU is enabled for user processes.
    unsafe { fp.save() };

    // Build the whole block in one buffer, [sp, end), padding included.
    let mut buf: Vec<u8> = Vec::new();
    buf.resize((end - sp) as usize, 0);
    let put64 = |buf: &mut Vec<u8>, addr: u64, v: u64| {
        let o = (addr - sp) as usize;
        buf[o..o + 8].copy_from_slice(&v.to_le_bytes());
    };
    put64(&mut buf, sp, ret_addr);
    let uc = |off: usize| uc_addr + off as u64;
    // uc_stack: what the program has configured, and whether it is on it.
    let st_flags: i32 = if alt.size == 0 {
        SS_DISABLE
    } else if on_alt {
        SS_ONSTACK
    } else {
        0
    };
    put64(&mut buf, uc(UC_STACK), alt.sp);
    {
        let o = (uc(UC_STACK + 8) - sp) as usize;
        buf[o..o + 4].copy_from_slice(&st_flags.to_le_bytes());
    }
    put64(&mut buf, uc(UC_STACK + 16), alt.size);
    // gregs
    let gregs: [(usize, u64); 23] = [
        (REG_R8, frame.r8),
        (REG_R9, frame.r9),
        (REG_R10, frame.r10),
        (REG_R11, frame.r11),
        (REG_R12, frame.r12),
        (REG_R13, frame.r13),
        (REG_R14, frame.r14),
        (REG_R15, frame.r15),
        (REG_RDI, frame.rdi),
        (REG_RSI, frame.rsi),
        (REG_RBP, frame.rbp),
        (REG_RBX, frame.rbx),
        (REG_RDX, frame.rdx),
        (REG_RAX, frame.rax),
        (REG_RCX, frame.rcx),
        (REG_RSP, frame.rsp),
        (REG_RIP, frame.rip),
        (REG_EFL, frame.rflags),
        (REG_CSGSFS, (frame.cs & 0xFFFF) | ((frame.ss & 0xFFFF) << 48)),
        (REG_ERR, frame.errcode),
        (REG_TRAPNO, frame.vector),
        (REG_OLDMASK, u64::from(saved_mask)),
        (REG_CR2, 0),
    ];
    for (i, v) in gregs {
        put64(&mut buf, uc(UC_GREGS + i * 8), v);
    }
    put64(&mut buf, uc(UC_FPREGS_PTR), uc(UC_FPREGS_MEM));
    put64(&mut buf, uc(UC_SIGMASK), u64::from(saved_mask));
    {
        let o = (uc(UC_FPREGS_MEM) - sp) as usize;
        buf[o..o + 512].copy_from_slice(fp.as_bytes());
    }
    // siginfo_t
    {
        let si = build_siginfo(sig, rec);
        let o = (info_addr - sp) as usize;
        buf[o..o + SI_SIZE].copy_from_slice(&si);
    }

    // The pages we touch may be copy-on-write after a fork: resolve EVERY
    // page of [sp, end) first, then write (a failure leaves the signal
    // pending, not fatal -- unless the stack itself is gone, which ends the
    // process).  The block spans several pages, so the loop covers every
    // middle page too, not just the two ends.
    let mut ok = true;
    let mut page = sp & !0xFFF;
    while page < end {
        ok &= aspace.ensure_writable(page);
        page += 0x1000;
    }
    ok &= aspace.write(sp, &buf, false);
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
        s.saved_mask = None;
        s.info[sig as usize] = SigInfoRec::KERNEL;
        if action.flags & SA_RESETHAND != 0 {
            s.actions[sig as usize] = SigAction::new();
        }
        let mut m = interrupted_mask | action.mask as u32 | bit;
        if action.flags & SA_NODEFER != 0 {
            m &= !bit;
        }
        m &= !(1 << SIGKILL);
        s.blocked = m;
    });

    // The handler starts from a pristine FPU (the saved image is in the
    // frame) and with the direction flag clear, as the ABI promises.
    // SAFETY: as above.
    unsafe { Box::new(FpuState::new()).restore() };

    // Enter the handler like a call: rdi = signal, rsi / rdx = siginfo /
    // ucontext (only an SA_SIGINFO handler looks at them), rax = 0
    // (clobbered by design), return address = restorer / trampoline.
    frame.rdi = sig;
    frame.rsi = info_addr;
    frame.rdx = uc_addr;
    frame.rax = 0;
    frame.rip = action.handler;
    frame.rsp = sp;
    frame.rflags &= !0x400; // DF
}

/// The 128-byte `siginfo_t` of signal `sig`: signo, errno 0, the code, and
/// the sender's pid / uid at the offsets Linux's x86-64 layout uses
/// (`_sifields` starts at 16: si_pid, si_uid).
fn build_siginfo(sig: u64, rec: SigInfoRec) -> [u8; SI_SIZE] {
    let mut si = [0u8; SI_SIZE];
    si[0..4].copy_from_slice(&(sig as i32).to_le_bytes());
    // si_errno (4..8) stays 0.
    si[8..12].copy_from_slice(&rec.code.to_le_bytes());
    si[16..20].copy_from_slice(&rec.pid.to_le_bytes());
    si[20..24].copy_from_slice(&rec.uid.to_le_bytes());
    si
}