// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// CPU interrupt infrastructure: GDT (+ TSS), IDT, exception handlers with a
// full register dump, and the IRQ dispatch table.
//
// Selector map (compatible with the loader's GDT, so the cached CS stays
// valid across the swap):
//   0x08  kernel code (64-bit)
//   0x10  kernel data
//   0x18  user code (64-bit, for a future ring 3)
//   0x20  user data
//   0x28  TSS
//
// Entry path: IDT gate -> naked stub -> isr_common -> isr_handler(frame).

use core::arch::{asm, naked_asm};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
#[allow(dead_code)] // ring 3 later
pub const USER_CS: u16 = 0x18;
#[allow(dead_code)] // ring 3 later
pub const USER_DS: u16 = 0x20;
pub const TSS_SEL: u16 = 0x28;

pub const N_VECTORS: usize = 256;
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

// --------------------------------------------------------------------- GDT

// The GDT is an array of 8-byte words: code/data descriptors take one word
// each, the TSS descriptor takes two consecutive words (selector 0x28 spans
// words 5 and 6).  Total 8 words, GDTR limit = 63.

const GDT_WORDS: usize = 8;

/// Classic flat 64-bit code/data descriptor: base 0, limit 0xFFFFF,
/// 4 KiB granularity -- one 8-byte word:
///   bytes 0-1  limit[15:0]     bytes 2-4  base[23:0] (zero)
///   byte  5    access          byte  6    flags<<4 | limit[19:16]
///   byte  7    base[31:24]     (zero)
const fn flat(access: u8, flags: u8) -> u64 {
    0xFFFFu64 // limit[15:0]
        | ((access as u64) << 40)
        | (0xFu64 << 48) // limit[19:16]
        | ((flags as u64) << 52)
}

/// 16-byte system-segment descriptor pointing at the TSS -> two words:
///   word 0: limit[15:0] | base[23:0]<<16 | access(0x89)<<40 | (flags+limit[19:16])<<48 | base[31:24]<<56
///   word 1: base[63:32]
const fn tss_low(base: u64, limit: u32) -> u64 {
    (limit & 0xFFFF) as u64
        | ((base & 0xFF_FFFF) << 16)
        | (0x89u64 << 40) // present, 64-bit TSS available
        | (((limit >> 16) & 0xF) as u64) << 48
        | (((base >> 24) & 0xFF) as u64) << 56
}

const fn tss_high(base: u64) -> u64 {
    (base >> 32) & 0xFFFF_FFFF
}

/// Hardware task state (64-bit layout, 100 bytes packed).  Only RSP0 (a
/// future ring 3) and IST1 (double fault) are used.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Tss {
    reserved1: u32,       // 0x00
    rsp0: u64,            // 0x04
    rsp1: u64,            // 0x0C
    rsp2: u64,            // 0x14
    reserved2: u64,       // 0x1C
    ist: [u64; 7],        // 0x24
    reserved3: u32,       // 0x5C
    reserved4: u16,       // 0x60
    iopb: u16,            // 0x62
}

/// LGDT/LIDT pseudo-descriptor (64-bit mode): u16 limit + u64 base packed,
/// base at byte offset 2.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct PseudoDesc {
    limit: u16,
    base: u64,
}

#[repr(C, align(16))]
struct Gates {
    gdt: [u64; GDT_WORDS],
    tss: Tss,
    gdtr: PseudoDesc,
    df_stack: [u8; 8192], // IST1: dedicated #DF stack
}

static mut GATES: Gates = Gates {
    gdt: [
        0,                     // 0x00 null
        flat(0x9A, 0xA),       // 0x08 kernel code (L=1)
        flat(0x92, 0xC),       // 0x10 kernel data
        flat(0xFA, 0xA),       // 0x18 user code
        flat(0xF2, 0xC),       // 0x20 user data
        0,                     // 0x28 TSS low (patched at init)
        0,                     //      TSS high
        0,                     // padding
    ],
    tss: Tss {
        reserved1: 0,
        rsp0: 0,
        rsp1: 0,
        rsp2: 0,
        reserved2: 0,
        ist: [0; 7],
        reserved3: 0,
        reserved4: 0,
        iopb: 100, // >= TSS limit: no I/O permission bitmap
    },
    gdtr: PseudoDesc { limit: (GDT_WORDS * 8 - 1) as u16, base: 0 },
    df_stack: [0; 8192],
};

/// Install the GDT/TSS.  Selector values match the loader's, so the live CS
/// descriptor stays valid; only the data registers are reloaded, plus LTR.
///
/// The table is copied to a fixed low (identity-mapped) address first:
/// every code path that runs before our own page tables are active can
/// reach it, and the descriptor reads during segment loads stay simple.
pub fn gdt_init() {
    // SAFETY: single-CPU boot context, no concurrency yet.
    let g = unsafe { &mut *addr_of_mut!(GATES) };
    let tss_addr = &g.tss as *const Tss as u64;
    g.gdt[5] = tss_low(tss_addr, (core::mem::size_of::<Tss>() - 1) as u32);
    g.gdt[6] = tss_high(tss_addr);
    g.tss.rsp0 = 0; // no ring 3 yet
    g.tss.ist[0] = g.df_stack.as_ptr() as u64 + g.df_stack.len() as u64;
    g.gdtr.base = g.gdt.as_ptr() as u64;

    // SAFETY: the tables live in the kernel image (the PMM never hands
    // those pages out) and are mapped before and after the CR3 switch.
    unsafe {
        asm!("lgdt [{}]", in(reg) &g.gdtr as *const PseudoDesc, options(preserves_flags));
        asm!(
            "mov ax, {ds}\nmov ds, ax\nmov es, ax\nmov ss, ax\nmov gs, ax\nmov fs, ax",
            ds = const KERNEL_DS,
            out("ax") _,
        );
        asm!("ltr ax", in("ax") TSS_SEL, options(preserves_flags));
    }
}

