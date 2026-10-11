/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdio.h>
#include <string.h>
#include "platform.h"

int console_getkey(int timeout_ms)
{
    uint64_t start = plat_ms();
    for (;;) {
        int k = plat_key_poll();
        if (k) return k;
        if (timeout_ms >= 0 && plat_ms() - start >= (uint64_t)timeout_ms)
            return -1;
        plat_idle();
    }
}

const char *key_name(int key, char *buf)
{
    static const char *const names[] = {
        [KEY_UP - 0x101] = "up", [KEY_DOWN - 0x101] = "down",
        [KEY_LEFT - 0x101] = "left", [KEY_RIGHT - 0x101] = "right",
        [KEY_HOME - 0x101] = "home", [KEY_END - 0x101] = "end",
        [KEY_PGUP - 0x101] = "pageup", [KEY_PGDN - 0x101] = "pagedown",
        [KEY_DEL - 0x101] = "delete", [KEY_INS - 0x101] = "insert",
    };
    if (key >= 0x101 && key <= KEY_INS && names[key - 0x101])
        return names[key - 0x101];
    if (key >= KEY_F1 && key < KEY_F1 + 12) { snprintf(buf, 8, "f%d", key - KEY_F1 + 1); return buf; }
    if (key == 13 || key == 10) return "enter";
    if (key == 27) return "esc";
    if (key == 9) return "tab";
    if (key == 8 || key == 127) return "backspace";
    if (key >= 32 && key < 127) { buf[0] = (char)key; buf[1] = 0; return buf; }
    if (key >= 1 && key < 27) { snprintf(buf, 8, "ctrl-%c", 'a' + key - 1); return buf; }
    return "unknown";
}
