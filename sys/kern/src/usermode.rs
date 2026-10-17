// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Phase 2.1 -- ring 3.
//
//   * `init`   : EFER.SCE + STAR/LSTAR/SFMASK for `syscall`/`sysret`, a
//                per-CPU block reached through GS (swapgs), SMEP/SMAP/UMIP.
//   * `run`    : build a private address space (vmm::AddressSpace), load a
//                flat program image and a stack, then drop to ring 3 with
//                `iretq` on a dedicated kernel stack (TSS.RSP0).
//   * traps    : syscalls enter through `syscall_entry`, interrupts and
//                exceptions through int.rs; both end the user program or
//                return to it.  Anything that goes wrong in ring 3 (page
//                fault, #GP, ...) kills the program, never the kernel; a
//                tick watchdog stops runaway loops.
//
// There is still a single flow of control: `run` blocks the shell until the
// program ends (exit, fault or time-out).  Processes, threads and a real
// scheduler arrive in phases 2.2 / 2.3, which will reuse everything here.
//
// GS convention: whenever the kernel runs, GS base is the `PerCpu` block
// (IA32_GS_BASE) and IA32_KERNEL_GS_BASE holds the user's value (0).  Code
// entering from ring 3 executes `swapgs` first, code returning to ring 3
// executes it last.
//
// System call ABI (Linux-like):
//   rax = number, rdi/rsi/rdx/r10/r8/r9 = arguments, result in rax
//   (negative errno on failure); rcx and r11 are clobbered by the CPU.
//     0 exit(code)            1 write(fd, buf, len)       2 getpid()
//     3 uptime_ms()

use alloc::alloc::{alloc_zeroed, dealloc, Layout};
use core::arch::{asm, global_asm, naked_asm};
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::int::{IsrFrame, KERNEL_CS, USER_CS, USER_DS};
use crate::vmm::{AddressSpace, PAGE, USER_CODE_BASE, USER_END, USER_STACK_TOP};

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
/// Kernel stack used for traps coming from ring 3.
const KSTACK_SIZE: usize = 16 * 1024;
/// Default watchdog: 3 s at the 100 Hz system tick.
const DEFAULT_TIMEOUT_TICKS: u64 = 300;

pub const ENOSYS: i64 = 38;
pub const EBADF: i64 = 9;
pub const EFAULT: i64 = 14;

unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    ((hi as u64) << 32) | lo as u64
}

unsafe fn wrmsr(msr: u32, val: u64) {
    asm!("wrmsr", in("ecx") msr, in("eax") val as u32, in("edx") (val >> 32) as u32,
         options(nomem, nostack, preserves_flags));
}

// ------------------------------------------------------------ per-CPU block

/// Reached through GS while in the kernel.  The `syscall` entry stub uses
/// the two fixed offsets below.
#[repr(C)]
struct PerCpu {
    kstack_top: u64, // gs:[0]  kernel stack for syscalls
    user_rsp: u64,   // gs:[8]  scratch: user RSP during the entry sequence
}

static mut PERCPU: PerCpu = PerCpu { kstack_top: 0, user_rsp: 0 };

// ------------------------------------------------------------- saved context

/// Kernel context saved by `enter_user` and restored by `leave_user`
/// (a setjmp/longjmp pair): callee-saved registers, RSP and RFLAGS.
#[repr(C)]
struct KernCtx {
    rsp: u64,    // 0
    rbx: u64,    // 8
    rbp: u64,    // 16
    r12: u64,    // 24
    r13: u64,    // 32
    r14: u64,    // 40
    r15: u64,    // 48
    rflags: u64, // 56
}

static mut CTX: KernCtx = KernCtx { rsp: 0, rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0, rflags: 0 };

/// Save the kernel context, then `iretq` to ring 3 at `rip` with stack `rsp`.
/// Returns (to the caller, as if it were an ordinary call) when `leave_user`
/// is invoked.  Every general register is cleared so no kernel value leaks.
#[unsafe(naked)]
unsafe extern "C" fn enter_user(ctx: *mut KernCtx, rip: u64, rsp: u64) -> u64 {
    naked_asm!(
        "pushfq",
        "pop rax",
        "mov [rdi + 56], rax",
        "mov [rdi + 8], rbx",
        "mov [rdi + 16], rbp",
        "mov [rdi + 24], r12",
        "mov [rdi + 32], r13",
        "mov [rdi + 40], r14",
        "mov [rdi + 48], r15",
        "mov [rdi], rsp",          // points at the return address
        "cli",                     // no interrupt between swapgs and iretq
        "push {ds}",               // ss
        "push rdx",                // user rsp
        "push 0x202",              // rflags: IF set, IOPL 0
        "push {cs}",               // cs
        "push rsi",                // rip
        "xor eax, eax",
        "xor ebx, ebx",
        "xor ecx, ecx",
        "xor edx, edx",
        "xor esi, esi",
        "xor edi, edi",
        "xor ebp, ebp",
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",
        "xor r11d, r11d",
        "xor r12d, r12d",
        "xor r13d, r13d",
        "xor r14d, r14d",
        "xor r15d, r15d",
        "swapgs",
        "iretq",
        ds = const USER_DS,
        cs = const USER_CS,
    );
}

