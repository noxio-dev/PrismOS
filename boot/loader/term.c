/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
/*
 * A small ANSI terminal on top of a cell based backend (VGA text memory,
 * UEFI text output).  The Lua scripts only emit ANSI escape sequences, so
 * the very same menu works on the screen and on a serial terminal.
 *
 * Supported: CR LF BS TAB, CSI H f A B C D J K m s u and ?25h/?25l.
 */
#include <string.h>
#include "platform.h"

static struct term_backend *be;
static int x, y, fg = 7, bg = 0, bold, rev, curvis = 1;
static int sx, sy;
static int state, params[8], nparams, priv;

static const int ansi2vga[8] = { 0, 4, 2, 6, 1, 5, 3, 7 };

static void colors(int *f, int *b)
{
    int ff = fg | (bold ? 8 : 0), bb = bg;
    if (rev) { *f = bb; *b = ff & 7; } else { *f = ff; *b = bb; }
}

static void sync_cursor(void) { if (be->cursor) be->cursor(x, y, curvis); }

static void newline(void)
{
    int f, b;
    colors(&f, &b);
    if (++y >= be->rows) { be->scroll(f, b); y = be->rows - 1; }
}

static void cell(char c)
{
    int f, b;
    colors(&f, &b);
    be->put(x, y, c, f, b);
    if (++x >= be->cols) { x = 0; newline(); }
}

static void clear_range(int x0, int y0, int x1, int y1)   /* inclusive */
{
    int f, b;
    colors(&f, &b);
    for (int j = y0; j <= y1; j++)
        for (int i = (j == y0 ? x0 : 0); i <= (j == y1 ? x1 : be->cols - 1); i++)
            be->put(i, j, ' ', f, b);
}

static int param(int i, int def) { return (i < nparams && params[i] > 0) ? params[i] : def; }

static void sgr(void)
{
    if (!nparams) { params[0] = 0; nparams = 1; }
    for (int i = 0; i < nparams; i++) {
        int p = params[i];
        if (p == 0) { fg = 7; bg = 0; bold = 0; rev = 0; }
        else if (p == 1) bold = 1;
        else if (p == 22) bold = 0;
        else if (p == 7) rev = 1;
        else if (p == 27) rev = 0;
        else if (p >= 30 && p <= 37) fg = ansi2vga[p - 30];
        else if (p == 39) fg = 7;
        else if (p >= 40 && p <= 47) bg = ansi2vga[p - 40];
        else if (p == 49) bg = 0;
        else if (p >= 90 && p <= 97) { fg = ansi2vga[p - 90]; bold = 1; }
        else if (p >= 100 && p <= 107) bg = ansi2vga[p - 100];
    }
}

static void csi(int final)
{
    switch (final) {
    case 'H': case 'f':
        y = param(0, 1) - 1; x = param(1, 1) - 1;
        if (y < 0) y = 0;
        if (y >= be->rows) y = be->rows - 1;
        if (x < 0) x = 0;
        if (x >= be->cols) x = be->cols - 1;
        break;
    case 'A': y -= param(0, 1); if (y < 0) y = 0; break;
    case 'B': y += param(0, 1); if (y >= be->rows) y = be->rows - 1; break;
    case 'C': x += param(0, 1); if (x >= be->cols) x = be->cols - 1; break;
    case 'D': x -= param(0, 1); if (x < 0) x = 0; break;
    case 'J':
        if (nparams && params[0] == 2) { int f, b; colors(&f, &b); be->clear(f, b); }
        else if (nparams && params[0] == 1) clear_range(0, 0, x, y);
        else clear_range(x, y, be->cols - 1, be->rows - 1);
        break;
    case 'K':
        if (nparams && params[0] == 1) clear_range(0, y, x, y);
        else if (nparams && params[0] == 2) clear_range(0, y, be->cols - 1, y);
        else clear_range(x, y, be->cols - 1, y);
        break;
    case 'm': sgr(); break;
    case 's': sx = x; sy = y; break;
    case 'u': x = sx; y = sy; break;
    case 'h': case 'l':
        if (priv && nparams && params[0] == 25) curvis = (final == 'h');
        break;
    }
}

static void put_char(unsigned char c)
{
    switch (state) {
    case 1:
        if (c == '[') { state = 2; nparams = 0; priv = 0; memset(params, 0, sizeof params); }
        else state = 0;
        return;
    case 2:
        if (c >= '0' && c <= '9') {
            if (!nparams) nparams = 1;
            params[nparams - 1] = params[nparams - 1] * 10 + (c - '0');
        } else if (c == ';') {
            if (!nparams) nparams = 1;
            if (nparams < 8) nparams++;
        } else if (c == '?') priv = 1;
        else if (c >= 0x40 && c <= 0x7E) { csi(c); state = 0; }
        else state = 0;
        return;
    }
    switch (c) {
    case 27: state = 1; break;
    case '\n': x = 0; newline(); break;
    case '\r': x = 0; break;
    case '\b': if (x > 0) x--; break;
    case '\t': x = (x + 8) & ~7; if (x >= be->cols) x = be->cols - 1; break;
    case 7: break;
    default: if (c >= 32) cell(c < 127 ? c : '?'); break;
    }
}

void console_init(struct term_backend *b)
{
    be = b;
    x = y = 0; fg = 7; bg = 0; bold = rev = 0; curvis = 1; state = 0;
    be->clear(7, 0);
    sync_cursor();
}

void console_write(const char *s, size_t n)
{
    if (!be) return;
    for (size_t i = 0; i < n; i++) {
        if (be->raw) be->raw(s[i]);
        put_char((unsigned char)s[i]);
    }
    sync_cursor();
    if (be->flush) be->flush();
}

void console_puts(const char *s) { console_write(s, strlen(s)); }

void console_size(int *rows, int *cols) { *rows = be ? be->rows : 25; *cols = be ? be->cols : 80; }

void console_panic(const char *msg)
{
    console_puts("\x1b[0m\r\npanic: ");
    console_puts(msg);
    console_puts("\r\nSystem halted.\r\n");
    for (;;) __asm__ volatile("cli; hlt");
}
