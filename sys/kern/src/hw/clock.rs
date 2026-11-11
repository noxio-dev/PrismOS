// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The system clock (phase 3.5.1): nanoseconds since boot.
//
// Until 3.5 the only clock was the 100 Hz tick count -- 10 ms resolution,
// and `clock_gettime` could only claim milliseconds.  Three counters can
// drive the clock now, the best one available wins:
//
//   1. the TSC.  Its frequency is not written down anywhere reliable, so it
//      is measured once at boot against a counter whose frequency *is*
//      known: the HPET (three 10 ms windows) or, on a machine without one,
//      PIT channel 2 (three 20 ms windows).  The median of the three
//      samples is kept, which shrugs off one window an SMI or a hypervisor
//      stretched.  Reads are `lfence; rdtsc`, converted to nanoseconds with
//      a 32.32 fixed-point multiplier -- one multiply, no division.
//   2. a 64-bit HPET main counter, read through MMIO (hundreds of ns per
//      read, but exact).
//   3. the tick count, as before: 10 ms resolution, the last resort.
//
// `now_ns` never goes backwards: the last value handed out is kept and a
// reading below it is clamped (the TSCs of two CPUs are not guaranteed to
// agree, and the scheduler only runs on the BSP today, but the guard is one
// atomic and keeps the promise whoever ends up calling).
//
// A non-invariant TSC (CPUID.80000007H:EDX[8] clear) may change rate with
// the CPU's power state or stop in deep C-states; this is only logged -- an
// emulator without `+invtsc` does not set the bit and its TSC is fine, and
// the machines where it matters are twenty years old.
//
// Command line: `clock=pit` keeps the tick-count clock and the periodic
// timer (the pre-3.5.1 behaviour, for bisecting); `clock=periodic` keeps the
// nanosecond clock but the LAPIC timer ticks periodically (apic.rs) so
// sleeps wake on tick boundaries.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use crate::arch::cpu::{cpuid, Cpu, Feature};
use crate::hw::{hpet, pit};

/// Fixed-point shift of the cycles -> nanoseconds multiplier.
const SHIFT: u32 = 32;

/// Plausible TSC range: anything outside is a failed measurement.
const MIN_HZ: u64 = 10_000_000;
const MAX_HZ: u64 = 20_000_000_000;

const SRC_PIT: u8 = 0;
const SRC_HPET: u8 = 1;
const SRC_TSC: u8 = 2;

const REF_NONE: u8 = 0;
const REF_HPET: u8 = 1;
const REF_PIT: u8 = 2;

static SOURCE: AtomicU8 = AtomicU8::new(SRC_PIT);
/// ns = ((raw - BASE) * MULT) >> SHIFT
static MULT: AtomicU64 = AtomicU64::new(0);
static BASE: AtomicU64 = AtomicU64::new(0);
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
static CAL_REF: AtomicU8 = AtomicU8::new(REF_NONE);
static INVARIANT: AtomicBool = AtomicBool::new(false);
static NOISY: AtomicBool = AtomicBool::new(false);
static FORCE_PIT: AtomicBool = AtomicBool::new(false);
static LAST: AtomicU64 = AtomicU64::new(0);

/// Which counter drives `now_ns`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Pit,
    Hpet,
    Tsc,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Pit => "pit tick count",
            Source::Hpet => "hpet",
            Source::Tsc => "tsc",
        }
    }
}

// ------------------------------------------------------------ fixed point

/// The multiplier turning `hz`-rate cycles into nanoseconds:
/// `ns = (cycles * mult) >> 32`.  `hz` must be non-zero.
pub fn mult_for(hz: u64) -> u64 {
    // 1e9 << 32 = 4.29e18 fits in u64.
    (1_000_000_000u64 << SHIFT) / hz
}

/// `cycles` converted with a multiplier from `mult_for`.  The product is
/// taken in 128 bits (one `mul`), so a counter that has run for years does
/// not overflow.
#[inline]
pub fn scale(cycles: u64, mult: u64) -> u64 {
    ((cycles as u128 * mult as u128) >> SHIFT) as u64
}

// -------------------------------------------------------------- the TSC

/// `lfence; rdtsc`: the fence keeps the read from being executed ahead of
/// the instructions before it (Linux's `rdtsc_ordered`).  No `nomem`, so it
/// is also a compiler barrier.
#[inline]
pub fn rdtsc_ordered() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: rdtsc is available (checked at init); CR4.TSD only affects ring 3.
    unsafe {
        asm!("lfence", "rdtsc", out("eax") lo, out("edx") hi,
             options(nostack, preserves_flags));
    }
    ((hi as u64) << 32) | lo as u64
}

/// CPUID.80000007H:EDX[8]: the TSC ticks at a constant rate in every
/// power state and never stops.
fn invariant_tsc() -> bool {
    cpuid(0x8000_0000, 0).eax >= 0x8000_0007 && cpuid(0x8000_0007, 0).edx & (1 << 8) != 0
}