/// Abandon whatever kernel stack we are on and resume the caller of
/// `enter_user`.  Must be called in kernel context (GS already swapped).
#[unsafe(naked)]
unsafe extern "C" fn leave_user(ctx: *const KernCtx) -> ! {
    naked_asm!(
        "mov rsp, [rdi]",
        "mov rbx, [rdi + 8]",
        "mov rbp, [rdi + 16]",
        "mov r12, [rdi + 24]",
        "mov r13, [rdi + 32]",
        "mov r14, [rdi + 40]",
        "mov r15, [rdi + 48]",
        "push qword ptr [rdi + 56]",
        "popfq",                   // restores IF as the caller had it
        "ret",
    );
}

// ----------------------------------------------------------- syscall entry

/// `syscall` lands here with: rcx = user rip, r11 = user rflags, user rsp
/// untouched, CS/SS already kernel, interrupts masked (SFMASK).  The stub
/// builds an `IsrFrame` identical to the interrupt path's, calls the Rust
/// dispatcher and returns with `sysretq`.
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
        dispatch = sym syscall_dispatch,
    );
}

// ------------------------------------------------------------------ state

/// What happened to the last user program.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    None,
    Exited(u64),
    Fault { vector: u64, err: u64, rip: u64, cr2: u64 },
    Timeout,
}

struct Task {
    aspace: AddressSpace,
    kstack: *mut u8,
}

static mut TASK: Option<Task> = None;
static mut OUTCOME: Outcome = Outcome::None;
static mut DEADLINE: u64 = 0;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);

/// Which protections `init` managed to switch on.
#[derive(Clone, Copy)]
pub struct InitInfo {
    pub smep: bool,
    pub smap: bool,
    pub umip: bool,
}

/// Program the `syscall` MSRs and the GS-based per-CPU block.  Call after
/// the GDT is loaded (selector layout) and before the first `run`.
pub fn init() -> InitInfo {
    use crate::cpu::cpuid;

    let max_leaf = cpuid(0, 0).eax;
    let l7 = if max_leaf >= 7 { cpuid(7, 0) } else { crate::cpu::Cpuid { eax: 0, ebx: 0, ecx: 0, edx: 0 } };
    let smep = l7.ebx & (1 << 7) != 0;
    let smap = l7.ebx & (1 << 20) != 0;
    let umip = l7.ecx & (1 << 2) != 0;

    // SAFETY: privileged configuration at boot, single CPU, interrupts off
    // or irrelevant (nothing uses these MSRs yet).
    unsafe {
        wrmsr(MSR_EFER, rdmsr(MSR_EFER) | EFER_SCE);
        // SYSCALL: CS = STAR[47:32] (0x08), SS = +8 (0x10).
        // SYSRET64: SS = STAR[63:48] + 8 (0x18 -> 0x1b), CS = +16 (0x20 -> 0x23).
        wrmsr(MSR_STAR, (0x10u64 << 48) | ((KERNEL_CS as u64) << 32));
        wrmsr(MSR_LSTAR, syscall_entry as usize as u64);
        wrmsr(MSR_SFMASK, SFMASK);
        wrmsr(MSR_GS_BASE, addr_of_mut!(PERCPU) as u64);
        wrmsr(MSR_KERNEL_GS_BASE, 0);

        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        if smep {
            cr4 |= 1 << 20;
        }
        if smap {
            cr4 |= 1 << 21;
        }
        if umip {
            cr4 |= 1 << 11;
        }
        asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack, preserves_flags));
    }
    READY.store(true, Ordering::Release);
    InitInfo { smep, smap, umip }
}

pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

// ----------------------------------------------------------------- running

fn leave(o: Outcome) -> ! {
    // SAFETY: single CPU; called in kernel context (GS swapped) on the
    // kernel stack of the user task, which is abandoned here.
    unsafe {
        addr_of_mut!(OUTCOME).write(o);
        leave_user(addr_of!(CTX))
    }
}

