// SPDX-License-Identifier: GPL-3.0-or-later
//
// VGA text mode console (80x25 at 0xB8000), used when booted through BIOS.

use core::ptr::{read_volatile, write_volatile};

const BASE: *mut u16 = 0xB8000 as *mut u16;
const COLS: usize = 80;
const ROWS: usize = 25;
const ATTR: u16 = 0x0F00; // white on black

pub struct Vga {
    x: usize,
    y: usize,
}

impl Vga {
    pub const fn new() -> Self {
        Vga { x: 0, y: 0 }
    }

    pub fn clear(&mut self) {
        for i in 0..COLS * ROWS {
            // SAFETY: the VGA text buffer is identity mapped by the loader.
            unsafe { write_volatile(BASE.add(i), ATTR | b' ' as u16) };
        }
        self.x = 0;
        self.y = 0;
        self.cursor();
    }

    /// Cursor to the top-left corner, screen contents untouched.
    pub fn home(&mut self) {
        self.x = 0;
        self.y = 0;
        self.cursor();
    }

    fn scroll(&mut self) {
        for i in 0..COLS * (ROWS - 1) {
            // SAFETY: indices stay inside the 80x25 buffer.
            unsafe { write_volatile(BASE.add(i), read_volatile(BASE.add(i + COLS))) };
        }
        for i in COLS * (ROWS - 1)..COLS * ROWS {
            // SAFETY: as above.
            unsafe { write_volatile(BASE.add(i), ATTR | b' ' as u16) };
        }
    }

    fn newline(&mut self) {
        self.x = 0;
        if self.y + 1 >= ROWS {
            self.scroll();
        } else {
            self.y += 1;
        }
    }

    /// Print one character.  The VGA text hardware only has the 256 glyphs of
    /// code page 437, so characters outside it show as '?' (the framebuffer
    /// console, used on UEFI, draws all of Unicode).
    pub fn put_char(&mut self, ch: char) {
        match ch {
            '\n' => self.newline(),
            '\r' => self.x = 0,
            '\x08' => self.x = self.x.saturating_sub(1),
            '\t' => {
                let next = (self.x / 8 + 1) * 8;
                while self.x < next.min(COLS) {
                    self.put_char(' ');
                }
            }
            c if (c as u32) < 0x20 || c == '\u{7F}' => {}
            c if crate::console::unifont::width(c) == 0 => {} // combining marks: not representable
            c => {
                let b = to_cp437(c).unwrap_or(b'?');
                // SAFETY: x < COLS and y < ROWS are maintained by newline().
                unsafe { write_volatile(BASE.add(self.y * COLS + self.x), ATTR | b as u16) };
                self.x += 1;
                if self.x >= COLS {
                    self.newline();
                }
            }
        }
        self.cursor();
    }

    fn cursor(&self) {
        let pos = (self.y * COLS + self.x) as u16;
        // SAFETY: CRT controller index/data ports.
        unsafe {
            core::arch::asm!("out dx, al", in("dx") 0x3D4u16, in("al") 0x0Fu8, options(nomem, nostack));
            core::arch::asm!("out dx, al", in("dx") 0x3D5u16, in("al") (pos & 0xFF) as u8, options(nomem, nostack));
            core::arch::asm!("out dx, al", in("dx") 0x3D4u16, in("al") 0x0Eu8, options(nomem, nostack));
            core::arch::asm!("out dx, al", in("dx") 0x3D5u16, in("al") (pos >> 8) as u8, options(nomem, nostack));
        }
    }
}

/// Code page 437, bytes 0x80..=0xFF.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å',
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ',
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»',
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐',
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧',
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀',
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩',
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{A0}',
];

/// The code page 437 byte that displays as `c` on VGA text hardware.
fn to_cp437(c: char) -> Option<u8> {
    if (' '..='~').contains(&c) {
        return Some(c as u8);
    }
    CP437_HIGH.iter().position(|&x| x == c).map(|i| 0x80 + i as u8)
}
