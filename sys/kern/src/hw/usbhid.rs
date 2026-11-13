// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// USB HID boot keyboard: turns the 8-byte boot reports the xHCI driver
// collects into key events for the console keyboard engine (hw/kbd.rs).
//
// A boot report is [modifiers, reserved, key1 .. key6].  Each report is
// diffed against the previous one: modifier bits and usages that appeared
// are key presses, the ones that vanished are releases.  Usages are mapped to
// PS/2 set-1 scancodes and injected through `kbd::inject_key`, so layouts,
// modifiers, Caps Lock and raw mode work exactly as with a PS/2 keyboard.
//
// The last key pressed repeats (500 ms delay, ~30 Hz) from the timer tick,
// since USB keyboards only report changes.

#![allow(dead_code)]

use crate::hw::kbd;
use crate::lib::spin::SpinLock;

pub const MAX_KBD: usize = 4;
const REPEAT_DELAY_TICKS: u32 = 50; // 500 ms at 100 Hz
const REPEAT_EVERY_TICKS: u32 = 3;

#[derive(Clone, Copy)]
struct State {
    used: bool,
    prev: [u8; 8],
    /// Usage currently repeating (0 = none) and its tick counter.
    rep_usage: u8,
    rep_ticks: u32,
}

const IDLE: State = State { used: false, prev: [0; 8], rep_usage: 0, rep_ticks: 0 };

static KBDS: SpinLock<[State; MAX_KBD]> = SpinLock::new([IDLE; MAX_KBD]);

/// (scancode, extended) per HID usage 0x00..=0x65; 0 = no key.
const fn usage_table() -> [(u8, bool); 0x66] {
    let mut t = [(0u8, false); 0x66];
    // Letters a..z.
    let letters: [u8; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19, 0x10,
        0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    ];
    let mut i = 0;
    while i < 26 {
        t[0x04 + i] = (letters[i], false);
        i += 1;
    }
    // 1..9, 0.
    i = 0;
    while i < 10 {
        t[0x1E + i] = (0x02 + i as u8, false);
        i += 1;
    }
    t[0x28] = (0x1C, false); // Enter
    t[0x29] = (0x01, false); // Esc
    t[0x2A] = (0x0E, false); // Backspace
    t[0x2B] = (0x0F, false); // Tab
    t[0x2C] = (0x39, false); // Space
    t[0x2D] = (0x0C, false); // -
    t[0x2E] = (0x0D, false); // =
    t[0x2F] = (0x1A, false); // [
    t[0x30] = (0x1B, false); // ]
    t[0x31] = (0x2B, false); // backslash
    t[0x32] = (0x2B, false); // non-US #
    t[0x33] = (0x27, false); // ;
    t[0x34] = (0x28, false); // '
    t[0x35] = (0x29, false); // `
    t[0x36] = (0x33, false); // ,
    t[0x37] = (0x34, false); // .
    t[0x38] = (0x35, false); // /
    t[0x39] = (0x3A, false); // Caps Lock
    i = 0;
    while i < 10 {
        t[0x3A + i] = (0x3B + i as u8, false); // F1..F10
        i += 1;
    }
    t[0x44] = (0x57, false); // F11
    t[0x45] = (0x58, false); // F12
    t[0x46] = (0x37, true); // Print Screen
    t[0x47] = (0x46, false); // Scroll Lock
    t[0x49] = (0x52, true); // Insert
    t[0x4A] = (0x47, true); // Home
    t[0x4B] = (0x49, true); // Page Up
    t[0x4C] = (0x53, true); // Delete
    t[0x4D] = (0x4F, true); // End
    t[0x4E] = (0x51, true); // Page Down
    t[0x4F] = (0x4D, true); // Right
    t[0x50] = (0x4B, true); // Left
    t[0x51] = (0x50, true); // Down
    t[0x52] = (0x48, true); // Up
    t[0x53] = (0x45, false); // Num Lock
    t[0x54] = (0x35, true); // KP /
    t[0x55] = (0x37, false); // KP *
    t[0x56] = (0x4A, false); // KP -
    t[0x57] = (0x4E, false); // KP +
    t[0x58] = (0x1C, true); // KP Enter
    let kp: [u8; 9] = [0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49];
    i = 0;
    while i < 9 {
        t[0x59 + i] = (kp[i], false); // KP 1..9
        i += 1;
    }
    t[0x62] = (0x52, false); // KP 0
    t[0x63] = (0x53, false); // KP .
    t[0x64] = (0x56, false); // non-US backslash
    t[0x65] = (0x5D, true); // Application
    t
}

