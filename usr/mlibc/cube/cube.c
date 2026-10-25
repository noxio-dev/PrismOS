/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/cube -- the spinning cube of sys/kern/src/lib/cube.rs, ported off
 * the kernel console (phase 3.3): the same fixed-point maths, the same
 * faces, the same light, the same controls -- drawn by a process now.
 *
 * The kernel original ran with the console surface in hand: its Surface
 * (console/gfx.rs), its keyboard queue, its tick counter.  This one owns
 * none of that, and that is the point of the port: everything a program
 * needs to animate the screen is two raw calls and a file descriptor --
 *
 *   prism_fb_info()   the screen's geometry and pixel format, so the
 *                     program packs its pixels the way the framebuffer
 *                     wants them (the kernel's Surface::rgb, mirrored)
 *   prism_fb_blit()   copy a rectangle of them onto the screen, clipped
 *                     -- the whole graphics API (usr/mlibc/prism_raw.h)
 *   /dev/console      opened O_NONBLOCK: read() returns whatever keys
 *                     are pending, EAGAIN when dry
 *   prism_uptime_ms() the same tick counter the kernel cube timed with
 *
 * The renderer keeps its own back buffer (an anonymous mmap, exactly the
 * shape the kernel's would have), draws the cube into it with its own
 * rect / line / convex-fill code -- straight ports of Surface's -- and
 * sends only the dirty rectangle over per frame, the same bounding-box
 * trick the kernel cube used.  A per-frame blit of a few hundred KB is
 * the cost of there being no /dev/fb to mmap through yet.
 *
 * The maths stays fixed point (Q14: 16384 = 1.0), line for line from
 * the kernel original: a Bhaskara sine table of one turn, the same
 * rotation order, the same faces with their counter-clockwise corners,
 * the same back-face culling on the signed screen area.  In ring 3 it
 * could all be doubles -- but then it would not be the same cube.
 *
 * Controls, as in the kernel: SPACE or P pause, + and - the speed,
 * M cycles solid+edges / solid / wire, ESC quits.
 *
 * The watchdog is the one kernel rule this program meets that the
 * kernel original did not: a process is killed after 3 s of charged
 * CPU.  Rendering twenty frames a second burns maybe a fifth of that,
 * so the default budget lasts a couple of minutes; 'exec -t 100000
 * cube' buys hours (ticks at 100 Hz, only CPU time is charged).
 *
 *   cube                     run it (from the shell: exec cube)
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#include "../prism_raw.h"

/* ---- fixed point Q14: 16384 = 1.0 ----------------------------------- */

#define TURN 1024        /* a full turn, in angle units */
#define HALF 256         /* half an edge, cube units */
#define Q14 16384        /* 1.0 */
#define FRAME_MS 20      /* 50 fps */
#define CAM_Z 1000       /* camera distance, cube units */

static int16_t SIN[TURN];

/* The sine table, built once at start: Bhaskara's approximation over a
 * half turn (max error ~0.0016, invisible on screen), the kernel's
 * build_sin() word for word. */
static void build_sin(void)
{
        for (int i = 0; i < TURN; i++) {
                int neg = i >= 512;
                int a = neg ? i - 512 : i;
                /* Half-turn index -> degrees * 256, then 4x(180-x) / (40500 - x(180-x)). */
                int64_t x = (int64_t)a * 90;
                int64_t p = x * (46080 - x);
                int64_t v = 4 * p * Q14 / (40500L * 65536 - p);
                SIN[i] = (int16_t)(neg ? -v : v);
        }
}

static int sin_q14(int a)
{
        return SIN[a & (TURN - 1)];
}

static int cos_q14(int a)
{
        return sin_q14(a + TURN / 4);
}

typedef struct { int32_t x, y, z; } V3;

/* Rotate around Y, then X, then Z -- the kernel's rotate(), verbatim. */
static V3 rotate(V3 v, int ax, int ay, int az)
{
        V3 r;
        int s = sin_q14(ay), c = cos_q14(ay);
        int x = (v.x * c + v.z * s) >> 14, y = v.y, z = (v.z * c - v.x * s) >> 14;
        s = sin_q14(ax); c = cos_q14(ax);
        int x2 = x, y2 = (y * c - z * s) >> 14, z2 = (y * s + z * c) >> 14;
        s = sin_q14(az); c = cos_q14(az);
        r.x = (x2 * c - y2 * s) >> 14;
        r.y = (x2 * s + y2 * c) >> 14;
        r.z = z2;
        return r;
}

/* Vertex i: bit 0 = +x, bit 1 = +y, bit 2 = +z. */
static V3 vertex(int i)
{
        V3 v;
        v.x = (i >> 0 & 1) ? HALF : -HALF;
        v.y = (i >> 1 & 1) ? HALF : -HALF;
        v.z = (i >> 2 & 1) ? HALF : -HALF;
        return v;
}

typedef struct {
        int verts[4];
        V3 normal;
        uint8_t base[3];
} Face;

/* Every face lists its corners counter-clockwise seen from outside
 * (right handed axes), which the culling below relies on. */
static const Face FACES[6] = {
        { { 4, 6, 2, 0 }, { -1, 0, 0 }, { 235, 72, 72 } },
        { { 1, 3, 7, 5 }, { 1, 0, 0 }, { 240, 150, 50 } },
        { { 0, 1, 5, 4 }, { 0, -1, 0 }, { 80, 200, 110 } },
        { { 6, 7, 3, 2 }, { 0, 1, 0 }, { 70, 150, 240 } },
        { { 2, 3, 1, 0 }, { 0, 0, -1 }, { 190, 90, 230 } },
        { { 4, 5, 7, 6 }, { 0, 0, 1 }, { 60, 210, 220 } },
};

/* Direction towards the light, Q14, roughly unit length. */
static const V3 LIGHT = { -5400, 8200, -12400 };

enum { M_SOLID_EDGES, M_SOLID, M_WIRE };

static const char *mode_name(int m)
{
        return m == M_SOLID ? "solid" : m == M_WIRE ? "wire" : "solid+edges";
}

typedef struct { int32_t x, y; } P2;

/* Screen points of the 8 corners + the projected bounding box. */
typedef struct {
        P2 pts[8];
        int bx0, by0, bx1, by1;
} Frame;

/* Rotate and project the 8 corners; `d` is the focal length in pixels. */
static Frame project(int cx, int cy, int d, int ax, int ay, int az)
{
        Frame fr;
        fr.bx0 = fr.by0 = INT32_MAX;
        fr.bx1 = fr.by1 = INT32_MIN;
        for (int i = 0; i < 8; i++) {
                V3 r = rotate(vertex(i), ax, ay, az);
                int z = r.z + CAM_Z; /* always > 0: |r.z| <= 443 < CAM_Z */
                int sx = cx + (int)((int64_t)r.x * d / z);
                int sy = cy - (int)((int64_t)r.y * d / z);
                fr.pts[i].x = sx;
                fr.pts[i].y = sy;
                if (sx < fr.bx0) fr.bx0 = sx;
                if (sy < fr.by0) fr.by0 = sy;
                if (sx > fr.bx1) fr.bx1 = sx;
                if (sy > fr.by1) fr.by1 = sy;
        }
        return fr;
}

/* Twice the signed area of a screen-space quad (shoelace formula). */
static int64_t area2(const P2 q[4])
{
        int64_t a = 0;
        for (int i = 0; i < 4; i++) {
                int64_t x0 = q[i].x, y0 = q[i].y;
                int64_t x1 = q[(i + 1) % 4].x, y1 = q[(i + 1) % 4].y;
                a += x0 * y1 - x1 * y0;
        }
        return a;
}

static int visible(const P2 q[4])
{
        return area2(q) > 0;
}

/* Diffuse light on a face as a percentage (30 = ambient, 100 = full). */
static int light_pct(const Face *f, int ax, int ay, int az)
{
        V3 n = rotate((V3){ f->normal.x * HALF, f->normal.y * HALF, f->normal.z * HALF },
            ax, ay, az);
        /* |n| = HALF, LIGHT is ~unit in Q14, so the dot product is Q14. */
        int64_t dot = ((int64_t)n.x * LIGHT.x + (int64_t)n.y * LIGHT.y +
            (int64_t)n.z * LIGHT.z) / HALF;
        if (dot < 0)
                dot = 0;
        if (dot > Q14)
                dot = Q14;
        return 30 + (int)dot * 70 / Q14;
}

/* ---- the canvas: a back buffer the size of the screen ---------------- */

static struct prism_fb_info FB; /* the screen, as the kernel described it */
static uint32_t *canvas;        /* CW x CH pixels, packed its way */
static int CW, CH;

/* Pack 8-bit channels into the screen's pixel format: the kernel's
 * Surface::rgb(), on the numbers fb_info() brought back. */
static uint32_t rgb_pack(uint8_t r, uint8_t g, uint8_t b)
{
        uint32_t v = 0;

        if (FB.r_bits && FB.r_bits <= 8)
                v |= (uint32_t)(r >> (8 - FB.r_bits)) << FB.r_shift;
        if (FB.g_bits && FB.g_bits <= 8)
                v |= (uint32_t)(g >> (8 - FB.g_bits)) << FB.g_shift;
        if (FB.b_bits && FB.b_bits <= 8)
                v |= (uint32_t)(b >> (8 - FB.b_bits)) << FB.b_shift;
        return v;
}

static void put(int x, int y, uint32_t c)
{
        if (x < 0 || y < 0 || x >= CW || y >= CH)
                return;
        canvas[(size_t)y * CW + x] = c;
}

/* Fill a clipped rectangle (Surface::rect_raw). */
static void fill_rect(int x, int y, int w, int h, uint32_t c)
{
        int x0 = x > 0 ? x : 0;
        int y0 = y > 0 ? y : 0;
        int64_t xe = (int64_t)x + w, ye = (int64_t)y + h;
        int x1 = xe < 0 ? 0 : xe > CW ? CW : (int)xe;
        int y1 = ye < 0 ? 0 : ye > CH ? CH : (int)ye;

        if (x0 >= x1 || y0 >= y1)
                return;
        for (int yy = y0; yy < y1; yy++) {
                uint32_t *row = canvas + (size_t)yy * CW + x0;
                for (int i = 0; i < x1 - x0; i++)
                        row[i] = c;
        }
}

/* Bresenham line, `t` pixels thick (Surface::line). */
static void draw_line(int x0, int y0, int x1, int y1, int t, uint32_t c)
{
        int x = x0, y = y0;
        int dx = x1 > x0 ? x1 - x0 : x0 - x1;
        int dy = -(y1 > y0 ? y1 - y0 : y0 - y1);
        int sx = x0 < x1 ? 1 : -1;
        int sy = y0 < y1 ? 1 : -1;
        int err = dx + dy;

        if (t < 1)
                t = 1;
        for (;;) {
                if (t == 1)
                        put(x, y, c);
                else
                        fill_rect(x - t / 2, y - t / 2, t, t, c);
                if (x == x1 && y == y1)
                        break;
                int e2 = 2 * err;
                if (e2 >= dy) {
                        err += dy;
                        x += sx;
                }
                if (e2 <= dx) {
                        err += dx;
                        y += sy;
                }
        }
}

/* Fill a convex polygon with a scanline walk (Surface::fill_convex). */
static void fill_convex(const P2 pts[], int n, uint32_t c)
{
        int ymin = INT32_MAX, ymax = INT32_MIN;

        for (int i = 0; i < n; i++) {
                if (pts[i].y < ymin) ymin = pts[i].y;
                if (pts[i].y > ymax) ymax = pts[i].y;
        }
        for (int y = ymin > 0 ? ymin : 0; y <= (ymax < CH - 1 ? ymax : CH - 1); y++) {
                int lo = INT32_MAX, hi = INT32_MIN;
                for (int i = 0; i < n; i++) {
                        P2 a = pts[i], b = pts[(i + 1) % n];
                        if (a.y == b.y) {
                                if (a.y == y) {
                                        int xa = a.x < b.x ? a.x : b.x;
                                        int xb = a.x < b.x ? b.x : a.x;
                                        if (xa < lo) lo = xa;
                                        if (xb > hi) hi = xb;
                                }
                                continue;
                        }
                        int top = a.y < b.y ? a.y : b.y;
                        int bot = a.y < b.y ? b.y : a.y;
                        if (y < top || y > bot)
                                continue;
                        int x = (int)((int64_t)a.x + (int64_t)(y - a.y) * (b.x - a.x) /
                            (b.y - a.y));
                        if (x < lo) lo = x;
                        if (x > hi) hi = x;
                }
                if (lo <= hi)
                        fill_rect(lo, y, hi - lo + 1, 1, c);
        }
}

/* ---- the HUD font: an 8x8 hand set, HUD words only ------------------- */

/* Bits 7..0 of each byte are the row's 8 pixels, left to right.  Only
 * what the HUD spells is here; anything else draws as a space.  (The
 * kernel original prints in Unifont -- a whole megabyte of glyphs, on
 * the kernel's side of the fence; this is the userland compromise.) */
static const struct { char c; uint8_t rows[8]; } FONT[] = {
        { ' ', { 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00 } },
        { '%', { 0xC6, 0xCC, 0x18, 0x30, 0x60, 0xCC, 0x86, 0x00 } },
        { '+', { 0x00, 0x30, 0x30, 0xFC, 0x30, 0x30, 0x00, 0x00 } },
        { '-', { 0x00, 0x00, 0x00, 0xFC, 0x00, 0x00, 0x00, 0x00 } },
        { '/', { 0x06, 0x0C, 0x18, 0x30, 0x60, 0xC0, 0x80, 0x00 } },
        { '0', { 0x78, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0x78, 0x00 } },
        { '1', { 0x30, 0x70, 0x30, 0x30, 0x30, 0x30, 0xFC, 0x00 } },
        { '2', { 0x78, 0xCC, 0x0C, 0x38, 0x60, 0xCC, 0xFC, 0x00 } },
        { '3', { 0x78, 0xCC, 0x0C, 0x38, 0x0C, 0xCC, 0x78, 0x00 } },
        { '4', { 0x1C, 0x3C, 0x6C, 0xCC, 0xFC, 0x0C, 0x0C, 0x00 } },
        { '5', { 0xFC, 0xC0, 0xF8, 0x0C, 0x0C, 0xCC, 0x78, 0x00 } },
        { '6', { 0x3C, 0x60, 0xC0, 0xF8, 0xCC, 0xCC, 0x78, 0x00 } },
        { '7', { 0xFC, 0x0C, 0x18, 0x30, 0x60, 0x60, 0x60, 0x00 } },
        { '8', { 0x78, 0xCC, 0xCC, 0x78, 0xCC, 0xCC, 0x78, 0x00 } },
        { '9', { 0x78, 0xCC, 0xCC, 0x7C, 0x0C, 0x18, 0x70, 0x00 } },
        { '[', { 0x3C, 0x30, 0x30, 0x30, 0x30, 0x30, 0x3C, 0x00 } },
        { ']', { 0x3C, 0x0C, 0x0C, 0x0C, 0x0C, 0x0C, 0x3C, 0x00 } },
        { 'A', { 0x78, 0xCC, 0xCC, 0xFC, 0xCC, 0xCC, 0xCC, 0x00 } },
        { 'B', { 0xF8, 0xCC, 0xCC, 0xF8, 0xCC, 0xCC, 0xF8, 0x00 } },
        { 'C', { 0x78, 0xCC, 0xC0, 0xC0, 0xC0, 0xCC, 0x78, 0x00 } },
        { 'D', { 0xF8, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xF8, 0x00 } },
        { 'E', { 0xFC, 0xC0, 0xC0, 0xF8, 0xC0, 0xC0, 0xFC, 0x00 } },
        { 'F', { 0xFC, 0xC0, 0xC0, 0xF8, 0xC0, 0xC0, 0xC0, 0x00 } },
        { 'G', { 0x78, 0xCC, 0xC0, 0xDC, 0xCC, 0xCC, 0x7C, 0x00 } },
        { 'I', { 0xFC, 0x30, 0x30, 0x30, 0x30, 0x30, 0xFC, 0x00 } },
        { 'L', { 0xC0, 0xC0, 0xC0, 0xC0, 0xC0, 0xC0, 0xFC, 0x00 } },
        { 'M', { 0xC0, 0xE0, 0xF0, 0xD8, 0xCC, 0xCC, 0xCC, 0x00 } },
        { 'P', { 0xF8, 0xCC, 0xCC, 0xF8, 0xC0, 0xC0, 0xC0, 0x00 } },
        { 'Q', { 0x78, 0xCC, 0xCC, 0xCC, 0xDC, 0x78, 0x1E, 0x00 } },
        { 'R', { 0xF8, 0xCC, 0xCC, 0xF8, 0xD8, 0xCC, 0xCC, 0x00 } },
        { 'S', { 0x7C, 0xC0, 0xC0, 0x78, 0x0C, 0x0C, 0xF8, 0x00 } },
        { 'T', { 0xFC, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x00 } },
        { 'U', { 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0x78, 0x00 } },
        { 'W', { 0xCC, 0xCC, 0xCC, 0xD8, 0x78, 0x30, 0x30, 0x00 } },
        { 'a', { 0x00, 0x00, 0x78, 0x0C, 0x7C, 0xCC, 0x7C, 0x00 } },
        { 'b', { 0xC0, 0xC0, 0xF8, 0xCC, 0xCC, 0xCC, 0xF8, 0x00 } },
        { 'c', { 0x00, 0x00, 0x78, 0xCC, 0xC0, 0xCC, 0x78, 0x00 } },
        { 'd', { 0x0C, 0x0C, 0x7C, 0xCC, 0xCC, 0xCC, 0x7C, 0x00 } },
        { 'e', { 0x00, 0x00, 0x78, 0xCC, 0xFC, 0xC0, 0x78, 0x00 } },
        { 'f', { 0x38, 0x60, 0xF8, 0x60, 0x60, 0x60, 0x60, 0x00 } },
        { 'g', { 0x00, 0x00, 0x7C, 0xCC, 0xCC, 0x7C, 0x0C, 0x78 } },
        { 'i', { 0x00, 0x30, 0x00, 0x30, 0x30, 0x30, 0x30, 0x78 } },
        { 'l', { 0x60, 0x60, 0x60, 0x60, 0x60, 0x60, 0x78, 0x00 } },
        { 'm', { 0x00, 0x00, 0xD8, 0xFC, 0xD8, 0xD8, 0xD8, 0x00 } },
        { 'o', { 0x00, 0x00, 0x78, 0xCC, 0xCC, 0xCC, 0x78, 0x00 } },
        { 'p', { 0x00, 0x00, 0xF8, 0xCC, 0xCC, 0xF8, 0xC0, 0xC0 } },
        { 'q', { 0x00, 0x00, 0x7C, 0xCC, 0xCC, 0x7C, 0x0C, 0x0C } },
        { 'r', { 0x00, 0x00, 0x78, 0xCC, 0xF0, 0xC0, 0xC0, 0x00 } },
        { 's', { 0x00, 0x00, 0x7C, 0xC0, 0x78, 0x0C, 0xF8, 0x00 } },
        { 't', { 0x30, 0x30, 0xFC, 0x30, 0x30, 0x30, 0x1E, 0x00 } },
        { 'u', { 0x00, 0x00, 0xCC, 0xCC, 0xCC, 0xCC, 0x7C, 0x00 } },
        { 'w', { 0x00, 0x00, 0xCC, 0xCC, 0xD8, 0x78, 0x30, 0x00 } },
};

static const uint8_t *glyph_of(int c)
{
        for (size_t i = 0; i < sizeof FONT / sizeof FONT[0]; i++)
                if (FONT[i].c == c)
                        return FONT[i].rows;
        return FONT[0].rows; /* the space: anything unlisted draws blank */
}

/* Pixel width of `s` at `scale`. */
static int text_width(const char *s, int scale)
{
        return (int)strlen(s) * 8 * scale;
}

/* Draw a line of text on an opaque background, `scale` x `scale` per
 * font pixel (Surface::text's discipline, a lighter font). */
static void draw_text(int x, int y, const char *s, int scale, uint32_t fg, uint32_t bg)
{
        if (scale < 1)
                scale = 1;
        if (scale > 3)
                scale = 3;
        for (; *s; s++, x += 8 * scale) {
                const uint8_t *g = glyph_of(*s);
                for (int gy = 0; gy < 8; gy++) {
                        for (int gx = 0; gx < 8; gx++) {
                                uint32_t col = (g[gy] & (0x80 >> gx)) ? fg : bg;
                                for (int sy = 0; sy < scale; sy++)
                                        for (int sx = 0; sx < scale; sx++)
                                                put(x + gx * scale + sx,
                                                    y + gy * scale + sy, col);
                        }
                }
        }
}

/* ---- the cube --------------------------------------------------------- */

/* Draw the cube into the canvas; returns its bounding box.  The kernel's
 * draw_cube(): back-face culling on the rounded screen points (the
 * silhouette tiles exactly, no slivers), the lit face colour, the
 * edges unless solid mode. */
static void draw_cube(int cx, int cy, int d, int ax, int ay, int az,
        int mode, uint32_t edge, int *bb)
{
        Frame fr = project(cx, cy, d, ax, ay, az);

        for (int f = 0; f < 6; f++) {
                P2 q[4];

                for (int i = 0; i < 4; i++) {
                        q[i].x = fr.pts[FACES[f].verts[i]].x;
                        q[i].y = fr.pts[FACES[f].verts[i]].y;
                }
                if (!visible(q))
                        continue;
                if (mode != M_WIRE) {
                        int lit = light_pct(&FACES[f], ax, ay, az);
                        int r = (int)FACES[f].base[0] * lit / 100;
                        int g = (int)FACES[f].base[1] * lit / 100;
                        int b = (int)FACES[f].base[2] * lit / 100;

                        if (r > 255) r = 255;
                        if (g > 255) g = 255;
                        if (b > 255) b = 255;
                        fill_convex(q, 4, rgb_pack((uint8_t)r, (uint8_t)g, (uint8_t)b));
                }
                if (mode != M_SOLID)
                        for (int i = 0; i < 4; i++)
                                draw_line(q[i].x, q[i].y, q[(i + 1) % 4].x,
                                    q[(i + 1) % 4].y, 2, edge);
        }
        bb[0] = fr.bx0; bb[1] = fr.by0; bb[2] = fr.bx1; bb[3] = fr.by1;
}

/* ---- keyboard: /dev/console, O_NONBLOCK ------------------------------- */

static int kbd = -1;

/* The keys of the command line itself may still be in the queue; the
 * kernel original drained pop_char() the same way before starting. */
static void kbd_drain(void)
{
        uint8_t c;

        while (read(kbd, &c, 1) == 1)
                ;
}

/* ---- the dirty rectangle: what this frame's blit sends --------------- */

static int dx0, dy0, dx1, dy1; /* [dx0, dx1) x [dy0, dy1), clipped */

static void dirty_reset(void)
{
        dx0 = dy0 = INT32_MAX;
        dx1 = dy1 = INT32_MIN;
}

static void dirty_add(int x0, int y0, int x1, int y1)
{
        if (x0 < 0) x0 = 0;
        if (y0 < 0) y0 = 0;
        if (x1 > CW) x1 = CW;
        if (y1 > CH) y1 = CH;
        if (x0 >= x1 || y0 >= y1)
                return;
        if (x0 < dx0) dx0 = x0;
        if (y0 < dy0) dy0 = y0;
        if (x1 > dx1) dx1 = x1;
        if (y1 > dy1) dy1 = y1;
}

/* Send the dirty rectangle to the screen: one fb_blit, source packed
 * in the format fb_info() described. */
static int blit_dirty(void)
{
        if (dx0 >= dx1 || dy0 >= dy1)
                return 0;
        const uint32_t *src = canvas + (size_t)dy0 * CW + dx0;

        if (prism_fb_blit(src, (uint32_t)(dx1 - dx0), (uint32_t)(dy1 - dy0),
            (uint32_t)CW * 4, (int32_t)dx0, (int32_t)dy0) < 0)
                return -1;
        return 0;
}

/* ---- run --------------------------------------------------------------- */

static int run(void)
{
        uint32_t F_BG, F_EDGE, F_HUD, F_HELP;
        int cx, cy, half_view, d, scale;
        int mode = M_SOLID_EDGES, paused = 0;
        long speed = 100;       /* percent */
        int64_t clock = 0;      /* scaled milliseconds */
        long last, next_frame;
        int have_prev = 0, prev[4];
        uint32_t frames = 0, fps = 0;
        long fps_at;
        int hud_dirty = 1;

        long r = prism_fb_info(&FB);

        if (r < 0) {
                if (r == -PRISM_ENODEV) {
                        printf("cube: needs a graphics framebuffer, but this machine is in VGA text mode.\n");
                        printf("      (BIOS boot: check that loader.conf does not set vbe_mode=\"off\")\n");
                } else {
                        printf("cube: fb_info failed: %ld\n", r);
                }
                return 1;
        }
        CW = (int)FB.w;
        CH = (int)FB.h;
        if (CW < 8 || CH < 16) {
                printf("cube: a %dx%d screen is too small\n", CW, CH);
                return 1;
        }
        canvas = mmap(0, (size_t)CW * CH * 4, PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (canvas == MAP_FAILED) {
                printf("cube: cannot mmap a %dx%d back buffer (%s)\n",
                    CW, CH, strerror(errno));
                return 1;
        }

        kbd = open("/dev/console", O_RDONLY | O_NONBLOCK);
        if (kbd < 0) {
                printf("cube: cannot open /dev/console (%s)\n", strerror(errno));
                return 1;
        }
        kbd_drain();

        F_BG = rgb_pack(8, 10, 18);
        F_EDGE = rgb_pack(245, 248, 255);
        F_HUD = rgb_pack(150, 160, 190);
        F_HELP = rgb_pack(110, 120, 150);

        cx = CW / 2;
        cy = CH / 2 + 8;
        half_view = (CW < CH ? CW : CH) / 2 - 24;
        if (half_view < 40)
                half_view = 40;
        /* Focal length: the widest silhouette then reaches ~0.9 of the
         * view radius -- the kernel's arithmetic, kept. */
        d = (int)((int64_t)half_view * 9 / 8);
        scale = CH / 540;
        if (scale < 1) scale = 1;
        if (scale > 3) scale = 3;

        /* The opening frame: the whole screen, to the cube's background. */
        fill_rect(0, 0, CW, CH, F_BG);
        dirty_reset();
        dirty_add(0, 0, CW, CH);
        if (blit_dirty() < 0)
                goto blit_failed;

        last = prism_uptime_ms();
        next_frame = last;
        fps_at = last;

        for (;;) {
                char hud[96];
                int bb[4], redraw_hud = 0, quit = 0;
                long now;

                /* The keyboard: everything pending, one byte per read. */
                for (;;) {
                        uint8_t c;
                        ssize_t n = read(kbd, &c, 1);

                        if (n <= 0)
                                break; /* EAGAIN (or worse): nothing more */
                        switch (c) {
                        case 0x1b:
                                quit = 1;
                                break;
                        case ' ': case 'p': case 'P':
                                paused = !paused;
                                redraw_hud = 1;
                                break;
                        case '+': case '=':
                                speed += 25;
                                if (speed > 800)
                                        speed = 800;
                                redraw_hud = 1;
                                break;
                        case '-':
                                speed -= 25;
                                if (speed < 25)
                                        speed = 25;
                                redraw_hud = 1;
                                break;
                        case 'm': case 'M':
                                mode = mode == M_SOLID_EDGES ? M_SOLID
                                    : mode == M_SOLID ? M_WIRE : M_SOLID_EDGES;
                                redraw_hud = 1;
                                break;
                        }
                        if (quit)
                                break;
                }
                if (quit) {
                        /* The kernel original cleared the console on its way
                         * out; the same courtesy, through the same blit. */
                        fill_rect(0, 0, CW, CH, 0);
                        dirty_reset();
                        dirty_add(0, 0, CW, CH);
                        (void)blit_dirty();
                        close(kbd);
                        return 0;
                }
                hud_dirty |= redraw_hud;

                now = prism_uptime_ms();
                if (now < next_frame) {
                        prism_sleep_ms(1); /* the userland hlt(): park to the tick */
                        continue;
                }
                next_frame = now + FRAME_MS;
                if (!paused)
                        clock += (now - last) * speed / 100;
                last = now;

                /* Three axes at different rates: one full turn every 6 s, 9 s, 15 s. */
                int ax = (int32_t)(clock * TURN / 9000);
                int ay = (int32_t)(clock * TURN / 6000);
                int az = (int32_t)(clock * TURN / 15000);

                frames++;
                if (now - fps_at >= 1000) {
                        fps = (uint32_t)((uint64_t)frames * 1000 / (uint64_t)(now - fps_at));
                        frames = 0;
                        fps_at = now;
                        hud_dirty = 1;
                }

                snprintf(hud, sizeof hud,
                    "PRISM CUBE   %u fps   speed %ld%%   mode %s%s",
                    fps, speed, mode_name(mode), paused ? "   [paused]" : "");

                dirty_reset();
                /* Erase where the cube was (the kernel's prev_box wipe,
                 * a little wider for the thick edges). */
                if (have_prev)
                        fill_rect(prev[0] - 6, prev[1] - 6,
                            prev[2] - prev[0] + 13, prev[3] - prev[1] + 13, F_BG);
                draw_cube(cx, cy, d, ax, ay, az, mode, F_EDGE, bb);
                if (have_prev)
                        dirty_add(prev[0] - 6, prev[1] - 6,
                            prev[2] + 7, prev[3] + 7);
                dirty_add(bb[0] - 6, bb[1] - 6, bb[2] + 7, bb[3] + 7);
                have_prev = 1;
                prev[0] = bb[0]; prev[1] = bb[1]; prev[2] = bb[2]; prev[3] = bb[3];

                if (hud_dirty) {
                        const char *help = "SPACE pause   +/- speed   M mode   ESC quit";
                        int hw = text_width(help, scale);
                        int hy = CH - 8 * scale - 10;

                        fill_rect(0, 8, CW, 8 * scale + 4, F_BG);
                        draw_text(16, 10, hud, scale, F_HUD, F_BG);
                        fill_rect(0, CH - 8 * scale - 14, CW, 8 * scale + 6, F_BG);
                        draw_text((CW - hw) / 2, hy, help, scale, F_HELP, F_BG);
                        dirty_add(0, 8, CW, 8 + 8 * scale + 8);
                        dirty_add(0, CH - 8 * scale - 14, CW, CH);
                }
                hud_dirty = 0;

                if (blit_dirty() < 0) {
blit_failed:
                        printf("cube: fb_blit failed\n");
                        close(kbd);
                        return 1;
                }
        }
}

int main(int argc, char **argv, char **envp)
{
        (void)argc;
        (void)argv;
        (void)envp;
        /* Unbuffered, like every port: the shell's prompt must not wait. */
        setvbuf(stdout, 0, _IONBF, 0);
        build_sin();
        return run();
}
