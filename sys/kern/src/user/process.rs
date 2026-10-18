// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Phase 2.2 -- processes: the PCB, kernel stacks and the context switch.
//
// A process is one user program plus the kernel state needed to run it:
//
//   * the PCB (`Process`): pid, name, state, outcome, its `AddressSpace`
//     (own PML4, kernel half shared), its own kernel stack, and the saved
//     kernel context used by the switch;
//   * kernel stacks: every process gets a private 16 KiB stack (allocated
//     from the kernel heap, which lives in the shared kernel half, so it is
//     reachable whichever CR3 is loaded).  All traps coming from the process
//     land on it (TSS.RSP0 / gs:[0]), so a suspended process keeps its whole
//     interrupt/syscall frame intact and resumes exactly where it stopped;
//   * the context switch (`switch_ctx`): callee-saved registers + RSP +
//     RFLAGS, the classic cooperative switch.  The CPU-side state (CR3,
//     TSS.RSP0, current-process pointer) is re-published by whoever performs
//     the switch, *before* the asm switch, which is safe because the kernel
//     half is mapped in every address space.
//
// Scheduling (phase 2.3): preemptive, strict priority + round-robin.
//
//   * priorities: `PRIO_LEVELS` levels, 0 is the best.  The CPU always goes
//     to the best ready process; processes of the same level take turns in
//     table order.  A lower level only runs when no better process is ready
//     (strict priority: a never-yielding high priority process starves the
//     others until its watchdog budget runs out);
//   * time slice: `SLICE_TICKS` ticks (20 ms at 100 Hz).  The LAPIC timer
//     tick (pit::on_tick -> `on_tick` below) charges the running process and,
//     when its slice is used up or a strictly better process became ready,
//     switches away from inside the interrupt frame -- the whole trap frame
//     stays on the process's private kernel stack, exactly like a `yield`;
//   * sleep / wakeup: `sleep(ms)` parks the process (`State::Sleeping`); the
//     tick wakes it when its time is up, and a woken process that beats the
//     running one preempts it at once;
//   * a process also leaves the CPU with `yield` (to its own level or
//     better), ends with `exit`, or is killed by the watchdog (CPU budget in
//     ticks, charged only while it runs: sleeping is free) or by a ring 3
//     exception.
//
// The kernel is NOT preemptible: only a tick that interrupted ring 3 may
// switch.  Syscalls and exception handlers run with IF=0 (SFMASK / interrupt
// gates) and kernel threads never enable interrupts, so no kernel data
// structure needs a lock on this single CPU.  That stops being true with SMP
// or kernel preemption (phase 1.5 / later).
//
// The shell/kernel itself is the implicit PID 0 ("main" context): it runs on
// the boot stack, `run_until_idle()` drives the run queue until every
// process is dead (idling with `sti; hlt` while the only survivors sleep),
// and returns to the caller with the outcomes.
//
// Invariants (single CPU):
//   * every context switch runs with interrupts masked, and both sides'
//     saved RFLAGS have IF=0;
//   * `CURRENT` points at the running process, or is null while the kernel
//     (shell) runs;
//   * the process table's *structure* is only changed (spawn, reap, kill)
//     with interrupts masked, because the tick walks it to wake sleepers;
//     the tick itself only flips `state` fields.
//
// Not saved on a switch yet: FPU/SSE state.  The test programs and the
// soft-float kernel never touch it; fxsave/xsave is required before the ELF
// loader (phase 2.4) runs compiled code.

use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::arch::naked_asm;
use core::ptr::addr_of_mut;

use crate::arch::intr::IsrFrame;
use crate::mem::vmm::{self, AddressSpace};
use crate::user::usermode::{self, Prog, DEFAULT_TIMEOUT_TICKS};

pub use crate::user::usermode::Outcome;

/// Kernel stack size per process.  Traps from ring 3 (syscall frame 168
/// bytes, nested interrupt frames, handler frames) must fit in it.
pub const KSTACK_SIZE: usize = 16 * 1024;

