// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Interrupt entry and dispatch: ISR stubs, the common prologue/epilogue,
// exception handlers with a full register dump, and the IRQ dispatch table.
// The tables themselves live in gdt.rs / tss.rs / idt.rs.
//
// Entry path: IDT gate -> naked stub -> isr_common -> isr_handler(frame).
//
// Ring 3: `isr_common` swaps GS whenever the trapped context comes from user
// mode (the saved CS at [rsp+24] has RPL 3), so the kernel always runs with
// its own GS base, and swaps back on the way out (see user/usermode.rs for
// the per-CPU block and the GS convention).  Any exception raised in ring 3
// is routed to user/process.rs, which kills the program instead of the
// kernel.

use core::arch::{asm, naked_asm};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, Ordering};

#[allow(dead_code)]
pub const IRQ_BASE: u8 = 0x20; // after the 8259 remap
pub const N_IRQ: usize = 16;

// ------------------------------------------------------------------- frame

/// Register state pushed by the stub prologue.  `dummy` exists so the whole
/// register block is a multiple of 16 bytes: the CPU guarantees a 16-byte
/// aligned RSP at interrupt entry, the stubs push 16 bytes (error code +
/// vector) and the prologue 128 bytes of registers, so the `call` to the
/// dispatcher happens on an ABI-aligned stack.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IsrFrame {
    pub dummy: u64,
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub vector: u64,
    pub errcode: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// ------------------------------------------------------------ IRQ registry

pub type IrqHandler = fn(&mut IsrFrame);

static mut IRQ_TABLE: [Option<IrqHandler>; N_IRQ] = [None; N_IRQ];

/// Register `handler` for IRQ `irq` (0..16).  Call before unmasking the line.
pub fn register_irq(irq: u8, handler: IrqHandler) {
    assert!((irq as usize) < N_IRQ, "irq out of range");
    // SAFETY: boot-time or IRQ-masked context.
    unsafe { (*addr_of_mut!(IRQ_TABLE))[irq as usize] = Some(handler) };
}

/// Delivery count per IRQ line, for `sh` introspection.
pub static IRQ_COUNTS: [AtomicU32; N_IRQ] = [const { AtomicU32::new(0) }; N_IRQ];

// ------------------------------------------------------------------ stubs

/// Stub for vectors without a CPU-pushed error code: push a 0 placeholder
/// first, then the vector number, then jump to the common prologue.  Final
/// stack (low to high): vector, error, rip, cs, rflags, rsp, ss.
macro_rules! stub {
    ($name:ident, $vec:expr) => {
        #[unsafe(naked)]
        pub(super) unsafe extern "C" fn $name() {
            naked_asm!(
                "push 0",
                "push {}",
                "jmp {}",
                const $vec,
                sym isr_common,
            );
        }
    };
}

/// Stub for vectors the CPU pushes an error code for (8, 10-14, 17): the
/// code is already under RIP, only the vector number is needed.
macro_rules! stub_err {
    ($name:ident, $vec:expr) => {
        #[unsafe(naked)]
        pub(super) unsafe extern "C" fn $name() {
            naked_asm!(
                "push {}",
                "jmp {}",
                const $vec,
                sym isr_common,
            );
        }
    };
}

// CPU exceptions 0..=20.
stub!(stub_de, 0);
stub!(stub_db, 1);
stub!(stub_nmi, 2);
stub!(stub_bp, 3);
stub!(stub_of, 4);
stub!(stub_br, 5);
stub!(stub_ud, 6);
stub!(stub_nm, 7);
stub_err!(stub_df, 8);
stub!(stub_co, 9);
stub_err!(stub_ts, 10);
stub_err!(stub_np, 11);
stub_err!(stub_ss, 12);
stub_err!(stub_gp, 13);
stub_err!(stub_pf, 14);
stub!(stub_rv, 15);
stub!(stub_mf, 16);
stub_err!(stub_ac, 17);
stub!(stub_mc, 18);
stub!(stub_xm, 19);
stub!(stub_ve, 20);

// LAPIC timer and spurious vector (apic.rs).
stub!(stub_apic_timer, 0x30);
stub!(stub_spurious, 0xFF);

// AHCI disk completion (MSI, phase 1.5).
stub!(stub_ahci, 0x31);

