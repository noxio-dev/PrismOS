// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Double-buffered 32 bpp surface on top of the loader's linear framebuffer.
//
// Everything is drawn into a back buffer in ordinary RAM (fast to read and
// write, unlike video memory), the rectangle that changed is remembered, and
// `present()` copies just that rectangle to the screen in one burst.  That
// fixes the three problems of drawing straight into video memory:
//
//   * scrolling no longer *reads* video memory (very slow on real hardware);
//   * a frame becomes visible all at once instead of line by line;
//   * an animation only pays for the pixels it actually touched.
//
// Before the memory manager is up (early boot) there is no back buffer and
// the surface draws directly into the framebuffer; `present()` is then a
// no-op.  `enable_backbuffer()` switches over once pmm/vmm are ready.
//
// No floating point anywhere: the kernel runs with SSE disabled.

use crate::boot::Framebuffer;
use crate::console::unifont;

/// Virtual address of the back buffer (unused hole above the kernel heap).
const BACK_BASE: u64 = crate::mem::vmm::KERNBASE + 0x2000_0000;
/// Largest back buffer we are willing to build (4K at 32 bpp is ~33 MB).
const BACK_MAX: u64 = 48 * 1024 * 1024;

pub struct Surface {
    pub w: usize,
    pub h: usize,
    /// The real framebuffer.
    front: *mut u8,
    front_pitch: usize,
    /// Where drawing goes: the back buffer, or `front` before it exists.
    base: *mut u8,
    pitch: usize,
    buffered: bool,
    /// Pending rectangle to copy to the screen: [x0, x1) x [y0, y1).
    dx0: usize,
    dy0: usize,
    dx1: usize,
    dy1: usize,
    r_shift: u8,
    r_bits: u8,
    g_shift: u8,
    g_bits: u8,
    b_shift: u8,
    b_bits: u8,
}

// A raw-pointer holder: the console lives in a `static mut` guarded by IRQ
// masking on a single CPU, exactly like the rest of the console state.
unsafe impl Send for Surface {}

impl Surface {
    /// Returns `None` for framebuffers we cannot draw on (not 32 bpp, or too
    /// small to be useful).
    pub fn new(fb: Framebuffer) -> Option<Surface> {
        if fb.addr == 0 || fb.bpp != 32 || fb.width < 8 || fb.height < 16 {
            return None;
        }
        let (w, h) = (fb.width as usize, fb.height as usize);
        let pitch = fb.pitch as usize;
        if pitch < w * 4 {
            return None;
        }
        Some(Surface {
            w,
            h,
            front: fb.addr as usize as *mut u8,
            front_pitch: pitch,
            base: fb.addr as usize as *mut u8,
            pitch,
            buffered: false,
            dx0: usize::MAX,
            dy0: usize::MAX,
            dx1: 0,
            dy1: 0,
            r_shift: fb.red_shift,
            r_bits: fb.red_bits,
            g_shift: fb.green_shift,
            g_bits: fb.green_bits,
            b_shift: fb.blue_shift,
            b_bits: fb.blue_bits,
        })
    }

    /// Allocate the back buffer and switch to it.  Needs the PMM and the live
    /// page tables.  Returns false (and keeps drawing directly) when memory
    /// or address space is short.
    pub fn enable_backbuffer(&mut self) -> bool {
        if self.buffered {
            return true;
        }
        let bytes = (self.w * self.h * 4) as u64;
        if bytes > BACK_MAX {
            return false;
        }
        if crate::mem::vmm::map_region(BACK_BASE, bytes) < bytes {
            return false;
        }
        let back = BACK_BASE as usize as *mut u8;
        // One read of the screen so that what is already printed survives.
        for y in 0..self.h {
            // SAFETY: both rows are inside their buffers (w*4 <= pitch).
            unsafe {
                core::ptr::copy_nonoverlapping(
                    self.front.add(y * self.front_pitch),
                    back.add(y * self.w * 4),
                    self.w * 4,
                );
            }
        }
        self.base = back;
        self.pitch = self.w * 4;
        self.buffered = true;
        true
    }

    pub fn is_buffered(&self) -> bool {
        self.buffered
    }