/// Number of priority levels (0 = best .. PRIO_LEVELS-1 = worst).
pub const PRIO_LEVELS: u8 = 4;
/// Level of a process spawned without an explicit priority.
pub const PRIO_DEFAULT: u8 = 2;
/// Time slice in ticks (100 Hz: 20 ms).
pub const SLICE_TICKS: u32 = 2;
/// Longest single `sleep` in milliseconds: the shell is blocked while a
/// process sleeps, so an absurd request must not wedge the machine.
pub const MAX_SLEEP_MS: u64 = 10_000;

// ------------------------------------------------------------------- state

/// Life cycle of a process.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Spawned, waiting for the CPU.
    Ready,
    /// On the CPU right now.
    Running,
    /// Parked by `sleep` until `wake_at`.
    Sleeping,
    /// Terminated; resources freed by the reaper, outcome kept for the shell.
    Zombie,
}

/// Kernel context saved by the context switch: the callee-saved registers,
/// the stack pointer (pointing at the saved return address, setjmp style)
/// and RFLAGS.  Layout consumed by the naked `switch_ctx` -- keep in sync.
#[repr(C)]
pub struct SavedCtx {
    rsp: u64,    // 0
    rbx: u64,    // 8
    rbp: u64,    // 16
    r12: u64,    // 24
    r13: u64,    // 32
    r14: u64,    // 40
    r15: u64,    // 48
    rflags: u64, // 56
}

impl SavedCtx {
    const ZERO: SavedCtx = SavedCtx {
        rsp: 0,
        rbx: 0,
        rbp: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        rflags: 2, // IF clear: a new process starts with interrupts masked
    };
}

/// What a process runs.  Kernel entries are for in-kernel worker/demo
/// threads; user entries describe the initial ring 3 context.
pub enum Entry {
    User,
    Kernel(fn()),
}

/// Process control block.
pub struct Process {
    /// Sequential process id (0 belongs to the kernel itself).
    pub pid: u64,
    /// Program name (shell display, `ps`).
    pub name: &'static str,
    /// Life cycle state.
    pub state: State,
    /// Set when the process left the stage.
    pub outcome: Outcome,
    /// Own page tables (user half); kernel threads have none.
    pub aspace: Option<AddressSpace>,
    /// Private kernel stack (kernel heap, 16 KiB, 4096-aligned).
    kstack: *mut u8,
    /// Top of the kernel stack: where traps from ring 3 land.
    kstack_top: u64,
    /// Switch bookkeeping.
    saved: SavedCtx,
    /// Thread entry, taken by the trampoline on first run.
    entry: Option<Entry>,
    /// Priority level, 0 = best.
    pub prio: u8,
    /// Ticks left in the current time slice.
    slice_left: u32,
    /// CPU ticks charged so far (only while running).
    pub used: u64,
    /// Watchdog: CPU ticks the process may use in total.
    budget: u64,
    /// Tick at which a sleeping process becomes ready again.
    wake_at: u64,
    /// Times the process was given the CPU.
    pub switches: u64,
    /// Times the tick took the CPU away (slice used up or better process).
    pub preempts: u64,
    /// Order of death (1 = first to end since boot), 0 while alive.
    pub exit_seq: u64,
}

/// The saved context of the main context (the shell / boot flow): the other
/// side of every process <-> kernel switch.
static mut MAIN_SAVED: SavedCtx = SavedCtx::ZERO;

/// The process on the CPU, null while the kernel runs.
static mut CURRENT: *mut Process = core::ptr::null_mut();

/// The process table, in pid order (pushed at spawn, drained by reap).
static mut TABLE: Vec<Box<Process>> = Vec::new();
static mut NEXT_PID: u64 = 1;
/// Death counter behind `Process::exit_seq`.
static mut EXIT_SEQ: u64 = 0;

/// Next death number (interrupts masked at every call site).
fn next_exit_seq() -> u64 {
    // SAFETY: single CPU, callers run with IF=0.
    unsafe {
        EXIT_SEQ += 1;
        EXIT_SEQ
    }
}

