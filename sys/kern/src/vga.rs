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

    pub fn put(&mut self, b: u8) {
        match b {
            b'\n' => self.newline(),
            b'\r' => self.x = 0,
            _ => {
                let c = if (0x20..0x7F).contains(&b) { b } else { b'?' };
                // SAFETY: x < COLS and y < ROWS are maintained by newline().
                unsafe { write_volatile(BASE.add(self.y * COLS + self.x), ATTR | c as u16) };
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
