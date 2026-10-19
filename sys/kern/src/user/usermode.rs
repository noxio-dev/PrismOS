// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Ring 3 entry and exit (phase 2.1, reworked for processes in 2.2).
//
//   * `init`          : EFER.SCE + STAR/LSTAR/SFMASK for `syscall`/`sysret`,
//                       a per-CPU block reached through GS (swapgs),
//                       SMEP/SMAP/UMIP when the CPU has them.
//   * `syscall_entry` : naked stub that builds an `IsrFrame` on the current
//                       process's kernel stack and calls user/syscall.rs.
//   * `iretq_to_user` : naked stub that consumes a prepared `IsrFrame` and
//                       drops into ring 3 -- the first entry of a process
//                       (called from the trampoline in process.rs).
//   * `set_kstack_top`: publish the current kernel stack top through the
//                       per-CPU block (gs:[0]) and TSS.RSP0.
//
// GS convention: whenever the kernel runs, GS base is the `PerCpu` block
// (IA32_GS_BASE) and IA32_KERNEL_GS_BASE holds the user's value (0).  Code
// entering from ring 3 executes `swapgs` first, code returning to ring 3
// executes it last.
//
// Program images, `Outcome`/`Expect` and the self-test programs live here
// too; user/process.rs owns the PCB / kernel stacks / context switch and
// user/syscall.rs the dispatch.

use core::arch::{global_asm, naked_asm};
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::gdt::{KERNEL_CS, USER_CS, USER_DS};
use crate::arch::intr::IsrFrame;
use crate::mem::vmm::{AddressSpace, PAGE, USER_CODE_BASE, USER_STACK_TOP};

const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;
const MSR_GS_BASE: u32 = 0xC000_0101;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

const EFER_SCE: u64 = 1;

/// RFLAGS bits cleared on `syscall` entry: TF, IF, DF, NT, AC.
const SFMASK: u64 = 0x100 | 0x200 | 0x400 | 0x4000 | 0x4_0000;

/// User stack: 16 KiB below USER_STACK_TOP (the page under it stays unmapped
/// as a guard).
pub const USER_STACK_SIZE: u64 = 4 * PAGE;
/// Default watchdog: 3 s of CPU time (300 ticks at 100 Hz, charged only while running).
pub const DEFAULT_TIMEOUT_TICKS: u64 = 300;

pub const ENOSYS: i64 = 38;
pub const EBADF: i64 = 9;
pub const EFAULT: i64 = 14;
pub const EPERM: i64 = 1;
pub const EINVAL: i64 = 22;

// ------------------------------------------------------------ per-CPU block

/// Reached through GS while in the kernel.  The `syscall` entry stub uses
/// the two fixed offsets below.  One slot per CPU (phase 1.5 SMP): each
/// CPU's GS base points at its own slot.
#[repr(C)]
struct PerCpu {
    kstack_top: u64, // gs:[0]  kernel stack for syscalls
    user_rsp: u64,   // gs:[8]  scratch: user RSP during the entry sequence
}

const PERCPU_ZERO: PerCpu = PerCpu { kstack_top: 0, user_rsp: 0 };

static mut PERCPU: [PerCpu; crate::hw::acpi::MAX_CPUS] =
    [PERCPU_ZERO; crate::hw::acpi::MAX_CPUS];

/// The per-CPU block of the running CPU, through its GS base (each CPU's
/// GS base was set to its own slot at boot / AP bring-up).
fn current_percpu() -> &'static mut PerCpu {
    // SAFETY: reading MSR_GS_BASE in kernel context yields our slot's
    // address; the value is one of the PERCPU entries by construction.
    unsafe {
        let gs = crate::arch::io::rdmsr(MSR_GS_BASE) as usize;
        let base = addr_of_mut!(PERCPU).cast::<PerCpu>() as usize;
        let idx = (gs - base) / core::mem::size_of::<PerCpu>();
        let idx = idx.min(crate::hw::acpi::MAX_CPUS - 1);
        &mut *addr_of_mut!(PERCPU).cast::<PerCpu>().add(idx)
    }
}