/// Bring-up: nothing to do beyond the static initialisers (the table is
/// empty and PIDs restart at 1); the call site logs the phase line.
pub fn init() {}

// ---------------------------------------------------------------- accessors

/// The running process, or None while the kernel (shell) runs.
pub fn current() -> Option<&'static mut Process> {
    // SAFETY: CURRENT is null or a process owned by the table / the CPU.
    unsafe {
        let p = CURRENT;
        if p.is_null() {
            None
        } else {
            Some(&mut *p)
        }
    }
}

/// Pid of the running process (0 = the kernel itself).
pub fn getpid() -> u64 {
    current().map_or(0, |p| p.pid)
}

/// Iterate over the table, main context only (shell commands: `ps`).
pub fn for_each<F: FnMut(&Process)>(mut f: F) {
    // SAFETY: read-only walk from the main context with no process running
    // (the run queue is drained before the shell gets control back).
    unsafe {
        for p in &*addr_of_mut!(TABLE) {
            f(p);
        }
    }
}

/// Process count still in the table (ready or zombie, before reaping).
pub fn count() -> usize {
    // SAFETY: as `for_each`.
    unsafe { (*addr_of_mut!(TABLE)).len() }
}

// ------------------------------------------------------------------ spawn

/// Create a process, ready to be scheduled.  `timeout_ticks` is the
/// watchdog budget (0 = default 3 s).
fn spawn_common(
    name: &'static str,
    aspace: Option<AddressSpace>,
    entry: Entry,
    timeout_ticks: u64,
    prio: u8,
) -> Result<u64, &'static str> {
    if prio >= PRIO_LEVELS {
        return Err("bad priority");
    }
    let layout = Layout::from_size_align(KSTACK_SIZE, 4096).map_err(|_| "bad layout")?;
    // SAFETY: valid non-zero layout.
    let kstack = unsafe { alloc_zeroed(layout) };
    if kstack.is_null() {
        return Err("out of memory (kernel stack)");
    }
    let ktop = kstack as u64 + KSTACK_SIZE as u64;

    // The first switch lands on the trampoline through the saved return
    // address slot, with rflags = 2 (interrupts masked until ring 3).
    // SAFETY: the stack was just allocated; writing its top slot is ours.
    unsafe {
        *(ktop as *mut u64).wrapping_sub(1) = proc_trampoline as usize as u64;
    }

    let proc = Process {
        pid: 0, // assigned below, under the interrupt guard
        name,
        state: State::Ready,
        outcome: Outcome::None,
        aspace,
        kstack,
        kstack_top: ktop,
        saved: SavedCtx { rsp: ktop - 8, ..SavedCtx::ZERO },
        entry: Some(entry),
        prio,
        slice_left: SLICE_TICKS,
        used: 0,
        budget: if timeout_ticks == 0 { DEFAULT_TIMEOUT_TICKS } else { timeout_ticks },
        wake_at: 0,
        switches: 0,
        preempts: 0,
        exit_seq: 0,
    };
    // The tick walks the table to wake sleepers, and the shell calls us with
    // interrupts on: the push (which may reallocate) must not be interrupted.
    let _g = crate::arch::io::IrqGuard::new();
    // SAFETY: interrupts masked by the guard, single CPU.
    unsafe {
        let pid = NEXT_PID;
        NEXT_PID += 1;
        let mut b = Box::new(proc);
        b.pid = pid;
        (*addr_of_mut!(TABLE)).push(b);
        Ok(pid)
    }
}

/// Spawn a user program from its flat image (built-in test programs) at the
/// default priority.
pub fn spawn_user(prog: &Prog) -> Result<u64, &'static str> {
    spawn_user_prio(prog, PRIO_DEFAULT)
}

