// SPDX-License-Identifier: GPL-3.0-or-later
//
// GNU Unifont glyph lookup.  The packed font (data/unifont.bin, built by
// tools/mkfont.py, see data/UNIFONT.txt for the license) holds a bitmap for
// every character of Unicode 8.0 and later that Unifont draws: 8x16 pixels,
// or 16x16 for wide characters (CJK, Hangul, ...).  Lookup is O(1).

static DATA: &[u8] = include_bytes!("../../data/unifont.bin");

const HEADER: usize = 16; // magic, nblocks, comb_off, comb_count
const EMPTY: u32 = 0xFFFF_FFFF;

pub const CELL_H: usize = 16;

#[derive(Clone, Copy)]
pub struct Glyph {
    /// One byte per row (8 px) or two (16 px), leftmost pixel = MSB.
    pub rows: &'static [u8],
    pub wide: bool,
}

#[inline]
fn u32_at(off: usize) -> u32 {
    match DATA.get(off..off + 4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        None => EMPTY,
    }
}

fn valid() -> bool {
    DATA.len() >= HEADER && &DATA[..4] == b"UNIF"
}

/// Bitmap for `c`, or None when Unifont has no glyph for it.
pub fn glyph(c: char) -> Option<Glyph> {
    if !valid() {
        return None;
    }
    let cp = c as u32;
    let block = (cp >> 8) as usize;
    if block >= u32_at(4) as usize {
        return None;
    }
    let off = u32_at(HEADER + block * 4);
    if off == EMPTY {
        return None;
    }
    let off = off as usize;
    let present = DATA.get(off..off + 32)?;
    let wide = DATA.get(off + 32..off + 64)?;
    let n = (cp & 255) as usize;
    let (byte, bit) = (n >> 3, n & 7);
    if present[byte] >> bit & 1 == 0 {
        return None;
    }
    // Glyphs are packed in code point order: skip the ones before this one.
    let (mut narrow_before, mut wide_before) = (0usize, 0usize);
    for i in 0..byte {
        narrow_before += (present[i] & !wide[i]).count_ones() as usize;
        wide_before += wide[i].count_ones() as usize;
    }
    let below = (1u8 << bit) - 1;
    narrow_before += (present[byte] & !wide[byte] & below).count_ones() as usize;
    wide_before += (wide[byte] & below).count_ones() as usize;

    let is_wide = wide[byte] >> bit & 1 == 1;
    let start = off + 64 + narrow_before * 16 + wide_before * 32;
    let len = if is_wide { 32 } else { 16 };
    Some(Glyph { rows: DATA.get(start..start + len)?, wide: is_wide })
}

/// Glyph to draw for `c`: its own, else U+FFFD, else '?'.
pub fn glyph_or_fallback(c: char) -> Option<Glyph> {
    glyph(c).or_else(|| glyph('\u{FFFD}')).or_else(|| glyph('?'))
}

/// Zero-advance character (combining mark, zero-width format character):
/// returns how many pixels left of the pen the glyph is drawn.
pub fn combining_shift(c: char) -> Option<u32> {
    if !valid() {
        return None;
    }
    let off = u32_at(8) as usize;
    let count = u32_at(12) as usize;
    let cp = c as u32;
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let e = u32_at(off + mid * 4);
        let ecp = e & 0x00FF_FFFF;
        if ecp == cp {
            return Some(e >> 24);
        }
        if ecp < cp {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    None
}

/// Cells `c` advances the cursor by: 0 (combining), 1 or 2 (wide).
pub fn width(c: char) -> usize {
    if (c as u32) < 0x20 || c == '\u{7F}' {
        return 0;
    }
    if combining_shift(c).is_some() {
        return 0;
    }
    match glyph(c) {
        Some(g) if g.wide => 2,
        _ => 1,
    }
}
