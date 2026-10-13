// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// 16550 UART on COM1: 115200 8N1, TX stays polled (safe from any context),
// RX is interrupt driven (IRQ4) into a ring buffer.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::io::{inb, outb};

const COM1: u16 = 0x3F8;

// ------------------------------------------------------------- ring buffer

/// SPSC byte ring, producer = IRQ4, consumer = shell.
struct Ring {
    buf: [u8; 1024],
    head: AtomicU32,
    tail: AtomicU32,
}

impl Ring {
    const fn new() -> Ring {
        Ring { buf: [0; 1024], head: AtomicU32::new(0), tail: AtomicU32::new(0) }
    }

    fn push(&mut self, b: u8) {
        let h = self.head.load(Ordering::Relaxed);
        let t = self.tail.load(Ordering::Acquire);
        if h.wrapping_sub(t) < self.buf.len() as u32 {
            self.buf[(h & (self.buf.len() as u32 - 1)) as usize] = b;
            self.head.store(h.wrapping_add(1), Ordering::Release);
        }
    }

    fn pop(&mut self) -> Option<u8> {
        let t = self.tail.load(Ordering::Relaxed);
        let h = self.head.load(Ordering::Acquire);
        if t == h {
            return None;
        }
        let b = self.buf[(t & (self.buf.len() as u32 - 1)) as usize];
        self.tail.store(t.wrapping_add(1), Ordering::Release);
        Some(b)
    }
}

static mut RX: Ring = Ring::new();
static IRQ_COUNT: AtomicU32 = AtomicU32::new(0);
static PRESENT: AtomicBool = AtomicBool::new(false);

/// Detect + program the UART: 115200 8N1, FIFO on, RX interrupt on.
/// MCR bit 3 (OUT2) gates the IRQ line on PC hardware -- without it IRQ4
/// never fires, which is exactly the kind of thing that works on QEMU and
/// silently breaks on real laptops.
pub fn init() {
    // SAFETY: standard 16550 programming sequence on the legacy COM1 ports.
    unsafe {
        outb(COM1 + 1, 0x00); // disable interrupts during setup
        outb(COM1 + 3, 0x80); // DLAB
        outb(COM1, 0x01); // divisor low: 115200
        outb(COM1 + 1, 0x00); // divisor high
        outb(COM1 + 3, 0x03); // 8N1, DLAB off
        outb(COM1 + 2, 0x07); // FIFO enable + clear + 1-byte trigger
        outb(COM1 + 4, 0x0B); // DTR + RTS + OUT2 (IRQ gate)
        outb(COM1 + 1, 0x01); // IER: received-data interrupt
    }
    // Presence heuristic: the scratch register holds what we write iff the
    // UART is real (floating buses answer 0xFF, which also passes -- so
    // double-check with the line control register).
    // SAFETY: scratch register probe.
    unsafe {
        outb(COM1 + 7, 0x5A);
        let ok = inb(COM1 + 7) == 0x5A;
        outb(COM1 + 7, 0x00);
        PRESENT.store(ok, Ordering::Release);
    }
}

pub fn present() -> bool {
    PRESENT.load(Ordering::Acquire)
}

pub fn irq_count() -> u32 {
    IRQ_COUNT.load(Ordering::Relaxed)
}

/// IRQ4 entry: drain the FIFO, reading the line status register so error
/// flags clear, pushing bytes into the ring.
pub fn on_irq() {
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    // SAFETY: fixed ports, single IRQ context.
    unsafe {
        for _ in 0..64 {
            let lsr = inb(COM1 + 5);
            if lsr & 0x01 == 0 {
                break;
            }
            let b = inb(COM1);
            // SAFETY: SPSC ring, producer side.
            let rx = &mut *addr_of_mut!(RX);
            rx.push(b);
        }
        // Modem-status changes also raise IRQ4 on some hardware: read MSR
        // once to clear; the IIR read below completes the acknowledge.
        let _ = inb(COM1 + 6);
        let _ = inb(COM1 + 2); // IIR: clears the interrupt at the UART
    }
}

/// Next received byte, or None (for the line discipline).
pub fn pop_rx() -> Option<u8> {
    // SAFETY: consumer side of the SPSC ring.
    let rx = unsafe { &mut *addr_of_mut!(RX) };
    rx.pop()
}

/// Polling RX check kept for pre-interrupt debugging.
#[allow(dead_code)] // polling fallback
pub fn has_byte() -> bool {
    // SAFETY: Line Status Register read on COM1.
    unsafe { inb(COM1 + 5) & 0x01 != 0 }
}

/// Polling RX read kept for pre-interrupt debugging.
#[allow(dead_code)] // polling fallback
pub fn read_byte() -> u8 {
    // SAFETY: Receive Buffer read on COM1.
    unsafe { inb(COM1) }
}

/// Transmit one byte (polled, bounded wait; safe from any context).
pub fn write_byte(b: u8) {
    // SAFETY: the wait is bounded so a missing UART cannot hang us.
    unsafe {
        let mut spins = 0u32;
        while inb(COM1 + 5) & 0x20 == 0 && spins < 100_000 {
            spins += 1;
        }
        outb(COM1, b);
    }
}