    // ------------------------------------------------------------ colours

    /// Pack an 8-bit-per-channel colour into the framebuffer's pixel format.
    #[inline]
    pub fn rgb(&self, r: u8, g: u8, b: u8) -> u32 {
        #[inline]
        fn ch(v: u8, shift: u8, bits: u8) -> u32 {
            if bits == 0 || bits > 8 {
                return 0;
            }
            ((v as u32) >> (8 - bits)) << shift
        }
        ch(r, self.r_shift, self.r_bits) | ch(g, self.g_shift, self.g_bits) | ch(b, self.b_shift, self.b_bits)
    }

    /// The (shift, bits) pair of each colour channel, in r, g, b order --
    /// what a renderer outside the kernel needs to pack its own pixels
    /// (`fb_info`, phase 3.3).  `rgb()` above is the kernel-side packing
    /// of the same format.
    pub fn rgb_bits(&self) -> [(u8, u8); 3] {
        [
            (self.r_shift, self.r_bits),
            (self.g_shift, self.g_bits),
            (self.b_shift, self.b_bits),
        ]
    }

    // -------------------------------------------------------- dirty rect

    /// Remember that [x0, x1) x [y0, y1) must be copied to the screen.
    #[inline]
    pub fn mark(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
        let (x1, y1) = (x1.min(self.w), y1.min(self.h));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.dx0 = self.dx0.min(x0);
        self.dy0 = self.dy0.min(y0);
        self.dx1 = self.dx1.max(x1);
        self.dy1 = self.dy1.max(y1);
    }

