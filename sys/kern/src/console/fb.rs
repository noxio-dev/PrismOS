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
//
// It carries the same VT100 state as the VGA console (console/vga.rs): the
// SGR attribute pair (colors from the 16-color palette), cursor moves and
// erases for the ANSI interpreter, and the wrap-pending discipline.

use crate::boot::Framebuffer;
use crate::console::ansi::PALETTE16;
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
    /// Set when a glyph just filled the last column: the next printable
    /// wraps first (DECAWM), the same rule the VGA console follows.
    pending: bool,
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
            pending: false,
        };
        c.clear();
        c.flush();
        Some(c)
    }

    /// The drawing surface, for programs that draw graphics (snake, cube).
    pub fn surface(&mut self) -> &mut Surface {
        &mut self.surf
    }

    /// The text grid, (columns, rows) -- the geometry a TIOCGWINSZ on the
    /// console answers (phase 3.3).
    pub fn text_dims(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// Move the text cursor to the top-left corner without erasing anything.
    pub fn home(&mut self) {
        self.cursor_to(0, 0);
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
        self.pending = false;
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
                let target = ((self.x / 8 + 1) * 8).min(self.cols);
                for _ in 0..target.saturating_sub(self.x) {
                    self.put_char(' ');
                }
            }
            c if (c as u32) < 0x20 || c == '\u{7F}' => return,
            _ => {}
        }
        if (c as u32) < 0x20 || c == '\u{7F}' {
            return;
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
            // DECAWM: the cursor waits at the last column; the next
            // printable does the wrap.
            self.x = self.cols - 1;
            self.pending = true;
        }
    }

    // ------------------------------------------- the ANSI interpreter's side

    /// SGR: the palette indices for every glyph printed from now on,
    /// reverse video swapping the pair.
    pub fn set_sgr(&mut self, fg: u8, bg: u8, reverse: bool) {
        let (f, b) = if reverse { (bg, fg) } else { (fg, bg) };
        let (fr, fgc, fb) = PALETTE16[(f & 15) as usize];
        let (br, bgc, bb) = PALETTE16[(b & 15) as usize];
        self.fg = self.surf.rgb(fr, fgc, fb);
        self.bg = self.surf.rgb(br, bgc, bb);
    }

    /// Fill the cells of row `y`, columns [from, to), with the background.
    fn fill_cells(&mut self, y: usize, from: usize, to: usize) {
        let w = (to - from).min(self.cols.saturating_sub(from)) * self.cell_w;
        if w > 0 {
            self.surf
                .fill_rect((from * self.cell_w) as i32, (y * self.cell_h) as i32, w as i32, self.cell_h as i32, self.bg);
        }
    }

    /// Absolute cursor move (CUP), 0-based, clamped to the grid.
    pub fn cursor_to(&mut self, row: usize, col: usize) {
        self.x = col.min(self.cols - 1);
        self.y = row.min(self.rows - 1);
        self.pending = false;
    }

    /// Relative cursor move (CUU/CUD/CUF/CUB), clamped to the grid.
    pub fn move_rel(&mut self, drow: isize, dcol: isize) {
        let y = (self.y as isize + drow).clamp(0, self.rows as isize - 1) as usize;
        let x = (self.x as isize + dcol).clamp(0, self.cols as isize - 1) as usize;
        self.y = y;
        self.x = x;
        self.pending = false;
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
        match mode {
            0 => self.fill_cells(self.y, self.x, self.cols),
            1 => self.fill_cells(self.y, 0, self.x + 1),
            2 => self.fill_cells(self.y, 0, self.cols),
            _ => {}
        }
    }

    /// Erase in display (ED): 0 cursor..end, 1 start..cursor, 2 all --
    /// the cursor position itself never moves.
    pub fn erase_display(&mut self, mode: u8) {
        self.pending = false;
        match mode {
            0 => {
                self.fill_cells(self.y, self.x, self.cols);
                for y in self.y + 1..self.rows {
                    self.fill_cells(y, 0, self.cols);
                }
            }
            1 => {
                for y in 0..self.y {
                    self.fill_cells(y, 0, self.cols);
                }
                self.fill_cells(self.y, 0, self.x + 1);
            }
            2 => {
                for y in 0..self.rows {
                    self.fill_cells(y, 0, self.cols);
                }
            }
            _ => {}
        }
    }

    /// Index (ESC D): one line down, scrolling at the bottom edge.  The
    /// column stays.
    pub fn index(&mut self) {
        self.pending = false;
        if self.y + 1 >= self.rows {
            self.scroll();
        } else {
            self.y += 1;
        }
    }

    /// Reverse index (ESC M): one line up, scrolling at the top edge.
    pub fn reverse_index(&mut self) {
        self.pending = false;
        if self.y == 0 {
            self.surf
                .scroll_down(0, self.rows * self.cell_h, self.cell_h, self.bg);
        } else {
            self.y -= 1;
        }
    }

    /// Where the cursor stands, (row, column) 0-based (save-cursor).
    pub fn cursor_pos(&self) -> (usize, usize) {
        (self.y, self.x)
    }
}
