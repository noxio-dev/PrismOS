// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// A thin hardware access layer shared by the NVMe and USB drivers: memory
// mapped register access, DMA memory, and the wait / sleep primitives that
// every "poke a register, wait for the device" loop needs.
//
// Everything that touches the machine is a one-line primitive at the top of
// this file.  They exist as a seam: the drivers only ever go through them, so
// `tests/hw_host` can build the very same driver sources on the host
// (`--cfg hostsim`) and plug a software model of an NVMe or xHCI controller
// behind the register accesses.  The kernel build never sets that cfg.

#![allow(dead_code, unexpected_cfgs)]

use core::cell::UnsafeCell;
use core::sync::atomic::{fence, Ordering};

pub const PAGE: usize = 4096;

// -------------------------------------------------------------- global cell

/// Interior-mutable global for driver state.  The drivers document, at each
/// use, which contexts may touch which field: this is only the escape hatch
/// that lets a `static` hold state the compiler cannot prove exclusive.
pub struct Global<T>(UnsafeCell<T>);

// SAFETY: every user of `Global` serializes its accesses itself (single
// kernel context, IRQ-masked sections, or atomics inside T).
unsafe impl<T> Sync for Global<T> {}

impl<T> Global<T> {
    pub const fn new(v: T) -> Self {
        Global(UnsafeCell::new(v))
    }

    /// # Safety
    /// The caller guarantees no other reference to the contents is live.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get(&self) -> &mut T {
        &mut *self.0.get()
    }
}

// ----------------------------------------------------------------- MMIO / DMA

#[cfg(not(hostsim))]
mod prim {
    #[inline]
    pub fn rd32(a: u64) -> u32 {
        // SAFETY: callers pass addresses inside a BAR they mapped and sized.
        unsafe { (a as *const u32).read_volatile() }
    }

    #[inline]
    pub fn wr32(a: u64, v: u32) {
        // SAFETY: as above.
        unsafe { (a as *mut u32).write_volatile(v) }
    }

    #[inline]
    pub fn rd8(a: u64) -> u8 {
        // SAFETY: as above.
        unsafe { (a as *const u8).read_volatile() }
    }

    #[inline]
    pub fn wr8(a: u64, v: u8) {
        // SAFETY: as above.
        unsafe { (a as *mut u8).write_volatile(v) }
    }

    /// Are interrupts enabled in RFLAGS right now?
    #[inline]
    pub fn interrupts_on() -> bool {
        let flags: u64;
        // SAFETY: reading RFLAGS has no side effect.
        unsafe { core::arch::asm!("pushfq; pop {}", out(reg) flags, options(nomem)) };
        flags & 0x200 != 0
    }

    /// Sleep until the next interrupt when they are on, spin otherwise.  No
    /// trailing `cli`: the handler's `iretq` restores IF set, so the caller
    /// comes back with interrupts in the state it found them (the same
    /// discipline as the AHCI wait loop).
    #[inline]
    pub fn idle() {
        if interrupts_on() {
            // SAFETY: deliberate sti;hlt, the CPU delays delivery by one
            // instruction so the wake-up cannot slip between the two.
            unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
        } else {
            core::hint::spin_loop();
        }
    }

    /// Monotonic milliseconds: TSC/HPET precise when the boot clock found
    /// one, the 10 ms tick count otherwise.
    #[inline]
    pub fn now_ms() -> u64 {
        crate::hw::clock::now_ns() / 1_000_000
    }

    /// Margin added to every wait: with only the 10 ms tick as a clock, a
    /// difference of two readings can undershoot the real time by a tick.
    #[inline]
    pub fn slack_ms() -> u64 {
        if crate::hw::clock::precise() { 1 } else { 10 }
    }

    /// `n` physically contiguous zeroed pages below 4 GiB (identity mapped).
    pub fn dma_alloc(n: usize) -> Option<u64> {
        crate::mem::pmm::alloc_pages_low_contig(n)
    }

    pub fn dma_free(phys: u64, n: usize) {
        for i in 0..n {
            crate::mem::pmm::free_page(phys + (super::PAGE * i) as u64);
        }
    }

    /// Kernel virtual address of `len` bytes of device registers (None when
    /// out of reach).  Reaches BARs above 4 GiB through the MMIO window.
    pub fn map_bar(phys: u64, len: u64) -> Option<u64> {
        crate::mem::vmm::map_mmio(phys, len)
    }
}

