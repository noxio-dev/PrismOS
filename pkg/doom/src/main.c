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
 *   DG_GetMouse        the PS/2 mouse, read from evdev (/dev/input/event1):
 *                      motion accumulated per poll, the buttons, and the
 *                      wheel as a previous / next weapon press
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
static void prism_bind_movement(void);
static int prism_mouse_init(void);
static void prism_exit(void);
static int kbd_medium;                  /* 1: console keycodes, press/release */

/* doom's mouse button bits: 0 left (fire), 1 right (strafe), 2 middle
 * (forward) -- the vanilla bindings.  The wheel has no button of its
 * own in evdev; it is turned into a press of bit 3 / 4, bound to the
 * previous / next weapon in DG_Init (see DG_GetMouse). */
#define MB_PREVWEAPON 3
#define MB_NEXTWEAPON 4

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

        /* The mouse is optional: without the node the keyboard plays
         * the whole game.  The wheel cycles the weapons (m_config's
         * ORIGCODE is off, so the binds are set here, like the keys). */
        if (prism_mouse_init() == 0) {
                mousebprevweapon = MB_PREVWEAPON;
                mousebnextweapon = MB_NEXTWEAPON;
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
         * fire, use and speed on the keys a tty does send.  A/Q and D
         * are direct strafe binds now (prism_bind_movement), so the
         * vanilla alt-strafe modifier has no tty key left. */
        if (!kbd_medium) {
                key_fire = 0x03;    /* ^C (ISIG is off) -- the original fire key */
                key_use = ' ';
                key_speed = 'r';
                key_strafe = 0;
                key_nextweapon = '/';
                key_prevweapon = '.';
        }

        /* ZQSD on AZERTY, WASD on QWERTY/QWERTZ: the movement cluster
         * is bound to the layout, not to one spelling of the keys. */
        prism_bind_movement();
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

/* The console's keyboard modes (<linux/kd.h>, answered by hw/keymap.rs
 * through user/fd.rs).  K_MEDIUMRAW hands over one byte per key event:
 * the Linux keycode, bit 7 set on release -- the real "key is down /
 * key went up" a tty's text bytes cannot give.  Only the console
 * answers; the serial line says -ENOTTY and keeps the hold model below. */
#define PRISM_KDGKBMODE 0x4B44
#define PRISM_KDSKBMODE 0x4B45
#define PRISM_KDGKBENT 0x4B46
#define PRISM_K_UNICODE 3
#define PRISM_K_MEDIUMRAW 2

static unsigned int kbd_mode_saved = PRISM_K_UNICODE;
static uint8_t kc_down[128];            /* keycodes currently held */

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
                /* First: a console left in MEDIUMRAW would leave the
                 * shell with keycodes instead of characters. */
                if (kbd_medium) {
                        prism_ioctl(kbd_fd, PRISM_KDSKBMODE,
                                    (void *)(long)kbd_mode_saved);
                        kbd_medium = 0;
                }
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

        /* On the console, ask for real key events: a held key is then a
         * key that stays down until its release arrives (no timer, so
         * no gap between the first press and the host's autorepeat).
         * Anything that fails leaves the text-byte model in charge. */
        {
                unsigned int mode = PRISM_K_UNICODE;

                if (prism_ioctl(kbd_fd, PRISM_KDGKBMODE, &mode) == 0 &&
                    prism_ioctl(kbd_fd, PRISM_KDSKBMODE,
                                (void *)(long)PRISM_K_MEDIUMRAW) == 0) {
                        kbd_mode_saved = mode;
                        kbd_medium = 1;
                }
        }
        return 0;
}

/* ---------------------------------------------------------- layout keys */

/* struct kbentry { u8 kb_table; u8 kb_index; u16 kb_value; } -- KDGKBENT
 * reads the first two bytes and writes the keysym back at offset 2
 * (user/fd.rs).  Only the console answers it; the serial line says
 * -ENOTTY. */
struct prism_kbentry {
        uint8_t table;
        uint8_t index;
        uint16_t value;
};

/* The character the live console keymap puts on keycode `kc` (table 0,
 * unshifted): a keysym is (type << 8) | value and KT_LATIN is 0.  A
 * non-Latin entry (an arrow, a dead key) or a descriptor without a
 * keymap answers 0, and the caller keeps the US fallback. */
static char prism_layout_char(int fd, int kc)
{
        struct prism_kbentry e;
        uint16_t v;

        if (fd < 0)
                return 0;
        e.table = 0;
        e.index = (uint8_t)kc;
        e.value = 0;
        if (prism_ioctl(fd, PRISM_KDGKBENT, &e) != 0)
                return 0;
        v = e.value;
        if ((v >> 8) != 0 || v < 0x20 || v > 0x7e)
                return 0;
        return (char)v;
}

/* Bind doom's movement actions to the ZQSD/WASD cluster.
 *
 * On the console (K_MEDIUMRAW) the bytes are physical keycodes, and
 * kc_doom[] names them the QWERTY way: 'w', 'a', 's' and 'd' are the
 * very keys whose caps say Z, Q, S and D on an AZERTY keyboard, so the
 * same binds cover both layouts.  A tty (the serial line, or a console
 * left in a text mode) delivers characters with the layout already
 * applied; ask the live console keymap what the physical cluster types
 * there, so the binds follow the selected layout (ZQSD on fr, WASD on
 * us) instead of assuming one. */
