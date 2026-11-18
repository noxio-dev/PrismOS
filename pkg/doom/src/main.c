/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * /usr/bin/doom -- doomgeneric on the console the ANSI interpreter
 * (sys/kern/src/console/ansi.rs) made into a terminal.
 *
 * The vendored tree (src/doomgeneric/) is chocolate doom's machine
 * state wired to six hooks (doomgeneric.h).  Every hook is this file:
 *
 *   DG_Init            the framebuffer's geometry (fb_info 44) and the
 *                      keyboard (the tty the game was launched on --
 *                      fd 0 -- made raw and non-blocking, restored on
 *                      every way out)
 *   DG_DrawFrame       i_video.c has already scaled 320x200 to
 *                      DOOMGENERIC_RES 640x400 and folded the palette
 *                      in -- the screen buffer is BGRA uint32s; pack
 *                      them for the framebuffer the way cube does
 *                      (rgb_pack over fb_info's shifts), scale once
 *                      more, and blit (fb_blit 45)
 *   DG_GetKey          the tty's bytes, translated: a hold-then-release
 *                      model, because a tty never says a key went up
 *   DG_SleepMs         sleep 5
 *   DG_GetTicksMs      uptime_ms 3
 *   DG_SetWindowTitle  a no-op: the console has no title to set
 *
 * main() is the platform's own loop, the same shape the other
 * platforms spell (doomgeneric_soso.c): create, then tick forever.
 * I_Quit's exit(0) lands in the atexit below, which gives the tty back
 * to whatever login owns it next.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <signal.h>
#include <termios.h>
#include <unistd.h>
#include <fcntl.h>

#include "prism_raw.h"
#include "doomgeneric/doomgeneric.h"
#include "doomgeneric/doomkeys.h"
#include "doomgeneric/m_controls.h"
#include "doomgeneric/i_system.h"

static int prism_kbd_init(void);
static void prism_exit(void);

/* ---------------------------------------------------------------- screen */

static struct prism_fb_info FB;

/* The screen buffer in the framebuffer's own packing (one pixel per
 * source pixel), and the scaled copy the blit actually sends. */
static uint32_t *packed, *scaled;
static uint32_t scr_w, scr_h;
static int scale;

/* Pack 8-bit channels into the screen's pixel format: the kernel's
 * Surface::rgb(), on the numbers fb_info() brought back (cube.c). */
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

void DG_Init(void)
{
        struct termios raw;
        int w = DOOMGENERIC_RESX, h = DOOMGENERIC_RESY;

        if (prism_fb_info(&FB) != 0) {
                printf("doom: fb_info failed: no framebuffer to draw on\n");
                exit(1);
        }

        /* Fit the 640x400 screen in the framebuffer, centered.  A QEMU
         * GOP is 1920x1080: x2, a 1280x800 window with margins. */
        scale = FB.w / w;
        if (FB.h / h < scale)
                scale = FB.h / h;
        if (scale < 1)
                scale = 1;
        if (scale > 3)
                scale = 3;
        scr_w = (uint32_t)w * scale;
        scr_h = (uint32_t)h * scale;
        printf("doom: %ux%u framebuffer, the 640x400 screen scaled x%d "
               "(%ux%u, centered)\n", FB.w, FB.h, scale, scr_w, scr_h);

        packed = malloc(sizeof(uint32_t) * w * h);
        scaled = scale > 1 ? malloc(sizeof(uint32_t) * scr_w * scr_h) : NULL;
        if (!packed || (scale > 1 && !scaled)) {
                printf("doom: no memory for the screen buffers\n");
                exit(1);
        }

        if (prism_kbd_init() != 0) {
                printf("doom: no keyboard (the game plays blind)\n");
        }

        /* I_Quit runs the doom exit handlers and then -- the vendored
         * ORIGCODE being off -- returns: the SDL exit(0) it ended with
         * upstream is compiled out, so the game would keep running
         * after a confirmed quit.  Every quit path funnels into I_Quit
         * (the menu's confirm, the endgame quits, testcontrols), so the
         * last handler is the platform's: exit for real.  Registered
         * here, before D_DoomMain registers its own, I_AtExit's head
         * insertion puts it after all of them -- the sound and
         * joystick shutdowns, the demo status, the config save run
         * first.  run_on_error stays false: a fatal I_Error keeps its
         * own exit(-1) (and -nogui already skipped the zenity box). */
        I_AtExit(prism_exit, false);

        /* A tty's bytes cannot speak the doomkeys codes the in-code
         * defaults bind: a terminal never delivers shift / alt / ctrl
         * as scancodes, it translates them away before the byte exists.
         * m_config's ORIGCODE is off (no config file is read or
         * written), so rebind here, before the game ever reads these:
         * fire, use, strafe and speed on the keys a tty does send. */
        key_fire = 0x03;            /* ^C (ISIG is off) -- the original fire key */
        key_use = ' ';
        key_strafe = 'a';           /* vanilla's alt: hold + left/right arrows */
        key_speed = 'r';
        key_strafeleft = 'd';
        key_straferight = 'f';
        key_nextweapon = '/';
        key_prevweapon = '.';
}

void DG_DrawFrame(void)
{
        uint32_t y, x;

        /* Pack the screen buffer: its pixels are BGRA uint32s (little
         * endian: bytes B,G,R,A), the shape i_video.c's cmap_to_fb
         * leaves when the framebuffer it was written for said RGBA.
         * One packed word per source pixel, then the scale repeats. */
        for (y = 0; y < DOOMGENERIC_RESY; y++) {
                const uint8_t *row = (const uint8_t *)DG_ScreenBuffer
                        + y * DOOMGENERIC_RESX * 4;
                uint32_t *dst = packed + y * DOOMGENERIC_RESX;

                for (x = 0; x < DOOMGENERIC_RESX; x++, row += 4)
                        dst[x] = rgb_pack(row[2], row[1], row[0]);
        }

        if (scale == 1) {
                prism_fb_blit(packed, scr_w, scr_h, scr_w * 4,
                              (FB.w - scr_w) / 2, (FB.h - scr_h) / 2);
                return;
        }

        for (y = 0; y < scr_h; y++) {
                const uint32_t *src = packed + (y / scale) * DOOMGENERIC_RESX;
                uint32_t *dst = scaled + y * scr_w;

                for (x = 0; x < scr_w; x++)
                        dst[x] = src[x / scale];
        }
        prism_fb_blit(scaled, scr_w, scr_h, scr_w * 4,
                      (FB.w - scr_w) / 2, (FB.h - scr_h) / 2);
}

/* --------------------------------------------------------------- keyboard */

#define KB_QUEUE 32     /* (pressed, key) pairs waiting for the game */
#define KB_BUF 64       /* tty bytes waiting to be parsed */
#define HOLD_MS 120     /* a byte keeps its key down this long */
#define ESC_HOLD_MS 60  /* an ESC waits this long to become a sequence */

static int kbd_fd = -1;
static struct termios kbd_saved;
static int kbd_fl;              /* the fd's own flags (O_NONBLOCK is ours) */

static struct {
        int pressed;
        int key;
} key_q[KB_QUEUE];
static int key_qh, key_qt;
static uint8_t key_buf[KB_BUF];
static int key_len;
static int held_key;            /* the key our model keeps down */
static uint32_t held_at;        /* the tick of the last byte for it */
static uint32_t esc_at;         /* the tick a held lone ESC arrived (0: none) */

/* The terminal restores itself on every way out: I_Quit's exit(0)
 * (atexit), a fatal I_Error (exit), and the signals a shell kills the
 * game with (the handlers).  A tty left raw leaves the next login
 * blind -- cooked reads still work, the echo and the line editing do
 * not -- and a left-over O_NONBLOCK lives on the very description the
 * launching shell shares with us, so the flags go back too. */
static void kbd_restore(void)
{
        if (kbd_fd >= 0) {
                tcsetattr(kbd_fd, TCSANOW, &kbd_saved);
                fcntl(kbd_fd, F_SETFL, kbd_fl);
                kbd_fd = -1;
        }
}

static void kbd_on_signal(int sig)
{
        (void)sig;
        kbd_restore();
        _exit(2);
}

/* The last doom exit handler: the process ends here -- the C atexit
 * below runs (the terminal's restore), the shell gets its prompt
 * back. */
static void prism_exit(void)
{
        exit(0);
}

/* Take the terminal the game was launched on and make it raw: the
 * return path (0 / -1) is the "keyboard or not" DG_Init reports.
 *
 * The keyboard is fd 0, not a device of our own choosing: the session
 * we inherited (a login on the serial line, a console login on a vt)
 * is the only terminal whose bytes this game can see.  /dev/console
 * would be wrong twice over -- its line is the kernel console's, fed
 * by the i8042 keyboard, while a serial session's bytes arrive on
 * COM1's own queue (user/fd.rs) -- and O_NONBLOCK is what keeps
 * DG_GetKey from parking the game loop in the tty read's 10 s wait. */
static int prism_kbd_init(void)
{
        struct termios raw;
        int fl;

        kbd_fd = 0;
        if (tcgetattr(kbd_fd, &kbd_saved) != 0) {
                kbd_fd = -1;
                return -1;
        }
        fl = fcntl(kbd_fd, F_GETFL);
        kbd_fl = fl >= 0 ? fl : 0;
        atexit(kbd_restore);
        signal(SIGINT, kbd_on_signal);
        signal(SIGTERM, kbd_on_signal);

        /* Raw, but keep OPOST|ONLCR: the game's own printf should wrap
         * its lines the way every other console writer does.  The
         * input side loses its line discipline (ICRNL with it, so
         * Enter is the 0x0d the keyboard sent): the bytes are what the
         * terminal delivers, EAGAIN when none are pending. */
        raw = kbd_saved;
        raw.c_iflag &= ~(unsigned)(IGNBRK | BRKINT | PARMRK | ISTRIP |
                                   INLCR | IGNCR | ICRNL | IXON);
        raw.c_lflag &= ~(unsigned)(ECHO | ECHONL | ICANON | ISIG | IEXTEN);
        raw.c_cflag &= ~(unsigned)(CSIZE | PARENB);
        raw.c_cflag |= CS8;
        tcsetattr(kbd_fd, TCSANOW, &raw);
        fcntl(kbd_fd, F_SETFL, kbd_fl | O_NONBLOCK);
        return 0;
}

static void key_push(int pressed, int k)
{
        key_q[key_qh].pressed = pressed;
        key_q[key_qh].key = k;
        key_qh = (key_qh + 1) % KB_QUEUE;
        if (key_qh == key_qt)       /* full: drop the oldest */
                key_qt = (key_qt + 1) % KB_QUEUE;
}

static void key_press(int k)
{
        if (k <= 0)
                return;
        if (held_key == k) {
                /* The autorepeat of a held key: the hold continues, no
                 * second press is queued. */
                held_at = DG_GetTicksMs();
                return;
        }
        if (held_key)
                key_push(0, held_key);      /* the old key never said up */
        held_key = k;
        held_at = DG_GetTicksMs();
        key_push(1, k);
}

/* A tty byte is a key, except the few a terminal cannot say: the
 * action keys the in-code binds live in the C0 range, and ^H stands in
 * for 0x7f (both are backspace). */
static int translate_key(uint8_t b)
{
        switch (b) {
        case 0x03:      return KEY_FIRE;        /* ^C, ISIG is off */
        case ' ':       return KEY_USE;
        case 0x08:      return KEY_BACKSPACE;   /* ^H, beside 0x7f */
        default:        return b;
        }
}

/* Read everything pending and parse it.  The keyboard's escape
 * sequences arrive as three bytes (keymap.rs's cursor(): "\x1b[" plus
 * one of BDCA); a sequence split across reads waits for its tail -- a
 * few frames, then a lone ESC is delivered after all. */
static void key_drain(void)
{
        uint8_t tmp[KB_BUF];
        static const int arrows[4] = {
                KEY_UPARROW, KEY_DOWNARROW, KEY_RIGHTARROW, KEY_LEFTARROW
        };
        long n;
        int pos;

        while ((n = read(kbd_fd, tmp, sizeof(tmp))) > 0) {
                for (long i = 0; i < n; i++) {
                        if (key_len == KB_BUF) {
                                memmove(key_buf, key_buf + 1, KB_BUF - 1);
                                key_len--;
                        }
                        key_buf[key_len++] = tmp[i];
                }
        }

        pos = 0;
        while (pos < key_len) {
                uint8_t b = key_buf[pos];

                if (b == 0x1b && pos + 2 < key_len &&
                    key_buf[pos + 1] == '[' &&
                    key_buf[pos + 2] >= 'A' && key_buf[pos + 2] <= 'D') {
                        key_press(arrows[key_buf[pos + 2] - 'A']);
                        pos += 3;
                        esc_at = 0;
                        continue;
                }
                if (b == 0x1b && pos + 2 >= key_len) {
                        /* Could still grow into a sequence: hold the
                         * ESC for ESC_HOLD_MS of real time, then send
                         * it alone.  A wall-clock deadline, not a frame
                         * count -- the game polls at tic rate, and a
                         * slow TCG makes a tic tens of ms while the
                         * tty's own sequences are a handful of bytes
                         * delivered together (keymap.rs's cursor() puts
                         * them in one write). */
                        if (esc_at == 0)
                                esc_at = DG_GetTicksMs();
                        if (DG_GetTicksMs() - esc_at >= ESC_HOLD_MS) {
                                key_press(KEY_ESCAPE);
                                pos++;
                                esc_at = 0;
                        }
                        break;
                }
                key_press(translate_key(b));
                pos++;
                esc_at = 0;
        }
        memmove(key_buf, key_buf + pos, key_len - pos);
        key_len -= pos;
}

int DG_GetKey(int *pressed, unsigned char *key)
{
        if (kbd_fd < 0)
                return 0;

        key_drain();

        /* Nothing new and the hold expired: the key goes up. */
        if (held_key && key_qh == key_qt &&
            DG_GetTicksMs() - held_at >= HOLD_MS) {
                key_push(0, held_key);
                held_key = 0;
        }

        if (key_qh == key_qt)
                return 0;
        *pressed = key_q[key_qt].pressed;
        *key = (unsigned char)key_q[key_qt].key;
        key_qt = (key_qt + 1) % KB_QUEUE;
        return 1;
}

/* ------------------------------------------------------------- the clock */

void DG_SleepMs(uint32_t ms)
{
        prism_sleep_ms(ms > 10000 ? 10000 : ms);        /* the kernel's cap */
}

uint32_t DG_GetTicksMs(void)
{
        return (uint32_t)prism_uptime_ms();
}

void DG_SetWindowTitle(const char *title)
{
        (void)title;
}

/* ------------------------------------------------------------------- main */

/* I_Error's error box runs zenity through system(3) -- a fork+exec of a
 * shell the console has no use for.  Its M_ParmExists("-nogui") gate
 * skips the whole thing, so the option rides the argv passed to
 * doomgeneric_Create unless the caller spelled it (M_ParmExists scans
 * myargv, set inside Create, so the injection has to happen here). */
static char *argv2[64];

int main(int argc, char **argv)
{
        int n = argc > 60 ? 60 : argc, i;
        int has_nogui = 0;

        /* d_iwad.c's IWAD search starts at the current directory
         * (config.h's FILES_DIR is "." and ORIGCODE is off): the image
         * carries the shareware IWAD at /usr/share/games/doom, so the
         * game runs from there -- and M_SetConfigDir's "." puts its
         * saves beside the WAD.  A failed chdir means the caller runs
         * it from a directory of their own making: their IWAD, their
         * cwd, their choice. */
        if (chdir("/usr/share/games/doom") != 0)
                printf("doom: no /usr/share/games/doom (running from the cwd)\n");

        for (i = 0; i < n; i++)
                argv2[i] = argv[i];
        for (i = 0; i < n; i++) {
                if (strcmp(argv2[i], "-nogui") == 0) {
                        has_nogui = 1;
                        break;
                }
        }
        if (!has_nogui)
                argv2[n++] = (char *)"-nogui";

        doomgeneric_Create(n, argv2);

        for (;;)
                doomgeneric_Tick();

        return 0;
}