/// TSC cycles per second, measured over a `window_ms` HPET window.  The
/// window is the HPET's own elapsed time between two reads, not the nominal
/// length, so the spin loop's overshoot does not count against the result.
fn measure_hpet(window_ms: u64) -> Option<u64> {
    let target = hpet::ns_to_ticks(window_ms * 1_000_000);
    if target == 0 {
        return None;
    }
    let c0 = hpet::counter();
    let t0 = rdtsc_ordered();
    let mut spins = 0u64;
    let (c1, t1) = loop {
        let c = hpet::counter();
        if hpet::elapsed(c0, c) >= target {
            break (c, rdtsc_ordered());
        }
        spins += 1;
        if spins > 200_000_000 {
            return None;
        }
    };
    let ns = hpet::ticks_to_ns(hpet::elapsed(c0, c1));
    if ns == 0 {
        return None;
    }
    // dt_tsc is at most ~2e8 for a 10 ms window at 20 GHz: *1e9 fits.
    Some(t1.wrapping_sub(t0).saturating_mul(1_000_000_000) / ns)
}

/// TSC cycles per second, measured over a `window_ms` PIT channel 2 window
/// (nominal length: the port writes around the gate add a few microseconds
/// of error, hence the longer windows than with the HPET).
fn measure_pit(window_ms: u32) -> Option<u64> {
    let t0 = rdtsc_ordered();
    if !pit::wait_ms_ch2(window_ms) {
        return None;
    }
    let t1 = rdtsc_ordered();
    Some(t1.wrapping_sub(t0).saturating_mul(1000) / window_ms as u64)
}

struct Cal {
    hz: u64,
    reference: u8,
    noisy: bool,
}

/// Three samples against the best reference, median kept.
fn calibrate() -> Option<Cal> {
    let mut s = [0u64; 3];
    let mut reference = REF_NONE;
    if hpet::init() {
        let mut n = 0;
        for _ in 0..3 {
            if let Some(hz) = measure_hpet(10) {
                s[n] = hz;
                n += 1;
            }
        }
        if n == 3 {
            reference = REF_HPET;
        }
    }
    if reference == REF_NONE {
        let mut n = 0;
        for _ in 0..3 {
            if let Some(hz) = measure_pit(20) {
                s[n] = hz;
                n += 1;
            }
        }
        if n < 3 {
            return None;
        }
        reference = REF_PIT;
    }
    s.sort_unstable();
    let hz = s[1];
    if !(MIN_HZ..=MAX_HZ).contains(&hz) {
        return None;
    }
    Some(Cal { hz, reference, noisy: s[2] - s[0] > hz / 50 })
}

// ---------------------------------------------------------------- public

/// `clock=pit` on the command line: keep the tick-count clock.  Call
/// before `init`.
pub fn force_pit(on: bool) {
    FORCE_PIT.store(on, Ordering::Relaxed);
}

/// Pick and start the clock source.  Needs ACPI (for the HPET address) and
/// the direct map; runs with interrupts masked, before the timer tick.
pub fn init() -> Source {
    if FORCE_PIT.load(Ordering::Relaxed) {
        return Source::Pit;
    }
    INVARIANT.store(invariant_tsc(), Ordering::Relaxed);
    if Cpu::detect().has(Feature::Tsc) {
        if let Some(cal) = calibrate() {
            TSC_HZ.store(cal.hz, Ordering::Relaxed);
            CAL_REF.store(cal.reference, Ordering::Relaxed);
            NOISY.store(cal.noisy, Ordering::Relaxed);
            MULT.store(mult_for(cal.hz), Ordering::Relaxed);
            BASE.store(rdtsc_ordered(), Ordering::Relaxed);
            SOURCE.store(SRC_TSC, Ordering::Release);
            return Source::Tsc;
        }
    }
    if hpet::init() && hpet::is_wide() {
        // The multiplier straight from the period: ns = ticks * period_fs / 1e6.
        MULT.store((hpet::period_fs() << SHIFT) / 1_000_000, Ordering::Relaxed);
        BASE.store(hpet::counter(), Ordering::Relaxed);
        SOURCE.store(SRC_HPET, Ordering::Release);
        return Source::Hpet;
    }
    Source::Pit
}

pub fn source() -> Source {
    match SOURCE.load(Ordering::Acquire) {
        SRC_TSC => Source::Tsc,
        SRC_HPET => Source::Hpet,
        _ => Source::Pit,
    }
}

/// True when the clock is finer than a tick (TSC or HPET driven).
pub fn precise() -> bool {
    SOURCE.load(Ordering::Acquire) != SRC_PIT
}

pub fn is_tsc() -> bool {
    SOURCE.load(Ordering::Acquire) == SRC_TSC
}

/// The measured TSC frequency in Hz (0 when the TSC is not the source).
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

pub fn invariant() -> bool {
    INVARIANT.load(Ordering::Relaxed)
}

/// The calibration samples disagreed by more than 2%.
pub fn noisy() -> bool {
    NOISY.load(Ordering::Relaxed)
}

/// What the TSC was measured against.
pub fn calibration_ref() -> &'static str {
    match CAL_REF.load(Ordering::Relaxed) {
        REF_HPET => "HPET",
        REF_PIT => "PIT ch2",
        _ => "none",
    }
}

/// Monotonic nanoseconds since the clock was started at boot (the uptime).
/// Before `init`, or when no fine counter is available, the tick count.
pub fn now_ns() -> u64 {
    let raw = match SOURCE.load(Ordering::Acquire) {
        SRC_TSC => scale(
            rdtsc_ordered().wrapping_sub(BASE.load(Ordering::Relaxed)),
            MULT.load(Ordering::Relaxed),
        ),
        SRC_HPET => scale(
            hpet::counter().wrapping_sub(BASE.load(Ordering::Relaxed)),
            MULT.load(Ordering::Relaxed),
        ),
        _ => return pit::tick_ms() * 1_000_000,
    };
    LAST.fetch_max(raw, Ordering::Relaxed).max(raw)
}
