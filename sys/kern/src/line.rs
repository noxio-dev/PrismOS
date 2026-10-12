// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Line discipline for the kernel terminal: merges PS/2 and serial input,
// echoes, handles backspace, and returns a full line.

use crate::kbd::{has_key, Kbd};
use crate::serial;
use core::ptr::addr_of_mut;

const BACKSPACE: u8 = 0x7F;

// SAFETY: single CPU, no interrupts enabled yet, so no reentrancy.
static mut KBD: Kbd = Kbd::new();

/// Next input character from either source, or None if nothing is pending.
fn get_char() -> Option<u8> {
    if serial::has_byte() {
        return Some(serial::read_byte());
    }
    // SAFETY: see KBD above.
    let kbd = unsafe { &mut *addr_of_mut!(KBD) };
    if has_key() {
        return kbd.get_char();
    }
    None
}

/// Read one line into `buf` (echo + basic editing).  Returns its length.
pub fn read_line(buf: &mut [u8]) -> usize {
    let mut n = 0usize;
    loop {
        let Some(c) = get_char() else { continue };
        match c {
            b'\r' | b'\n' => {
                crate::kprint!("\n");
                return n;
            }
            b'\x08' | BACKSPACE => {
                if n > 0 {
                    n -= 1;
                    crate::kprint!("\x08 \x08");
                }
            }
            c if (0x20..0x7F).contains(&c) && n < buf.len() => {
                buf[n] = c;
                n += 1;
                crate::kprint!("{}", c as char);
            }
            _ => {} // releases, control characters: ignored
        }
    }
}