    /// Copy everything drawn since the last call to the screen.
    pub fn present(&mut self) {
        if self.dx0 >= self.dx1 || self.dy0 >= self.dy1 {
            return;
        }
        if self.buffered {
            let bytes = (self.dx1 - self.dx0) * 4;
            for y in self.dy0..self.dy1 {
                // SAFETY: the rectangle was clamped to the screen by mark().
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        self.base.add(y * self.pitch + self.dx0 * 4),
                        self.front.add(y * self.front_pitch + self.dx0 * 4),
                        bytes,
                    );
                }
            }
        }
        self.dx0 = usize::MAX;
        self.dy0 = usize::MAX;
        self.dx1 = 0;
        self.dy1 = 0;
    }

    // ------------------------------------------------------------ pixels

    #[inline]
    fn put(&mut self, x: i32, y: i32, c: u32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        // SAFETY: bounds checked just above.
        unsafe { (self.base.add(y as usize * self.pitch + x as usize * 4) as *mut u32).write(c) };
    }

    pub fn pixel(&mut self, x: i32, y: i32, c: u32) {
        self.put(x, y, c);
        if x >= 0 && y >= 0 {
            self.mark(x as usize, y as usize, x as usize + 1, y as usize + 1);
        }
    }

    /// Copy a run of pixels -- already packed in the framebuffer's own
    /// format, by the caller -- into row `y` starting at column `x`,
    /// clipped on every side.  `fb_blit` (phase 3.3) draws a user-space
    /// renderer's rectangle through this, a line at a time; one `mark()`
    /// covers the run.
    pub fn blit_row(&mut self, x: i32, y: i32, px: &[u32]) {
        if px.is_empty() || y < 0 || y as usize >= self.h {
            return;
        }
        let skip = (-x).max(0) as usize; // leading pixels off the left edge
        if skip >= px.len() {
            return;
        }
        let x0 = (x + skip as i32).max(0) as usize;
        if x0 >= self.w {
            return; // the whole run is past the right edge
        }
        let n = (px.len() - skip).min(self.w - x0);
        // SAFETY: the destination run was clipped to the surface above.
        unsafe {
            let dst = self.base.add(y as usize * self.pitch + x0 * 4) as *mut u32;
            for i in 0..n {
                dst.add(i).write(px[skip + i]);
            }
        }
        self.mark(x0, y as usize, x0 + n, y as usize + 1);
    }

    /// Fill a clipped rectangle without touching the dirty rectangle.
    fn rect_raw(&mut self, x: i32, y: i32, w: i32, h: i32, c: u32) {
        let x0 = x.max(0) as usize;
        let y0 = y.max(0) as usize;
        let x1 = ((x as i64 + w as i64).clamp(0, self.w as i64)) as usize;
        let y1 = ((y as i64 + h as i64).clamp(0, self.h as i64)) as usize;
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for yy in y0..y1 {
            // SAFETY: clipped to the surface above.
            unsafe {
                let row = self.base.add(yy * self.pitch + x0 * 4) as *mut u32;
                for i in 0..(x1 - x0) {
                    row.add(i).write(c);
                }
            }
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: u32) {
        self.rect_raw(x, y, w, h, c);
        if w > 0 && h > 0 {
            let (x0, y0) = (x.max(0) as usize, y.max(0) as usize);
            let x1 = (x as i64 + w as i64).max(0) as usize;
            let y1 = (y as i64 + h as i64).max(0) as usize;
            self.mark(x0, y0, x1, y1);
        }
    }

    /// Rectangle outline `t` pixels thick, drawn inside the given box.
    pub fn stroke_rect(&mut self, x: i32, y: i32, w: i32, h: i32, t: i32, c: u32) {
        self.fill_rect(x, y, w, t, c);
        self.fill_rect(x, y + h - t, w, t, c);
        self.fill_rect(x, y + t, t, h - 2 * t, c);
        self.fill_rect(x + w - t, y + t, t, h - 2 * t, c);
    }

    pub fn clear(&mut self, c: u32) {
        let (w, h) = (self.w as i32, self.h as i32);
        self.fill_rect(0, 0, w, h, c);
    }

    // ------------------------------------------------------------- lines

    /// Bresenham line, `t` pixels thick (t = 1 is a single pixel wide).
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, t: i32, c: u32) {
        let t = t.max(1);
        let (mut x, mut y) = (x0, y0);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            if t == 1 {
                self.put(x, y, c);
            } else {
                self.rect_raw(x - t / 2, y - t / 2, t, t, c);
            }
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
        let pad = t / 2 + 1;
        let (bx0, by0) = ((x0.min(x1) - pad).max(0) as usize, (y0.min(y1) - pad).max(0) as usize);
        let (bx1, by1) = ((x0.max(x1) + pad + 1).max(0) as usize, (y0.max(y1) + pad + 1).max(0) as usize);
        self.mark(bx0, by0, bx1, by1);
    }

    // ---------------------------------------------------------- polygons

    /// Fill a convex polygon (any winding) with a scanline walk.
    pub fn fill_convex(&mut self, pts: &[(i32, i32)], c: u32) {
        let n = pts.len();
        if n < 3 {
            return;
        }
        let (mut ymin, mut ymax) = (i32::MAX, i32::MIN);
        let (mut xmin, mut xmax) = (i32::MAX, i32::MIN);
        for &(x, y) in pts {
            ymin = ymin.min(y);
            ymax = ymax.max(y);
            xmin = xmin.min(x);
            xmax = xmax.max(x);
        }
        for y in ymin.max(0)..=ymax.min(self.h as i32 - 1) {
            let (mut lo, mut hi) = (i32::MAX, i32::MIN);
            for i in 0..n {
                let (ax, ay) = pts[i];
                let (bx, by) = pts[(i + 1) % n];
                if ay == by {
                    if ay == y {
                        lo = lo.min(ax.min(bx));
                        hi = hi.max(ax.max(bx));
                    }
                    continue;
                }
                let (top, bot) = if ay < by { (ay, by) } else { (by, ay) };
                if y < top || y > bot {
                    continue;
                }
                let x = ax as i64 + (y - ay) as i64 * (bx - ax) as i64 / (by - ay) as i64;
                lo = lo.min(x as i32);
                hi = hi.max(x as i32);
            }
            if lo <= hi {
                self.rect_raw(lo, y, hi - lo + 1, 1, c);
            }
        }
        self.mark(xmin.max(0) as usize, ymin.max(0) as usize, (xmax + 1).max(0) as usize, (ymax + 1).max(0) as usize);
    }

    // -------------------------------------------------------------- text

    /// Draw one Unifont glyph with its top-left corner at (px, py), each
    /// font pixel being `scale` x `scale` screen pixels.  With `bg = None`
    /// only the set pixels are painted (used for combining marks).
    pub fn draw_glyph(&mut self, g: unifont::Glyph, px: isize, py: isize, scale: usize, fg: u32, bg: Option<u32>) {
        let gw = if g.wide { 16 } else { 8 };
        let bytes = gw / 8;
        let s = scale.clamp(1, 8);
        let len = gw * s;
        let mut row = [0u32; 128];
        let mut on_row = [false; 128];
        let xs = px.max(0);
        let xe = (px + len as isize).min(self.w as isize);
        for gy in 0..unifont::CELL_H {
            for gx in 0..gw {
                let on = g.rows[gy * bytes + gx / 8] & (0x80 >> (gx % 8)) != 0;
                let col = if on { fg } else { bg.unwrap_or(0) };
                for sx in 0..s {
                    row[gx * s + sx] = col;
                    on_row[gx * s + sx] = on;
                }
            }
            if xe <= xs {
                continue;
            }
            for sy in 0..s {
                let y = py + (gy * s + sy) as isize;
                if y < 0 || y >= self.h as isize {
                    continue;
                }
                // SAFETY: x range and y are clipped to the surface above.
                unsafe {
                    let dst = self.base.add(y as usize * self.pitch + xs as usize * 4) as *mut u32;
                    let first = (xs - px) as usize;
                    let count = (xe - xs) as usize;
                    if bg.is_some() {
                        core::ptr::copy_nonoverlapping(row.as_ptr().add(first), dst, count);
                    } else {
                        for i in 0..count {
                            if on_row[first + i] {
                                dst.add(i).write(row[first + i]);
                            }
                        }
                    }
                }
            }
        }
        if px + len as isize > 0 && py + (unifont::CELL_H * s) as isize > 0 {
            self.mark(
                px.max(0) as usize,
                py.max(0) as usize,
                (px + len as isize) as usize,
                (py + (unifont::CELL_H * s) as isize) as usize,
            );
        }
    }

    /// Pixel width of `text` at `scale` (combining marks take no room).
    pub fn text_width(text: &str, scale: usize) -> usize {
        let mut w = 0;
        for c in text.chars() {
            w += unifont::width(c) * 8 * scale.clamp(1, 8);
        }
        w
    }

    /// Draw a line of text on an opaque background.
    pub fn text(&mut self, x: i32, y: i32, text: &str, scale: usize, fg: u32, bg: u32) {
        let mut pen = x as isize;
        for c in text.chars() {
            if unifont::width(c) == 0 {
                continue;
            }
            let Some(g) = unifont::glyph_or_fallback(c) else { continue };
            self.draw_glyph(g, pen, y as isize, scale, fg, Some(bg));
            pen += (if g.wide { 16 } else { 8 } * scale.clamp(1, 8)) as isize;
        }
    }

    // ----------------------------------------------------------- scroll

    /// Move rows [y0 + dy, y1) up to y0 and blank the freed rows at the
    /// bottom with `bg`.  Pure RAM-to-RAM copy once the back buffer exists.
    pub fn scroll_up(&mut self, y0: usize, y1: usize, dy: usize, bg: u32) {
        let y1 = y1.min(self.h);
        if dy == 0 || y0 + dy >= y1 {
            self.rect_raw(0, y0 as i32, self.w as i32, (y1 - y0.min(y1)) as i32, bg);
            self.mark(0, y0, self.w, y1);
            return;
        }
        let rows = y1 - y0 - dy;
        // SAFETY: both ranges lie inside the surface; dst < src so a forward
        // copy is correct even where they overlap (memmove handles it).
        unsafe {
            if self.pitch == self.w * 4 {
                core::ptr::copy(self.base.add((y0 + dy) * self.pitch), self.base.add(y0 * self.pitch), rows * self.pitch);
            } else {
                for r in 0..rows {
                    core::ptr::copy(
                        self.base.add((y0 + dy + r) * self.pitch),
                        self.base.add((y0 + r) * self.pitch),
                        self.w * 4,
                    );
                }
            }
        }
        self.rect_raw(0, (y1 - dy) as i32, self.w as i32, dy as i32, bg);
        self.mark(0, y0, self.w, y1);
    }
}
