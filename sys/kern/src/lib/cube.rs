// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev

//! A spinning, flat-shaded 3D cube on the double-buffered framebuffer.
//!
//! All the maths is fixed point (Q14: 16384 = 1.0) because the kernel runs
//! with SSE disabled.  Each frame only redraws, and only copies to the
//! screen, the bounding box of the cube (plus where it was last frame).

use alloc::format;

use crate::console::gfx::Surface;

/// Full turn = 1024 angle units.
const TURN: i32 = 1024;
const HALF: i32 = 256;
const FRAME_MS: u64 = 20;

/// sin() in Q14 over one turn, built at compile time with Bhaskara's
/// approximation (max error ~0.0016, invisible on screen).
static SIN: [i16; TURN as usize] = build_sin();

const fn build_sin() -> [i16; TURN as usize] {
    let mut t = [0i16; TURN as usize];
    let mut i = 0usize;
    while i < TURN as usize {
        let (a, neg) = if i < 512 { (i, false) } else { (i - 512, true) };
        // Half-turn index -> degrees * 256, then 4x(180-x) / (40500 - x(180-x)).
        let x = (a * 90) as i64;
        let p = x * (46080 - x);
        let v = 4 * p * 16384 / (40500 * 65536 - p);
        t[i] = if neg { -v as i16 } else { v as i16 };
        i += 1;
    }
    t
}

#[inline]
fn sin(a: i32) -> i32 {
    SIN[(a & (TURN - 1)) as usize] as i32
}

#[inline]
fn cos(a: i32) -> i32 {
    sin(a + TURN / 4)
}

type V3 = (i32, i32, i32);

/// Rotate around Y, then X, then Z.
fn rotate(v: V3, ax: i32, ay: i32, az: i32) -> V3 {
    let (s, c) = (sin(ay), cos(ay));
    let (x, y, z) = ((v.0 * c + v.2 * s) >> 14, v.1, (v.2 * c - v.0 * s) >> 14);
    let (s, c) = (sin(ax), cos(ax));
    let (x, y, z) = (x, (y * c - z * s) >> 14, (y * s + z * c) >> 14);
    let (s, c) = (sin(az), cos(az));
    ((x * c - y * s) >> 14, (x * s + y * c) >> 14, z)
}

/// Vertex i: bit 0 = +x, bit 1 = +y, bit 2 = +z.
fn vertex(i: usize) -> V3 {
    let s = |bit: usize| if i >> bit & 1 == 1 { HALF } else { -HALF };
    (s(0), s(1), s(2))
}

struct Face {
    verts: [usize; 4],
    normal: V3,
    base: (u8, u8, u8),
}

/// Every face lists its corners counter-clockwise seen from outside (right
/// handed axes), which `visible()` relies on.
static FACES: [Face; 6] = [
    Face { verts: [4, 6, 2, 0], normal: (-1, 0, 0), base: (235, 72, 72) },
    Face { verts: [1, 3, 7, 5], normal: (1, 0, 0), base: (240, 150, 50) },
    Face { verts: [0, 1, 5, 4], normal: (0, -1, 0), base: (80, 200, 110) },
    Face { verts: [6, 7, 3, 2], normal: (0, 1, 0), base: (70, 150, 240) },
    Face { verts: [2, 3, 1, 0], normal: (0, 0, -1), base: (190, 90, 230) },
    Face { verts: [4, 5, 7, 6], normal: (0, 0, 1), base: (60, 210, 220) },
];

/// How the cube is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Solid,
    SolidEdges,
    Wire,
}

/// Distance from the camera to the cube centre, in the same units as HALF.
const CAM_Z: i32 = 1000;
/// Direction towards the light, Q14, roughly unit length.
const LIGHT: V3 = (-5400, 8200, -12400);

struct Frame {
    /// Screen position of each vertex.
    pts: [(i32, i32); 8],
    /// Bounding box of the projected cube: x0, y0, x1, y1.
    bbox: (i32, i32, i32, i32),
}