// SMP (phase 1.5): per-AP LAPIC timer and inter-processor interrupts.
stub!(stub_ap_timer, 0x32);
stub!(stub_ap_ipi, 0x33);

// IRQ lines 0..15 after the 8259 remap.
stub!(stub_irq0, 0x20);
stub!(stub_irq1, 0x21);
stub!(stub_irq2, 0x22);
stub!(stub_irq3, 0x23);
stub!(stub_irq4, 0x24);
stub!(stub_irq5, 0x25);
stub!(stub_irq6, 0x26);
stub!(stub_irq7, 0x27);
stub!(stub_irq8, 0x28);
stub!(stub_irq9, 0x29);
stub!(stub_irq10, 0x2A);
stub!(stub_irq11, 0x2B);
stub!(stub_irq12, 0x2C);
stub!(stub_irq13, 0x2D);
stub!(stub_irq14, 0x2E);
stub!(stub_irq15, 0x2F);

/// Common prologue/epilogue (naked).  The push order is the reverse of the
/// `IsrFrame` field order; the pops mirror it exactly.  The conditional
/// `swapgs` pair (CS RPL 3 test) keeps the kernel on its own GS base while
// it handles a trap from ring 3, and hands the user's value back before
/// `iretq`.
#[unsafe(naked)]
unsafe extern "C" fn isr_common() {
    naked_asm!(
        "test qword ptr [rsp + 24], 3", // saved CS RPL 3?  (cs is [rsp+24])
        "jz 2f",
        "swapgs",
        "2:",
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
        "push 0",            // dummy: keeps the stack 16-byte aligned for the call
        "mov rdi, rsp",
        "cld",
        "call {}",
        "add rsp, 8",        // drop dummy
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
        "add rsp, 16",       // drop vector + error code
        "test qword ptr [rsp + 8], 3", // returning to ring 3?  (cs at [rsp+8])
        "jz 3f",
        "swapgs",
        "3:",
        "iretq",
        sym isr_handler,
    );
}

// ---------------------------------------------------------------- dispatch

fn exception_name(v: u8) -> &'static str {
    match v {
        0 => "#DE divide error",
        1 => "#DB debug",
        2 => "NMI",
        3 => "#BP breakpoint",
        4 => "#OF overflow",
        5 => "#BR bound range",
        6 => "#UD invalid opcode",
        7 => "#NM device not available",
        8 => "#DF double fault",
        9 => "coprocessor segment overrun",
        10 => "#TS invalid tss",
        11 => "#NP segment not present",
        12 => "#SS stack fault",
        13 => "#GP general protection",
        14 => "#PF page fault",
        15 => "reserved",
        16 => "#MF x87",
        17 => "#AC alignment check",
        18 => "#MC machine check",
        19 => "#XM simd",
        20 => "#VE virtualization",
        _ => "exception",
    }
}

/// The Rust dispatcher called from `isr_common`.
extern "C" fn isr_handler(frame: &mut IsrFrame) {
    let v = frame.vector as u8;
    match v {
        0..=31 => handle_exception(frame),
        0x20..=0x2F => {
            let irq = v - 0x20;
            IRQ_COUNTS[irq as usize].fetch_add(1, Ordering::Relaxed);
            // Acknowledge the PIC first so the line can fire again while
            // the handler runs (classic EOI-first discipline).
            crate::hw::apic::eoi(irq);
            // SAFETY: IRQ table is written only at init with interrupts off.
            let table = unsafe { &*addr_of_mut!(IRQ_TABLE) };
            if let Some(h) = table[irq as usize] {
                h(frame);
            }
        }
        0x30 => crate::hw::apic::on_timer(frame.cs & 3 == 3),
        0x31 => {
            crate::hw::ahci::on_irq();
            crate::hw::apic::lapic_eoi();
        }
        0x32 => {
            crate::hw::smp::on_ap_timer();
            crate::hw::apic::lapic_eoi();
        }
        0x33 => {
            crate::hw::smp::on_ipi();
            crate::hw::apic::lapic_eoi();
        }
        0xFF => {} // spurious: no EOI by definition
        _ => {
            crate::kprintln!("\nunexpected interrupt vector {}", v);
            dump_frame(frame);
            park();
        }
    }
}