static USAGE: [(u8, bool); 0x66] = usage_table();

/// Modifier bit -> (scancode, extended): LCtrl LShift LAlt LGui RCtrl RShift RAlt RGui.
const MODS: [(u8, bool); 8] =
    [(0x1D, false), (0x2A, false), (0x38, false), (0x5B, true), (0x1D, true), (0x36, false), (0x38, true), (0x5C, true)];

fn key_event(usage: u8, release: bool) {
    if let Some(&(sc, ext)) = USAGE.get(usage as usize) {
        if sc != 0 {
            kbd::inject_key(sc, ext, release);
        }
    }
}

/// Reserve a keyboard slot; None when MAX_KBD keyboards are already in use.
pub fn attach() -> Option<usize> {
    let mut k = KBDS.lock();
    let i = k.iter().position(|s| !s.used)?;
    k[i] = State { used: true, ..IDLE };
    Some(i)
}

/// Keyboard unplugged: release whatever was held down.
pub fn detach(i: usize) {
    let mut k = KBDS.lock();
    if let Some(s) = k.get_mut(i) {
        let prev = s.prev;
        *s = IDLE;
        for b in 0..8 {
            if prev[0] & (1 << b) != 0 {
                kbd::inject_key(MODS[b].0, MODS[b].1, true);
            }
        }
        for &u in &prev[2..] {
            if u > 3 {
                key_event(u, true);
            }
        }
    }
}

/// One boot report from keyboard `i`.  Runs from the timer tick or from a
/// synchronous USB wait; never blocks (try_lock: a report that collides with
/// the repeat timer is simply handled on the next one -- but a *lost* report
/// would lose a key, so it takes the lock, which is only ever held briefly).
pub fn on_report(i: usize, rep: &[u8; 8]) {
    let mut k = KBDS.lock();
    let Some(s) = k.get_mut(i) else { return };
    if !s.used {
        return;
    }
    let prev = s.prev;
    // Rollover error (all usages 1): ignore the report, keep the old state.
    if rep[2..].iter().all(|&u| u == 1) {
        return;
    }
    // Modifiers.
    let changed = prev[0] ^ rep[0];
    for b in 0..8 {
        if changed & (1 << b) != 0 {
            kbd::inject_key(MODS[b].0, MODS[b].1, rep[0] & (1 << b) == 0);
        }
    }
    // Keys released, then keys pressed.
    for &u in &prev[2..] {
        if u > 3 && !rep[2..].contains(&u) {
            key_event(u, true);
            if s.rep_usage == u {
                s.rep_usage = 0;
            }
        }
    }
    for &u in &rep[2..] {
        if u > 3 && !prev[2..].contains(&u) {
            key_event(u, false);
            s.rep_usage = u;
            s.rep_ticks = 0;
        }
    }
    s.prev = *rep;
}

/// Timer tick (100 Hz): auto-repeat the last key still held.
pub fn tick() {
    let Some(mut k) = KBDS.try_lock() else { return };
    for s in k.iter_mut() {
        if !s.used || s.rep_usage == 0 {
            continue;
        }
        s.rep_ticks += 1;
        if s.rep_ticks >= REPEAT_DELAY_TICKS && (s.rep_ticks - REPEAT_DELAY_TICKS) % REPEAT_EVERY_TICKS == 0 {
            key_event(s.rep_usage, false);
        }
    }
}