/// Rotate and project the 8 corners.  `d` is the focal length in pixels.
fn project(cx: i32, cy: i32, d: i32, ax: i32, ay: i32, az: i32) -> Frame {
    let mut pts = [(0, 0); 8];
    let mut bbox = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for i in 0..8 {
        let r = rotate(vertex(i), ax, ay, az);
        let z = r.2 + CAM_Z;
        let sx = cx + (r.0 as i64 * d as i64 / z as i64) as i32;
        let sy = cy - (r.1 as i64 * d as i64 / z as i64) as i32;
        pts[i] = (sx, sy);
        bbox = (bbox.0.min(sx), bbox.1.min(sy), bbox.2.max(sx), bbox.3.max(sy));
    }
    Frame { pts, bbox }
}

/// Twice the signed area of a screen-space quad (shoelace formula).
fn area2(q: &[(i32, i32); 4]) -> i64 {
    let mut a = 0i64;
    for i in 0..4 {
        let (x0, y0) = (q[i].0 as i64, q[i].1 as i64);
        let (x1, y1) = (q[(i + 1) % 4].0 as i64, q[(i + 1) % 4].1 as i64);
        a += x0 * y1 - x1 * y0;
    }
    a
}

/// The four screen points of face `f`.
fn quad(frame: &Frame, f: &Face) -> [(i32, i32); 4] {
    [frame.pts[f.verts[0]], frame.pts[f.verts[1]], frame.pts[f.verts[2]], frame.pts[f.verts[3]]]
}

/// Back-face culling from the geometry that is actually drawn: with the
/// counter-clockwise corner order above and screen y pointing down, a face
/// turned towards the camera has a positive signed area.  Judging the
/// rounded screen points (not the 3D normal) means the visible faces always
/// tile the silhouette exactly, with no sliver gaps or overlaps.
fn visible(q: &[(i32, i32); 4]) -> bool {
    area2(q) > 0
}

/// Diffuse light on face `f` as a percentage (30 = ambient only, 100 = full).
fn light(f: &Face, angles: (i32, i32, i32)) -> i32 {
    let n = rotate((f.normal.0 * HALF, f.normal.1 * HALF, f.normal.2 * HALF), angles.0, angles.1, angles.2);
    // |n| = HALF, LIGHT is ~unit in Q14, so the dot product is Q14.
    let dot = (n.0 as i64 * LIGHT.0 as i64 + n.1 as i64 * LIGHT.1 as i64 + n.2 as i64 * LIGHT.2 as i64) / HALF as i64;
    30 + (dot.clamp(0, 16384) as i32) * 70 / 16384
}

/// Draw the cube; returns its bounding box.
fn draw_cube(s: &mut Surface, cx: i32, cy: i32, d: i32, angles: (i32, i32, i32), mode: Mode, edge: u32) -> (i32, i32, i32, i32) {
    let frame = project(cx, cy, d, angles.0, angles.1, angles.2);
    for f in FACES.iter() {
        let quad = quad(&frame, f);
        if !visible(&quad) {
            continue;
        }
        if mode != Mode::Wire {
            let lit = light(f, angles);
            let sh = |v: u8| (v as i32 * lit / 100).min(255) as u8;
            s.fill_convex(&quad, s.rgb(sh(f.base.0), sh(f.base.1), sh(f.base.2)));
        }
        if mode != Mode::Solid {
            for i in 0..4 {
                let (a, b) = (quad[i], quad[(i + 1) % 4]);
                s.line(a.0, a.1, b.0, b.1, 2, edge);
            }
        }
    }
    frame.bbox
}

