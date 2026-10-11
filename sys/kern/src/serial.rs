// SPDX-License-Identifier: GPL-3.0-or-later
//
// 16550 UART on COM1, 115200 8N1.

use core::arch::asm;

const COM1: u16 = 0x3F8;

#[inline]
unsafe fn outb(port: u16, val: u8) {
    asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

#[inline]
unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags));
    v
}

pub fn init() {
    // SAFETY: standard UART programming sequence on the legacy COM1 ports.
    unsafe {
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x80);
        outb(COM1, 0x01);
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03);
        outb(COM1 + 2, 0xC7);
        outb(COM1 + 4, 0x0B);
    }
}

pub fn write_byte(b: u8) {
    // SAFETY: port I/O on COM1; the wait is bounded so a missing UART cannot hang us.
    unsafe {
        let mut spins = 0u32;
        while inb(COM1 + 5) & 0x20 == 0 && spins < 100_000 {
            spins += 1;
        }
        outb(COM1, b);
    }
}