/// Point this CPU's GS base at its per-CPU slot (BSP: slot 0 in init();
/// APs: their own slot from hw/smp.rs).
///
/// SAFETY: privileged MSR write; must run on the CPU it configures.
pub unsafe fn set_gs_for_cpu(cpu: usize) {
    let p = unsafe { addr_of_mut!(PERCPU).cast::<PerCpu>().add(cpu.min(crate::hw::acpi::MAX_CPUS - 1)) };
    // SAFETY: as above.
    unsafe {
        crate::arch::io::wrmsr(MSR_GS_BASE, p as u64);
        crate::arch::io::wrmsr(MSR_KERNEL_GS_BASE, 0);
    }
}

/// Publish the kernel stack that traps coming from ring 3 must land on:
/// TSS.RSP0 (interrupts / exceptions) and gs:[0] (`syscall`).  Called by the
/// scheduler on every switch to a user process.
pub fn set_kstack_top(top: u64) {
    // SAFETY: scalar write through GS; called with interrupts masked.
    current_percpu().kstack_top = top;
    crate::arch::tss::set_rsp0(top);
}

// -------------------------------------------------------------- entry stubs

/// `syscall` lands here with: rcx = user rip, r11 = user rflags, user rsp
/// untouched, CS/SS already kernel, interrupts masked (SFMASK).  The stub
/// builds an `IsrFrame` identical to the interrupt path's, calls the Rust
/// dispatcher and returns with `sysretq`.
///
/// The frame is pushed on the kernel stack published through gs:[0] -- the
/// kernel stack of the process that made the call, so a context switch in
/// the middle of a syscall (phase 2.2: `yield`) suspends the whole trap
/// frame with it and `sysretq` resumes the user program exactly where it
/// yielded.
///
/// `sysretq` faults in ring 0 on a non-canonical RCX; that cannot happen
/// here: user code only ever lives below USER_END, so the address after a
/// `syscall` instruction is at most USER_END, which is canonical.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    naked_asm!(
        "swapgs",
        "mov gs:[8], rsp",
        "mov rsp, gs:[0]",
        "push {ds}",               // ss
        "push qword ptr gs:[8]",   // rsp
        "push r11",                // rflags
        "push {cs}",               // cs
        "push rcx",                // rip
        "push 0",                  // errcode
        "push 0x100",              // vector: not an exception, a syscall
        "push r15",
        "push r14",
        "push r13",
        "push r12",
        "push r11",
        "push r10",
        "push r9",
        "push r8",
        "push rbp",
        "push rdi",
        "push rsi",
        "push rdx",
        "push rcx",
        "push rbx",
        "push rax",
        "push 0",                  // dummy (alignment)
        "mov rdi, rsp",
        "cld",
        "call {dispatch}",
        "add rsp, 8",
        "pop rax",
        "pop rbx",
        "pop rcx",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rbp",
        "pop r8",
        "pop r9",
        "pop r10",
        "pop r11",
        "pop r12",
        "pop r13",
        "pop r14",
        "pop r15",
        "add rsp, 16",             // vector + errcode
        "pop rcx",                 // rip
        "add rsp, 8",              // cs
        "pop r11",                 // rflags
        "pop rsp",                 // user rsp (we are now off the kernel stack)
        "swapgs",
        "sysretq",
        ds = const USER_DS,
        cs = const USER_CS,
        dispatch = sym crate::user::syscall::dispatch,
    );
}

/// Consume a prepared `IsrFrame` (in memory, layout identical to what
/// `isr_common` pushes) and drop into ring 3 at frame.rip with frame.rsp.
/// Used once per process by the trampoline in process.rs; never returns.
/// Must be called with interrupts masked (the frame's RFLAGS turns them back
/// on in ring 3), and with GS already holding the kernel base (the `swapgs`
/// below hands it to the user side).
///
/// # Safety
/// `frame` must point to a fully initialised `IsrFrame` with a ring 3
/// CS/SS/RIP/RSP/RFLAGS.
#[unsafe(naked)]
pub unsafe extern "C" fn iretq_to_user(frame: *mut IsrFrame) -> ! {
    naked_asm!(
        "mov rsp, rdi",
        "add rsp, 8",              // dummy
        "pop rax",
        "pop rbx",
        "pop rcx",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rbp",
        "pop r8",
        "pop r9",
        "pop r10",
        "pop r11",
        "pop r12",
        "pop r13",
        "pop r14",
        "pop r15",
        "add rsp, 16",             // vector + errcode
        "swapgs",
        "iretq",                   // rip, cs, rflags, rsp, ss
    );
}

// ------------------------------------------------------------------ state

