// SPDX-License-Identifier: GPL-3.0-or-later
//
// Linear framebuffer text console (UEFI GOP).  Text is Unicode: glyphs come
// from GNU Unifont (unifont.rs), 8x16 pixels per cell and 16x16 for wide
// characters, scaled by an integer factor chosen from the screen size so
// that at least 80x25 cells always fit.
//
// Drawing goes through a `gfx::Surface`: text lands in a RAM back buffer and
// reaches the screen when `flush()` is called (once per print call, see
// console.rs), as a single copy of the rows that changed.

use crate::boot::Framebuffer;
use crate::console::gfx::Surface;
use crate::console::unifont;

pub struct FbConsole {
    surf: Surface,
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

impl FbConsole {
    /// Returns `None` for framebuffers we cannot draw on (not 32 bpp, or too
    /// small for a single cell).
    pub fn new(fb: Framebuffer) -> Option<Self> {
        let surf = Surface::new(fb)?;
        // Largest integer scale that still gives an 80x25 grid (capped: the
        // glyph row buffer in gfx.rs holds at most 16 * 8 pixels).
        let scale = (fb.width as usize / (80 * 8)).min(fb.height as usize / (25 * 16)).clamp(1, 8);
        let (cell_w, cell_h) = (8 * scale, unifont::CELL_H * scale);
        let fg = surf.rgb(0xFF, 0xFF, 0xFF);
        let bg = surf.rgb(0, 0, 0);
        let mut c = FbConsole {
            cols: fb.width as usize / cell_w,
            rows: fb.height as usize / cell_h,
            surf,
            scale,
            cell_w,
            cell_h,
            x: 0,
            y: 0,
            fg,
            bg,
        };
        c.clear();
        c.flush();
        Some(c)
    }

    /// The drawing surface, for programs that draw graphics (snake, cube).
    pub fn surface(&mut self) -> &mut Surface {
        &mut self.surf
    }

    /// Move the text cursor to the top-left corner without erasing anything.
    pub fn home(&mut self) {
        self.x = 0;
        self.y = 0;
    }

    /// Move to the back buffer once the memory manager is up.
    pub fn enable_backbuffer(&mut self) -> bool {
        self.surf.enable_backbuffer()
    }

    /// Make everything printed so far visible.
    pub fn flush(&mut self) {
        self.surf.present();
    }

    pub fn clear(&mut self) {
        self.surf.clear(self.bg);
        self.x = 0;
        self.y = 0;
    }

    fn scroll(&mut self) {
        self.surf.scroll_up(0, self.rows * self.cell_h, self.cell_h, self.bg);
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
                let px = pen - (shift as usize * self.scale) as isize;
                self.surf.draw_glyph(g, px, (self.y * self.cell_h) as isize, self.scale, self.fg, None);
            }
            return;
        }

        let Some(g) = unifont::glyph_or_fallback(c) else { return };
        let cells = if g.wide { 2 } else { 1 };
        if self.x + cells > self.cols {
            self.newline();
        }
        let (px, py) = ((self.x * self.cell_w) as isize, (self.y * self.cell_h) as isize);
        self.surf.draw_glyph(g, px, py, self.scale, self.fg, Some(self.bg));
        self.x += cells;
        if self.x >= self.cols {
            self.newline();
        }
    }
}
