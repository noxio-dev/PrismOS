/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /bin/flappy -- Flappy Bird, ported to PrismOS off /bin/cube's discipline.
 *
 * The game is Dong Nguyen's, with the day/night skylines, the three bird
 * palettes and the medals of the original; the random events (wind, moving
 * pipes, gravity shifts) of Tatone26's version are not ported -- the core
 * loop is, constants line for line.
 *
 * Everything a process needs, the cube already proved:
 *
 *   prism_fb_info()    the screen's geometry and pixel layout
 *   prism_fb_blit()    push a rectangle of pixels to the screen
 *   /dev/console       O_NONBLOCK: read() returns the pending keys
 *   prism_uptime_ms()  the tick counter
 *
 * The scene is composed in a 320x240 logical back buffer -- the original's
 * coordinate system, so its tables (the pipe speeds, the gravity/flap pairs,
 * the sprite bitmaps, the font, the medal discs) port verbatim -- and the
 * logical pixels are mapped onto the WHOLE panel with two nearest-neighbour
 * tables, one per axis: every screen pixel samples x*320/CW, y*240/CH, so
 * the game fills the screen edge to edge at any resolution (640x480 keeps
 * the proportions; other panels stretch, the tuning never changes).
 *
 * No dirty rectangles, no park: every pass of the loop recomposes the whole
 * logical buffer from scratch -- skyline, pipes, ground, bird, score, board
 * -- and every panel pixel is rewritten before the full-screen blit, so a
 * new frame can never pile up over an old one.
 *
 * Physics runs in fixed 18 ms steps off prism_uptime_ms(), the tables being
 * per-step; rendering happens exactly when a step ran, and the bird's
 * rotation uses the same Q8 Bhaskara trick as the cube's cube (built once,
 * 256 entries, amplitude 256).
 *
 * The best score persists across boots in a small text file (/var/lib,
 * /etc or /tmp -- first writable wins).
 *
 * Controls: SPACE flap (and start, and retry), ESC quit.
 *
 * The watchdog rule still holds: when nothing stepped and nothing was
 * pressed, the loop parks 1 ms between frames, so a finished game charges
 * a parked blit a pass and a session fits the default budget for a long
 * while; 'exec -t 60000 flappy' buys an afternoon (ticks at 100 Hz, only
 * CPU time is charged).
 *
 *   flappy                   run it (from the shell: exec flappy)
 */
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <termios.h>
#include <unistd.h>

#include "prism_raw.h"

#if defined(__GNUC__) && !defined(__clang__) && !defined(FLAPPY_NO_OPT_PRAGMA)
#pragma GCC optimize ("O2")
#endif

/* ---- the logical screen: the original's 320x240 ---------------------- */

#define W 320
#define H 240
#define GY 200 /* the ground's top */
#define BX 96  /* the bird's centre, across */
#define LW 38  /* a pipe's width (the shaft is 2 pixels narrower each side) */
#define LH 16  /* a pipe's cap height */
#define NP 6
#define OFF (-999)

typedef uint16_t color; /* the logical buffer is RGB565, as the original */
#define RGB(c) (color)((((c) >> 8) & 0xF800) | (((c) >> 5) & 0x07E0) | (((c) >> 3) & 0x1F))
#define WHITE 0xFFFF
#define BLACK 0x0000
#define DARK RGB(0x543847)
#define ORANGE RGB(0xE86101)
#define BEIGE RGB(0xDED895)
#define ADV(k) (5 * (k) + ((k) + 1) / 2) /* letter advance */

static color lb[W * H]; /* the logical back buffer */

static inline void lput(int x, int y, color c)
{
        if ((unsigned)x < (unsigned)W && (unsigned)y < (unsigned)H)
                lb[(size_t)y * W + x] = c;
}

static void lfill(int x, int y, int w, int h, color c)
{
        int x0 = x > 0 ? x : 0, y0 = y > 0 ? y : 0;
        int x1 = x + w < W ? x + w : W, y1 = y + h < H ? y + h : H;

        for (int yy = y0; yy < y1; yy++) {
                color *p = lb + (size_t)yy * W + x0;

                for (int xx = x0; xx < x1; xx++)
                        *p++ = c;
        }
}

static void lrrect(int x, int y, int w, int h, color c) /* corners cut */
{
        lfill(x + 1, y, w - 2, h, c);
        lfill(x, y + 1, w, h - 2, c);
}

/* ---- small helpers ---------------------------------------------------- */

static uint32_t seed = 0x2545F491;

static uint32_t rnd(void)
{
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        return seed;
}

static int rr(int a, int b) { return a + (int)(rnd() % (uint32_t)(b - a + 1)); }
static int imin(int a, int b) { return a < b ? a : b; }
static int imax(int a, int b) { return a > b ? a : b; }
static int iabs(int v) { return v < 0 ? -v : v; }
static int clamp(int v, int a, int b) { return v < a ? a : v > b ? b : v; }

/* Q8 sine, 256 entries a turn, amplitude 256 -- the bird's rotation.
 * Built once in double precision (ring 3 is allowed its doubles); what
 * matters is that the table is there when the sprite walk needs it. */
static int16_t sn[256];
#define SIN(a) sn[(a) & 255]
#define COS(a) sn[((a) + 64) & 255]

static void build_sin(void)
{
        for (int i = 0; i < 256; i++)
                sn[i] = (int16_t)(sin(i * 6.28318530718 / 256.0) * 256.0 + 0.5);
}