/// What happened to a user program when it ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    None,
    Exited(u64),
    Fault { vector: u64, err: u64, rip: u64, cr2: u64 },
    Timeout,
    /// Killed by the shell (`kill` command).
    Killed,
    /// Terminated by a signal (phase 2.5): `sys_kill`, or the shell's kill
    /// command, ended it.  The number is the signal (9 for SIGKILL ...).
    Signaled(u64),
}

/// Which protections `init` managed to switch on.
#[derive(Clone, Copy)]
pub struct InitInfo {
    pub smep: bool,
    pub smap: bool,
    pub umip: bool,
}

static READY: AtomicBool = AtomicBool::new(false);

/// Program the `syscall` MSRs and the GS-based per-CPU block.  Call after
/// the GDT is loaded (selector layout) and before the first process runs.
pub fn init() -> InitInfo {
    let max_leaf = crate::arch::cpu::cpuid(0, 0).eax;
    let l7 = if max_leaf >= 7 {
        crate::arch::cpu::cpuid(7, 0)
    } else {
        crate::arch::cpu::Cpuid { eax: 0, ebx: 0, ecx: 0, edx: 0 }
    };
    let smep = l7.ebx & (1 << 7) != 0;
    let smap = l7.ebx & (1 << 20) != 0;
    let umip = l7.ecx & (1 << 2) != 0;

    // SAFETY: privileged configuration at boot, single CPU, interrupts off
    // or irrelevant (nothing uses these MSRs yet).
    unsafe {
        let efer = crate::arch::io::rdmsr(MSR_EFER);
        crate::arch::io::wrmsr(MSR_EFER, efer | EFER_SCE);
        // SYSCALL: CS = STAR[47:32] (0x08), SS = +8 (0x10).
        // SYSRET64: SS = STAR[63:48] + 8 (0x18 -> 0x1b), CS = +16 (0x20 -> 0x23).
        crate::arch::io::wrmsr(MSR_STAR, (0x10u64 << 48) | ((KERNEL_CS as u64) << 32));
        crate::arch::io::wrmsr(MSR_LSTAR, syscall_entry as usize as u64);
        crate::arch::io::wrmsr(MSR_SFMASK, SFMASK);
        // The BSP takes per-CPU slot 0; APs set their own in hw/smp.rs.
        // SAFETY: we are the BSP, cpu 0.
        unsafe { set_gs_for_cpu(0) };

        let mut cr4 = crate::arch::regs::read_cr4();
        if smep {
            cr4 |= 1 << 20;
        }
        if smap {
            cr4 |= 1 << 21;
        }
        if umip {
            cr4 |= 1 << 11;
        }
        crate::arch::regs::write_cr4(cr4);
    }
    // Compiled programs use x87 / SSE (state saved per process, user/fpu.rs).
    crate::user::fpu::init_cpu();
    READY.store(true, Ordering::Release);
    InitInfo { smep, smap, umip }
}

pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// An `IsrFrame` holding the initial ring 3 context of a program: image
/// entered at its first byte, private stack, IF on, every register zero.
pub fn initial_user_frame() -> IsrFrame {
    user_frame(USER_CODE_BASE, USER_STACK_TOP)
}

/// Like `initial_user_frame`, with an explicit entry point and stack (ELF
/// programs: the entry and initial `rsp` come from the loader).
pub fn user_frame(rip: u64, rsp: u64) -> IsrFrame {
    IsrFrame {
        dummy: 0,
        rax: 0,
        rbx: 0,
        rcx: 0,
        rdx: 0,
        rsi: 0,
        rdi: 0,
        rbp: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        vector: 0x100, // "syscall-like": not an exception
        errcode: 0,
        rip,
        cs: USER_CS as u64 + 3,
        rflags: 0x202, // IF set, IOPL 0
        rsp,
        ss: USER_DS as u64 + 3,
    }
}

/// Map a flat program image and its stack into a fresh address space.
/// The image is copied to USER_CODE_BASE (mapped r-x) and entered at its
/// first byte; the stack is a private rw- NX region ending at
/// USER_STACK_TOP with an unmapped guard page below.  The signal-return
/// trampoline page (phase 2.5) is mapped too, so every process can take
/// deliveries even if it never installs a handler.
pub fn build_aspace(image: &[u8]) -> Result<AddressSpace, &'static str> {
    if !ready() {
        return Err("usermode not initialised");
    }
    if image.is_empty() || image.len() as u64 > 64 * PAGE {
        return Err("bad program size");
    }
    let mut aspace = AddressSpace::new().ok_or("out of memory (page tables)")?;
    if !aspace.alloc_map(USER_CODE_BASE, image.len() as u64, false, true)
        || !aspace.write(USER_CODE_BASE, image, true)
    {
        return Err("cannot map the program image");
    }
    if !aspace.alloc_map(USER_STACK_TOP - USER_STACK_SIZE, USER_STACK_SIZE, true, false) {
        return Err("cannot map the user stack");
    }
    if !aspace.map_sigcode() {
        return Err("cannot map the signal trampoline");
    }
    Ok(aspace)
}

