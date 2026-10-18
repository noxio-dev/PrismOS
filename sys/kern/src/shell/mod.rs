// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Kernel shell: the read-eval loop and the helpers shared by the command
// modules.
//
//   cmds.rs   command table, dispatch, generic / hardware commands
//   shfs.rs   file system commands (ls, cat, mount, ...)
//   shmem.rs  memory commands (lsmem, meminfo, heaptest, peek/poke/dump)
//   shuser.rs user-space commands (exec, selftest) -- phase 2

mod cmds;
mod shfs;
mod shmem;
mod shuser;

use crate::boot::BootInfo;
use core::sync::atomic::{AtomicPtr, Ordering};

pub(crate) use cmds::usage;

static BOOT: AtomicPtr<BootInfo> = AtomicPtr::new(core::ptr::null_mut());

/// Keep the boot info around for later commands.
pub fn set_boot_info(bi: Option<&'static BootInfo>) {
    let p = bi.map_or(core::ptr::null_mut(), |b| b as *const BootInfo as *mut BootInfo);
    BOOT.store(p, Ordering::Relaxed);
}

fn boot_info() -> Option<&'static BootInfo> {
    let p = BOOT.load(Ordering::Relaxed);
    if p.is_null() {
        None
    } else {
        // SAFETY: stored once at boot from the loader-provided pointer.
        Some(unsafe { &*p })
    }
}

// ------------------------------------------------------------------ shell

pub fn run() -> ! {
    crate::kprintln!(
        "PrismOS {} kernel console -- type 'help' for commands.",
        env!("CARGO_PKG_VERSION")
    );
    // The shell takes over: from here on the log ring only keeps explicit
    // kernel records (klog!), not the shell echo.
    crate::lib::klog::end_boot_capture();
    loop {
        crate::kprint!("prism# ");
        let mut line = [0u8; 256];
        let n = crate::lib::line::read_line(&mut line);
        let Ok(text) = core::str::from_utf8(&line[..n]) else { continue };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let (name, tail) = split_head(text);
        let mut args = Args::new(tail);
        cmds::dispatch(name, &mut args);
    }
}

/// Split off the first whitespace-delimited word (the command name).
fn split_head(line: &str) -> (&str, &str) {
    match line.find(char::is_whitespace) {
        Some(i) => (&line[..i], &line[i..]),
        None => (line, ""),
    }
}

// ---------------------------------------------------------------- parsing

/// Whitespace-separated argument scanner over the tail of a command line.
pub(crate) struct Args<'a> {
    rest: &'a str,
}

impl<'a> Args<'a> {
    fn new(s: &'a str) -> Self {
        Args { rest: s }
    }

    /// Next whitespace-delimited token, if any.
    pub(crate) fn next(&mut self) -> Option<&'a str> {
        self.rest = self.rest.trim_start();
        let end = self.rest.find(char::is_whitespace).unwrap_or(self.rest.len());
        if end == 0 {
            return None;
        }
        let (tok, tail) = self.rest.split_at(end);
        self.rest = tail;
        Some(tok)
    }

    /// Untouched remainder of the line (trimmed); for `echo` and friends.
    pub(crate) fn rest(&self) -> &'a str {
        self.rest.trim()
    }
}

/// Parse a number: decimal by default, hex with a `0x`/`0X` prefix.
pub(crate) fn parse_u64(s: &str) -> Option<u64> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u64>().ok()
    }
}

fn bad_number(cmd: &str, tok: &str) {
    crate::kprintln!("{}: '{}' is not a number (decimal, or 0x-prefixed hex)", cmd, tok);
}

pub(crate) fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// 16 bytes per line, hex + ASCII.  Repeated identical lines are folded.
pub(crate) fn hexdump(buf: &[u8], base: u64) {
    let mut prev: Option<&[u8]> = None;
    let mut folded = false;
    for (i, line) in buf.chunks(16).enumerate() {
        if prev == Some(line) && i + 1 != (buf.len() + 15) / 16 {
            if !folded {
                crate::kprintln!("*");
                folded = true;
            }
            continue;
        }
        folded = false;
        prev = Some(line);
        crate::kprint!("{:08x} ", base + (i * 16) as u64);
        for j in 0..16 {
            match line.get(j) {
                Some(b) => crate::kprint!(" {:02x}", b),
                None => crate::kprint!("   "),
            }
        }
        crate::kprint!("  |");
        for &b in line {
            crate::kprint!("{}", if (0x20..0x7F).contains(&b) { b as char } else { '.' });
        }
        crate::kprintln!("|");
    }
}

/// Byte count rendered as B / KiB / MiB / GiB with at most one decimal.
pub(crate) struct Size(pub u64);

impl core::fmt::Display for Size {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let n = self.0;
        let (div, unit) = if n >= 1 << 30 {
            (1u64 << 30, "GiB")
        } else if n >= 1 << 20 {
            (1u64 << 20, "MiB")
        } else if n >= 1 << 10 {
            (1u64 << 10, "KiB")
        } else {
            return write!(f, "{} B", n);
        };
        let whole = n / div;
        let tenth = ((n % div) * 10 + div / 2) / div; // rounded
        if tenth >= 10 {
            write!(f, "{} {}", whole + 1, unit)
        } else if tenth == 0 {
            write!(f, "{} {}", whole, unit)
        } else {
            write!(f, "{}.{} {}", whole, tenth, unit)
        }
    }
}
