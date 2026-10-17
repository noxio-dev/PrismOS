// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Control and debug register access, plus RFLAGS / RSP snapshots.  Ring 0
// only.  The raw `asm!` blocks live here so the rest of the kernel (the
// exception handlers, the page-fault path, the shell) stays free of them.

#![allow(dead_code)]

use core::arch::asm;

macro_rules! read_reg {
    ($(#[$m:meta])* $name:ident, $reg:literal) => {
        $(#[$m])*
        #[inline]
        pub fn $name() -> u64 {
            let v: u64;
            // SAFETY: plain ring-0 register read, no side effects.
            unsafe { asm!(concat!("mov {}, ", $reg), out(reg) v, options(nomem, preserves_flags)) };
            v
        }
    };
}

macro_rules! write_reg {
    ($(#[$m:meta])* $name:ident, $reg:literal) => {
        $(#[$m])*
        ///
        /// # Safety
        /// Changes global CPU state; the caller must know the new value is valid.
        #[inline]
        pub unsafe fn $name(v: u64) {
            asm!(concat!("mov ", $reg, ", {}"), in(reg) v, options(nostack, preserves_flags));
        }
    };
}

read_reg!(read_cr0, "cr0");
read_reg!(
    /// Faulting linear address of the last #PF.
    read_cr2, "cr2"
);
read_reg!(read_cr3, "cr3");
read_reg!(read_cr4, "cr4");
write_reg!(write_cr0, "cr0");
write_reg!(
    /// Switching CR3 flushes the non-global TLB entries.
    write_cr3, "cr3"
);
write_reg!(write_cr4, "cr4");

read_reg!(read_dr0, "dr0");
read_reg!(read_dr1, "dr1");
read_reg!(read_dr2, "dr2");
read_reg!(read_dr3, "dr3");
read_reg!(read_dr6, "dr6");
read_reg!(read_dr7, "dr7");
write_reg!(write_dr0, "dr0");
write_reg!(write_dr1, "dr1");
write_reg!(write_dr2, "dr2");
write_reg!(write_dr3, "dr3");
write_reg!(write_dr6, "dr6");
write_reg!(write_dr7, "dr7");

/// Current RFLAGS (pushfq/pop on the live stack).
#[inline]
pub fn read_rflags() -> u64 {
    let v: u64;
    // SAFETY: pushfq/pop uses the live stack and leaves RFLAGS untouched.
    unsafe { asm!("pushfq\npop {}", out(reg) v, options(preserves_flags)) };
    v
}

/// Current stack pointer.
#[inline]
pub fn read_rsp() -> u64 {
    let v: u64;
    // SAFETY: plain register read.
    unsafe { asm!("mov {}, rsp", out(reg) v, options(nomem, preserves_flags)) };
    v
}