/// Called by int.rs for every exception raised in ring 3: record it and end
/// the program.  NMI is not the program's fault and is left to the kernel.
pub fn on_user_exception(frame: &mut IsrFrame) {
    if frame.vector == 2 {
        return;
    }
    if !ACTIVE.load(Ordering::Acquire) {
        crate::kprintln!("\nring 3 exception with no user task (vector {})", frame.vector);
        crate::int::park();
    }
    let cr2: u64;
    // SAFETY: plain register read.
    unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags)) };
    leave(Outcome::Fault { vector: frame.vector, err: frame.errcode, rip: frame.rip, cr2 });
}

/// Called by int.rs after a timer tick that interrupted ring 3 (EOI done).
pub fn on_tick() {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: single CPU, scalar read.
    let deadline = unsafe { addr_of!(DEADLINE).read() };
    if crate::pit::ticks() >= deadline {
        leave(Outcome::Timeout);
    }
}

/// Run a flat program image in ring 3 and wait for it to end.
///
/// `image` is copied to USER_CODE_BASE (mapped r-x) and entered at its first
/// byte; the stack is a private rw- NX region ending at USER_STACK_TOP.
/// `timeout_ticks` is the watchdog (0 = default).
pub fn run(image: &[u8], timeout_ticks: u64) -> Result<Outcome, &'static str> {
    if !ready() {
        return Err("usermode not initialised");
    }
    if ACTIVE.load(Ordering::Acquire) {
        return Err("a user program is already running");
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

    let layout = Layout::from_size_align(KSTACK_SIZE, 4096).map_err(|_| "bad layout")?;
    // SAFETY: valid non-zero layout.
    let kstack = unsafe { alloc_zeroed(layout) };
    if kstack.is_null() {
        return Err("out of memory (kernel stack)");
    }
    let ktop = kstack as u64 + KSTACK_SIZE as u64;

    let flags = crate::io::cli_save();
    let cr3 = aspace.cr3();
    // SAFETY: single CPU, interrupts masked while the shared state is set up;
    // the kernel half of the new PML4 is shared so the running code, its
    // stack and the heap stay mapped across the CR3 load.
    unsafe {
        addr_of_mut!(TASK).write(Some(Task { aspace, kstack }));
        addr_of_mut!(OUTCOME).write(Outcome::None);
        let t = if timeout_ticks == 0 { DEFAULT_TIMEOUT_TICKS } else { timeout_ticks };
        addr_of_mut!(DEADLINE).write(crate::pit::ticks() + t);
        (*addr_of_mut!(PERCPU)).kstack_top = ktop;
        crate::int::set_rsp0(ktop);
        asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
        ACTIVE.store(true, Ordering::Release);

        enter_user(addr_of_mut!(CTX), USER_CODE_BASE, USER_STACK_TOP);

        // Back from ring 3 (leave_user): interrupts have the state they had
        // on entry; switch back to the kernel tables and release everything.
        ACTIVE.store(false, Ordering::Release);
        crate::vmm::activate_kernel();
        crate::int::set_rsp0(0);
        (*addr_of_mut!(PERCPU)).kstack_top = 0;
        let task = (*addr_of_mut!(TASK)).take();
        let outcome = addr_of!(OUTCOME).read();
        drop(task); // frees the address space (pages + tables)
        dealloc(kstack, layout);
        crate::io::restore_flags(flags);
        Ok(outcome)
    }
}

// ----------------------------------------------------------------- syscalls

/// Copy `len` user bytes starting at `buf` to the console, in chunks.
/// Returns the number of bytes written or -EFAULT when nothing was readable.
fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    if fd != 1 && fd != 2 {
        return -EBADF;
    }
    let len = len.min(4096) as usize;
    // SAFETY: single CPU; the task exists while ACTIVE.
    let Some(task) = (unsafe { (*addr_of_mut!(TASK)).as_ref() }) else { return -EFAULT };
    let mut chunk = [0u8; 256];
    let mut done = 0usize;
    while done < len {
        let n = (len - done).min(chunk.len());
        if !task.aspace.read(buf + done as u64, &mut chunk[..n]) {
            break;
        }
        put_user_bytes(&chunk[..n]);
        done += n;
    }
    if done == 0 && len > 0 {
        -EFAULT
    } else {
        done as i64
    }
}

