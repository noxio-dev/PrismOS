// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Line discipline for the kernel terminal: merges the PS/2 keyboard queue
// and the serial RX queue, echoes, handles backspace / Ctrl-U / Ctrl-C,
// and sleeps in `hlt` between characters instead of busy-polling (the PIT
// keeps a 100 Hz heartbeat so the loop never sleeps forever).
//
// Input is UTF-8: multi-byte sequences are assembled before they are stored
// and echoed, and editing works on whole characters (a wide CJK character
// is erased from both of its cells).

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::io::hlt;

const BACKSPACE: u8 = 0x7F;

/// A CR was the last line terminator: swallow the LF of a CRLF pair.
static LAST_CR: AtomicBool = AtomicBool::new(false);

/// Next input byte from either source, or None when idle.
fn get_char() -> Option<u8> {
    if let Some(c) = crate::hw::kbd::pop_char() {
        return Some(c);
    }
    if let Some(b) = crate::hw::serial::pop_rx() {
        return Some(b);
    }
    None
}

/// Start index of the last character stored in `buf[..n]`.
fn last_char_start(buf: &[u8], n: usize) -> usize {
    let mut i = n;
    while i > 0 {
        i -= 1;
        if buf[i] & 0xC0 != 0x80 {
            break; // not a continuation byte
        }
    }
    i
}

/// Remove the last character from the line and from the screen.
fn erase_last(buf: &[u8], n: &mut usize) {
    let start = last_char_start(buf, *n);
    let w = core::str::from_utf8(&buf[start..*n])
        .ok()
        .and_then(|s| s.chars().next())
        .map_or(1, crate::console::char_width);
    *n = start;
    for _ in 0..w {
        crate::kprint!("\x08");
    }
    for _ in 0..w {
        crate::kprint!(" ");
    }
    for _ in 0..w {
        crate::kprint!("\x08");
    }
}

/// Read one line into `buf` (echo + basic editing).  Returns its length.
pub fn read_line(buf: &mut [u8]) -> usize {
    let mut n = 0usize;
    // UTF-8 sequence being assembled, and terminal escape sequence state
    // (0 = none, 1 = after ESC, 2 = inside a CSI/SS3 sequence).
    let mut seq = [0u8; 4];
    let (mut seq_len, mut seq_need) = (0usize, 0usize);
    let mut esc = 0u8;
    loop {
        let Some(c) = get_char() else {
            // Nothing pending: sleep until the next interrupt.
            hlt();
            continue;
        };

        // Arrow keys and friends arrive from serial terminals as ESC [ x.
        match esc {
            1 => {
                esc = if c == b'[' || c == b'O' { 2 } else { 0 };
                continue;
            }
            2 => {
                if (0x40..=0x7E).contains(&c) {
                    esc = 0;
                }
                continue;
            }
            _ => {}
        }

        // Continuation of a multi-byte character.
        if seq_need > 0 {
            if c & 0xC0 == 0x80 {
                seq[seq_len] = c;
                seq_len += 1;
                if seq_len == seq_need {
                    seq_need = 0;
                    if let Ok(s) = core::str::from_utf8(&seq[..seq_len]) {
                        if n + seq_len <= buf.len() {
                            buf[n..n + seq_len].copy_from_slice(&seq[..seq_len]);
                            n += seq_len;
                            crate::kprint!("{}", s);
                        }
                    }
                }
                continue;
            }
            seq_need = 0; // truncated sequence: drop it and handle `c` normally
        }

        if c != b'\n' {
            LAST_CR.store(false, Ordering::Relaxed);
        }
        match c {
            b'\r' | b'\n' => {
                if c == b'\n' && LAST_CR.swap(false, Ordering::Relaxed) {
                    continue; // second half of CRLF
                }
                LAST_CR.store(c == b'\r', Ordering::Relaxed);
                crate::kprint!("\n");
                return n;
            }
            b'\x08' | BACKSPACE => {
                if n > 0 {
                    erase_last(buf, &mut n);
                }
            }
            0x03 => {
                // Ctrl-C: erase the partial line and cancel it.
                while n > 0 {
                    erase_last(buf, &mut n);
                }
                crate::kprint!("^C\n");
                return 0;
            }
            0x15 => {
                // Ctrl-U: kill the line.
                while n > 0 {
                    erase_last(buf, &mut n);
                }
            }
            0x1B => esc = 1,
            0x20..=0x7E => {
                if n < buf.len() {
                    buf[n] = c;
                    n += 1;
                    crate::kprint!("{}", c as char);
                }
            }
            0xC2..=0xDF | 0xE0..=0xEF | 0xF0..=0xF4 => {
                seq[0] = c;
                seq_len = 1;
                seq_need = if c < 0xE0 { 2 } else if c < 0xF0 { 3 } else { 4 };
            }
            _ => {} // other control characters, stray continuation bytes
        }
    }
}