/// Spawn a user program at priority `prio` (0 = best).
pub fn spawn_user_prio(prog: &Prog, prio: u8) -> Result<u64, &'static str> {
    // On a late failure the address space is dropped here with the locals
    // (RAII: its Drop frees the user half through the direct map).
    let aspace = usermode::build_aspace(prog.image())?;
    spawn_common(prog.name, Some(aspace), Entry::User, prog.timeout, prio)
}

/// Spawn a kernel thread (runs `f` in ring 0 on its own kernel stack).
/// Kernel threads have no watchdog and are never preempted: they must
/// cooperate on their own.
pub fn spawn_kernel(name: &'static str, f: fn()) -> Result<u64, &'static str> {
    spawn_common(name, None, Entry::Kernel(f), 0, PRIO_DEFAULT)
}

// ----------------------------------------------------------- context switch

/// The context switch.  Saves the callee-saved registers, RFLAGS and RSP of
/// the *current* CPU state into `cur`, loads those of `next`, and returns
/// into `next`'s saved stack.  The `rsp` slots point at a saved return
/// address (setjmp/longjmp style); for a never-run process it is the
/// trampoline address planted at spawn time.
///
/// rdi = `cur` (saved into), rsi = `next` (restored from).
///
/// # Safety
/// Interrupts must be masked; both contexts must stay valid for the whole
/// switch (the process table / MAIN own them); `cur` and `next` must differ.
#[unsafe(naked)]
pub unsafe extern "C" fn switch_ctx(cur: *mut SavedCtx, next: *const SavedCtx) {
    naked_asm!(
        "pushfq",
        "pop rax",
        "mov [rdi + 56], rax",   // rflags of the outgoing context
        "mov [rdi + 8], rbx",
        "mov [rdi + 16], rbp",
        "mov [rdi + 24], r12",
        "mov [rdi + 32], r13",
        "mov [rdi + 40], r14",
        "mov [rdi + 48], r15",
        "mov [rdi], rsp",        // points at the return address of this call
        "mov rbx, [rsi + 8]",
        "mov rbp, [rsi + 16]",
        "mov r12, [rsi + 24]",
        "mov r13, [rsi + 32]",
        "mov r14, [rsi + 40]",
        "mov r15, [rsi + 48]",
        "push qword ptr [rsi + 56]",
        "popfq",                 // restored context had IF clear (invariant)
        "mov rsp, [rsi]",        // onto the incoming stack, at its saved ret addr
        "ret",
    );
}

/// First instruction of a freshly switched-to process: the return address
/// planted at spawn time.  Runs on the process's own kernel stack with the
/// callee-saved registers still zero; `CURRENT` and the CPU state (CR3,
/// RSP0, gs:[0]) were already published by the switcher.
#[unsafe(naked)]
unsafe extern "C" fn proc_trampoline() {
    naked_asm!(
        "call {start}",
        "ud2",                   // proc_start never returns
        start = sym proc_start,
    );
}

/// Rust side of the trampoline, running on the fresh kernel stack.
/// Never returns: a user process dives into ring 3 through `iretq_to_user`,
/// a kernel thread ends in `exit_current`.
extern "C" fn proc_start() -> ! {
    // SAFETY: CURRENT points at us, set by the switcher before the switch.
    let entry = unsafe { (*CURRENT).entry.take() };
    match entry {
        Some(Entry::User) => {
            let mut frame = usermode::initial_user_frame();
            // SAFETY: fully initialised ring 3 frame; interrupts masked, so
            // no window between the (kernel) GS hand-off and iretq.
            unsafe { usermode::iretq_to_user(&mut frame) }
        }
        Some(Entry::Kernel(f)) => {
            f();
            exit_current(Outcome::Exited(0))
        }
        None => exit_current(Outcome::Exited(0)), // defensive: re-entry of a dead process
    }
}

/// Publish the CPU-side state for `next` *before* switching to it.  Safe
/// under any of our CR3s: the kernel half (code, boot stack, heap, direct
/// map) is shared by every address space.
///
/// # Safety
/// Interrupts must be masked; `next` must be owned by the table.
unsafe fn cpu_state_for(next: *mut Process) {
    let p = &mut *next;
    CURRENT = next;
    p.state = State::Running;
    p.slice_left = SLICE_TICKS;
    p.switches += 1;
    match &p.aspace {
        Some(a) => a.activate(),
        None => vmm::activate_kernel(),
    }
    usermode::set_kstack_top(p.kstack_top);
}

/// Switch from the running process to the main (kernel/shell) context.
/// CPU state first, then the stack.  Control returns into `run_until_idle`.
///
/// # Safety
/// Interrupts masked; called from the process being suspended.
unsafe fn switch_to_main(cur: *mut SavedCtx) {
    CURRENT = core::ptr::null_mut();
    vmm::activate_kernel();
    usermode::set_kstack_top(0);
    switch_ctx(cur, addr_of_mut!(MAIN_SAVED));
}

/// Give up the CPU to a ready process of the same priority level or a better
/// one (round-robin in table order inside a level).  When nobody qualifies
/// the call returns at once: the caller keeps the CPU.  Returns, in the
/// switching case, once somebody switched back to us (the `yield` syscall
/// path).
pub fn yield_current() {
    // SAFETY: the running process exists; interrupts are masked (syscall
    // entry clears IF via SFMASK).
    unsafe {
        let cur = CURRENT;
        if cur.is_null() {
            return; // the kernel context never yields to itself
        }
        if let Some(next) = pick_ready((*cur).pid, (*cur).prio) {
            (*cur).state = State::Ready;
            cpu_state_for(next);
            switch_ctx(&mut (*cur).saved, &(*next).saved);
            // Back in our own context: the switcher republished the CPU
            // state (CURRENT, CR3, RSP0) for us before the switch.
            (*CURRENT).state = State::Running;
        }
    }
}

/// The ready process to run next, or None.  Candidates are ready processes
/// other than `after` whose level is `limit` or better (numerically <=);
/// the best level wins, and inside a level the first one in table order after
/// `after` (round-robin; `after` = 0 means "from the start of the table").
fn pick_ready(after: u64, limit: u8) -> Option<*mut Process> {
    // SAFETY: table walk with interrupts masked (main context under cli, a
    // syscall, or the tick handler); the pointer stays owned by the table.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        let n = table.len();
        if n == 0 {
            return None;
        }
        let start = table.iter().position(|p| p.pid == after).unwrap_or(n - 1);
        let mut best: Option<*mut Process> = None;
        let mut best_prio = u8::MAX;
        for k in 1..=n {
            let p = &mut *table[(start + k) % n];
            if p.pid != after
                && p.state == State::Ready
                && p.prio <= limit
                && (best.is_none() || p.prio < best_prio)
            {
                best_prio = p.prio;
                best = Some(p as *mut Process);
            }
        }
        best
    }
}