/// Print user-supplied bytes: valid UTF-8 as is, control characters (other
/// than newline / tab) and invalid sequences as '?', so a program cannot
/// inject terminal escapes into the console.
fn put_user_bytes(mut b: &[u8]) {
    while !b.is_empty() {
        let (valid, skip, bad) = match core::str::from_utf8(b) {
            Ok(s) => (s, b.len(), false),
            Err(e) => {
                let v = e.valid_up_to();
                // SAFETY: from_utf8 vouches for the prefix.
                (unsafe { core::str::from_utf8_unchecked(&b[..v]) }, v + e.error_len().unwrap_or(b.len() - v), true)
            }
        };
        for ch in valid.chars() {
            if ch.is_control() && ch != '\n' && ch != '\t' {
                crate::kprint!("?");
            } else {
                crate::kprint!("{}", ch);
            }
        }
        if bad {
            crate::kprint!("?");
        }
        b = &b[skip.min(b.len())..];
    }
}

/// Rust side of the `syscall` stub; the result goes back in `frame.rax`.
extern "C" fn syscall_dispatch(frame: &mut IsrFrame) {
    let (a0, a1, a2) = (frame.rdi, frame.rsi, frame.rdx);
    let ret: i64 = match frame.rax {
        0 => leave(Outcome::Exited(a0)),
        1 => sys_write(a0, a1, a2),
        2 => 1, // single process for now
        3 => crate::pit::uptime_ms() as i64,
        _ => -ENOSYS,
    };
    frame.rax = ret as u64;
    // A user program can never reach non-canonical code addresses, but keep
    // the invariant that makes `sysretq` safe explicit.
    if frame.rip > USER_END {
        leave(Outcome::Fault { vector: 0x101, err: 0, rip: frame.rip, cr2: 0 });
    }
}

// ------------------------------------------------------------ test programs
//
// Tiny position-independent images, assembled here and copied into user
// memory by `run`.  They use only `syscall` (no libc, nothing else mapped).

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
    "    mov edx, UPROG_HELLO_LEN",
    "    syscall",
    "    ret",
    "uprog_hello_msg:",
    "    .ascii \"  [ring 3] hello from user mode\\n\"",
    "uprog_hello_msg_end:",
    ".equ UPROG_HELLO_LEN, uprog_hello_msg_end - uprog_hello_msg",
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
    "    mov eax, 2",                    // getpid -> 1
    "    syscall",
    "    cmp rax, 1",
    "    jne uprog_sys_f4",
    "    mov eax, 1",                    // write(1, buf, 0) -> 0
    "    mov edi, 1",
    "    lea rsi, [rip + uprog_sys_start]",
    "    xor edx, edx",
    "    syscall",
    "    test rax, rax",
    "    jne uprog_sys_f5",
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
    "uprog_sys_exit:",
    "    xor eax, eax",
    "    syscall",
    "uprog_sys_end:",
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
    timeout: u64,
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
pub static PROGRAMS: [Prog; 10] = [
    prog!("hello", "prints a line, exits with status 7", uprog_hello_start, uprog_hello_end, Expect::Exit(7), 0),
    prog!("sys", "syscall error paths (EFAULT, EBADF, ENOSYS, getpid)", uprog_sys_start, uprog_sys_end, Expect::Exit(0), 0),
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

    pub fn run(&self) -> Result<Outcome, &'static str> {
        run(self.image(), self.timeout)
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
        Outcome::Fault { vector, err, rip, cr2 } => crate::kprintln!(
            "killed by exception {} at rip {:#x} (errcode {:#x}, cr2 {:#x})",
            vector, rip, err, cr2
        ),
    }
}

/// Run every test program, check outcome and memory accounting.  Returns
/// (passed, total).
pub fn selftest() -> (usize, usize) {
    let mut ok = 0;
    // Address-space sanity: a fresh space shares the kernel half.
    match AddressSpace::new() {
        Some(a) if a.shares_kernel() => {
            crate::kprintln!("  {:8} ok (kernel half shared)", "aspace");
            ok += 1;
        }
        _ => crate::kprintln!("  {:8} FAILED (kernel half not shared)", "aspace"),
    }
    let mut total = 1;
    for p in PROGRAMS.iter() {
        total += 1;
        let before = crate::pmm::stats_snapshot().free_pages.load(Ordering::Relaxed);
        let res = p.run();
        let after = crate::pmm::stats_snapshot().free_pages.load(Ordering::Relaxed);
        match res {
            Ok(o) if p.check(o) && before == after => {
                crate::kprint!("  {:8} ok -- ", p.name);
                describe(o);
                ok += 1;
            }
            Ok(o) => {
                crate::kprint!("  {:8} FAILED -- ", p.name);
                describe(o);
                if before != after {
                    crate::kprintln!("           leaked {} page(s)", before.wrapping_sub(after) as i64);
                }
            }
            Err(e) => crate::kprintln!("  {:8} FAILED -- {}", p.name, e),
        }
    }
    (ok, total)
}