#[cfg(hostsim)]
mod prim {
    extern "Rust" {
        fn hostsim_rd32(a: u64) -> u32;
        fn hostsim_wr32(a: u64, v: u32);
        fn hostsim_rd8(a: u64) -> u8;
        fn hostsim_wr8(a: u64, v: u8);
        fn hostsim_idle();
        fn hostsim_now_ms() -> u64;
        fn hostsim_dma_alloc(n: usize) -> u64;
        fn hostsim_dma_free(p: u64, n: usize);
    }
    // SAFETY (all): the host test crate provides these symbols.
    pub fn rd32(a: u64) -> u32 { unsafe { hostsim_rd32(a) } }
    pub fn wr32(a: u64, v: u32) { unsafe { hostsim_wr32(a, v) } }
    pub fn rd8(a: u64) -> u8 { unsafe { hostsim_rd8(a) } }
    pub fn wr8(a: u64, v: u8) { unsafe { hostsim_wr8(a, v) } }
    pub fn interrupts_on() -> bool { true }
    pub fn idle() { unsafe { hostsim_idle() } }
    pub fn now_ms() -> u64 { unsafe { hostsim_now_ms() } }
    pub fn slack_ms() -> u64 { 1 }
    pub fn dma_alloc(n: usize) -> Option<u64> {
        let p = unsafe { hostsim_dma_alloc(n) };
        if p == 0 { None } else { Some(p) }
    }
    pub fn dma_free(p: u64, n: usize) { unsafe { hostsim_dma_free(p, n) } }
    pub fn map_bar(phys: u64, _len: u64) -> Option<u64> { Some(phys) }
}

pub use prim::{dma_alloc, dma_free, idle, interrupts_on, map_bar, now_ms, rd32, rd8, slack_ms, wr32, wr8};

/// 64-bit register read as two dwords (low first), which every NVMe and xHCI
/// controller accepts, unlike a single 64-bit access on some of them.
#[inline]
pub fn rd64(a: u64) -> u64 {
    let lo = rd32(a) as u64;
    let hi = rd32(a + 4) as u64;
    lo | hi << 32
}

/// 64-bit register write, low dword first (Linux's `lo_hi_writeq`).
#[inline]
pub fn wr64(a: u64, v: u64) {
    wr32(a, v as u32);
    wr32(a + 4, (v >> 32) as u32);
}

/// Full barrier between building a descriptor in memory and telling the
/// device about it (doorbell write), or between reading a "done" flag the
/// device wrote and the data it guards.
#[inline]
pub fn mb() {
    fence(Ordering::SeqCst);
}

/// Volatile access to device-shared memory (rings, queues, contexts).
///
/// # Safety
/// `addr` must be a valid, suitably aligned address inside memory we own.
#[inline]
pub unsafe fn mem_rd32(addr: u64) -> u32 {
    (addr as *const u32).read_volatile()
}

/// # Safety
/// As for [`mem_rd32`].
#[inline]
pub unsafe fn mem_wr32(addr: u64, v: u32) {
    (addr as *mut u32).write_volatile(v)
}

/// # Safety
/// As for [`mem_rd32`].
#[inline]
pub unsafe fn mem_rd64(addr: u64) -> u64 {
    (addr as *const u64).read_volatile()
}

/// # Safety
/// As for [`mem_rd32`].
#[inline]
pub unsafe fn mem_wr64(addr: u64, v: u64) {
    (addr as *mut u64).write_volatile(v)
}

// ------------------------------------------------------------------- waiting

/// Polls tried back to back before a wait starts sleeping: enough for the
/// fast devices (an NVMe command completes in tens of microseconds) without
/// paying a whole timer tick of `hlt` latency per command.
const FAST_SPINS: u32 = 20_000;
/// Hard cap on loop iterations, so a machine whose clock is not advancing
/// (interrupts off, tick-based uptime) cannot hang in a wait.
const SPIN_CAP: u64 = 50_000_000;

/// Poll `cond` for up to `ms` milliseconds without ever sleeping.
pub fn wait_spin(ms: u64, mut cond: impl FnMut() -> bool) -> bool {
    let start = now_ms();
    let mut spins = 0u64;
    loop {
        if cond() {
            return true;
        }
        spins += 1;
        if now_ms().wrapping_sub(start) >= ms + slack_ms() || spins > SPIN_CAP {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// Wait for `cond`: spin briefly (fast devices), then sleep between polls
/// with `sti; hlt` so an interrupt (or the next timer tick) re-checks it.
/// True when the condition came true within `ms` milliseconds.
pub fn wait_event(ms: u64, mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..FAST_SPINS {
        if cond() {
            return true;
        }
        core::hint::spin_loop();
    }
    let start = now_ms();
    let mut spins = 0u64;
    loop {
        if cond() {
            return true;
        }
        if now_ms().wrapping_sub(start) >= ms + slack_ms() {
            return false;
        }
        spins += 1;
        if spins > SPIN_CAP {
            return false;
        }
        idle();
    }
}

/// Sleep for at least `ms` milliseconds (tick granularity).
pub fn delay_ms(ms: u64) {
    let start = now_ms();
    let mut spins = 0u64;
    while now_ms().wrapping_sub(start) < ms + slack_ms() {
        spins += 1;
        if spins > SPIN_CAP {
            return;
        }
        idle();
    }
}
