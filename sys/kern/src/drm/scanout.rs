// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The "hardware" half of the driver: the one display PrismOS has is the
// firmware (GOP) linear framebuffer, so scanout is a copy -- from the GEM
// buffer the CRTC shows into the real framebuffer, converting the pixel
// format on the way when the panel is not XRGB8888.  This is exactly what
// Linux's simpledrm does on every flip / dirtyfb, and it is why a dumb
// buffer reports DRM_CAP_DUMB_PREFER_SHADOW.
//
// Console hand-over: while a CRTC shows a client's buffer the kernel text
// console must not touch the screen.  `OWNED` is the switch the console's
// `present()` checks; clearing it asks the console for a full repaint.
//
// The copy runs in bands of a few rows with interrupts masked per band, never
// for a whole frame: a 1080p flip moves ~8 MiB into write-combined memory and
// the timer / scheduler must not stall for that long.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::io::IrqGuard;
use crate::boot::Framebuffer;
use crate::drm::gem::Gem;
use crate::drm::uapi;

/// Rows copied per masked section.
const BAND_ROWS: usize = 8;

static OWNED: AtomicBool = AtomicBool::new(false);

/// True while a DRM client's buffer is on screen (the console keeps its hands
/// off the framebuffer).
pub fn owned() -> bool {
    OWNED.load(Ordering::Acquire)
}

/// Take / give back the screen.  Giving it back repaints the text console.
pub fn set_owned(on: bool) {
    let was = OWNED.swap(on, Ordering::AcqRel);
    if was && !on {
        crate::console::repaint();
    }
}

/// How XRGB8888 pixels map to the panel's pixel format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conv {
    /// The panel is B8G8R8X8 (what GOP almost always is): rows copy as is.
    Copy,
    /// Anything else with 32 bits per pixel: shift each channel.
    Shift { r: (u8, u8), g: (u8, u8), b: (u8, u8) },
}

#[derive(Clone, Copy)]
pub struct Native {
    pub fb: Framebuffer,
    pub conv: Conv,
}

impl Native {
    /// None for a panel we cannot drive: no framebuffer, not 32 bpp, or a
    /// pitch smaller than a row.
    pub fn new(fb: Framebuffer) -> Option<Native> {
        if fb.addr == 0 || fb.bpp != 32 || fb.width < 8 || fb.height < 8 || fb.pitch < fb.width * 4 {
            return None;
        }
        let conv = if (fb.red_shift, fb.red_bits) == (16, 8)
            && (fb.green_shift, fb.green_bits) == (8, 8)
            && (fb.blue_shift, fb.blue_bits) == (0, 8)
        {
            Conv::Copy
        } else {
            Conv::Shift {
                r: (fb.red_shift, fb.red_bits),
                g: (fb.green_shift, fb.green_bits),
                b: (fb.blue_shift, fb.blue_bits),
            }
        };
        Some(Native { fb, conv })
    }

    pub fn width(&self) -> u32 {
        self.fb.width
    }
    pub fn height(&self) -> u32 {
        self.fb.height
    }
}

/// Convert one XRGB8888 pixel to the panel format.
#[inline]
pub fn convert_pixel(px: u32, r: (u8, u8), g: (u8, u8), b: (u8, u8)) -> u32 {
    let ch = |v: u32, (shift, bits): (u8, u8)| -> u32 {
        if bits == 0 {
            return 0;
        }
        let v = if bits >= 8 { v << (bits - 8) } else { v >> (8 - bits) };
        (v & ((1u32 << bits) - 1)) << shift
    };
    ch((px >> 16) & 0xFF, r) | ch((px >> 8) & 0xFF, g) | ch(px & 0xFF, b)
}

/// What one flush copies: `w` x `h` pixels of `gem` (rows `pitch` bytes apart,
/// starting `offset` bytes in, at pixel (`sx`, `sy`) of the buffer) to pixel
/// (`dx`, `dy`) of the panel.
#[derive(Clone)]
pub struct Blit {
    pub gem: Rc<Gem>,
    pub offset: usize,
    pub pitch: usize,
    pub sx: u32,
    pub sy: u32,
    pub dx: u32,
    pub dy: u32,
    pub w: u32,
    pub h: u32,
}