// ------------------------------------------------------------ test programs
//
// Tiny position-independent images, assembled here and copied into user
// memory.  They use only `syscall` (no libc, nothing else mapped).

global_asm!(
    ".section .rodata.uprog,\"a\"",
    ".balign 16",
    // hello: uses the stack (call/ret), prints, exits with status 7.
    ".globl uprog_hello_start",
    ".globl uprog_hello_end",
    "uprog_hello_start:",
    "    call uprog_hello_print",
    "    mov eax, 0",
    "    mov edi, 7",
    "    syscall",
    "uprog_hello_print:",
    "    mov eax, 1",
    "    mov edi, 1",
    "    lea rsi, [rip + uprog_hello_msg]",
    "    lea rdx, [rip + uprog_hello_msg_end]",
    "    sub rdx, rsi",             // length, as an immediate-free computation
    "    syscall",
    "    ret",
    "uprog_hello_msg:",
    "    .ascii \"  [ring 3] hello from user mode\\n\"",
    "uprog_hello_msg_end:",
    "uprog_hello_end:",
    ".balign 16",
    // sys: checks the error paths of the syscall layer; exit status is the
    // number of the first failing check (0 = all good).
    ".globl uprog_sys_start",
    ".globl uprog_sys_end",
    "uprog_sys_start:",
    "    mov eax, 1",                    // write(1, <kernel address>, 4)
    "    mov edi, 1",
    "    movabs rsi, 0xFFFFFFFF80200000",
    "    mov edx, 4",
    "    syscall",
    "    cmp rax, -14",                  // -EFAULT
    "    jne uprog_sys_f1",
    "    mov eax, 1",                    // write(9, ...) -> -EBADF
    "    mov edi, 9",
    "    lea rsi, [rip + uprog_sys_start]",
    "    mov edx, 1",
    "    syscall",
    "    cmp rax, -9",
    "    jne uprog_sys_f2",
    "    mov eax, 999",                  // unknown number -> -ENOSYS
    "    syscall",
    "    cmp rax, -38",
    "    jne uprog_sys_f3",
    "    mov eax, 2",                    // getpid -> this pid (> 0)
    "    syscall",
    "    cmp rax, 1",
    "    jl uprog_sys_f4",
    "    mov eax, 1",                    // write(1, buf, 0) -> 0
    "    mov edi, 1",
    "    lea rsi, [rip + uprog_sys_start]",
    "    xor edx, edx",
    "    syscall",
    "    test rax, rax",
    "    jne uprog_sys_f5",
    "    mov eax, 6",                    // setprio(0): raising the priority -> -EPERM
    "    xor edi, edi",
    "    syscall",
    "    cmp rax, -1",
    "    jne uprog_sys_f6",
    "    mov eax, 6",                    // setprio(9): out of range -> -EINVAL
    "    mov edi, 9",
    "    syscall",
    "    cmp rax, -22",
    "    jne uprog_sys_f7",
    "    mov eax, 6",                    // setprio(3): lowering works, returns the old level (2)
    "    mov edi, 3",
    "    syscall",
    "    cmp rax, 2",
    "    jne uprog_sys_f8",
    "    xor edi, edi",
    "    jmp uprog_sys_exit",
    "uprog_sys_f1: mov edi, 1",
    "    jmp uprog_sys_exit",
    "uprog_sys_f2: mov edi, 2",
    "    jmp uprog_sys_exit",
    "uprog_sys_f3: mov edi, 3",
    "    jmp uprog_sys_exit",
    "uprog_sys_f4: mov edi, 4",
    "    jmp uprog_sys_exit",
    "uprog_sys_f5: mov edi, 5",
    "    jmp uprog_sys_exit",
    "uprog_sys_f6: mov edi, 6",
    "    jmp uprog_sys_exit",
    "uprog_sys_f7: mov edi, 7",
    "    jmp uprog_sys_exit",
    "uprog_sys_f8: mov edi, 8",
    "uprog_sys_exit:",
    "    xor eax, eax",
    "    syscall",
    "uprog_sys_end:",
    ".balign 16",
    // ping: prints and yields 5 times, then exits -- one side of the
    // phase 2.2 ping-pong: the scheduler interleaves it with `pong`.
    ".globl uprog_ping_start",
    ".globl uprog_ping_end",
    "uprog_ping_start:",
    "    mov ebx, 5",
    "uprog_ping_loop:",
    "    mov eax, 1",
    "    mov edi, 1",
    "    lea rsi, [rip + uprog_ping_msg]",
    "    lea rdx, [rip + uprog_ping_msg_end]",
    "    sub rdx, rsi",             // length
    "    syscall",
    "    mov eax, 4",                    // yield
    "    syscall",
    "    dec ebx",
    "    jnz uprog_ping_loop",
    "    xor eax, eax",
    "    xor edi, edi",
    "    syscall",
    "uprog_ping_msg:",
    "    .ascii \"  [ring 3] ping\\n\"",
    "uprog_ping_msg_end:",
    "uprog_ping_end:",
    ".balign 16",
    // pong: the other side of the ping-pong.
    ".globl uprog_pong_start",
    ".globl uprog_pong_end",
    "uprog_pong_start:",
    "    mov ebx, 5",
    "uprog_pong_loop:",
    "    mov eax, 1",
    "    mov edi, 1",
    "    lea rsi, [rip + uprog_pong_msg]",
    "    lea rdx, [rip + uprog_pong_msg_end]",
    "    sub rdx, rsi",             // length
    "    syscall",
    "    mov eax, 4",                    // yield
    "    syscall",
    "    dec ebx",
    "    jnz uprog_pong_loop",
    "    mov eax, 0",
    "    mov edi, 3",                    // exit status 3: proves PID != identity
    "    syscall",
    "uprog_pong_msg:",
    "    .ascii \"  [ring 3] pong\\n\"",
    "uprog_pong_msg_end:",
    "uprog_pong_end:",
    ".balign 16",
    // yield: gives the CPU back to the scheduler 3 times, printing a
    // counter between yields; exit status = number of yields.
    ".globl uprog_yield_start",
    ".globl uprog_yield_end",
    "uprog_yield_start:",
    "    mov ebx, 3",
    "uprog_yield_loop:",
    "    mov eax, 4",                    // yield
    "    syscall",
    "    dec ebx",
    "    jnz uprog_yield_loop",
    "    xor eax, eax",
    "    mov edi, 3",
    "    syscall",
    "uprog_yield_end:",
    ".balign 16",
    // sleep (phase 2.3): sleeps 50 ms, then checks that the uptime moved by
    // at least 50 ms (exit 0) or not (exit 1).
    ".globl uprog_sleep_start",
    ".globl uprog_sleep_end",
    "uprog_sleep_start:",
    "    mov eax, 3",                    // uptime_ms
    "    syscall",
    "    mov rbx, rax",
    "    mov eax, 5",                    // sleep(50)
    "    mov edi, 50",
    "    syscall",
    "    mov eax, 3",
    "    syscall",
    "    sub rax, rbx",
    "    cmp rax, 50",
    "    jb uprog_sleep_fail",
    "    xor edi, edi",
    "    jmp uprog_sleep_exit",
    "uprog_sleep_fail: mov edi, 1",
    "uprog_sleep_exit:",
    "    xor eax, eax",
    "    syscall",
    "uprog_sleep_end:",
    ".balign 16",
    // spin (phase 2.3): burns the CPU in user mode, never yielding, until
    // the uptime moved by 120 ms, then exits 0.  The only way another process
    // gets the CPU meanwhile is the timer tick preempting it.
    ".globl uprog_spin_start",
    ".globl uprog_spin_end",
    "uprog_spin_start:",
    "    mov eax, 3",                    // uptime_ms
    "    syscall",
    "    mov rbx, rax",
    "uprog_spin_loop:",
    "    mov ecx, 200000",
    "uprog_spin_burn:",
    "    dec ecx",
    "    jnz uprog_spin_burn",
    "    mov eax, 3",
    "    syscall",
    "    sub rax, rbx",
    "    cmp rax, 120",
    "    jb uprog_spin_loop",
    "    xor eax, eax",
    "    xor edi, edi",
    "    syscall",
    "uprog_spin_end:",
    ".balign 16",
    // kread: read kernel memory (supervisor-only page) -> #PF
    ".globl uprog_kread_start",
    ".globl uprog_kread_end",
    "uprog_kread_start:",
    "    movabs rax, 0xFFFFFFFF80200000",
    "    mov rbx, [rax]",
    "uprog_kread_end:",
    ".balign 16",
    // lowmem: read the identity map (kernel-owned, no U/S) -> #PF
    ".globl uprog_lowmem_start",
    ".globl uprog_lowmem_end",
    "uprog_lowmem_start:",
    "    mov eax, 0x1000",
    "    mov rbx, [rax]",
    "uprog_lowmem_end:",
    ".balign 16",
    // priv: privileged instruction -> #GP
    ".globl uprog_priv_start",
    ".globl uprog_priv_end",
    "uprog_priv_start:",
    "    cli",
    "uprog_priv_end:",
    ".balign 16",
    // int80: software interrupt through a DPL 0 gate -> #GP
    ".globl uprog_int80_start",
    ".globl uprog_int80_end",
    "uprog_int80_start:",
    "    int 0x80",
    "uprog_int80_end:",
    ".balign 16",
    // wcode: write to the (read-only) code page -> #PF write protection
    ".globl uprog_wcode_start",
    ".globl uprog_wcode_end",
    "uprog_wcode_start:",
    "    mov byte ptr [rip + uprog_wcode_start], 1",
    "uprog_wcode_end:",
    ".balign 16",
    // nx: jump to the (non-executable) stack -> #PF instruction fetch
    ".globl uprog_nx_start",
    ".globl uprog_nx_end",
    "uprog_nx_start:",
    "    lea rax, [rsp - 16]",
    "    jmp rax",
    "uprog_nx_end:",
    ".balign 16",
    // loop: never ends -> watchdog
    ".globl uprog_loop_start",
    ".globl uprog_loop_end",
    "uprog_loop_start:",
    "    jmp uprog_loop_start",
    "uprog_loop_end:",
    ".balign 16",
    // stack: overflow the stack until it hits the guard page -> #PF
    ".globl uprog_stack_start",
    ".globl uprog_stack_end",
    "uprog_stack_start:",
    "    push rax",
    "    jmp uprog_stack_start",
    "uprog_stack_end:",
    ".previous",
);

