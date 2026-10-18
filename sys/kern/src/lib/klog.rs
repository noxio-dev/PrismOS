// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Kernel log ring buffer (phase 1.5): everything the kernel prints during
// bring-up, plus explicit `klog!` records afterwards, land in a fixed-size
// ring that the `dmesg` shell command can dump -- the boot log stays
// available long after it scrolled off screen.
//
// Records are length-prefixed and never straddle the wrap point (the tail is
// zero-filled as padding), so a reader can always walk record boundaries.
// Two cursors describe the live region: `W` (next free byte) and `S` (start
// of the oldest surviving record), both in [0, CAP]; `S == CAP` means the
// upper half is exhausted.  With the wrapped flag set, the live stream is
// [S..CAP) followed by [0..W); writers retire records the new write would
// clobber.  Access is serialized by the IRQ guard the console already holds.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
    fn as_tag(self) -> &'static str {
        match self {
            Level::Debug => "",
            Level::Info => "",
            Level::Warn => "warning: ",
            Level::Error => "error: ",
        }
    }
    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "debug" | "d" => Some(Level::Debug),
            "info" | "i" => Some(Level::Info),
            "warn" | "w" => Some(Level::Warn),
            "error" | "err" | "e" => Some(Level::Error),
            _ => None,
        }
    }
}

const CAP: usize = 48 * 1024; // power of two
const HDR: usize = 7; // len u16 + level u8 + ts u32
const MAX_MSG: usize = 512;
const MIN_REC: usize = HDR + 1;

static mut RING: [u8; CAP] = [0; CAP];
static W: AtomicUsize = AtomicUsize::new(0);
static S: AtomicUsize = AtomicUsize::new(0);
static WRAPPED: AtomicBool = AtomicBool::new(false);
static RETIRED: AtomicUsize = AtomicUsize::new(0);

/// The console tee is active while the boot log plays (until the shell starts).
static BOOT_CAPTURE: AtomicBool = AtomicBool::new(true);
static READY: AtomicBool = AtomicBool::new(false);

/// Milliseconds since boot; wired to the PIT/LAPIC tick in klog::init.
static mut NOW_MS: fn() -> u64 = || 0;

pub fn init(now_fn: fn() -> u64) {
    // SAFETY: written once at boot before READY is set.
    unsafe { NOW_MS = now_fn };
    READY.store(true, Ordering::Release);
}

/// Stop recording the console stream (called when the shell takes over).
pub fn end_boot_capture() {
    BOOT_CAPTURE.store(false, Ordering::Release);
}

pub fn boot_capturing() -> bool {
    BOOT_CAPTURE.load(Ordering::Acquire)
}

fn timestamp() -> u64 {
    if READY.load(Ordering::Acquire) {
        // SAFETY: NOW_MS is installed before READY.
        unsafe { (NOW_MS)() }
    } else {
        0
    }
}

fn ring() -> &'static mut [u8; CAP] {
    // SAFETY: access is serialized by the IRQ guard in push() / read();
    // RING is a static array and is never moved.
    unsafe { &mut *core::ptr::addr_of_mut!(RING) }
}

/// Append one record.  IRQs must be masked (callers hold the console guard).
fn push(level: Level, msg: &str) {
    let bytes = msg.as_bytes();
    let take = bytes.len().min(MAX_MSG);
    let total = HDR + take;
    let _g = crate::arch::io::IrqGuard::new();
    let r = ring();
    let mut w = W.load(Ordering::Relaxed);

    // A record never straddles the wrap point.
    if w + total > CAP {
        r[w..].fill(0); // padding for the reader
        WRAPPED.store(true, Ordering::Relaxed);
        w = 0;
        // The old lower half becomes the new upper half of the stream.
        if S.load(Ordering::Relaxed) >= CAP {
            S.store(0, Ordering::Relaxed);
        }
    }

    // Retire upper-half records the new write would clobber.
    if WRAPPED.load(Ordering::Relaxed) {
        let mut old = S.load(Ordering::Relaxed);
        while old < CAP && old < w + total {
            if old + HDR > CAP {
                old = CAP;
                break;
            }
            let l = u16::from_le_bytes([r[old], r[old + 1]]) as usize;
            if l == 0 {
                old = CAP; // padding run: upper half exhausted
                break;
            }
            RETIRED.fetch_add(1, Ordering::Relaxed);
            old += HDR + l;
        }
        S.store(if old > CAP - MIN_REC { CAP } else { old }, Ordering::Relaxed);
    }

    let ts = timestamp() as u32;
    r[w..w + 2].copy_from_slice(&(take as u16).to_le_bytes());
    r[w + 2] = level as u8;
    r[w + 3..w + HDR].copy_from_slice(&ts.to_le_bytes());
    r[w + HDR..w + total].copy_from_slice(&bytes[..take]);
    core::sync::atomic::fence(Ordering::Release);
    W.store(w + total, Ordering::Release);
}

