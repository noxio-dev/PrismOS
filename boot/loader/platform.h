/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#ifndef LOADER_PLATFORM_H
#define LOADER_PLATFORM_H

#include <stddef.h>
#include <stdint.h>
#include <prism/boot.h>

/* ---- console ------------------------------------------------------ */

struct term_backend {
    int cols, rows;
    void (*put)(int x, int y, char c, int fg, int bg);   /* VGA palette 0-15 */
    void (*clear)(int fg, int bg);
    void (*scroll)(int fg, int bg);                      /* scroll up 1 line */
    void (*cursor)(int x, int y, int visible);
    void (*raw)(char c);          /* optional: mirror the raw stream */
    void (*flush)(void);          /* optional: called after each console_write */
};

void console_init(struct term_backend *b);
void console_write(const char *s, size_t n);
void console_puts(const char *s);
void console_panic(const char *msg) __attribute__((noreturn));
void console_size(int *rows, int *cols);

/* key codes: ASCII for printable, 8 backspace, 9 tab, 13 enter, 27 esc */
#define KEY_UP     0x101
#define KEY_DOWN   0x102
#define KEY_LEFT   0x103
#define KEY_RIGHT  0x104
#define KEY_HOME   0x105
#define KEY_END    0x106
#define KEY_PGUP   0x107
#define KEY_PGDN   0x108
#define KEY_DEL    0x109
#define KEY_INS    0x10A
#define KEY_F1     0x110            /* F1..F12 = 0x110..0x11B */

/* serial console (serial.c): 115200 8N1 on COM1, mirrors the screen */
void serial_init(void);
void serial_raw(char c);
int  serial_key(void);              /* key code or 0 */

/* returns a key code, or -1 on timeout (timeout_ms < 0 waits forever) */
int  console_getkey(int timeout_ms);
const char *key_name(int key, char *buf);

/* ---- platform backend (bios/ or efi/) ----------------------------- */

const char *plat_name(void);                 /* "bios" or "uefi" */
uint64_t plat_ms(void);                      /* monotonic milliseconds */
void plat_sleep(unsigned ms);
void plat_idle(void);                        /* wait briefly for events */
int  plat_key_poll(void);                    /* key code or 0 */
void plat_reboot(void) __attribute__((noreturn));

#define PLAT_ALLOC_KERNEL   1        /* kernel image and modules */
#define PLAT_ALLOC_BOOTDATA 2        /* boot info, tables, stack (< 4 GiB) */
int plat_alloc(int kind, uint64_t size, uint64_t align, uint64_t *phys);

/* typed copy of the firmware memory map, raw (not yet carved) */
int plat_memmap(struct prism_mmap_entry *buf, int max);

/* fill firmware dependent fields (framebuffer, rsdp, smbios, ...) */
void plat_fill_bootinfo(struct prism_bootinfo *bi);

/*
 * Point of no return: capture the final memory map into buf and release
 * firmware services (ExitBootServices on UEFI).  Memory used by the loader
 * itself must be typed PRISM_MEM_LOADER, firmware low memory RESERVED.
 */
int plat_exit(struct prism_mmap_entry *buf, int max);

int mmap_carve(struct prism_mmap_entry *m, int n, int max, uint64_t base, uint64_t len, uint32_t type);

/* ---- boot (boot.c) ------------------------------------------------- */

struct loaded_kernel {
    int      valid;
    char     path[128];
    uint64_t phys;
    uint64_t virt;                   /* virtual base */
    uint64_t size;
    uint64_t entry;                  /* virtual entry point */
};

int  kernel_load(const char *path, const char **err);
int  module_load(const char *path, const char *type, const char *name, const char **err);
void module_clear(void);
const struct loaded_kernel *kernel_info(void);
int  boot_kernel(const char *cmdline, const char **err);   /* only returns on error */

/* ---- environment (env.c) ------------------------------------------- */

const char *env_get(const char *name);
int  env_set(const char *name, const char *value, int overwrite);
void env_unset(const char *name);
void env_each(void (*cb)(const char *name, const char *value, void *arg), void *arg);

/* ---- helpers --------------------------------------------------------- */

static inline void outb(uint16_t p, uint8_t v) { __asm__ volatile("outb %0, %1" : : "a"(v), "Nd"(p)); }
static inline uint8_t inb(uint16_t p) { uint8_t v; __asm__ volatile("inb %1, %0" : "=a"(v) : "Nd"(p)); return v; }
static inline void outw(uint16_t p, uint16_t v) { __asm__ volatile("outw %0, %1" : : "a"(v), "Nd"(p)); }

int loader_run(void);                        /* lua_loader.c: runs loader.lua */

#endif
