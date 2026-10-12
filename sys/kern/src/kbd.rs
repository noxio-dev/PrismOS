// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// PS/2 keyboard, polled.  Scan code set 1 (what a PC gives us after POST).
// If the machine has no i8042 (possible on UEFI), has_key() never fires and
// the terminal simply stays on the serial console.

use crate::io::inb;

const KBD_DATA: u16 = 0x60;
const KBD_STATUS: u16 = 0x64;

/// A scan code is pending in the controller output buffer.
pub fn has_key() -> bool {
    // SAFETY: fixed legacy port, no memory involved.
    unsafe { inb(KBD_STATUS) & 0x01 != 0 }
}

/// Read one scan code.  Call only after has_key() returned true.
pub fn read_scancode() -> u8 {
    // SAFETY: as above.
    unsafe { inb(KBD_DATA) }
}

const EXT: u8 = 0xE0; // prefix for extended keys (arrows, keypad enter...)
const SC_LSHIFT: u8 = 0x2A;
const SC_RSHIFT: u8 = 0x36;
const SC_CAPS: u8 = 0x3A;

/// Fill `dst` starting at scan code `start` with the bytes of `s`.
const fn fill(dst: &mut [u8; 128], start: u8, s: &[u8]) {
    let mut i = 0;
    while i < s.len() {
        dst[start as usize + i] = s[i];
        i += 1;
    }
}

/// Scan code set 1, US layout, unshifted.  0 = no printable translation.
const PLAIN: [u8; 128] = {
    let mut t = [0u8; 128];
    t[0x01] = b'\x1b';                    // Esc
    fill(&mut t, 0x02, b"1234567890-=");
    t[0x0E] = b'\x7f';                    // Backspace
    t[0x0F] = b'\t';                      // Tab
    fill(&mut t, 0x10, b"qwertyuiop[]");
    t[0x1C] = b'\n';                      // Enter
    fill(&mut t, 0x1E, b"asdfghjkl;'");   // .. through 0x28
    t[0x29] = b'`';
    t[0x2B] = b'\\';
    fill(&mut t, 0x2C, b"zxcvbnm,./");
    t[0x39] = b' ';
    fill(&mut t, 0x47, b"789-456+1230."); // numeric keypad
    t
};

/// Same table with Shift held.
const SHIFT: [u8; 128] = {
    let mut t = [0u8; 128];
    t[0x01] = b'\x1b';
    fill(&mut t, 0x02, b"!@#$%^&*()_+");
    t[0x0E] = b'\x7f';
    t[0x0F] = b'\t';
    fill(&mut t, 0x10, b"QWERTYUIOP{}");
    t[0x1C] = b'\n';
    fill(&mut t, 0x1E, b"ASDFGHJKL:\"");   // .. through 0x28
    t[0x29] = b'~';
    t[0x2B] = b'|';
    fill(&mut t, 0x2C, b"ZXCVBNM<>?");
    t[0x39] = b' ';
    t
};

/// Keyboard state: modifier keys held, extended prefix seen.
pub struct Kbd {
    shift_l: bool,
    shift_r: bool,
    caps: bool,
    ext: bool,
}

impl Kbd {
    pub const fn new() -> Kbd {
        Kbd { shift_l: false, shift_r: false, caps: false, ext: false }
    }

    /// Translate the next pending scan code into a character, if any.
    /// Returns None for releases, modifiers and untranslated keys.
    pub fn get_char(&mut self) -> Option<u8> {
        let raw = read_scancode();
        if raw == EXT {
            self.ext = true;
            return None;
        }
        let release = raw & 0x80 != 0;
        let sc = raw & 0x7F;
        if self.ext {
            // Extended keys (arrows, right ctrl, keypad enter): ignored for now.
            self.ext = false;
            return None;
        }
        match sc {
            SC_LSHIFT => { self.shift_l = !release; None }
            SC_RSHIFT => { self.shift_r = !release; None }
            SC_CAPS if !release => { self.caps = !self.caps; None }
            _ if release => None,
            _ => {
                let shifted = self.shift_l || self.shift_r;
                let mut c = if shifted { SHIFT[sc as usize] } else { PLAIN[sc as usize] };
                if c == 0 {
                    return None; // F-keys, ctrl, alt: nothing to print
                }
                if self.caps && c.is_ascii_alphabetic() {
                    c = if shifted { c.to_ascii_lowercase() } else { c.to_ascii_uppercase() };
                }
                Some(c)
            }
        }
    }
}