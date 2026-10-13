// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// 8253/8254 programmable interval timer: channel 0 is wired to IRQ0 and is
// reprogrammed to a 100 Hz heartbeat.  The tick counter drives uptime and
// the `sleep` primitive used by the shell.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::io::outb;

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

/// IRQ0 handler body (called from the dispatch table).
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
        crate::io::hlt();
    }
}
