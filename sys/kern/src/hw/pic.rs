// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Intel 8259 programmable interrupt controller, remapped to vectors
// 0x20..0x2F so the CPU exceptions keep 0x00..0x1F.  All lines are masked
// at init; drivers unmask what they own via `unmask`.

use core::sync::atomic::{AtomicU8, Ordering};

use crate::arch::io::{inb, io_wait, outb};

const MASTER_CMD: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_CMD: u16 = 0xA0;
const SLAVE_DATA: u16 = 0xA1;

pub const IRQ_TIMER: u8 = 0;
pub const IRQ_KBD: u8 = 1;
pub const IRQ_CASCADE: u8 = 2;
pub const IRQ_COM1: u8 = 4;

/// Live mask (what we believe is unmasked), for `sh` introspection.
static MASK: AtomicU8 = AtomicU8::new(0xFF);

/// Remap both PICs to 0x20..0x2F and mask every line.
pub fn init() {
    // SAFETY: standard 8259 initialization sequence (ICW1..ICW4).
    unsafe {
        outb(MASTER_CMD, 0x11); // ICW1: init, cascade mode, ICW4 wanted
        io_wait();
        outb(SLAVE_CMD, 0x11);
        io_wait();
        outb(MASTER_DATA, 0x20); // ICW2: master vectors 0x20..0x27
        io_wait();
        outb(SLAVE_DATA, 0x28); // ICW2: slave vectors 0x28..0x2F
        io_wait();
        outb(MASTER_DATA, 0x04); // ICW3: slave on IRQ2
        io_wait();
        outb(SLAVE_DATA, 0x02); // ICW3: cascades through IRQ2
        io_wait();
        outb(MASTER_DATA, 0x01); // ICW4: 8086 mode
        io_wait();
        outb(SLAVE_DATA, 0x01);
        io_wait();
        outb(MASTER_DATA, 0xFF); // mask everything for now
        io_wait();
        outb(SLAVE_DATA, 0xFF);
    }
    MASK.store(0xFF, Ordering::Release);
}

/// Unmask one line (bit = 1 means masked on the PIC).
pub fn unmask(irq: u8) {
    let port = if irq < 8 { MASTER_DATA } else { SLAVE_DATA };
    let bit = if irq < 8 { irq } else { irq - 8 };
    // SAFETY: single-byte data port write, race-free while IRQs are masked.
    unsafe {
        let m = inb(port);
        outb(port, m & !(1 << bit));
    }
    MASK.fetch_and(!(1 << irq), Ordering::Release);
    if irq >= 8 {
        unmask(IRQ_CASCADE);
    }
}

/// Mask one line.
#[allow(dead_code)]
pub fn mask(irq: u8) {
    let port = if irq < 8 { MASTER_DATA } else { SLAVE_DATA };
    let bit = if irq < 8 { irq } else { irq - 8 };
    // SAFETY: as above.
    unsafe {
        let m = inb(port);
        outb(port, m | (1 << bit));
    }
    MASK.fetch_or(1 << irq, Ordering::Release);
}

/// End of interrupt.  For lines 8..15 the slave must be acknowledged first,
/// then the master.
pub fn eoi(irq: u8) {
    // SAFETY: plain EOI command writes.
    unsafe {
        if irq >= 8 {
            outb(SLAVE_CMD, 0x20);
        }
        outb(MASTER_CMD, 0x20);
    }
}

/// Spurious IRQ7/IRQ15 check: an IRQ is spurious when the corresponding
/// in-service bit is clear.  A spurious IRQ15 still needs a master EOI.
/// Returns true when the IRQ was real.
#[allow(dead_code)] // spurious IRQ logging comes later
pub fn is_spurious(irq: u8) -> bool {
    let (cmd, bit) = match irq {
        7 => (MASTER_CMD, 7u8),
        15 => (SLAVE_CMD, 7u8),
        _ => return false,
    };
    // SAFETY: OCW3 read of the in-service register.
    unsafe {
        outb(cmd, 0x0B); // OCW3: select ISR on read
        let isr = inb(cmd);
        outb(cmd, 0x0A); // OCW3: back to IRR (harmless default)
        isr & (1 << bit) == 0
    }
}

pub fn mask_state() -> u8 {
    MASK.load(Ordering::Acquire)
}
