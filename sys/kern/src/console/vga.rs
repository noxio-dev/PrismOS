// SPDX-License-Identifier: GPL-3.0-or-later
//
// VGA text mode console (80x25 at 0xB8000), used when booted through BIOS.
//
// Beyond the plain character path it carries the VT100 state the console's
// ANSI interpreter (console/ansi.rs) drives: the current attribute byte
// (SGR colors), the cursor as an absolute/relative thing (vi and ash's
// line editing address it), erases, and the wrap-pending discipline --
// printing at the last column leaves the cursor waiting there; the NEXT
// printable wraps.  That last rule is what keeps an editor's own column
// arithmetic and the hardware's in step.

use core::ptr::{read_volatile, write_volatile};

const BASE: *mut u16 = 0xB8000 as *mut u16;
const COLS: usize = 80;
const ROWS: usize = 25;
const ATTR: u16 = 0x0F00; // white on black -- what the screens boot with

pub struct Vga {
    x: usize,
    y: usize,
    attr: u16,
    /// Set when a glyph just filled the last column: the cursor stays
    /// there and the next printable wraps first (DECAWM).
    pending: bool,
}

impl Vga {
    pub const fn new() -> Self {
        Vga { x: 0, y: 0, attr: ATTR, pending: false }
    }

    pub fn clear(&mut self) {
        self.fill(0, COLS * ROWS);
        self.x = 0;
        self.y = 0;
        self.pending = false;
        self.cursor();
    }

    /// Cursor to the top-left corner, screen contents untouched.
    pub fn home(&mut self) {
        self.cursor_to(0, 0);
    }

    /// Fill cells [from, to) with blanks in the current attribute.
    fn fill(&self, from: usize, to: usize) {
        for i in from..to {
            // SAFETY: the callers keep the indices inside the 80x25 buffer.
            unsafe { write_volatile(BASE.add(i), self.attr | b' ' as u16) };
        }
    }

    fn scroll(&mut self) {
        for i in 0..COLS * (ROWS - 1) {
            // SAFETY: indices stay inside the 80x25 buffer.
            unsafe { write_volatile(BASE.add(i), read_volatile(BASE.add(i + COLS))) };
        }
        self.fill(COLS * (ROWS - 1), COLS * ROWS);
    }

