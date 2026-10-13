// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Line discipline for the kernel terminal: merges the PS/2 keyboard queue
// and the serial RX queue, echoes, handles backspace / Ctrl-U / Ctrl-C,
// and sleeps in `hlt` between characters instead of busy-polling (the PIT
// keeps a 100 Hz heartbeat so the loop never sleeps forever).

use crate::io::hlt;

const BACKSPACE: u8 = 0x7F;

/// Next input character from either source, or None when idle.
fn get_char() -> Option<u8> {
    if let Some(c) = crate::kbd::pop_char() {
        return Some(c);
    }
    if let Some(b) = crate::serial::pop_rx() {
        return Some(b);
    }
    None
}

/// Read one line into `buf` (echo + basic editing).  Returns its length.
pub fn read_line(buf: &mut [u8]) -> usize {
    let mut n = 0usize;
    loop {
        let Some(c) = get_char() else {
            // Nothing pending: sleep until the next interrupt.
            hlt();
            continue;
        };
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
            0x03 => {
                // Ctrl-C: erase the partial line and cancel it.
                while n > 0 {
                    n -= 1;
                    crate::kprint!("\x08 \x08");
                }
                crate::kprint!("^C\n");
                return 0;
            }
            0x15 => {
                // Ctrl-U: kill the line.
                while n > 0 {
                    n -= 1;
                    crate::kprint!("\x08 \x08");
                }
            }
            c if (0x20..0x7F).contains(&c) && n < buf.len() => {
                buf[n] = c;
                n += 1;
                crate::kprint!("{}", c as char);
            }
            c if c >= 0x80 && n < buf.len() => {
                // CP437 accents from the FR layout: store and echo raw.
                buf[n] = c;
                n += 1;
                crate::kprint!("{}", c as char);
            }
            _ => {} // other control characters: ignored
        }
    }
}