/// Park the running process for at least `ms` milliseconds (rounded up to
/// the tick, plus the partial tick in progress) and run somebody else; the
/// call returns once the process is woken and scheduled again.  `sleep(0)`
/// is a `yield`.
pub fn sleep_current(ms: u64) {
    // SAFETY: as `yield_current`: interrupts are masked in the syscall path,
    // so the tick cannot slip between the state change and the switch.
    unsafe {
        let cur = CURRENT;
        if cur.is_null() {
            return;
        }
        if ms == 0 {
            return yield_current();
        }
        let ticks = ms.min(MAX_SLEEP_MS).div_ceil(10) + 1;
        (*cur).wake_at = crate::hw::pit::ticks() + ticks;
        (*cur).state = State::Sleeping;
        match pick_ready((*cur).pid, u8::MAX) {
            Some(next) => {
                cpu_state_for(next);
                switch_ctx(&mut (*cur).saved, &(*next).saved);
            }
            None => switch_to_main(&mut (*cur).saved),
        }
        (*CURRENT).state = State::Running;
    }
}

/// `setprio`: a process may lower its own priority (raise the level number),
/// never raise it -- there are no users or permissions yet, so the safe rule
/// is that nobody can promote itself.  Returns the previous level, or a
/// negative errno.
pub fn setprio_current(new: u64) -> i64 {
    let Some(p) = current() else { return -(usermode::EFAULT) };
    if new >= PRIO_LEVELS as u64 {
        return -(usermode::EINVAL);
    }
    if (new as u8) < p.prio {
        return -(usermode::EPERM);
    }
    let old = p.prio;
    p.prio = new as u8;
    old as i64
}