unsafe extern "C" {
    static uprog_hello_start: u8;
    static uprog_hello_end: u8;
    static uprog_sys_start: u8;
    static uprog_sys_end: u8;
    static uprog_ping_start: u8;
    static uprog_ping_end: u8;
    static uprog_pong_start: u8;
    static uprog_pong_end: u8;
    static uprog_yield_start: u8;
    static uprog_yield_end: u8;
    static uprog_sleep_start: u8;
    static uprog_sleep_end: u8;
    static uprog_spin_start: u8;
    static uprog_spin_end: u8;
    static uprog_kread_start: u8;
    static uprog_kread_end: u8;
    static uprog_lowmem_start: u8;
    static uprog_lowmem_end: u8;
    static uprog_priv_start: u8;
    static uprog_priv_end: u8;
    static uprog_int80_start: u8;
    static uprog_int80_end: u8;
    static uprog_wcode_start: u8;
    static uprog_wcode_end: u8;
    static uprog_nx_start: u8;
    static uprog_nx_end: u8;
    static uprog_loop_start: u8;
    static uprog_loop_end: u8;
    static uprog_stack_start: u8;
    static uprog_stack_end: u8;
}

/// What a test program is expected to end with.
#[derive(Clone, Copy)]
pub enum Expect {
    Exit(u64),
    /// Fault vector, bits that must be set / clear in the error code.
    Fault { vector: u64, set: u64, clear: u64 },
    Timeout,
}