/// Record without echoing (the console tee: the text is already on screen).
pub fn record(level: Level, msg: &str) {
    push(level, msg);
}

/// Record and echo to the console: backing of the `klog!` macros.
pub fn log(level: Level, args: core::fmt::Arguments) {
    let text = alloc::string::ToString::to_string(&args);
    crate::kprint!("{}{}\n", level.as_tag(), &text);
    push(level, &text);
}

// ---------------------------------------------------------------- reading

pub struct Record {
    pub ts_ms: u32,
    pub level: Level,
    pub text: String,
}

fn walk(r: &[u8; CAP], from: usize, to: usize, min_level: Level, out: &mut Vec<Record>) {
    let mut pos = from;
    let mut guard = 0usize;
    while pos + HDR <= to && pos + HDR <= CAP {
        let len = u16::from_le_bytes([r[pos], r[pos + 1]]) as usize;
        if len == 0 {
            break; // padding or never-written
        }
        if pos + HDR + len > CAP || pos + HDR + len > to {
            break; // torn record: ignore
        }
        let level = match r[pos + 2] {
            0 => Level::Debug,
            1 => Level::Info,
            2 => Level::Warn,
            _ => Level::Error,
        };
        let ts = u32::from_le_bytes([r[pos + 3], r[pos + 4], r[pos + 5], r[pos + 6]]);
        let text = String::from_utf8_lossy(&r[pos + HDR..pos + HDR + len]).into_owned();
        if level >= min_level {
            out.push(Record { ts_ms: ts, level, text });
        }
        pos += HDR + len;
        guard += 1;
        if guard > 8192 {
            break; // corrupt ring: do not spin
        }
    }
}

/// Walk the live region, oldest record first; `min_level` filters.
pub fn read(min_level: Level) -> Vec<Record> {
    let mut out = Vec::new();
    let _g = crate::arch::io::IrqGuard::new();
    let r = ring();
    let w = W.load(Ordering::Acquire);
    if WRAPPED.load(Ordering::Acquire) {
        let s = S.load(Ordering::Acquire).min(CAP);
        walk(r, s, CAP, min_level, &mut out); // upper half
        walk(r, 0, w, min_level, &mut out); // lower half
    } else {
        walk(r, 0, w, min_level, &mut out);
    }
    out
}

/// Ring statistics for `dmesg`: (live bytes, records retired by wrapping).
pub fn usage() -> (usize, usize) {
    let w = W.load(Ordering::Acquire);
    let s = S.load(Ordering::Acquire);
    let used = if WRAPPED.load(Ordering::Acquire) { CAP - s + w } else { w - s };
    (used.min(CAP), RETIRED.load(Ordering::Relaxed))
}

// ---------------------------------------------------------------- macros

#[macro_export]
macro_rules! klog {
    ($lvl:ident, $($arg:tt)*) => {
        $crate::lib::klog::log($crate::lib::klog::Level::$lvl, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! klog_info {
    ($($arg:tt)*) => { $crate::klog!(Info, $($arg)*) };
}

#[macro_export]
macro_rules! klog_warn {
    ($($arg:tt)*) => { $crate::klog!(Warn, $($arg)*) };
}

#[macro_export]
macro_rules! klog_error {
    ($($arg:tt)*) => { $crate::klog!(Error, $($arg)*) };
}
