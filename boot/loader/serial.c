/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
/* COM1 console (115200 8N1): mirrors the screen and provides keyboard input. */
#include "platform.h"

#define COM1 0x3F8
static int serial_ok;

void serial_init(void)
{
    outb(COM1 + 1, 0); outb(COM1 + 3, 0x80); outb(COM1 + 0, 1); outb(COM1 + 1, 0);
    outb(COM1 + 3, 3); outb(COM1 + 2, 0xC7); outb(COM1 + 4, 0x0B);
    outb(COM1 + 7, 0xAE);
    serial_ok = inb(COM1 + 7) == 0xAE;
}

static void serial_putc(char c)
{
    if (!serial_ok) return;
    for (int i = 0; i < 100000 && !(inb(COM1 + 5) & 0x20); i++) ;
    outb(COM1, c);
}

void serial_raw(char c)
{
    if (c == '\n') serial_putc('\r');
    serial_putc(c);
}

static int serial_getc(void)           /* -1 if nothing */
{
    return (serial_ok && (inb(COM1 + 5) & 1)) ? inb(COM1) : -1;
}

/* wait (by spinning, timers are frozen) for the next serial byte */
static int serial_getc_wait(void)
{
    for (int i = 0; i < 400000; i++) {
        int c = serial_getc();
        if (c >= 0) return c;
    }
    return -1;
}

int serial_key(void)
{
    int c = serial_getc();
    if (c < 0) return 0;
    if (c == 127) return 8;
    if (c == 10) return 13;
    if (c != 27) return c;
    int c2 = serial_getc_wait();
    if (c2 < 0) return 27;
    if (c2 != '[' && c2 != 'O') return 27;
    int num = 0, c3;
    while ((c3 = serial_getc_wait()) >= 0) {
        if (c3 >= '0' && c3 <= '9') { num = num * 10 + (c3 - '0'); continue; }
        if (c3 == ';') { num = 0; continue; }
        switch (c3) {
        case 'A': return KEY_UP;
        case 'B': return KEY_DOWN;
        case 'C': return KEY_RIGHT;
        case 'D': return KEY_LEFT;
        case 'H': return KEY_HOME;
        case 'F': return KEY_END;
        case 'P': case 'Q': case 'R': case 'S': return KEY_F1 + (c3 - 'P');
        case '~':
            switch (num) {
            case 1: case 7: return KEY_HOME;
            case 2: return KEY_INS;
            case 3: return KEY_DEL;
            case 4: case 8: return KEY_END;
            case 5: return KEY_PGUP;
            case 6: return KEY_PGDN;
            case 11: case 12: case 13: case 14: case 15: return KEY_F1 + (num - 11);
            case 17: case 18: case 19: case 20: case 21: return KEY_F1 + 5 + (num - 17);
            case 23: case 24: return KEY_F1 + 10 + (num - 23);
            }
            return 0;
        default: return 0;
        }
    }
    return 0;
}