/* ---- the font: ASCII 32..122, columns of 8 bits, top first ------------- */

static const uint8_t font[91][5] = {
  {0x00,0x00,0x00,0x00,0x00},{0x00,0x00,0x5F,0x00,0x00},{0x00,0x07,0x00,0x07,0x00},{0x14,0x7F,0x14,0x7F,0x14},
  {0x24,0x2A,0x7F,0x2A,0x12},{0x23,0x13,0x08,0x64,0x62},{0x36,0x49,0x55,0x22,0x50},{0x00,0x05,0x03,0x00,0x00},
  {0x00,0x1C,0x22,0x41,0x00},{0x00,0x41,0x22,0x1C,0x00},{0x14,0x08,0x3E,0x08,0x14},{0x08,0x08,0x3E,0x08,0x08},
  {0x00,0x50,0x30,0x00,0x00},{0x08,0x08,0x08,0x08,0x08},{0x00,0x60,0x60,0x00,0x00},{0x20,0x10,0x08,0x04,0x02},
  {0x3E,0x41,0x41,0x41,0x3E},{0x00,0x42,0x7F,0x40,0x00},{0x42,0x61,0x51,0x49,0x46},{0x21,0x41,0x45,0x4B,0x31},
  {0x18,0x14,0x12,0x7F,0x10},{0x27,0x45,0x45,0x45,0x39},{0x3C,0x4A,0x49,0x49,0x30},{0x01,0x71,0x09,0x05,0x03},
  {0x36,0x49,0x49,0x49,0x36},{0x06,0x49,0x49,0x29,0x1E},{0x00,0x36,0x36,0x00,0x00},{0x00,0x56,0x36,0x00,0x00},
  {0x08,0x14,0x22,0x41,0x00},{0x14,0x14,0x14,0x14,0x14},{0x00,0x41,0x22,0x14,0x08},{0x02,0x01,0x51,0x09,0x06},
  {0x32,0x49,0x79,0x41,0x3E},{0x7E,0x11,0x11,0x11,0x7E},{0x7F,0x49,0x49,0x49,0x36},{0x3E,0x41,0x41,0x41,0x22},
  {0x7F,0x41,0x41,0x22,0x1C},{0x7F,0x49,0x49,0x49,0x41},{0x7F,0x09,0x09,0x09,0x01},{0x3E,0x41,0x41,0x49,0x3A},
  {0x7F,0x08,0x08,0x08,0x7F},{0x00,0x41,0x7F,0x41,0x00},{0x20,0x40,0x41,0x3F,0x01},{0x7F,0x08,0x14,0x22,0x41},
  {0x7F,0x40,0x40,0x40,0x40},{0x7F,0x02,0x0C,0x02,0x7F},{0x7F,0x04,0x08,0x10,0x7F},{0x3E,0x41,0x41,0x41,0x3E},
  {0x7F,0x09,0x09,0x09,0x06},{0x3E,0x41,0x51,0x21,0x5E},{0x7F,0x09,0x19,0x29,0x46},{0x46,0x49,0x49,0x49,0x31},
  {0x01,0x01,0x7F,0x01,0x01},{0x3F,0x40,0x40,0x40,0x3F},{0x1F,0x20,0x40,0x20,0x1F},{0x3F,0x40,0x38,0x40,0x3F},
  {0x63,0x14,0x08,0x14,0x63},{0x07,0x08,0x70,0x08,0x07},{0x61,0x51,0x49,0x45,0x43},{0x00,0x7F,0x41,0x41,0x00},
  {0x02,0x04,0x08,0x10,0x20},{0x00,0x41,0x41,0x7F,0x00},{0x04,0x02,0x01,0x02,0x04},{0x40,0x40,0x40,0x40,0x40},
  {0x00,0x01,0x02,0x04,0x00},{0x20,0x54,0x54,0x54,0x78},{0x7F,0x48,0x44,0x44,0x38},{0x38,0x44,0x44,0x44,0x20},
  {0x38,0x44,0x44,0x48,0x7F},{0x38,0x54,0x54,0x54,0x18},{0x08,0x7E,0x09,0x01,0x02},{0x18,0xA4,0xA4,0xA4,0x7C},
  {0x7F,0x08,0x04,0x04,0x78},{0x00,0x44,0x7D,0x40,0x00},{0x40,0x80,0x84,0x7D,0x00},{0x7F,0x10,0x28,0x44,0x00},
  {0x00,0x41,0x7F,0x40,0x00},{0x7C,0x04,0x18,0x04,0x78},{0x7C,0x08,0x04,0x04,0x78},{0x38,0x44,0x44,0x44,0x38},
  {0xFC,0x24,0x24,0x24,0x18},{0x18,0x24,0x24,0x18,0xFC},{0x7C,0x08,0x04,0x04,0x08},{0x48,0x54,0x54,0x54,0x20},
  {0x04,0x3F,0x44,0x40,0x20},{0x3C,0x40,0x40,0x20,0x7C},{0x1C,0x20,0x40,0x20,0x1C},{0x3C,0x40,0x30,0x40,0x3C},
  {0x44,0x28,0x10,0x28,0x44},{0x1C,0xA0,0xA0,0xA0,0x7C},{0x44,0x64,0x54,0x4C,0x44},
};