pub struct Prog {
    pub name: &'static str,
    pub desc: &'static str,
    start: *const u8,
    end: *const u8,
    pub expect: Expect,
    /// Watchdog budget in ticks (0 = default).
    pub timeout: u64,
}

// SAFETY: the pointers refer to read-only data in the kernel image.
unsafe impl Sync for Prog {}

macro_rules! prog {
    ($name:expr, $desc:expr, $s:ident, $e:ident, $expect:expr, $timeout:expr) => {
        Prog {
            name: $name,
            desc: $desc,
            start: addr_of!($s),
            end: addr_of!($e),
            expect: $expect,
            timeout: $timeout,
        }
    };
}

// #PF error code bits: 1 = protection (page present), 2 = write, 4 = user, 16 = instruction fetch.
pub static PROGRAMS: [Prog; 15] = [
    prog!("hello", "prints a line, exits with status 7", uprog_hello_start, uprog_hello_end, Expect::Exit(7), 0),
    prog!("sys", "syscall error paths (EFAULT, EBADF, ENOSYS, getpid)", uprog_sys_start, uprog_sys_end, Expect::Exit(0), 0),
    prog!("ping", "prints and yields 5 times (pairs with pong)", uprog_ping_start, uprog_ping_end, Expect::Exit(0), 0),
    prog!("pong", "the other half of the ping-pong, exits with status 3", uprog_pong_start, uprog_pong_end, Expect::Exit(3), 0),
    prog!("yield", "yields to the scheduler 3 times, exit status 3", uprog_yield_start, uprog_yield_end, Expect::Exit(3), 0),
    prog!("sleep", "sleeps 50 ms and checks the uptime moved", uprog_sleep_start, uprog_sleep_end, Expect::Exit(0), 0),
    prog!("spin", "burns the CPU ~120 ms without yielding (preemption)", uprog_spin_start, uprog_spin_end, Expect::Exit(0), 0),
    prog!("kread", "reads kernel memory -> #PF (user, protection)", uprog_kread_start, uprog_kread_end,
          Expect::Fault { vector: 14, set: 0x5, clear: 0x12 }, 0),
    prog!("lowmem", "reads the kernel identity map -> #PF", uprog_lowmem_start, uprog_lowmem_end,
          Expect::Fault { vector: 14, set: 0x5, clear: 0x12 }, 0),
    prog!("priv", "executes cli -> #GP", uprog_priv_start, uprog_priv_end,
          Expect::Fault { vector: 13, set: 0, clear: 0 }, 0),
    prog!("int80", "int 0x80 (no such gate for ring 3) -> #GP", uprog_int80_start, uprog_int80_end,
          Expect::Fault { vector: 13, set: 0, clear: 0 }, 0),
    prog!("wcode", "writes its read-only code page -> #PF (write)", uprog_wcode_start, uprog_wcode_end,
          Expect::Fault { vector: 14, set: 0x7, clear: 0x10 }, 0),
    prog!("nx", "jumps to its NX stack -> #PF (instruction fetch)", uprog_nx_start, uprog_nx_end,
          Expect::Fault { vector: 14, set: 0x15, clear: 0x2 }, 0),
    prog!("stack", "overflows the stack into the guard page -> #PF", uprog_stack_start, uprog_stack_end,
          Expect::Fault { vector: 14, set: 0x6, clear: 0x11 }, 0),
    prog!("loop", "spins forever -> killed by the watchdog", uprog_loop_start, uprog_loop_end, Expect::Timeout, 50),
];

