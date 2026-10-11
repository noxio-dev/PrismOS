// SPDX-License-Identifier: GPL-3.0-or-later
//
// Linear framebuffer text console (UEFI GOP), 8x8 font drawn at 2x.

use crate::boot::Framebuffer;
use crate::font::FONT8X8;

const SCALE: usize = 2;
const CELL: usize = 8 * SCALE; // cell width in pixels
const CELL_H: usize = CELL + 4; // cell height: glyph plus 2 px of leading above and below

pub struct FbConsole {
    fb: Framebuffer,
    cols: usize,
    rows: usize,
    x: usize,
    y: usize,
    fg: u32,
    bg: u32,
}

fn mask(shift: u8, bits: u8) -> u32 {
    if bits == 0 || bits >= 32 {
        return 0;
    }
    ((1u32 << bits) - 1) << shift
}

impl FbConsole {
    /// Returns `None` for framebuffers we cannot draw on (not 32 bpp).
    pub fn new(fb: Framebuffer) -> Option<Self> {
        if fb.addr == 0 || fb.bpp != 32 || fb.width < CELL as u32 || fb.height < CELL_H as u32 {
            return None;
        }
        let fg = mask(fb.red_shift, fb.red_bits)
            | mask(fb.green_shift, fb.green_bits)
            | mask(fb.blue_shift, fb.blue_bits);
        let mut c = FbConsole {
            cols: fb.width as usize / CELL,
            rows: fb.height as usize / CELL_H,
            fb,
            x: 0,
            y: 0,
            fg,
            bg: 0,
        };
        c.clear();
        Some(c)
    }

    fn pixel_ptr(&self, px: usize, py: usize) -> *mut u32 {
        (self.fb.addr as usize + py * self.fb.pitch as usize + px * 4) as *mut u32
    }

    pub fn clear(&mut self) {
        for py in 0..self.fb.height as usize {
            for px in 0..self.fb.width as usize {
                // SAFETY: inside the framebuffer reported by the loader.
                unsafe { self.pixel_ptr(px, py).write_volatile(self.bg) };
            }
        }
        self.x = 0;
        self.y = 0;
    }

    fn draw(&self, c: u8, col: usize, row: usize) {
        let glyph = &FONT8X8[(c & 0x7F) as usize];
        for (gy, bits) in glyph.iter().enumerate() {
            for gx in 0..8 {
                let on = bits & (1 << gx) != 0;
                let color = if on { self.fg } else { self.bg };
                for sy in 0..SCALE {
                    for sx in 0..SCALE {
                        let px = col * CELL + gx * SCALE + sx;
                        let py = row * CELL_H + 2 + gy * SCALE + sy;
                        // SAFETY: col < cols and row < rows keep us inside the screen.
                        unsafe { self.pixel_ptr(px, py).write_volatile(color) };
                    }
                }
            }
        }
    }

    fn scroll(&mut self) {
        let pitch = self.fb.pitch as usize;
        let base = self.fb.addr as usize;
        let rows_px = self.rows * CELL_H;
        for py in 0..rows_px - CELL_H {
            // SAFETY: copies one scan line inside the framebuffer; lines do not overlap.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (base + (py + CELL_H) * pitch) as *const u8,
                    (base + py * pitch) as *mut u8,
                    self.cols * CELL * 4,
                );
            }
        }
        for py in rows_px - CELL_H..rows_px {
            for px in 0..self.cols * CELL {
                // SAFETY: inside the framebuffer.
                unsafe { self.pixel_ptr(px, py).write_volatile(self.bg) };
            }
        }
    }

    fn newline(&mut self) {
        self.x = 0;
        if self.y + 1 >= self.rows {
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
                self.draw(c, self.x, self.y);
                self.x += 1;
                if self.x >= self.cols {
                    self.newline();
                }
            }
        }
    }
}