pub fn run() {
    while crate::hw::kbd::pop_char().is_some() {}
    let Some((w, h)) = crate::console::with_surface(|s| (s.w as i32, s.h as i32)) else {
        crate::kprintln!("cube: needs a graphics framebuffer, but this machine is in VGA text mode.");
        crate::kprintln!("      (BIOS boot: check that loader.conf does not set vbe_mode=\"off\")");
        return;
    };

    let (cx, cy) = (w / 2, h / 2 + 8);
    let half_view = (w.min(h) / 2 - 24).max(40);
    // Focal length: the widest silhouette (a corner, 443 units out, up to
    // 557 units from the camera) then reaches ~0.9 of the view radius.
    let d = (half_view as i64 * 9 / 8) as i32;

    let mut mode = Mode::SolidEdges;
    let mut paused = false;
    let mut speed = 100i64; // percent
    let mut clock: i64 = 0; // scaled milliseconds
    let mut last = crate::hw::pit::uptime_ms();
    let mut next_frame = last;
    let mut prev_box: Option<(i32, i32, i32, i32)> = None;
    let (mut frames, mut fps, mut fps_at) = (0u32, 0u32, last);
    let mut hud_dirty = true;

    crate::console::with_surface(|s| {
        let bg = s.rgb(8, 10, 18);
        s.clear(bg);
    });

    loop {
        let mut redraw_hud = false;
        while let Some(c) = crate::hw::kbd::pop_char() {
            match c {
                0x1b => {
                    crate::console::clear();
                    return;
                }
                b' ' | b'p' | b'P' => {
                    paused = !paused;
                    redraw_hud = true;
                }
                b'+' | b'=' => {
                    speed = (speed + 25).min(800);
                    redraw_hud = true;
                }
                b'-' => {
                    speed = (speed - 25).max(25);
                    redraw_hud = true;
                }
                b'm' | b'M' => {
                    mode = match mode {
                        Mode::SolidEdges => Mode::Solid,
                        Mode::Solid => Mode::Wire,
                        Mode::Wire => Mode::SolidEdges,
                    };
                    redraw_hud = true;
                }
                _ => {}
            }
        }
        hud_dirty |= redraw_hud;

        let now = crate::hw::pit::uptime_ms();
        if now < next_frame {
            crate::arch::io::hlt();
            continue;
        }
        next_frame = now + FRAME_MS;
        if !paused {
            clock += (now - last) as i64 * speed / 100;
        }
        last = now;

        // Three axes at different rates: one full turn every 6 s, 9 s, 15 s.
        let angles = (
            (clock * TURN as i64 / 9000) as i32,
            (clock * TURN as i64 / 6000) as i32,
            (clock * TURN as i64 / 15000) as i32,
        );

        frames += 1;
        if now - fps_at >= 1000 {
            fps = (frames as u64 * 1000 / (now - fps_at)) as u32;
            frames = 0;
            fps_at = now;
            hud_dirty = true;
        }

        let hud = format!(
            "PRISM CUBE   {} fps   speed {}%   mode {}{}",
            fps,
            speed,
            match mode {
                Mode::Solid => "solid",
                Mode::SolidEdges => "solid+edges",
                Mode::Wire => "wire",
            },
            if paused { "   [paused]" } else { "" }
        );

        crate::console::with_surface(|s| {
            let bg = s.rgb(8, 10, 18);
            let edge = s.rgb(245, 248, 255);
            if let Some((x0, y0, x1, y1)) = prev_box {
                s.fill_rect(x0 - 4, y0 - 4, x1 - x0 + 9, y1 - y0 + 9, bg);
            }
            let bb = draw_cube(s, cx, cy, d, angles, mode, edge);
            prev_box = Some(bb);
            if hud_dirty {
                let scale = (h as usize / 540).clamp(1, 3);
                s.fill_rect(0, 8, w, 16 * scale as i32 + 4, bg);
                s.text(16, 10, &hud, scale, s.rgb(150, 160, 190), bg);
                let help = "SPACE pause   +/- speed   M mode   ESC quit";
                let hw = Surface::text_width(help, scale) as i32;
                s.fill_rect(0, h - 16 * scale as i32 - 14, w, 16 * scale as i32 + 6, bg);
                s.text((w - hw) / 2, h - 16 * scale as i32 - 10, help, scale, s.rgb(110, 120, 150), bg);
            }
        });
        hud_dirty = false;
    }
}