/// Exception entry: a few are recoverable test hooks, the rest are fatal.
/// Anything raised in ring 3 is the program's own fault: the process is
/// killed and the kernel returns to the scheduler (never a kernel panic).
/// Exception, since phase 2.5, a write #PF on a copy-on-write page (a fork
/// clone, mem/vmm.rs): the fault is serviced and the instruction retried
/// instead of ending the program.
fn handle_exception(frame: &mut IsrFrame) {
    let v = frame.vector as u8;
    if frame.cs & 3 == 3 && v != 2 {
        // NMI is never the program's fault; everything else ends it.
        // A *write* to a present page may be copy-on-write (a fork clone,
        // mem/vmm.rs): service it and retry the instruction instead.
        if v == 14 && frame.errcode & 0x2 != 0
            && crate::user::process::handle_cow_current(super::regs::read_cr2())
        {
            return;
        }
        crate::user::process::on_user_exception(frame);
    }
    match v {
        3 => {
            // int3 smoke test: #BP is a trap, the pushed RIP already sits
            // past the one-byte instruction -- just report and continue.
            crate::kprintln!("\ntrap #BP (int3) at {:#x}: handled, continuing", frame.rip);
        }
        14 => crate::mem::vmm::handle_page_fault(frame),
        0 => {
            // #DE from the `crash div` test: the instruction is a 3-byte
            // `div rax` (48 f7 f0).  Emulate "result 0" and move on.
            crate::kprintln!("\nexception #DE at {:#x}: divide by zero", frame.rip);
            frame.rax = 0;
            frame.rdx = 0;
            frame.rip += 3;
        }
        _ => {
            crate::kprintln!(
                "\nexception {} ({}) at {:#x}, errcode {:#x}",
                v,
                exception_name(v),
                frame.rip,
                frame.errcode
            );
            if v == 14 {
                crate::kprintln!("cr2 = {:#x}", super::regs::read_cr2());
            }
            dump_frame(frame);
            park();
        }
    }
}

fn dump_frame(f: &IsrFrame) {
    crate::kprintln!(
        "rax {:#018x}  rbx {:#018x}  rcx {:#018x}\n\
         rdx {:#018x}  rsi {:#018x}  rdi {:#018x}\n\
         rbp {:#018x}  rsp {:#018x}  rfl {:#018x}\n\
         r8  {:#018x}  r9  {:#018x}  r10 {:#018x}\n\
         r11 {:#018x}  r12 {:#018x}  r13 {:#018x}\n\
         r14 {:#018x}  r15 {:#018x}\n\
         rip {:#018x}  cs  {:#06x}    ss  {:#06x}",
        f.rax, f.rbx, f.rcx, f.rdx, f.rsi, f.rdi, f.rbp, f.rsp, f.rflags, f.r8, f.r9, f.r10,
        f.r11, f.r12, f.r13, f.r14, f.r15, f.rip, f.cs, f.ss
    );
}

/// Interrupts off, halt forever.  Used after fatal exceptions.
pub fn park() -> ! {
    crate::kprintln!("machine parked (cli; hlt) -- safe to power off or reboot");
    loop {
        // SAFETY: deliberate final halt with interrupts masked.
        unsafe { asm!("cli\nhlt", options(nomem, nostack)) };
    }
}

/// Sleep until the next interrupt, from a context that runs with IF=0:
/// `sti; hlt` (the CPU delays interrupt delivery by one instruction after
/// `sti`, so the wake-up cannot slip between the two), then `cli` again.
/// Used by the scheduler's idle loop while only sleeping processes remain.
#[inline]
pub fn idle_wait() {
    // SAFETY: deliberate enable/halt/disable sequence, IF is 0 again after.
    unsafe { asm!("sti", "hlt", "cli", options(nomem, nostack)) };
}

/// Enable interrupts.  (No `preserves_flags`: `sti` modifies RFLAGS.)
#[inline]
pub fn sti() {
    // SAFETY: deliberate interrupt enable.
    unsafe { asm!("sti", options(nomem, nostack)) };
}

/// Disable interrupts.  (No `preserves_flags`: `cli` modifies RFLAGS.)
#[inline]
pub fn cli() {
    // SAFETY: deliberate interrupt disable.
    unsafe { asm!("cli", options(nomem, nostack)) };
}
