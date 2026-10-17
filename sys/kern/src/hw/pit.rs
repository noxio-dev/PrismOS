// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// 8253/8254 programmable interval timer.  Channel 0 gives a 100 Hz heartbeat
// until the APIC takes over (apic.rs, which then also drives on_tick); channel 2
// is used as a calibration stopwatch for the LAPIC timer.  Formerly the only
// timer, now a fallback and a calibration source.  The tick counter drives uptime and
// the `sleep` primitive used by the shell.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::io::outb;

const CH0_DATA: u16 = 0x40;
const MODE_CMD: u16 = 0x43;

/// Input frequency of the PIT (Hz).
pub const BASE_HZ: u64 = 1_193_182;
/// Requested heartbeat.
pub const HZ: u64 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);
static BOOT_MS: AtomicU64 = AtomicU64::new(0);

/// Reprogram channel 0: mode 3 (square wave), divisor for `HZ`.
pub fn init() {
    let div: u16 = (BASE_HZ / HZ) as u16; // 11932 -> 100.0002 Hz
    // SAFETY: standard PIT channel 0 programming.
    unsafe {
        outb(MODE_CMD, 0x36); // ch2? no: 0x36 = ch0, lobyte/hibyte, mode 3
        outb(CH0_DATA, (div & 0xFF) as u8);
        outb(CH0_DATA, (div >> 8) as u8);
    }
}

/// Silence channel 0 once the LAPIC timer has taken over as the tick source
/// (mode 0 one-shot: at most one stray pulse on a line nobody routes).
pub fn stop() {
    // SAFETY: standard PIT channel 0 programming.
    unsafe {
        outb(MODE_CMD, 0x30);
        outb(CH0_DATA, 0);
        outb(CH0_DATA, 0);
    }
}

/// Wait `ms` milliseconds (1..=50) with channel 2 in one-shot mode, without
/// interrupts: used to calibrate the LAPIC timer when there is no HPET.
/// Returns false if the PIT never signalled (broken or absent).
pub fn wait_ms_ch2(ms: u32) -> bool {
    let count = BASE_HZ * ms as u64 / 1000;
    if count == 0 || count > 0xFFFF {
        return false;
    }
    // SAFETY: standard PIT channel 2 / port 0x61 gate sequence.
    unsafe {
        let p61 = crate::arch::io::inb(0x61);
        outb(0x61, p61 & 0xFC); // gate low, speaker off
        outb(MODE_CMD, 0xB0); // ch2, lobyte/hibyte, mode 0
        outb(0x42, (count & 0xFF) as u8);
        outb(0x42, (count >> 8) as u8);
        outb(0x61, (p61 & 0xFC) | 1); // gate high: start counting
        let mut spins = 0u32;
        let mut done = false;
        while spins < 20_000_000 {
            if crate::arch::io::inb(0x61) & 0x20 != 0 {
                done = true;
                break;
            }
            spins += 1;
        }
        outb(0x61, p61 & 0xFC);
        done
    }
}

/// IRQ0 / LAPIC timer tick handler body (called from the dispatch table).
pub fn on_tick() {
    let t = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    BOOT_MS.store(t * 10, Ordering::Relaxed); // 100 Hz -> 10 ms per tick
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Milliseconds since the PIT started.
pub fn uptime_ms() -> u64 {
    BOOT_MS.load(Ordering::Relaxed)
}

/// Busy-sleep `ms` milliseconds on the tick counter.  Requires IRQ0.
#[allow(dead_code)] // used once the scheduler arrives
pub fn sleep_ms(ms: u64) {
    let target = ticks() + ms / 10;
    while ticks() < target {
        crate::arch::io::hlt();
    }
}
