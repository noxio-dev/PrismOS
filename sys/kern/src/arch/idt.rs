// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Interrupt descriptor table: 256 interrupt gates pointing at the naked stubs
// of intr.rs.  CPU exceptions use gates 0..=20 (#DF on IST1), the remapped
// 8259 lines 0x20..=0x2F, the LAPIC timer 0x30 and the spurious vector 0xFF.

use super::gdt::{PseudoDesc, KERNEL_CS};
use super::intr as isr;
use core::arch::asm;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, Ordering};

pub const N_VECTORS: usize = 256;

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

/// Build and load the IDT.  CPU exceptions get gates (#DF on IST1), IRQ
/// lines 0..15 get gates at 0x20..0x2F.
pub fn init() {
    set_gate(0, isr::stub_de, 0);
    set_gate(1, isr::stub_db, 0);
    set_gate(2, isr::stub_nmi, 0);
    set_gate(3, isr::stub_bp, 0);
    set_gate(4, isr::stub_of, 0);
    set_gate(5, isr::stub_br, 0);
    set_gate(6, isr::stub_ud, 0);
    set_gate(7, isr::stub_nm, 0);
    set_gate(8, isr::stub_df, 1); // double fault on the IST1 stack
    set_gate(9, isr::stub_co, 0);
    set_gate(10, isr::stub_ts, 0);
    set_gate(11, isr::stub_np, 0);
    set_gate(12, isr::stub_ss, 0);
    set_gate(13, isr::stub_gp, 0);
    set_gate(14, isr::stub_pf, 0);
    set_gate(15, isr::stub_rv, 0);
    set_gate(16, isr::stub_mf, 0);
    set_gate(17, isr::stub_ac, 0);
    set_gate(18, isr::stub_mc, 0);
    set_gate(19, isr::stub_xm, 0);
    set_gate(20, isr::stub_ve, 0);

    set_gate(0x20, isr::stub_irq0, 0);
    set_gate(0x21, isr::stub_irq1, 0);
    set_gate(0x22, isr::stub_irq2, 0);
    set_gate(0x23, isr::stub_irq3, 0);
    set_gate(0x24, isr::stub_irq4, 0);
    set_gate(0x25, isr::stub_irq5, 0);
    set_gate(0x26, isr::stub_irq6, 0);
    set_gate(0x27, isr::stub_irq7, 0);
    set_gate(0x28, isr::stub_irq8, 0);
    set_gate(0x29, isr::stub_irq9, 0);
    set_gate(0x2A, isr::stub_irq10, 0);
    set_gate(0x2B, isr::stub_irq11, 0);
    set_gate(0x2C, isr::stub_irq12, 0);
    set_gate(0x2D, isr::stub_irq13, 0);
    set_gate(0x2E, isr::stub_irq14, 0);
    set_gate(0x2F, isr::stub_irq15, 0);

    set_gate(0x30, isr::stub_apic_timer, 0);
    set_gate(0xFF, isr::stub_spurious, 0);

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