/// Run a blit.  Pixel format is XRGB8888 / ARGB8888 (alpha ignored: the
/// panel has none).  Rows outside the panel are clipped; a buffer too small
/// for the request is clipped to what it holds.  Returns false only when the
/// scratch row cannot be allocated.
pub fn blit(nat: &Native, job: &Blit) -> bool {
    let fb = &nat.fb;
    let (pw, ph) = (fb.width, fb.height);
    if job.dx >= pw || job.dy >= ph {
        return true;
    }
    let w = job.w.min(pw - job.dx) as usize;
    let h = job.h.min(ph - job.dy) as usize;
    if w == 0 || h == 0 {
        return true;
    }
    let mut row: Vec<u8> = Vec::new();
    if row.try_reserve_exact(w * 4).is_err() {
        return false;
    }
    row.resize(w * 4, 0);
    let dst_base = fb.addr as usize as *mut u8;

    let mut y = 0;
    while y < h {
        let band = BAND_ROWS.min(h - y);
        // Masked: the Rc'd object, the framebuffer and the console's
        // `OWNED` view stay consistent for one band.
        let _g = IrqGuard::new();
        for r in y..y + band {
            let src_off = job.offset + (job.sy as usize + r) * job.pitch + job.sx as usize * 4;
            if !job.gem.read(src_off, &mut row) {
                continue; // a buffer smaller than the request: skip the row
            }
            let dst = unsafe { dst_base.add((job.dy as usize + r) * fb.pitch as usize + job.dx as usize * 4) };
            match nat.conv {
                // SAFETY: the row lies inside the panel (clipped above);
                // `row` holds w*4 bytes.
                Conv::Copy => unsafe { core::ptr::copy_nonoverlapping(row.as_ptr(), dst, w * 4) },
                Conv::Shift { r: rc, g: gc, b: bc } => {
                    for x in 0..w {
                        let px = u32::from_le_bytes([row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]]);
                        // SAFETY: as above, one pixel inside the row.
                        unsafe { (dst.add(x * 4) as *mut u32).write_unaligned(convert_pixel(px, rc, gc, bc)) };
                    }
                }
            }
        }
        drop(_g);
        y += band;
    }
    true
}

/// The native mode, synthesised: the firmware framebuffer has exactly one
/// resolution, so the connector offers exactly one mode, with plausible
/// 60 Hz CVT-ish timings (a client that computes a pixel clock from them
/// gets a sane number; nothing is programmed into hardware).
pub fn native_mode(w: u32, h: u32) -> uapi::ModeInfo {
    let (w16, h16) = (w.min(0xFFFF - 160) as u16, h.min(0xFFFF - 40) as u16);
    let htotal = w16 + 160;
    let vtotal = h16 + 40;
    let mut m = uapi::ModeInfo::default();
    m.hdisplay = w16;
    m.hsync_start = w16 + 48;
    m.hsync_end = w16 + 48 + 32;
    m.htotal = htotal;
    m.vdisplay = h16;
    m.vsync_start = h16 + 3;
    m.vsync_end = h16 + 3 + 5;
    m.vtotal = vtotal;
    m.vrefresh = 60;
    // kHz: htotal * vtotal * 60 / 1000
    m.clock = ((htotal as u64 * vtotal as u64 * 60) / 1000) as u32;
    m.flags = uapi::MODE_FLAG_NHSYNC | uapi::MODE_FLAG_PVSYNC;
    m.type_ = uapi::MODE_TYPE_DRIVER | uapi::MODE_TYPE_PREFERRED;
    // name: "<w>x<h>", NUL padded.
    let mut n = [0u8; uapi::DISPLAY_MODE_LEN];
    let mut len = 0;
    let put = |b: u8, n: &mut [u8; uapi::DISPLAY_MODE_LEN], len: &mut usize| {
        if *len < uapi::DISPLAY_MODE_LEN - 1 {
            n[*len] = b;
            *len += 1;
        }
    };
    for v in [w, h] {
        let mut digits = [0u8; 10];
        let mut d = 0;
        let mut x = v;
        loop {
            digits[d] = b'0' + (x % 10) as u8;
            d += 1;
            x /= 10;
            if x == 0 {
                break;
            }
        }
        while d > 0 {
            d -= 1;
            put(digits[d], &mut n, &mut len);
        }
        if v == w {
            put(b'x', &mut n, &mut len);
        }
    }
    m.name = n;
    m
}

/// Physical size in millimetres at 96 dpi (what a firmware framebuffer
/// without EDID is conventionally given).
pub fn size_mm(px: u32) -> u32 {
    ((px as u64 * 254 + 480) / 960) as u32
}
