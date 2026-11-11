// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// HPET (phase 3.5.1): the main counter as a free-running stopwatch.
//
// The comparators and their interrupts are not used -- the LAPIC timer
// (apic.rs) is the tick source.  What the HPET gives the kernel is a
// counter whose frequency the firmware states, so it can serve as the
// reference the other clocks are measured against: the LAPIC timer and the
// TSC (clock.rs) are calibrated on it, and a machine without a usable TSC
// can run its nanosecond clock on it directly (64-bit counters only: a
// 32-bit one wraps every few minutes and nothing here tracks the wrap).
//
// The block is mapped through the direct map; `init` starts the counter
// (ENABLE_CNF) and is idempotent.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::hw::acpi;

const REG_CAP: u64 = 0x00;
const REG_CFG: u64 = 0x10;
const REG_COUNTER: u64 = 0xF0;
/// General Capabilities: COUNT_SIZE_CAP, the main counter is 64 bits wide.
const CAP_COUNT_SIZE: u64 = 1 << 13;
/// General Configuration: ENABLE_CNF, the main counter runs.
const CFG_ENABLE: u64 = 1;
/// The spec caps the counter period at 100 ns (0x05F5E100 fs).
const MAX_PERIOD_FS: u64 = 100_000_000;

static BASE: AtomicU64 = AtomicU64::new(0);
static PERIOD_FS: AtomicU64 = AtomicU64::new(0);
static WIDE: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);

/// Map the HPET register block and start its main counter.  False when the
/// firmware reported no (memory-mapped) HPET or its period is out of range.
pub fn init() -> bool {
    if READY.load(Ordering::Acquire) {
        return true;
    }
    let Some(a) = acpi::info() else { return false };
    if !a.hpet {
        return false;
    }
    let Some(va) = crate::mem::vmm::phys_access(a.hpet_addr) else { return false };
    // SAFETY: the HPET register block is 1 KiB of MMIO behind the direct map.
    unsafe {
        let cap = ((va + REG_CAP) as *const u64).read_volatile();
        let period_fs = cap >> 32;
        if period_fs == 0 || period_fs > MAX_PERIOD_FS {
            return false;
        }
        let cfg = (va + REG_CFG) as *mut u64;
        cfg.write_volatile(cfg.read_volatile() | CFG_ENABLE);
        WIDE.store(cap & CAP_COUNT_SIZE != 0, Ordering::Relaxed);
        PERIOD_FS.store(period_fs, Ordering::Relaxed);
        BASE.store(va, Ordering::Relaxed);
    }
    READY.store(true, Ordering::Release);
    true
}

pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Whether the main counter is 64 bits wide.
pub fn is_wide() -> bool {
    WIDE.load(Ordering::Relaxed)
}

/// Femtoseconds per counter tick (0 before `init`).
pub fn period_fs() -> u64 {
    PERIOD_FS.load(Ordering::Relaxed)
}

/// Counter frequency in Hz (0 before `init`).
pub fn freq_hz() -> u64 {
    let p = period_fs();
    if p == 0 {
        0
    } else {
        1_000_000_000_000_000 / p
    }
}

fn mask() -> u64 {
    if is_wide() {
        u64::MAX
    } else {
        0xFFFF_FFFF
    }
}

/// The raw main counter (masked to its width).  Only after `init`.
#[inline]
pub fn counter() -> u64 {
    // SAFETY: BASE points at the mapped register block once `ready()`.
    let v = unsafe { ((BASE.load(Ordering::Relaxed) + REG_COUNTER) as *const u64).read_volatile() };
    v & mask()
}

/// Ticks elapsed from `start` to `now`, correct across one wrap of a
/// counter whose width is given by `mask`.
pub fn elapsed_masked(start: u64, now: u64, mask: u64) -> u64 {
    now.wrapping_sub(start) & mask
}

/// Ticks elapsed from `start` to `now` on the real counter.
#[inline]
pub fn elapsed(start: u64, now: u64) -> u64 {
    elapsed_masked(start, now, mask())
}

/// A span of `ticks` counter ticks in nanoseconds.  Meant for calibration
/// windows (the product must fit in 64 bits: spans up to a few hours).
pub fn ticks_to_ns(ticks: u64) -> u64 {
    ticks.saturating_mul(period_fs()) / 1_000_000
}

/// The number of counter ticks in `ns` nanoseconds (same range as above).
pub fn ns_to_ticks(ns: u64) -> u64 {
    match period_fs() {
        0 => 0,
        p => ns.saturating_mul(1_000_000) / p,
    }
}

/// Spin for `ms` milliseconds on the main counter.  False when the HPET is
/// missing or its counter does not advance.
pub fn wait_ms(ms: u64) -> bool {
    if !init() {
        return false;
    }
    let target = ns_to_ticks(ms * 1_000_000);
    let start = counter();
    let mut spins = 0u64;
    while elapsed(start, counter()) < target {
        spins += 1;
        if spins > 100_000_000 {
            return false;
        }
    }
    true
}
