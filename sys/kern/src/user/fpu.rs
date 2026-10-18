// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// FPU / SSE state (phase 2.4).  The kernel is compiled soft-float (it never
// touches x87/MMX/XMM registers), but compiled user programs do: every
// process therefore owns a 512-byte FXSAVE area, saved when the scheduler
// takes the CPU away from it and restored when it gets the CPU back.
//
// The switch is eager (no CR0.TS lazy trick): the kernel never uses the FPU
// itself, so "the registers belong to whoever ran in ring 3 last" is an
// invariant we can rely on -- `save` runs for the outgoing process before the
// incoming one is `restore`d, and a switch to the kernel just leaves the
// registers alone.  AVX/XSAVE is not enabled (CR4.OSXSAVE stays clear), so
// user code cannot touch state FXSAVE does not cover.

use core::arch::asm;

/// Default control words after reset / `fninit`: all x87 and SSE exceptions
/// masked, round to nearest, 64-bit precision.
const FCW_DEFAULT: u16 = 0x037F;
const MXCSR_DEFAULT: u32 = 0x1F80;

/// The FXSAVE image (the instruction demands 16-byte alignment).
#[repr(C, align(16))]
pub struct FpuState {
    area: [u8; 512],
}

impl FpuState {
    /// A pristine state: what a freshly started program sees.
    pub const fn new() -> FpuState {
        let mut area = [0u8; 512];
        let f = FCW_DEFAULT.to_le_bytes();
        area[0] = f[0];
        area[1] = f[1];
        let m = MXCSR_DEFAULT.to_le_bytes();
        area[24] = m[0];
        area[25] = m[1];
        area[26] = m[2];
        area[27] = m[3];
        FpuState { area }
    }

    /// Back to the pristine state (execve).
    pub fn reset(&mut self) {
        *self = FpuState::new();
    }

    /// Store the CPU's x87/SSE registers here.
    ///
    /// # Safety
    /// CR0.EM and CR0.TS must be clear and CR4.OSFXSR set (`init_cpu`).
    pub unsafe fn save(&mut self) {
        // SAFETY: `area` is 512 bytes, 16-byte aligned (repr(align)).
        unsafe { asm!("fxsave64 [{}]", in(reg) self.area.as_mut_ptr(), options(nostack, preserves_flags)) };
    }

    /// Load the CPU's x87/SSE registers from here.
    ///
    /// # Safety
    /// As `save`; the image must come from `new` / `save` (a corrupt MXCSR
    /// would raise #GP).
    pub unsafe fn restore(&self) {
        // SAFETY: as in `save`.
        unsafe { asm!("fxrstor64 [{}]", in(reg) self.area.as_ptr(), options(nostack, preserves_flags)) };
    }
}

/// Let ring 3 use x87 / SSE on this CPU: CR0.MP set, EM and TS clear,
/// CR4.OSFXSR (FXSAVE/FXRSTOR and SSE instructions) and OSXMMEXCPT.
pub fn init_cpu() {
    let cr0 = crate::arch::regs::read_cr0();
    let cr4 = crate::arch::regs::read_cr4();
    // SAFETY: boot-time control register setup on the BSP; MP = bit 1,
    // EM = bit 2, TS = bit 3; OSFXSR = CR4 bit 9, OSXMMEXCPT = bit 10.  The
    // FPU is then in a known state after `fninit`.
    unsafe {
        crate::arch::regs::write_cr0((cr0 | 0x2) & !0xC);
        crate::arch::regs::write_cr4(cr4 | (1 << 9) | (1 << 10));
        asm!("fninit", options(nomem, nostack));
    }
}
