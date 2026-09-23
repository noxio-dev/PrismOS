/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 */

#include <stddef.h>
#include <stdint.h>

#define VGA_MEM  ((uint16_t *) 0xB8000)
#define VGA_COLS 80
#define VGA_ROWS 25

static void vga_clear(uint8_t color) {
    for (int i = 0; i < VGA_COLS * VGA_ROWS; i++)
        VGA_MEM[i] = (uint16_t) ' ' | ((uint16_t) color << 8);
}

static void vga_print(const char *s, int row, int col, uint8_t color) {
    int pos = row * VGA_COLS + col;
    while (*s) {
        VGA_MEM[pos++] = (uint16_t) (unsigned char) *s | ((uint16_t) color << 8);
        s++;
    }
}

void kmain(void) {
    vga_clear(0x07);
    vga_print("Hello, World! -- noyau PrismOS (C, mode protege 32 bits)", 0, 0, 0x0F);
    vga_print("Charge depuis le disque par boot/loader.S.", 1, 0, 0x0A);

    for (;;) {
        __asm__ __volatile__("hlt");
    }
}