/// Wake every sleeper whose time is up.  Tick handler, IF=0; only flips
/// `state`, never changes the table's structure.
fn wake_sleepers(now: u64) {
    // SAFETY: structure changes are made with interrupts masked (spawn,
    // reap, kill), so this walk cannot overlap one.
    unsafe {
        for p in (*addr_of_mut!(TABLE)).iter_mut() {
            if p.state == State::Sleeping && now >= p.wake_at {
                p.state = State::Ready;
            }
        }
    }
}

/// End the current process and go back to the main context.  Never returns
/// to the caller: the process is a zombie, the scheduler will not pick it
/// again; its kernel stack is freed by the reaper.  The address space is
/// freed here (its Drop walks the tables through the shared direct map, so
/// it works from any of our CR3s).
pub fn exit_current(outcome: Outcome) -> ! {
    // SAFETY: the running process exists; interrupts are masked on every
    // exit path (syscall SFMASK, exception entry, tick handler).
    unsafe {
        let cur = CURRENT;
        if cur.is_null() {
            // A ring 3-style exit with no process: kernel bug, but do not
            // take the kernel down for user bookkeeping.
            crate::kprintln!("\nkernel: exit with no running process, parking");
            crate::arch::intr::park();
        }
        let p = &mut *cur;
        p.outcome = outcome;
        p.state = State::Zombie;
        p.exit_seq = next_exit_seq();
        // Free the user half now; the kernel stack is still in use -- the
        // reaper frees it from the main context once we are off it.
        drop(p.aspace.take());
        // Abandon this stack; execution continues in run_until_idle.
        switch_to_main(&mut (*cur).saved);
        // Only reachable if a bug resurrected a zombie: park defensively.
        crate::arch::intr::park();
    }
}

/// Free the kernel stacks of dead processes.  Called from the main context
/// (run_until_idle / drain_and_reap), when nobody can be running on them.
fn reap() {
    // SAFETY: main context, no process on the CPU (we run on the boot
    // stack), so freeing a departed process's stack is safe.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        let mut i = 0;
        while i < table.len() {
            if table[i].state == State::Zombie {
                let p = table.remove(i);
                // SAFETY: the layout is the one the stack was allocated with.
                dealloc(p.kstack, Layout::from_size_align(KSTACK_SIZE, 4096).unwrap());
            } else {
                i += 1;
            }
        }
    }
}

/// Run the queue until every process has terminated, then return to the
/// caller (the shell).  Dead processes stay in the table with their outcome
/// until the caller reaps them (`drain_and_reap`, right after reading the
/// outcomes) -- so `ps` and the `user` command can still report them.
pub fn run_until_idle() {
    let flags = crate::arch::io::cli_save();
    // SAFETY: main context; every switch below runs with interrupts masked
    // and preserves the IF=0 invariant on both sides.
    unsafe {
        loop {
            let table = &mut *addr_of_mut!(TABLE);
            if table.is_empty() {
                break;
            }
            // Best ready process (first in table order inside its level).
            match pick_ready(0, u8::MAX) {
                Some(next) => {
                    cpu_state_for(next);
                    switch_ctx(addr_of_mut!(MAIN_SAVED), &(*next).saved);
                    // Back from the process (it slept or died).  Loop.
                }
                None => {
                    if table.iter().any(|p| p.state == State::Sleeping) {
                        // Only sleepers left: let the tick wake one up.
                        crate::arch::intr::idle_wait();
                    } else {
                        // Nobody ready, nobody sleeping, nobody running
                        // (a process is Ready before it switches away, dead
                        // ones are zombies): all done.
                        break;
                    }
                }
            }
        }
        usermode::set_kstack_top(0);
    }
    // SAFETY: flags came from cli_save on this stack.
    unsafe { crate::arch::io::restore_flags(flags) };
}