    /// The mirror: push everything down one line, blank the top row
    /// (reverse index, ESC M).
    fn scroll_down(&mut self) {
        for r in (1..ROWS).rev() {
            for c in 0..COLS {
                // SAFETY: both rows lie inside the 80x25 buffer.
                unsafe {
                    write_volatile(
                        BASE.add(r * COLS + c),
                        read_volatile(BASE.add((r - 1) * COLS + c)),
                    )
                };
            }
        }
        self.fill(0, COLS);
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
            '\n' => {
                self.pending = false;
                self.newline();
            }
            '\r' => {
                self.pending = false;
                self.x = 0;
            }
            '\x08' => {
                self.pending = false;
                self.x = self.x.saturating_sub(1);
            }
            '\t' => {
                // Fixed iteration count: the last space may wrap to column 0
                // (newline), which would keep `x < target` true forever.
                let target = ((self.x / 8 + 1) * 8).min(COLS);
                for _ in 0..target.saturating_sub(self.x) {
                    self.put_char(' ');
                }
            }
            c if (c as u32) < 0x20 || c == '\u{7F}' => {}
            c if crate::console::unifont::width(c) == 0 => {} // combining marks: not representable
            c => {
                if self.pending {
                    self.pending = false;
                    self.newline();
                }
                let b = to_cp437(c).unwrap_or(b'?');
                // SAFETY: x < COLS and y < ROWS are maintained by newline().
                unsafe { write_volatile(BASE.add(self.y * COLS + self.x), self.attr | b as u16) };
                self.x += 1;
                if self.x >= COLS {
                    // DECAWM: the glyph stays in the last column, the cursor
                    // waits there, the next printable does the wrap.
                    self.x = COLS - 1;
                    self.pending = true;
                }
            }
        }
        self.cursor();
    }

    // ------------------------------------------- the ANSI interpreter's side

    /// The attribute byte for every glyph printed from now on (SGR).
    pub fn set_attr(&mut self, attr: u8) {
        self.attr = (attr as u16) << 8;
    }

    /// Absolute cursor move (CUP), 0-based, clamped to the screen.
    pub fn cursor_to(&mut self, row: usize, col: usize) {
        self.x = col.min(COLS - 1);
        self.y = row.min(ROWS - 1);
        self.pending = false;
        self.cursor();
    }

    /// Relative cursor move (CUU/CUD/CUF/CUB), clamped to the screen.
    pub fn move_rel(&mut self, drow: isize, dcol: isize) {
        let y = (self.y as isize + drow).clamp(0, ROWS as isize - 1) as usize;
        let x = (self.x as isize + dcol).clamp(0, COLS as isize - 1) as usize;
        self.y = y;
        self.x = x;
        self.pending = false;
        self.cursor();
    }

    /// Absolute column (CHA / HPA) on the current row.
    pub fn col_set(&mut self, col: usize) {
        self.cursor_to(self.y, col);
    }

    /// Absolute row (VPA) in the current column.
    pub fn row_set(&mut self, row: usize) {
        self.cursor_to(row, self.x);
    }

    /// Erase in line (EL): 0 cursor..end, 1 start..cursor, 2 the line.
    pub fn erase_line(&mut self, mode: u8) {
        self.pending = false;
        let base = self.y * COLS;
        match mode {
            0 => self.fill(base + self.x, base + COLS),
            1 => self.fill(base, base + self.x + 1),
            2 => self.fill(base, base + COLS),
            _ => {}
        }
        self.cursor();
    }

    /// Erase in display (ED): 0 cursor..end, 1 start..cursor, 2 all --
    /// the cursor position itself never moves.
    pub fn erase_display(&mut self, mode: u8) {
        self.pending = false;
        let here = self.y * COLS + self.x;
        match mode {
            0 => self.fill(here, COLS * ROWS),
            1 => self.fill(0, here + 1),
            2 => self.fill(0, COLS * ROWS),
            _ => {}
        }
        self.cursor();
    }

    /// Index (ESC D): one line down, scrolling when already on the last
    /// row.  The column stays.
    pub fn index(&mut self) {
        self.pending = false;
        if self.y + 1 >= ROWS {
            self.scroll();
        } else {
            self.y += 1;
        }
        self.cursor();
    }

    /// Reverse index (ESC M): one line up, scrolling when already on the
    /// first row.  The column stays.
    pub fn reverse_index(&mut self) {
        self.pending = false;
        if self.y == 0 {
            self.scroll_down();
        } else {
            self.y -= 1;
        }
        self.cursor();
    }

    /// Where the cursor stands, (row, column) 0-based (save-cursor).
    pub fn cursor_pos(&self) -> (usize, usize) {
        (self.y, self.x)
    }

    /// Hide the hardware cursor (CSI ?25l): the start register's bit 5.
    pub fn hide_cursor(&mut self) {
        self.cursor_bit(true);
    }

    /// Show it again (CSI ?25h).
    pub fn show_cursor(&mut self) {
        self.cursor_bit(false);
    }

    fn cursor_bit(&self, hide: bool) {
        // SAFETY: CRT controller index/data ports, read-modify-write of
        // the cursor-start register's disable bit.
        unsafe {
            core::arch::asm!("out dx, al", in("dx") 0x3D4u16, in("al") 0x0Au8, options(nomem, nostack));
            let mut start: u8;
            core::arch::asm!("in al, dx", out("al") start, in("dx") 0x3D5u16, options(nomem, nostack));
            let start = if hide { start | 0x20 } else { start & !0x20 };
            core::arch::asm!("out dx, al", in("dx") 0x3D4u16, in("al") 0x0Au8, options(nomem, nostack));
            core::arch::asm!("out dx, al", in("dx") 0x3D5u16, in("al") start, options(nomem, nostack));
        }
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
