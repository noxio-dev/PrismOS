// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Port I/O primitives shared by the legacy device drivers (PS/2, 8042, 8259,
// PIT, RTC, 16550 UART).

use core::arch::asm;

#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let v: u8;
    asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags));
    v
}

#[inline]
pub unsafe fn outb(port: u16, val: u8) {
    asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

#[inline]
#[allow(dead_code)]
pub unsafe fn inw(port: u16) -> u16 {
    let v: u16;
    asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack, preserves_flags));
    v
}

#[inline]
#[allow(dead_code)]
pub unsafe fn outw(port: u16, val: u16) {
    asm!("out dx, ax", in("dx") port, in("ax") val, options(nomem, nostack, preserves_flags));
}

#[inline]
#[allow(dead_code)]
pub unsafe fn inl(port: u16) -> u32 {
    let v: u32;
    asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack, preserves_flags));
    v
}

#[inline]
#[allow(dead_code)]
pub unsafe fn outl(port: u16, val: u32) {
    asm!("out dx, eax", in("dx") port, in("eax") val, options(nomem, nostack, preserves_flags));
}

/// Small delay (~1 us) used to give slow legacy devices time to breathe
/// between back-to-back commands to the same port.
#[inline]
pub fn io_wait() {
    // SAFETY: a write to the unused port 0x80 (POST diagnostic) takes about
    // 1 us on classic hardware, which is exactly the point.
    unsafe { outb(0x80, 0) };
}

// ------------------------------------------------------------- interrupts

/// Read the interrupt flag (IF) from RFLAGS.
#[inline]
#[allow(dead_code)]
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    // SAFETY: plain flag register read.
    unsafe { asm!("pushfq\npop {}", out(reg) flags) };
    flags & 0x200 != 0
}

/// Disable interrupts; returns the previous RFLAGS so it can be restored.
/// NOTE: no `preserves_flags` here -- `cli` modifies RFLAGS by design.
#[inline]
pub fn cli_save() -> u64 {
    let flags: u64;
    // SAFETY: standard cli with the old RFLAGS captured first.
    unsafe { asm!("pushfq\npop {}\ncli", out(reg) flags) };
    flags
}

/// Restore RFLAGS (and with it the interrupt state) saved by `cli_save`.
#[inline]
pub unsafe fn restore_flags(flags: u64) {
    asm!("push {}\npopfq", in(reg) flags);
}

/// Disable interrupts for the lifetime of the guard.  Each guard restores the
/// RFLAGS it captured, so nesting works naturally: an inner guard taken while
/// interrupts are already masked saves IF=0 and keeps them masked on drop.
pub struct IrqGuard {
    flags: u64,
}

impl IrqGuard {
    #[inline]
    pub fn new() -> IrqGuard {
        IrqGuard { flags: cli_save() }
    }
}

impl Drop for IrqGuard {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: flags came from cli_save on the same stack.
        unsafe { restore_flags(self.flags) };
    }
}

/// Halt until the next interrupt.  Interrupts must be enabled for this to
/// ever return; the PIT keeps a 100 Hz heartbeat so even a quiet machine
/// wakes up regularly.
#[inline]
pub fn hlt() {
    // SAFETY: hlt is idempotent and safe in any privilege-0 context.
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
}