impl Prog {
    pub fn image(&self) -> &'static [u8] {
        // SAFETY: start/end delimit a byte range in the kernel image.
        unsafe { core::slice::from_raw_parts(self.start, self.end as usize - self.start as usize) }
    }

    pub fn check(&self, o: Outcome) -> bool {
        match (self.expect, o) {
            (Expect::Exit(a), Outcome::Exited(b)) => a == b,
            (Expect::Timeout, Outcome::Timeout) => true,
            (Expect::Fault { vector, set, clear }, Outcome::Fault { vector: v, err, .. }) => {
                v == vector && err & set == set && err & clear == 0
            }
            _ => false,
        }
    }
}

pub fn find(name: &str) -> Option<&'static Prog> {
    PROGRAMS.iter().find(|p| p.name == name)
}

pub fn describe(o: Outcome) {
    match o {
        Outcome::None => crate::kprintln!("no outcome"),
        Outcome::Exited(c) => crate::kprintln!("exited with status {}", c),
        Outcome::Timeout => crate::kprintln!("killed by the watchdog (time-out)"),
        Outcome::Killed => crate::kprintln!("killed by the shell"),
        Outcome::Signaled(s) => crate::kprintln!("killed by signal {}", s),
        Outcome::Fault { vector, err, rip, cr2 } => crate::kprintln!(
            "killed by exception {} at rip {:#x} (errcode {:#x}, cr2 {:#x})",
            vector, rip, err, cr2
        ),
    }
}