// --------------------------------------------------------------------- IDT

#[repr(C, align(16))]
struct Idt {
    entries: [[u32; 4]; N_VECTORS],
    idtr: PseudoDesc,
}

static mut IDT: Idt = Idt { entries: [[0; 4]; N_VECTORS], idtr: PseudoDesc { limit: 0, base: 0 } };
static IDT_READY: AtomicBool = AtomicBool::new(false);

/// Install a gate pointing at `handler`: interrupt gate (IF cleared on
/// entry), DPL 0, optionally on an IST stack.
fn set_gate(vector: u8, handler: unsafe extern "C" fn(), ist: u8) {
    let h = handler as *const () as u64;
    // SAFETY: called only during boot with interrupts disabled.
    let idt = unsafe { &mut *addr_of_mut!(IDT) };
    let e = &mut idt.entries[vector as usize];
    e[0] = (h & 0xFFFF) as u32 | (KERNEL_CS as u32) << 16;
    e[1] = (((h >> 16) & 0xFFFF) as u32) << 16 // offset[31:16]
        | ((ist & 7) as u32) // IST index
        | 0x8000 // present
        | 0x0E00; // type: 64-bit interrupt gate (IF cleared on entry)
    e[2] = (h >> 32) as u32;
    e[3] = 0;
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
        unsafe extern "C" fn $name() {
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
        unsafe extern "C" fn $name() {
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
/// `IsrFrame` field order; the pops mirror it exactly.
#[unsafe(naked)]
unsafe extern "C" fn isr_common() {
    naked_asm!(
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
            crate::pic::eoi(irq);
            // SAFETY: IRQ table is written only at init with interrupts off.
            let table = unsafe { &*addr_of_mut!(IRQ_TABLE) };
            if let Some(h) = table[irq as usize] {
                h(frame);
            }
        }
        _ => {
            crate::kprintln!("\nunexpected interrupt vector {}", v);
            dump_frame(frame);
            park();
        }
    }
}

/// Exception entry: a few are recoverable test hooks, the rest are fatal.
fn handle_exception(frame: &mut IsrFrame) {
    let v = frame.vector as u8;
    match v {
        3 => {
            // int3 smoke test: #BP is a trap, the pushed RIP already sits
            // past the one-byte instruction -- just report and continue.
            crate::kprintln!("\ntrap #BP (int3) at {:#x}: handled, continuing", frame.rip);
        }
        14 => crate::vmm::handle_page_fault(frame),
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
                let cr2: u64;
                // SAFETY: plain register read.
                unsafe { asm!("mov {}, cr2", out(reg) cr2, options(nomem, preserves_flags)) };
                crate::kprintln!("cr2 = {:#x}", cr2);
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

/// Build and load the IDT.  CPU exceptions get gates (#DF on IST1), IRQ
/// lines 0..15 get gates at 0x20..0x2F.
pub fn idt_init() {
    set_gate(0, stub_de, 0);
    set_gate(1, stub_db, 0);
    set_gate(2, stub_nmi, 0);
    set_gate(3, stub_bp, 0);
    set_gate(4, stub_of, 0);
    set_gate(5, stub_br, 0);
    set_gate(6, stub_ud, 0);
    set_gate(7, stub_nm, 0);
    set_gate(8, stub_df, 1); // double fault on the IST1 stack
    set_gate(9, stub_co, 0);
    set_gate(10, stub_ts, 0);
    set_gate(11, stub_np, 0);
    set_gate(12, stub_ss, 0);
    set_gate(13, stub_gp, 0);
    set_gate(14, stub_pf, 0);
    set_gate(15, stub_rv, 0);
    set_gate(16, stub_mf, 0);
    set_gate(17, stub_ac, 0);
    set_gate(18, stub_mc, 0);
    set_gate(19, stub_xm, 0);
    set_gate(20, stub_ve, 0);

    set_gate(0x20, stub_irq0, 0);
    set_gate(0x21, stub_irq1, 0);
    set_gate(0x22, stub_irq2, 0);
    set_gate(0x23, stub_irq3, 0);
    set_gate(0x24, stub_irq4, 0);
    set_gate(0x25, stub_irq5, 0);
    set_gate(0x26, stub_irq6, 0);
    set_gate(0x27, stub_irq7, 0);
    set_gate(0x28, stub_irq8, 0);
    set_gate(0x29, stub_irq9, 0);
    set_gate(0x2A, stub_irq10, 0);
    set_gate(0x2B, stub_irq11, 0);
    set_gate(0x2C, stub_irq12, 0);
    set_gate(0x2D, stub_irq13, 0);
    set_gate(0x2E, stub_irq14, 0);
    set_gate(0x2F, stub_irq15, 0);

    // SAFETY: building and loading our own table with interrupts disabled.
    unsafe {
        let idt = &mut *addr_of_mut!(IDT);
        idt.idtr.limit = (N_VECTORS * 16 - 1) as u16;
        idt.idtr.base = idt.entries.as_ptr() as u64;
        asm!("lidt [{}]", in(reg) &idt.idtr as *const PseudoDesc, options(preserves_flags));
    }
    IDT_READY.store(true, Ordering::Release);
}

#[allow(dead_code)]
pub fn idt_ready() -> bool {
    IDT_READY.load(Ordering::Acquire)
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
