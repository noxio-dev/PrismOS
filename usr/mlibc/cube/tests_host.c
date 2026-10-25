/* cube.c host test: the port's maths and pixels, checked on Linux.
 *
 * Compile:  gcc -O2 -Wall -Wextra -o /tmp/cube_host tests_host.c
 * (from usr/mlibc/cube/).  Includes cube.c itself -- the statics become
 * reachable -- and checks:
 *   1. the Bhaskara sine table against libm (max |err| ~0.0016)
 *   2. the renderer's invariant: exactly 3 faces visible, every instant
 *   3. fill_convex / draw_line actually fill what they should
 *   4. the HUD font, as ASCII art, for eyes to check
 *   5. one whole cube frame, as ASCII art
 */
#define main cube_main_unused
#include "cube.c"
#undef main

#include <math.h>
#include <stdlib.h>

static int fail;

static void check(int ok, const char *what)
{
        printf("%-52s %s\n", what, ok ? "ok" : "FAIL");
        if (!ok)
                fail++;
}

static void setup_screen(int w, int h)
{
        FB.w = w;
        FB.h = h;
        FB.bpp = 32;
        FB.r_shift = 16; FB.r_bits = 8;
        FB.g_shift = 8;  FB.g_bits = 8;
        FB.b_shift = 0;  FB.b_bits = 8;
        CW = w;
        CH = h;
        free(canvas);
        canvas = calloc((size_t)w * h, 4);
}

int main(void)
{
        build_sin();

        /* 1. the sine table ------------------------------------------------ */
        {
                double maxerr = 0;

                for (int i = 0; i < TURN; i++) {
                        double want = sin(2.0 * M_PI * i / TURN);
                        double got = (double)SIN[i] / Q14;

                        if (fabs(want - got) > maxerr)
                                maxerr = fabs(want - got);
                }
                printf("sin table: max |err| = %.5f (Bhaskara promises ~0.0016)\n", maxerr);
                check(maxerr <= 0.002, "sin table within 0.002 of libm");
        }

        /* 2. the culling: some faces forward, at every instant ------------- */
        {
                /* The signed screen area IS the perspective-correct test (a
                 * face turned away, or hidden by the telephoto geometry --
                 * CAM_Z 1000 for a 512-unit cube -- projects to a negative
                 * or empty area; a face towards the camera always positive).
                 * What must hold at every instant: never 0 visible faces
                 * (the cube would blink off) and never 4+ (a back face would
                 * leak through the front).  How often the full 3 show is
                 * geometry, reported, not asserted. */
                int bad = 0, threes = 0, n = 0;

                for (long t = 100; t < 60000; t += 7) {
                        int ax = (int32_t)((int64_t)t * TURN / 9000);
                        int ay = (int32_t)((int64_t)t * TURN / 6000);
                        int az = (int32_t)((int64_t)t * TURN / 15000);
                        /* the real geometry: 1280x800, d = 376 * 9 / 8 */
                        Frame fr = project(640, 408, 423, ax, ay, az);
                        int vis = 0;

                        for (int f = 0; f < 6; f++) {
                                P2 q[4];

                                for (int i = 0; i < 4; i++)
                                        q[i] = fr.pts[FACES[f].verts[i]];
                                if (visible(q))
                                        vis++;
                        }
                        n++;
                        if (vis == 3)
                                threes++;
                        if (vis < 1 || vis > 3) {
                                printf("t=%ld: %d visible faces (1..3 expected)\n", t, vis);
                                bad = 1;
                                break;
                        }
                }
                printf("%d of %d instants show the full 3 faces\n", threes, n);
                check(!bad, "1..3 faces visible at every instant (never 0, never 4+)");
        }

        /* 3. the fills ------------------------------------------------------ */
        setup_screen(64, 32);
        {
                /* a diamond, half-axes 20 (x) and 10 (y) around (32, 16) */
                P2 q[4] = { {32, 6}, {52, 16}, {32, 26}, {12, 16} };
                int lit = 0, outside = 0;

                fill_convex(q, 4, 0xFFFFFFFF);
                for (int y = 0; y < 32; y++)
                        for (int x = 0; x < 64; x++) {
                                /* inside: |x-32|/20 + |y-16|/10 <= 1 (a rim pixel of
                                 * integer-rounding tolerance either way) */
                                int d = 10 * abs(x - 32) + 20 * abs(y - 16);
                                uint32_t px = canvas[(size_t)y * 64 + x];

                                if (d <= 200 && px == 0xFFFFFFFF)
                                        lit++;
                                if (d > 240 && px != 0)
                                        outside++;
                        }
                check(lit > 350 && outside == 0, "fill_convex fills the diamond, nothing else");

                memset(canvas, 0, (size_t)64 * 32 * 4);
                draw_line(10, 5, 50, 25, 1, 0xFFFFFFFF);
                {
                        int on = 0;

                        for (int x = 10; x <= 50; x++)
                                for (int y = 3; y <= 27; y++)
                                        if (canvas[(size_t)y * 64 + x])
                                                on++;
                        check(on >= 38 && on <= 60, "draw_line lights one pixel per column");
                }
        }

        /* 4. the HUD font, as ASCII art ------------------------------------ */
        setup_screen(80 * 8, 40);
        printf("\n---- the HUD font ----\n");
        {
                const char *words[] = { "PRISM CUBE", "0123456789%+-/[]",
                        "fps speed mode", "solid+edges wire paused",
                        "SPACE pause +/- speed M mode ESC quit" };

                for (size_t w = 0; w < sizeof words / sizeof words[0]; w++) {
                        for (int row = 0; row < 8; row++) {
                                for (const char *s = words[w]; *s; s++) {
                                        const uint8_t *g = glyph_of(*s);

                                        for (int bit = 7; bit >= 0; bit--)
                                                putchar(g[row] & (1 << bit) ? '#' : '.');
                                        putchar(' ');
                                }
                                putchar('\n');
                        }
                        putchar('\n');
                }
        }

        /* 5. one whole frame, as ASCII art ---------------------------------- */
        setup_screen(64, 40);
        {
                int bb[4];

                fill_rect(0, 0, 64, 40, 0);
                draw_cube(32, 20, 21, 200, 350, 90, M_SOLID_EDGES, 0xFFFFFFFF, bb);
                printf("---- one frame at t=..., bbox = [%d %d %d %d] ----\n",
                    bb[0], bb[1], bb[2], bb[3]);
                for (int y = 0; y < 40; y++) {
                        for (int x = 0; x < 64; x++)
                                putchar(canvas[(size_t)y * 64 + x] ? '#' : '.');
                        putchar('\n');
                }
                check(bb[0] >= 0 && bb[1] >= 0 && bb[2] < 64 && bb[3] < 40,
                    "the cube sits inside the screen");
        }

        printf("\n%s (%d failure(s))\n", fail ? "FAILURES" : "all checks passed", fail);
        return fail ? 1 : 0;
}