// ---------------------------------------------------------- kernel callbacks

/// Called by intr.rs for every exception raised in ring 3: end the process
/// (containment -- the kernel never panics because of user code).  The
/// caller filters NMI (vector 2) out before calling.  Never returns.
pub fn on_user_exception(frame: &mut IsrFrame) -> ! {
    exit_current(Outcome::Fault {
        vector: frame.vector,
        err: frame.errcode,
        rip: frame.rip,
        cr2: crate::arch::regs::read_cr2(),
    });
}

/// Called from the tick handler (pit::on_tick), `from_user` = the tick
/// interrupted ring 3.  The tick EOI is already sent and interrupts are
/// masked in the handler, so switching away from the interrupt frame is safe
/// (the trap frame stays on the process's kernel stack; a dying process's
/// stack is freed by the reaper).
///
///   1. wake the sleepers whose time is up;
///   2. charge the running process one tick and kill it when its CPU budget
///      is gone (watchdog; user processes only);
///   3. preempt, only if the tick interrupted ring 3: when the slice is over
///      hand the CPU to a ready process of the same level or better, and
///      before that to a strictly better one (just woken, say).
pub fn on_tick(from_user: bool) {
    // SAFETY: handler context with IF=0; `CURRENT` and the table are only
    // changed with interrupts masked.
    unsafe {
        wake_sleepers(crate::hw::pit::ticks());

        let cur = CURRENT;
        if cur.is_null() {
            return; // the shell or the idle loop was interrupted
        }
        (*cur).used += 1;
        if (*cur).aspace.is_some() && (*cur).used >= (*cur).budget {
            exit_current(Outcome::Timeout);
        }
        if !from_user {
            return; // the kernel is not preemptible
        }

        if (*cur).slice_left > 0 {
            (*cur).slice_left -= 1;
        }
        let limit = if (*cur).slice_left == 0 {
            (*cur).prio
        } else if (*cur).prio > 0 {
            (*cur).prio - 1
        } else {
            return;
        };
        match pick_ready((*cur).pid, limit) {
            Some(next) => {
                (*cur).state = State::Ready;
                (*cur).preempts += 1;
                cpu_state_for(next);
                switch_ctx(&mut (*cur).saved, &(*next).saved);
                // Resumed: whoever switched back republished our CPU state.
                (*CURRENT).state = State::Running;
            }
            None => {
                if (*cur).slice_left == 0 {
                    (*cur).slice_left = SLICE_TICKS; // alone at our level: new slice
                }
            }
        }
    }
}

// ---------------------------------------------------------------- shell API

/// `kill <pid>`: terminate a process from the shell (main context).  The
/// process cannot be running: the queue is drained while the shell runs.
pub fn kill(pid: u64) -> bool {
    let flags = crate::arch::io::cli_save();
    // SAFETY: main context, no process on the CPU, interrupts masked.
    let found = unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        let mut found = false;
        for p in table.iter_mut() {
            if p.pid == pid && p.state != State::Zombie {
                p.outcome = Outcome::Killed;
                p.state = State::Zombie;
                p.exit_seq = next_exit_seq();
                drop(p.aspace.take());
                found = true;
            }
        }
        found
    };
    // SAFETY: as cli_save.
    unsafe { crate::arch::io::restore_flags(flags) };
    found
}

/// Drain the queue and reap everything (used by `usertest` between
/// scenarios, so the page-leak accounting sees a clean state).
pub fn drain_and_reap() {
    run_until_idle();
    let flags = crate::arch::io::cli_save();
    // SAFETY: main context, no process on the CPU.
    reap();
    // SAFETY: as cli_save.
    unsafe { crate::arch::io::restore_flags(flags) };
}