static void prism_bind_movement(void)
{
        int fd = kbd_fd;
        char fwd, left, back, right;

        if (kbd_medium) {
                key_up = 'w';
                key_strafeleft = 'a';
                key_down = 's';
                key_straferight = 'd';
                return;
        }

        /* The serial descriptor has no keymap; /dev/console does. */
        if (prism_layout_char(fd, 17) == 0) {
                fd = open("/dev/console", O_RDONLY | O_NONBLOCK | O_NOCTTY);
                if (fd < 0)
                        fd = -1;
        }
        fwd = prism_layout_char(fd, 17);
        left = prism_layout_char(fd, 30);
        back = prism_layout_char(fd, 31);
        right = prism_layout_char(fd, 32);
        if (fd >= 0 && fd != kbd_fd)
                close(fd);

        key_up = fwd ? fwd : 'w';
        key_strafeleft = left ? left : 'a';
        key_down = back ? back : 's';
        key_straferight = right ? right : 'd';
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

/* Linux keycode -> doomkeys code, the convention of doomgeneric's own
 * platforms (ctrl fires, space uses, shift runs, alt strafes, , and .
 * strafe).  0 = a key the game has no use for. */
static const unsigned char kc_doom[128] = {
        [1] = KEY_ESCAPE,
        [2] = '1', [3] = '2', [4] = '3', [5] = '4', [6] = '5',
        [7] = '6', [8] = '7', [9] = '8', [10] = '9', [11] = '0',
        [12] = KEY_MINUS, [13] = KEY_EQUALS, [14] = KEY_BACKSPACE,
        [15] = KEY_TAB,
        [16] = 'q', [17] = 'w', [18] = 'e', [19] = 'r', [20] = 't',
        [21] = 'y', [22] = 'u', [23] = 'i', [24] = 'o', [25] = 'p',
        [26] = '[', [27] = ']', [28] = KEY_ENTER,
        [29] = KEY_FIRE,                        /* left ctrl */
        [30] = 'a', [31] = 's', [32] = 'd', [33] = 'f', [34] = 'g',
        [35] = 'h', [36] = 'j', [37] = 'k', [38] = 'l',
        [39] = ';', [40] = '\'', [41] = '`',
        [42] = KEY_RSHIFT,                      /* left shift */
        [43] = '\\',
        [44] = 'z', [45] = 'x', [46] = 'c', [47] = 'v', [48] = 'b',
        [49] = 'n', [50] = 'm',
        [51] = KEY_STRAFE_L,                    /* , */
        [52] = KEY_STRAFE_R,                    /* . */
        [53] = '/',
        [54] = KEY_RSHIFT,
        [55] = '*',
        [56] = KEY_LALT,                        /* left alt */
        [57] = KEY_USE,                         /* space */
        [59] = KEY_F1, [60] = KEY_F2, [61] = KEY_F3, [62] = KEY_F4,
        [63] = KEY_F5, [64] = KEY_F6, [65] = KEY_F7, [66] = KEY_F8,
        [67] = KEY_F9, [68] = KEY_F10, [87] = KEY_F11, [88] = KEY_F12,
        [96] = KEY_ENTER,                       /* keypad enter */
        [97] = KEY_FIRE,                        /* right ctrl */
        [100] = KEY_LALT,                       /* right alt */
        [102] = KEY_HOME, [103] = KEY_UPARROW, [104] = KEY_PGUP,
        [105] = KEY_LEFTARROW, [106] = KEY_RIGHTARROW, [107] = KEY_END,
        [108] = KEY_DOWNARROW, [109] = KEY_PGDN, [110] = KEY_INS,
        [111] = KEY_DEL, [119] = KEY_PAUSE,
};

/* K_MEDIUMRAW: every byte is one key event (keycode, bit 7 = release).
 * The game gets exactly what the keyboard did: a key stays down as long
 * as it is held, whatever the host's autorepeat delay is.  A repeated
 * make code (the autorepeat) is not a new press; two keys that mean the
 * same doom key (left and right shift) release it with the last one. */
static void key_drain_medium(void)
{
        uint8_t tmp[KB_BUF];
        long n;

        while ((n = read(kbd_fd, tmp, sizeof(tmp))) > 0) {
                for (long i = 0; i < n; i++) {
                        int kc = tmp[i] & 0x7f;
                        int down = !(tmp[i] & 0x80);
                        int k = kc_doom[kc];

                        if (!k || down == kc_down[kc])
                                continue;
                        kc_down[kc] = (uint8_t)down;
                        if (!down) {
                                int other = 0;

                                for (int j = 0; j < 128; j++)
                                        if (kc_down[j] && kc_doom[j] == k)
                                                other = 1;
                                if (other)
                                        continue;
                        }
                        key_push(down, k);
                }
        }
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

        if (kbd_medium) {
                key_drain_medium();
                return;
        }

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
        if (!kbd_medium && held_key && key_qh == key_qt &&
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

/* ---------------------------------------------------------------- mouse */

/* The PS/2 mouse is evdev's second node (sys/kern/src/input/): reads
 * return Linux's 24-byte struct input_event, REL_X / REL_Y for motion
 * (right / down positive), BTN_* for the buttons, one SYN_REPORT per
 * packet.  It is read apart from the tty -- the console keyboard's
 * K_MEDIUMRAW has nothing to say about it -- and non-blocking, the same
 * way DG_GetKey must never park the game loop.  No node (a serial-only
 * boot, no PS/2 mouse): the game simply plays without one. */
#define MOUSE_DEV "/dev/input/event1"

struct prism_input_event {
        int64_t sec;
        int64_t usec;
        uint16_t type;
        uint16_t code;
        int32_t value;
};

#define PRISM_EV_SYN 0x00
#define PRISM_EV_KEY 0x01
#define PRISM_EV_REL 0x02
#define PRISM_SYN_DROPPED 3
#define PRISM_REL_X 0x00
#define PRISM_REL_Y 0x01
#define PRISM_REL_WHEEL 0x08
#define PRISM_BTN_LEFT 0x110
#define PRISM_BTN_RIGHT 0x111
#define PRISM_BTN_MIDDLE 0x112
/* EVIOCGKEY(96): _IOC(_IOC_READ, 'E', 0x18, 96) -- the held keys, one
 * bit per keycode; the BTN_* codes sit in byte 34. */
#define PRISM_EVIOCGKEY96 ((2UL << 30) | (96UL << 16) | ((unsigned long)'E' << 8) | 0x18)
#define PRISM_KEYBIT(map, c) (((map)[(c) / 8] >> ((c) % 8)) & 1)

static int mouse_fd = -1;
static int mouse_btn;           /* the buttons held (bits 0..2) */
static int mouse_sent;          /* the mask the game was last told */

static void mouse_close(void)
{
        if (mouse_fd >= 0) {
                close(mouse_fd);
                mouse_fd = -1;
        }
}

static int prism_mouse_init(void)
{
        mouse_fd = open(MOUSE_DEV, O_RDONLY | O_NONBLOCK);
        if (mouse_fd < 0)
                return -1;
        atexit(mouse_close);
        return 0;
}

/* The device's ring overflowed (SYN_DROPPED): the events in between are
 * gone, a button release among them -- ask for the held keys instead of
 * leaving the fire button stuck down. */
static void mouse_resync(void)
{
        uint8_t keys[96];

        memset(keys, 0, sizeof(keys));
        if (prism_ioctl(mouse_fd, PRISM_EVIOCGKEY96, keys) != 0)
                return;
        mouse_btn = 0;
        if (PRISM_KEYBIT(keys, PRISM_BTN_LEFT))
                mouse_btn |= 1;
        if (PRISM_KEYBIT(keys, PRISM_BTN_RIGHT))
                mouse_btn |= 2;
        if (PRISM_KEYBIT(keys, PRISM_BTN_MIDDLE))
                mouse_btn |= 4;
}

int DG_GetMouse(int *dx, int *dy, int *buttons)
{
        struct prism_input_event ev[16];
        int mx = 0, my = 0, wheel = 0, state;
        long n;

        if (mouse_fd < 0)
                return 0;

        while ((n = read(mouse_fd, ev, sizeof(ev))) >= (long)sizeof(ev[0])) {
                for (long i = 0; i < n / (long)sizeof(ev[0]); i++) {
                        int bit = 0;

                        switch (ev[i].type) {
                        case PRISM_EV_REL:
                                if (ev[i].code == PRISM_REL_X)
                                        mx += ev[i].value;
                                else if (ev[i].code == PRISM_REL_Y)
                                        my += ev[i].value;
                                else if (ev[i].code == PRISM_REL_WHEEL)
                                        wheel += ev[i].value;
                                break;
                        case PRISM_EV_KEY:
                                if (ev[i].code == PRISM_BTN_LEFT)
                                        bit = 1;
                                else if (ev[i].code == PRISM_BTN_RIGHT)
                                        bit = 2;
                                else if (ev[i].code == PRISM_BTN_MIDDLE)
                                        bit = 4;
                                if (ev[i].value)
                                        mouse_btn |= bit;
                                else
                                        mouse_btn &= ~bit;
                                break;
                        case PRISM_EV_SYN:
                                if (ev[i].code == PRISM_SYN_DROPPED) {
                                        mx = my = wheel = 0;
                                        mouse_resync();
                                }
                                break;
                        }
                }
        }

        /* A wheel notch is a one-poll press: the game sees the button
         * go down now and up on the next poll, which is what its
         * weapon cycling (g_game.c's SetMouseButtons) detects. */
        state = mouse_btn;
        if (wheel > 0)
                state |= 1 << MB_PREVWEAPON;
        else if (wheel < 0)
                state |= 1 << MB_NEXTWEAPON;

        if (!mx && !my && state == mouse_sent)
                return 0;
        mouse_sent = state;
        *dx = mx;
        *dy = my;
        *buttons = state;
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