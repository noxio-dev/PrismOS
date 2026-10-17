// SPDX-License-Identifier: GPL-3.0-or-later
//
// Linear framebuffer text console (UEFI GOP).  Text is Unicode: glyphs come
// from GNU Unifont (unifont.rs), 8x16 pixels per cell and 16x16 for wide
// characters, scaled by an integer factor chosen from the screen size so
// that at least 80x25 cells always fit.

use crate::boot::Framebuffer;
use crate::console::unifont;

pub struct FbConsole {
    fb: Framebuffer,
    scale: usize,
    cell_w: usize,
    cell_h: usize,
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
    /// Returns `None` for framebuffers we cannot draw on (not 32 bpp, or too
    /// small for a single cell).
    pub fn new(fb: Framebuffer) -> Option<Self> {
        if fb.addr == 0 || fb.bpp != 32 || fb.width < 8 || fb.height < 16 {
            return None;
        }
        let fg = mask(fb.red_shift, fb.red_bits)
            | mask(fb.green_shift, fb.green_bits)
            | mask(fb.blue_shift, fb.blue_bits);
        // Largest integer scale that still gives an 80x25 grid.
        let scale = (fb.width as usize / (80 * 8)).min(fb.height as usize / (25 * 16)).max(1);
        let (cell_w, cell_h) = (8 * scale, unifont::CELL_H * scale);
        let mut c = FbConsole {
            cols: fb.width as usize / cell_w,
            rows: fb.height as usize / cell_h,
            fb,
            scale,
            cell_w,
            cell_h,
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

    /// Draw a glyph with its top-left corner at pixel (px, py).  With
    /// `overlay` only the set pixels are painted (combining marks).
    fn draw(&self, g: unifont::Glyph, px: isize, py: usize, overlay: bool) {
        let gw = if g.wide { 16 } else { 8 };
        let bytes = gw / 8;
        let s = self.scale;
        let (w, h) = (self.fb.width as isize, self.fb.height as usize);
        for gy in 0..unifont::CELL_H {
            for gx in 0..gw {
                let on = g.rows[gy * bytes + gx / 8] & (0x80 >> (gx % 8)) != 0;
                if overlay && !on {
                    continue;
                }
                let color = if on { self.fg } else { self.bg };
                for sy in 0..s {
                    for sx in 0..s {
                        let x = px + (gx * s + sx) as isize;
                        let y = py + gy * s + sy;
                        if x < 0 || x >= w || y >= h {
                            continue;
                        }
                        // SAFETY: bounds checked against the framebuffer size.
                        unsafe { self.pixel_ptr(x as usize, y).write_volatile(color) };
                    }
                }
            }
        }
    }

    fn scroll(&mut self) {
        let pitch = self.fb.pitch as usize;
        let base = self.fb.addr as usize;
        let rows_px = self.rows * self.cell_h;
        for py in 0..rows_px - self.cell_h {
            // SAFETY: copies one scan line inside the framebuffer; lines do not overlap.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (base + (py + self.cell_h) * pitch) as *const u8,
                    (base + py * pitch) as *mut u8,
                    self.cols * self.cell_w * 4,
                );
            }
        }
        for py in rows_px - self.cell_h..rows_px {
            for px in 0..self.cols * self.cell_w {
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

    /// Print one character: control codes, zero-width marks, then glyphs.
    pub fn put_char(&mut self, c: char) {
        match c {
            '\n' => return self.newline(),
            '\r' => {
                self.x = 0;
                return;
            }
            '\x08' => {
                self.x = self.x.saturating_sub(1);
                return;
            }
            '\t' => {
                let next = (self.x / 8 + 1) * 8;
                while self.x < next.min(self.cols) {
                    self.put_char(' ');
                }
                return;
            }
            c if (c as u32) < 0x20 || c == '\u{7F}' => return,
            _ => {}
        }

        // Zero advance: overlay on the previous glyph, `shift` pixels left of the pen.
        if let Some(shift) = unifont::combining_shift(c) {
            if let Some(g) = unifont::glyph(c) {
                let pen = (self.x * self.cell_w) as isize;
                self.draw(g, pen - (shift as usize * self.scale) as isize, self.y * self.cell_h, true);
            }
            return;
        }

        let Some(g) = unifont::glyph_or_fallback(c) else { return };
        let cells = if g.wide { 2 } else { 1 };
        if self.x + cells > self.cols {
            self.newline();
        }
        self.draw(g, (self.x * self.cell_w) as isize, self.y * self.cell_h, false);
        self.x += cells;
        if self.x >= self.cols {
            self.newline();
        }
    }
}