static int fpix(char ch, int i, int j)
{
        unsigned g = (uint8_t)ch - 32u;

        return g < 91 && (unsigned)i < 5 && (font[g][i] >> j & 1);
}

static int slen(const char *s)
{
        int n = 0;

        while (s[n]) n++;
        return n;
}

static int tw(const char *s, int k) { return slen(s) * ADV(k) - ADV(k) + 5 * k; }

static void text(const char *s, int x, int y, int k, color c)
{
        for (; *s; s++, x += ADV(k))
                for (int i = 0; i < 5; i++)
                        for (int j = 0; j < 8; j++)
                                if (fpix(*s, i, j))
                                        lfill(x + i * k, y + j * k, k, k, c);
}

static void ctext(const char *s, int cx, int y, int k, color c)
{
        text(s, cx - tw(s, k) / 2, y, k, c);
}

static void otext(const char *s, int x, int y, int k, color c) /* dark outline */
{
        for (int d = 0; d < 9; d++)
                text(s, x + d % 3 - 1, y + d / 3 - 1, k, DARK);
        text(s, x, y, k, c);
}

/* ---- the score's bold digits, 5x7, a bit per column -------------------- */

static const uint8_t digs[10][7] = {
  {0x0E,0x1B,0x1B,0x1B,0x1B,0x1B,0x0E},{0x06,0x07,0x06,0x06,0x06,0x06,0x06},{0x0E,0x1B,0x18,0x0C,0x06,0x03,0x1F},
  {0x0E,0x1B,0x18,0x0C,0x18,0x1B,0x0E},{0x1B,0x1B,0x1B,0x1F,0x18,0x18,0x18},{0x1F,0x03,0x0F,0x18,0x18,0x1B,0x0E},
  {0x0E,0x1B,0x03,0x0F,0x1B,0x1B,0x0E},{0x1F,0x18,0x0C,0x0C,0x06,0x06,0x06},{0x0E,0x1B,0x1B,0x0E,0x1B,0x1B,0x0E},
  {0x0E,0x1B,0x1B,0x1E,0x18,0x1B,0x0E},
};

static int bigw(const char *s, int k)
{
        int w = -k;

        for (; *s; s++) w += (*s == '1' ? 4 : 6) * k;
        return w;
}

static void num(char *o, int v)
{
        char t[8];
        int n = 0;

        do t[n++] = (char)('0' + v % 10); while (v /= 10);
        while (n) *o++ = t[--n];
        *o = 0;
}

static void big(int n, int x, int y, int k, int centre)
{
        char s[8];
        int o = k > 2 ? 2 : 1;

        num(s, n);
        if (centre) x -= bigw(s, k) / 2;
        for (int pass = 0; pass < 2; pass++)
                for (int i = 0, px = x; s[i]; px += (s[i++] == '1' ? 4 : 6) * k)
                        for (int r = 0; r < 7; r++)
                                for (int b = 0; b < 5; b++)
                                        if (digs[s[i] - '0'][r] >> b & 1) {
                                                if (pass) lfill(px + b * k, y + r * k, k, k, WHITE);
                                                else lfill(px + b * k - o, y + r * k - o, k + 2 * o, k + 2 * o, BLACK);
                                        }
}

/* ---- the canvas: logical pixels expanded to the panel ------------------ */

static struct prism_fb_info FB;
static uint32_t *canvas; /* CW x CH, packed the screen's way */
static int CW, CH;

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

/* RGB565 -> the screen's packing, one lookup: 65536 words (256 KB), built
 * once, so the scale pass is two adds and a table read a pixel. */
static uint32_t *P565;

