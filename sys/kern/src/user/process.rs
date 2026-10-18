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
// Scheduling in 2.2 is cooperative round-robin: a process gives the CPU back
// with the `yield` syscall, ends with `exit`, or is killed by the watchdog
// (a tick that finds its deadline passed) or a ring 3 exception.  Preemption
// on the LAPIC timer, priorities and sleep/wakeup arrive with phase 2.3.
//
// The shell/kernel itself is the implicit PID 0 ("main" context): it runs on
// the boot stack, `run_until_idle()` drives the run queue until every
// process is dead, and returns to the caller with the outcomes.
//
// Invariants (single CPU):
//   * every context switch runs with interrupts masked, and both sides'
//     saved RFLAGS have IF=0;
//   * `CURRENT` points at the running process, or is null while the kernel
//     (shell) runs;
//   * the process table is only touched from the main context (spawn, reap,
//     kill) or from the running process itself (yield, exit) with IF=0.

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

// ------------------------------------------------------------------- state

/// Life cycle of a process.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Spawned, waiting for the CPU.
    Ready,
    /// On the CPU right now.
    Running,
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
    /// Watchdog deadline in ticks.
    deadline: u64,
}

/// The saved context of the main context (the shell / boot flow): the other
/// side of every process <-> kernel switch.
static mut MAIN_SAVED: SavedCtx = SavedCtx::ZERO;

/// The process on the CPU, null while the kernel runs.
static mut CURRENT: *mut Process = core::ptr::null_mut();

/// The process table, in pid order (pushed at spawn, drained by reap).
static mut TABLE: Vec<Box<Process>> = Vec::new();
static mut NEXT_PID: u64 = 1;

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
) -> Result<u64, &'static str> {
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

    // SAFETY: main context, no process running, interrupts masked.
    let pid = unsafe { NEXT_PID };
    unsafe { NEXT_PID += 1 }
    let proc = Process {
        pid,
        name,
        state: State::Ready,
        outcome: Outcome::None,
        aspace,
        kstack,
        kstack_top: ktop,
        saved: SavedCtx { rsp: ktop - 8, ..SavedCtx::ZERO },
        entry: Some(entry),
        deadline: crate::hw::pit::ticks()
            + if timeout_ticks == 0 { DEFAULT_TIMEOUT_TICKS } else { timeout_ticks },
    };
    // SAFETY: as above.
    unsafe {
        (*addr_of_mut!(TABLE)).push(Box::new(proc));
    }
    Ok(pid)
}

/// Spawn a user program from its flat image (built-in test programs).
pub fn spawn_user(prog: &Prog) -> Result<u64, &'static str> {
    // On a late failure the address space is dropped here with the locals
    // (RAII: its Drop frees the user half through the direct map).
    let aspace = usermode::build_aspace(prog.image())?;
    spawn_common(prog.name, Some(aspace), Entry::User, prog.timeout)
}

/// Spawn a kernel thread (runs `f` in ring 0 on its own kernel stack).
/// Kernel threads have no watchdog: they must cooperate on their own.
pub fn spawn_kernel(name: &'static str, f: fn()) -> Result<u64, &'static str> {
    spawn_common(name, None, Entry::Kernel(f), 0)
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

/// Give up the CPU: round-robin to the next ready process, or back to the
/// main context when nobody else can run.  Returns once somebody switched
/// back to us (the `yield` syscall path, and the tick watchdog's way back).
pub fn yield_current() {
    // SAFETY: the running process exists; interrupts are masked (syscall
    // entry clears IF via SFMASK; the tick path runs in a handler).
    unsafe {
        let cur = CURRENT;
        if cur.is_null() {
            return; // the kernel context never yields to itself
        }
        (*cur).state = State::Ready;

        match pick_ready_after((*cur).pid) {
            Some(next) => {
                cpu_state_for(next);
                switch_ctx(&mut (*cur).saved, &(*next).saved);
            }
            None => switch_to_main(&mut (*cur).saved),
        }
        // Back in our own context: the switcher republished the CPU state
        // (CURRENT, CR3, RSP0) for us before the switch.
        (*CURRENT).state = State::Running;
    }
}

/// First ready process in the table strictly after `pid` (pid order,
/// wrapping around); None when no *other* process is ready.
fn pick_ready_after(pid: u64) -> Option<*mut Process> {
    // SAFETY: table walk with interrupts masked; the returned pointer stays
    // owned by the table.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        let n = table.len();
        if n == 0 {
            return None;
        }
        let start = table.iter().position(|p| p.pid == pid).unwrap_or(0);
        for k in 1..=n {
            let idx = (start + k) % n;
            if table[idx].pid != pid && table[idx].state == State::Ready {
                return Some(&mut *table[idx] as *mut Process);
            }
        }
        None
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
            // First ready process in table order.
            let mut next: *mut Process = core::ptr::null_mut();
            for p in table.iter_mut() {
                if p.state == State::Ready {
                    next = p.as_mut() as *mut Process;
                    break;
                }
            }
            if next.is_null() {
                // Nobody ready and nobody running (a process is Ready before
                // it switches away, dead ones are zombies): all done.
                break;
            }
            cpu_state_for(next);
            switch_ctx(addr_of_mut!(MAIN_SAVED), &(*next).saved);
            // Back from the process (it yielded or died).  Loop.
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

/// Called from the tick handler (pit::on_tick): kill a process that used up
/// its watchdog budget without yielding.  The tick EOI is already sent and
/// interrupts are masked in the handler, so switching away from the
/// interrupt frame is safe (the dying stack is freed by the reaper).
pub fn on_tick() {
    // SAFETY: read-only access to the running process from the handler.
    unsafe {
        let cur = CURRENT;
        if cur.is_null() {
            return;
        }
        let p = &mut *cur;
        if p.aspace.is_some() && crate::hw::pit::ticks() >= p.deadline {
            exit_current(Outcome::Timeout);
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