static void build_palette(void)
{
        P565 = mmap(0, 65536 * sizeof(uint32_t), PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

        for (long c = 0; c < 65536; c++) {
                int r5 = (int)(c >> 11) & 31, g6 = (int)(c >> 5) & 63, b5 = (int)c & 31;

                P565[c] = rgb_pack((uint8_t)((r5 << 3) | (r5 >> 2)),
                    (uint8_t)((g6 << 2) | (g6 >> 4)), (uint8_t)((b5 << 3) | (b5 >> 2)));
        }
}

/* ---- the frame: the whole logical screen mapped onto the whole panel ---- */

static uint16_t *mapx, *mapy; /* panel pixel -> logical pixel, one per axis */

static void build_maps(void)
{
        mapx = mmap(0, (size_t)CW * sizeof(uint16_t), PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        mapy = mmap(0, (size_t)CH * sizeof(uint16_t), PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        for (int x = 0; x < CW; x++) mapx[x] = (uint16_t)((long)x * W / CW);
        for (int y = 0; y < CH; y++) mapy[y] = (uint16_t)((long)y * H / CH);
}

/* Every panel pixel is rewritten from the logical buffer, then the whole
 * screen goes over in one blit: no stale pixel can survive a frame. */
static int blit_full(void)
{
        for (int y = 0; y < CH; y++) {
                const color *src = lb + (size_t)mapy[y] * W;
                uint32_t *dst = canvas + (size_t)y * CW;

                for (int x = 0; x < CW; x++)
                        dst[x] = P565[src[mapx[x]]];
        }
        return prism_fb_blit(canvas, (uint32_t)CW, (uint32_t)CH,
            (uint32_t)CW * 4, 0, 0) < 0 ? -1 : 0;
}

/* ---- the scene ----------------------------------------------------------
 * sky, cloud, cloud top, building, building edge, window, lit window, bush,
 * bush top; day then night. */
static const color themes[2][9] = {
  {RGB(0x4EC0CA), RGB(0xE6F9DC), RGB(0xD2F2E6), RGB(0xCDECD4), RGB(0xA8DCBC), RGB(0xB6E3C6), RGB(0xB6E3C6), RGB(0x5EE270), RGB(0x4AC45E)},
  {RGB(0x008793), RGB(0x1CA3AE), RGB(0x2DB2BC), RGB(0x0B5F66), RGB(0x084A50), RGB(0x0F6C73), RGB(0xF7E27A), RGB(0x1E8A4C), RGB(0x16703D)},
};
static const color *th;
static uint8_t cl[W], ct[W], bt[W], bf[W]; /* cloud, city and bush tops; building columns */
static int night;
static int32_t gx;        /* how far the ground went, 1/256 pixel */
static int gph, gph_drawn; /* ...and its stripes' phase */

static int hnext(uint32_t *h, int n)
{
        *h = *h * 1103515245u + 12345u;
        return (int)((*h >> 16) % (uint32_t)n);
}

/* a band of round puffs, without deep notches between them */
static void bumps(uint8_t *a, int base, int r0, int r1, uint32_t *h)
{
        for (int x = 0; x < W; x++) a[x] = (uint8_t)(base - r0 / 2);
        for (int cx = -6; cx < W + 6; cx += r0 + hnext(h, r1)) {
                int r = r0 + hnext(h, r1 - r0 + 1);

                for (int dx = -r; dx <= r; dx++) {
                        int x = cx + dx, y = base - (int)__builtin_sqrtf((float)(r * r - dx * dx));

                        if (x >= 0 && x < W && y < a[x]) a[x] = (uint8_t)y;
                }
        }
        for (int x = 1; x < W; x++) a[x] = (uint8_t)imin(a[x], a[x - 1] + 2);
        for (int x = W - 2; x >= 0; x--) a[x] = (uint8_t)imin(a[x], a[x + 1] + 2);
}

static void make_scene(void)
{
        th = themes[night];
        uint32_t h = 11; /* the same skyline every time */

        bumps(cl, 158, 9, 19, &h);
        for (int x = 0, i = 0, w = 0, top = 0; x < W; x++, i++) {
                if (i >= w) w = 10 + 4 * hnext(&h, 4), top = 176 - hnext(&h, 26), i = 0;
                ct[x] = (uint8_t)top;
                bf[x] = !i ? 1 : (i & 3) >= 2 && i < w - 2 ? 2 : 0;
        }
        bumps(bt, 196, 5, 11, &h);
}

/* one column of the static part of the sky (clouds, city, bushes), rows
 * 0..GY-1 -- the erase primitive: a frame redraws the columns the pipes
 * and the bird crossed, and the skyline behind them comes back for free */
static void bg_column(int x)
{
        for (int y = 0; y < GY; y++) {
                int i;

                if (y >= bt[x]) i = y == bt[x] ? 8 : 7;
                else if (y >= ct[x]) {
                        int t = y - ct[x];

                        i = 3;
                        if (!t || (bf[x] & 1)) i = 4;
                        else if ((bf[x] & 2) && (t & 3) >= 2)
                                i = night && ((x >> 2) * 7 + (t >> 2) * 5 + ct[x]) % 4 == 0 ? 6 : 5;
                } else if (y >= cl[x]) i = y == cl[x] ? 2 : 1;
                else i = 0;
                lput(x, y, th[i]);
        }
}

static void bg_columns(int x0, int x1)
{
        for (int x = x0; x < x1; x++)
                bg_column(x);
}

static void ground_row(int y) /* the whole width: the stripes shift together */
{
        int t = y - GY;
        color *d = lb + (size_t)y * W;

        if (t >= 2 && t < 12) { /* the green stripes, leaning, moving with the pipes */
                int k = (gph + 140 - t) % 14;

                for (int x = 0; x < W; x++, k = k == 13 ? 0 : k + 1)
                        d[x] = k < 7 ? RGB(0x9CE659) : RGB(0x73BF2E);
                return;
        }
        color c = t == 0 ? DARK : t == 1 ? RGB(0xE4FD8B) : t == 12 ? RGB(0x558022) : t == 13 ? RGB(0xD7A84C) : BEIGE;

        for (int x = 0; x < W; x++) d[x] = c;
}

static void ground_full(void)
{
        for (int y = GY; y < H; y++)
                ground_row(y);
}

/* ---- the pipes ---------------------------------------------------------- */

typedef struct {
        int32_t x;          /* left edge, 1/256 pixel */
        int16_t c, g;       /* gap centre and gap, 1/256 pixel */
        uint8_t on, scored;
        int16_t dx, dt, db; /* what the screen showed: left, gap top, gap bottom */
} pipe_t;

static pipe_t P[NP];
static color pcol[2][LW]; /* shaft, cap colour by column */

static int gap_top(const pipe_t *p) { return p->c + 0 - (p->g >> 9); }
static int gap_bot(const pipe_t *p) { return gap_top(p) + (p->g >> 8); }

static void make_pipes(void)
{
        static const char shaft[] = "  OLLHHHLLMMMMMMMMMMMMMMMDMDDDDDDDEO  ";
        static const char cap[] = "OLLHHHHLLMMMMMMMMMMMMMMMMMDMDDDDDDDDEO";
        static const char key[] = " OLHMDE";
        static const color cols[] = {0, DARK, RGB(0x9CE659), RGB(0xE4FD8B), RGB(0x73BF2E), RGB(0x558022), RGB(0x4A7320)};

        for (int u2 = 0; u2 < LW; u2++) {
                int a = 0, b = 0;

                while (key[a] != shaft[u2]) a++;
                while (key[b] != cap[u2]) b++;
                pcol[0][u2] = cols[a];
                pcol[1][u2] = cols[b];
        }
}

static void pipes_draw(void)
{
        for (int i = 0; i < NP; i++) {
                const pipe_t *p = &P[i];
                int px = p->x >> 8;

                if (!p->on || px >= W || px + LW <= 0) continue;
                int gt = gap_top(p), gb = gap_bot(p);

                for (int y = 0; y < GY; y++) {
                        int r = y < gt - LH ? -1 : y < gt ? y - gt + LH : y < gb ? -2 : y < gb + LH ? y - gb : -1;

                        if (r == -2) continue;
                        color *d = lb + (size_t)y * W;
                        if (r == 0 || r == LH - 1) { /* the cap's outline */
                                for (int x = imax(px, 0); x < imin(px + LW, W); x++) d[x] = DARK;
                                continue;
                        }
                        const color *c = pcol[r > 0];
                        for (int x = imax(px, r < 0 ? px + 2 : px); x < imin(px + LW, r < 0 ? px + LW - 2 : px + LW); x++)
                                if (x >= 0) d[x] = c[x - px];
                }
        }
}

/* ---- the bird -----------------------------------------------------------
 * 24x17, 3 bits a pixel: none, outline, white, body, light, belly, lips (2) */
static const uint8_t bird_body[154] = {
  0x00,0x00,0x00,0x48,0x92,0x04,0x00,0x00,0x00,0x00,0x00,0x20,0x21,0x49,0x26,0x09,0x00,0x00,0x00,0x00,0x84,0x24,
  0xC9,0x48,0x52,0x00,0x00,0x00,0x80,0x90,0xDB,0x16,0x49,0x92,0x02,0x00,0x00,0x90,0x6D,0xDB,0x22,0x49,0x92,0x14,
  0x00,0x00,0xB2,0x6D,0xDB,0x22,0x49,0x4A,0x14,0x00,0x40,0xB6,0x6D,0xDB,0x22,0x49,0x4A,0x14,0x00,0xC8,0xB6,0x6D,
  0xDB,0x22,0x49,0x4A,0x14,0x00,0xC8,0xB6,0x6D,0xDB,0x16,0x49,0x92,0x02,0x00,0xD9,0xB6,0x6D,0xDB,0xB6,0x24,0x49,
  0x92,0x00,0xD9,0xB6,0x6D,0xDB,0x16,0xDB,0xB6,0x6D,0x07,0xD9,0xB6,0x6D,0xDB,0x62,0x27,0x49,0x92,0x00,0x48,0xBB,
  0x6D,0xDB,0xE2,0xFF,0xFF,0xFF,0x00,0x40,0xDA,0xB6,0x6D,0x9B,0x24,0x49,0x12,0x00,0x00,0x92,0xB6,0x6D,0xDB,0x36,
  0x00,0x00,0x00,0x00,0x80,0x24,0x6D,0xDB,0x04,0x00,0x00,0x00,0x00,0x00,0x00,0x49,0x12,0x00,0x00,0x00,0x00,0x00,
};
/* the wing, up, level and down: 8x7, 2 bits (none, outline, white, light) */
static const uint8_t bird_wing[3][7][2] = {
  {{0x50,0x01},{0xA4,0x06},{0xA9,0x06},{0xA9,0x1A},{0xAD,0x1A},{0xF4,0x1B},{0x50,0x05}},
  {{0x54,0x05},{0xA9,0x1A},{0xAD,0x6A},{0xF4,0x1F},{0x50,0x05},{0},{0}},
  {{0x54,0x05},{0xA9,0x1A},{0xAD,0x1A},{0xB4,0x1A},{0xD0,0x07},{0x40,0x01},{0}},
};
static const uint8_t wing_y[3] = {3, 6, 7};
/* body, light, belly: yellow, blue, red */
static const color bird_cols[3][3] = {{RGB(0xF8C136), RGB(0xFCE88A), RGB(0xF39A22)},
                                     {RGB(0x55B8E8), RGB(0xA6E2F8), RGB(0x2F86C8)},
                                     {RGB(0xF0553A), RGB(0xFBA28A), RGB(0xC53A2A)}};
static color bpal[8];
static int bird_y, bird_a, bird_f, bird_s; /* centre, angle (256 a turn), wing, scale */

static void bird_colors(int c)
{
        static const color fixed[8] = {0, DARK, WHITE, 0, 0, 0, RGB(0xF76A2C), RGB(0xDB4A26)};

        for (int i = 0; i < 8; i++) bpal[i] = i >= 3 && i <= 5 ? bird_cols[c][i - 3] : fixed[i];
}

static int bird_pix(int i, int j)
{
        int wj = j - wing_y[bird_f];

        if ((unsigned)wj < 7 && i < 8) {
                int v = bird_wing[bird_f][wj][i >> 2] >> (i & 3) * 2 & 3;

                if (v) return "\1\2\4"[v - 1];
        }
        int b = (j * 24 + i) * 3;

        return (bird_body[b >> 3] | bird_body[(b >> 3) + 1] << 8) >> (b & 7) & 7;
}

static void bird_draw(void)
{
        int r = 15 << bird_s;
        int a = iabs(bird_a) < 6 ? 0 : bird_a, c = COS(a), s = SIN(a), sh = 8 + bird_s;

        for (int y = bird_y - r; y < bird_y + r; y++)
                for (int x = BX - r; x < BX + r; x++) {
                        int dy = 2 * (y - bird_y) + 1, dx = 2 * (x - BX) + 1;

                        if (x < 0 || x >= W || y < 0 || y >= GY) continue;
                        int uu = ((dx * c + dy * s) >> sh) + 24, v = ((dy * c - dx * s) >> sh) + 17;

                        if (uu < 0 || v < 0 || uu >= 48 || v >= 34) continue;
                        int i = bird_pix(uu >> 1, v >> 1);

                        if (i) lput(x, y, bpal[i]);
                }
}

/* ---- medals -------------------------------------------------------------- */

static void disc(int cx, int cy, int r, color c)
{
        for (int dy = -r; dy <= r; dy++) {
                int h = (int)__builtin_sqrtf((float)(r * r + r - dy * dy));

                lfill(cx - h, cy + dy, 2 * h + 1, 1, c);
        }
}

static int sparkle;

static void medal(int cx, int cy, int tier)
{
        static const color cols[4][3] = {{RGB(0xE8A36A), RGB(0xC77A3C), RGB(0xF6CFA0)}, {RGB(0xE3E3E3), RGB(0xAFAFAF), WHITE},
                                        {RGB(0xF8D23A), RGB(0xD39A14), RGB(0xFFF1A8)}, {RGB(0xE9F6F8), RGB(0x9CC8D4), WHITE}};
        color base = cols[tier][0], rim = cols[tier][1];

        disc(cx, cy, 13, rim);
        disc(cx - 1, cy - 1, 11, cols[tier][2]); /* a shine, top left */
        disc(cx + 1, cy + 1, 10, base);
        disc(cx, cy, 7, rim); /* the embossed ring */
        disc(cx, cy, 5, base);
        int s = sparkle, sx = cx - 9 + (s & 15) * 18 / 16, sy = cy - 9 + (s >> 4 & 15) * 18 / 16, k = s >> 8 & 1;

        lfill(sx - 1 - k, sy, 3 + 2 * k, 1, WHITE);
        lfill(sx, sy - 1 - k, 1, 3 + 2 * k, WHITE);
}

/* ---- the beige board of the original ------------------------------------ */

static void board(int x, int y, int w, int h)
{
        lrrect(x, y, w, h, DARK);
        lrrect(x + 1, y + 1, w - 2, h - 2, RGB(0xFCF8CF));
        lfill(x + 3, y + 3, w - 6, h - 6, RGB(0xCFC286));
        lfill(x + 3, y + 3, w - 6, h - 8, BEIGE);
}

/* ---- the best score, across boots --------------------------------------- */

static const char *SAV_PATHS[] = {"/var/lib/flappy.sav", "/etc/flappy.sav", "/tmp/flappy.sav"};
static int best;

static void best_load(void)
{
        for (int i = 0; i < 3; i++) {
                FILE *f = fopen(SAV_PATHS[i], "r");

                if (!f) continue;
                if (fscanf(f, "%d", &best) != 1 || best < 0 || best > 99999) best = 0;
                fclose(f);
                if (best) return;
                best = 0;
        }
}

static void best_save(void)
{
        for (int i = 0; i < 3; i++) {
                FILE *f = fopen(SAV_PATHS[i], "w");

                if (!f) continue;
                fprintf(f, "%d\n", best);
                fclose(f);
                return;
        }
}

/* ---- the game ------------------------------------------------------------ */

enum { S_READY, S_PLAY, S_DEAD, S_OVER };

static int state, tick, landed, done_t, shown, newbest, sparkle_until;
static int32_t by, vy; /* the bird's height and speed, 1/256 pixel */
static int score;
static int spd, space, gap0, grav, jump, maxfall, apex, last_c;

static void flap(void)
{
        vy = -jump;
        bird_a = -18; /* the beak goes up at once, as in the original */
}

/* where the next gap goes: never further than the bird can climb or dive
 * between two pipes.  The Normal difficulty's factors (the original's
 * fac[1], most[1]); the room a flap's apex leaves is accounted for. */
static int next_centre(int prev)
{
        int lo = 24 + gap0 / 2, hi = GY - 24 - gap0 / 2;
        int f = imax(((space - LW - 18) << 8) / spd, 0);
        int room = imax(gap0 - 12 - apex, 0) << 8, fall = room;

        for (int t = 1; t <= f; t++) fall += imin(grav * t, maxfall);
        int up = imin((room + f * (jump - 5 * grav)) * 154 >> 16, 90);
        int dn = imin(fall * 154 >> 16, 90);
        int a = imax(lo, prev - imax(up, 4)), b = imin(hi, prev + imax(dn, 4));

        if (a > b) return clamp(prev, lo, hi);
        return rr(a, b);
}

static void spawn(pipe_t *p, int32_t x)
{
        p->on = 1, p->scored = 0, p->x = x;
        p->g = (int16_t)(gap0 << 8);
        p->c = (int16_t)(last_c = next_centre(last_c));
        p->dx = (int16_t)(x >> 8), p->dt = 0, p->db = 0;
}

static bool hits(void)
{
        int y = by >> 8, t = y - 6, b = y + 6, l = BX - 9, r = BX + 9;

        if (b >= GY) return true;
        for (int i = 0; i < NP; i++) {
                const pipe_t *p = &P[i];
                int px = p->x >> 8, gt = gap_top(p), gb = gap_bot(p);

                if (!p->on || r <= px || l >= px + LW) continue;
                bool shaft = r > px + 2 && l < px + LW - 2;

                if ((t < gt && (b > gt - LH || shaft)) || (b > gb && (t < gb + LH || shaft))) return true;
        }
        return false;
}

/* a bird that flaps its wings */
static int bob(int period, int amp) { return SIN(tick * 256 / period) * amp / 256; }

static void flutter(int every)
{
        static const uint8_t cyc[4] = {0, 1, 2, 1};

        bird_f = cyc[tick / every & 3];
}

static void new_game(void)
{
        spd = 358, space = 120, gap0 = 70;         /* Normal, as shipped */
        grav = 51, jump = 870, maxfall = 1152;     /* jump[2]: Normal */
        apex = jump * jump / (grav * 512);
        night = (int)(rnd() & 1);
        bird_colors((int)(rnd() % 3));
        bird_s = 0;                                 /* 1x: on a full-screen panel the 24x17 sprite reads well enough, and smaller keeps the sky open */
        make_scene();
        for (int i = 0; i < NP; i++) {
                P[i].on = 0;
                P[i].dx = OFF;
        }
        last_c = GY / 2, by = 100 << 8, vy = 0, score = 0, bird_a = 0, bird_y = 100;
        landed = 0, done_t = 0, shown = 0, newbest = 0, sparkle = 0, sparkle_until = 0;
        state = S_READY, tick = 0;
}

static void die(void)
{
        state = S_DEAD, tick = 0, landed = 0;
        if (vy < 0) vy = 0;
}

static void game_over(void)
{
        state = S_OVER, tick = 0, shown = 0, done_t = 0x7fffffff;
        newbest = score > best;
        if (newbest) best = score, best_save();
}

/* one physics step, every 18 ms */
static void step(void)
{
        tick++;
        switch (state) {
        case S_READY:
                gx += 358, gph = (int)(gx >> 8) % 14; /* the ground keeps running */
                bird_y = 100 + bob(120, 6);
                bird_a = 0;
                flutter(6);
                break;
        case S_PLAY: {
                gx += spd, gph = (int)(gx >> 8) % 14;
                int32_t right = -((int32_t)1 << 30);

                for (int i = 0; i < NP; i++) {
                        pipe_t *p = &P[i];

                        if (!p->on) continue;
                        p->x -= spd;
                        if (!p->scored && (p->x >> 8) + LW / 2 < BX) p->scored = 1, score++;
                        if ((p->x >> 8) < -LW - 4) p->on = 0;
                        else if (p->x > right) right = p->x;
                }
                if (right < (W << 8))
                        for (int i = 0; i < NP; i++)
                                if (!P[i].on) {
                                        spawn(&P[i], right < -((int32_t)1 << 29) ? W << 8 : right + (space << 8));
                                        break;
                                }
                vy = imin(vy + grav, maxfall);
                by += vy;
                if (by < 0) by = 0, vy = imax(vy, 0);
                bird_y = by >> 8;
                bird_a = vy < 300 ? imax(bird_a - 6, -18) : imin(bird_a + 3, 64);
                if (bird_a > 40) bird_f = 1;
                else flutter(4);
                if ((by >> 8) + 8 >= GY) by = (GY - 8) << 8, die();
                else if (hits()) die();
                break;
        }
        case S_DEAD:
                if (!landed) {
                        vy = imin(vy + grav, maxfall);
                        by += vy;
                        bird_a = imin(bird_a + 6, 64), bird_f = 1;
                        bird_y = by >> 8;
                        if ((by >> 8) + 8 >= GY) by = (GY - 8) << 8, bird_y = by >> 8, landed = tick;
                } else if (tick - landed > 30) {
                        game_over();
                }
                break;
        case S_OVER:
                if (tick >= 40 && shown < score && tick % 2 == 0)
                        shown = imin(score, shown + imax(1, score / 24));
                if (shown >= score && done_t == 0x7fffffff) done_t = tick;
                if (done_t < 0x7fffffff && score >= 10 && tick < done_t + 120 && tick % 6 == 0)
                        sparkle = (int)(rnd() & 0xFFFF);
                sparkle_until = done_t < 0x7fffffff ? done_t + 120 : 0;
                break;
        }
}

/* ---- the keyboard: /dev/console, O_NONBLOCK ------------------------------ */

static int kbd = -1;
static struct termios flappy_tio;
static int tio_saved;

static void tty_raw(void)
{
        if (tcgetattr(kbd, &flappy_tio) == 0) {
                struct termios t = flappy_tio;

                t.c_lflag &= ~(tcflag_t)(ICANON | ECHO);
                tcsetattr(kbd, TCSANOW, &t);
                tio_saved = 1;
        }
}

static void tty_restore(void)
{
        if (tio_saved) {
                tcsetattr(kbd, TCSANOW, &flappy_tio);
                tio_saved = 0;
        }
}

static void kbd_drain(void)
{
        uint8_t buf[64];

        for (;;) {
                ssize_t n = read(kbd, buf, sizeof buf);

                if (n <= 0) break;
        }
}

/* ---- run ------------------------------------------------------------------ */

/* One complete frame: everything, from the skyline to the board, then the
 * whole-screen blit.  Called every pass of the loop. */
static int render_frame(void)
{
        bg_columns(0, W);
        pipes_draw();
        ground_full();
        bird_draw();
        if (state == S_PLAY)
                big(score, 160, 8, 2, 1);
        if (state == S_READY) {
                otext("Get Ready!", 160 - tw("Get Ready!", 3) / 2, 48, 3, RGB(0xB8F07A));
                ctext("SPACE to flap", 160, 100, 1, WHITE);
        }
        if (state == S_OVER) {
                static const int PY = 52;
                int y = PY;

                if (tick < 36) { /* the board slides up, slowing down */
                        int uu = clamp(tick - 16, 0, 20);

                        y = PY + (20 - uu) * (20 - uu) * 188 / 400;
                }
                board(75, y, 170, 88);
                text("MEDAL", 91, y + 10, 1, ORANGE);
                text("SCORE", 231 - tw("SCORE", 1), y + 10, 1, ORANGE);
                text("BEST", 231 - tw("BEST", 1), y + 46, 1, ORANGE);
                disc(111, y + 46, 16, RGB(0xCFC285));
                {
                        char n[8];

                        num(n, shown);
                        big(shown, 75 + 170 - 14 - bigw(n, 2), y + 22, 2, 0);
                        num(n, best);
                        big(best, 75 + 170 - 14 - bigw(n, 2), y + 58, 2, 0);
                }
                if (shown == score && tick >= done_t) {
                        if (score >= 10)
                                medal(111, y + 46, score >= 40 ? 3 : score >= 30 ? 2 : score >= 20 ? 1 : 0);
                        if (newbest) {
                                lrrect(181, y + 60, 24, 11, RGB(0xFC3800));
                                text("NEW", 184, y + 62, 1, WHITE);
                        }
                }
                ctext("SPACE retry   ESC quit", 160, 160, 1, WHITE);
        }
        return blit_full();
}

static int run(void)
{
        uint8_t kbuf[64];
        long last, now;

        long r = prism_fb_info(&FB);

        if (r < 0) {
                if (r == -PRISM_ENODEV) {
                        printf("flappy: needs a graphics framebuffer, but this machine is in VGA text mode.\n");
                        printf("        (BIOS boot: check that loader.conf does not set vbe_mode=\"off\")\n");
                } else {
                        printf("flappy: fb_info failed: %ld\n", r);
                }
                return 1;
        }
        CW = (int)FB.w, CH = (int)FB.h;
        canvas = mmap(0, (size_t)CW * CH * 4, PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (canvas == MAP_FAILED) {
                printf("flappy: cannot mmap a %dx%d back buffer (%s)\n", CW, CH, strerror(errno));
                return 1;
        }
        build_palette();
        build_maps(); /* the whole panel, whatever its size */

        kbd = open("/dev/console", O_RDONLY | O_NONBLOCK);
        if (kbd < 0) {
                printf("flappy: cannot open /dev/console (%s)\n", strerror(errno));
                return 1;
        }
        tty_raw();
        kbd_drain();

        best_load();
        new_game();

        /* The opening frame. */
        if (render_frame() < 0) goto blit_failed;

        last = prism_uptime_ms();
        for (;;) {
                int quit = 0, action = 0;

                /* The keyboard: everything pending, one read a batch. */
                for (;;) {
                        ssize_t n = read(kbd, kbuf, sizeof kbuf);

                        if (n <= 0) break;
                        for (ssize_t i = 0; i < n; i++) {
                                if (kbuf[i] == 0x1b) quit = 1;
                                else if (kbuf[i] == ' ') action = 1;
                        }
                        if (quit) break;
                }
                if (quit) {
                        lfill(0, 0, W, H, BLACK);
                        (void)blit_full();
                        tty_restore();
                        close(kbd);
                        return 0;
                }

                now = prism_uptime_ms();
                /* Fixed 18 ms steps off the tick counter. */
                int steps = (int)((now - last) / 18);
                if (steps > 0) {
                        last += (long)steps * 18;
                        if (steps > 4) steps = 4; /* a stall must not fast-forward */
                        for (int s = 0; s < steps; s++) step();
                }

                if (action) {
                        if (state == S_READY) state = S_PLAY, flap();
                        else if (state == S_PLAY) flap();
                        else if (state == S_OVER) new_game();
                        else if (state == S_DEAD && landed && tick - landed > 10) game_over();
                        action = 0;
                }

                /* The whole frame, every pass: the skyline, the pipes, the
                 * ground, the bird, the score or the board are recomposed
                 * from scratch and every panel pixel is rewritten before the
                 * blit, so a frame can never pile up over the last one. */
                if (render_frame() < 0) goto blit_failed;

                if (steps == 0 && !action)
                        prism_sleep_ms(1); /* the userland hlt(): park between frames */
        }

blit_failed:
        printf("flappy: fb_blit failed\n");
        tty_restore();
        close(kbd);
        return 1;
}

int main(int argc, char **argv, char **envp)
{
        (void)argc;
        (void)argv;
        (void)envp;
        setvbuf(stdout, 0, _IONBF, 0); /* the shell's prompt must not wait */
        build_sin();
        make_pipes();
        return run();
} 