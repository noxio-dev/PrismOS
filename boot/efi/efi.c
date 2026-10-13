/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "efi.h"
#include "../loader/platform.h"
#include "../loader/fs.h"

void loader_main(void);

/* referenced by the Windows ABI code generator when floating point is used */
int _fltused;

EFI_HANDLE efi_image;
EFI_SYSTEM_TABLE *efi_st;
EFI_BOOT_SERVICES *efi_bs;

static int guid_eq(const EFI_GUID *a, const EFI_GUID *b) { return !memcmp(a, b, sizeof *a); }

/* ---- text console ----------------------------------------------------------- */

#define MAXCOLS 256
#define MAXROWS 128

static struct {
    int cols, rows;
    uint16_t cell[MAXROWS][MAXCOLS];       /* (attr << 8) | char */
    int cur_x, cur_y, cur_attr;            /* what the firmware cursor/attribute are */
    CHAR16 run[MAXCOLS + 1];
    int run_len, run_x, run_y, run_attr;
} con;

static void con_set_attr(int attr)
{
    if (con.cur_attr != attr) {
        efi_st->con_out->set_attribute(efi_st->con_out, attr);
        con.cur_attr = attr;
    }
}

static void con_flush(void)
{
    if (!con.run_len) return;
    con_set_attr(con.run_attr);
    if (con.cur_x != con.run_x || con.cur_y != con.run_y)
        efi_st->con_out->set_cursor_position(efi_st->con_out, con.run_x, con.run_y);
    con.run[con.run_len] = 0;
    efi_st->con_out->output_string(efi_st->con_out, con.run);
    con.cur_x = con.run_x + con.run_len;
    con.cur_y = con.run_y;
    con.run_len = 0;
}

static void con_emit(int x, int y, char c, int attr)
{
    /* never write the bottom-right cell: it would scroll the firmware screen */
    if (x == con.cols - 1 && y == con.rows - 1) return;
    if (con.run_len && (y != con.run_y || x != con.run_x + con.run_len || attr != con.run_attr))
        con_flush();
    if (!con.run_len) { con.run_x = x; con.run_y = y; con.run_attr = attr; }
    con.run[con.run_len++] = (CHAR16)(unsigned char)c;
    if (x == con.cols - 2 && y == con.rows - 1) con_flush();
    if (x == con.cols - 1) con_flush();      /* do not rely on firmware line wrap */
}

static void efi_put(int x, int y, char c, int fg, int bg)
{
    int attr = (fg & 15) | ((bg & 7) << 4);
    con.cell[y][x] = (uint16_t)((attr << 8) | (unsigned char)c);
    con_emit(x, y, c, attr);
}

static void redraw_all(void)
{
    for (int y = 0; y < con.rows; y++)
        for (int x = 0; x < con.cols; x++) {
            uint16_t v = con.cell[y][x];
            con_emit(x, y, (char)(v & 0xFF), v >> 8);
        }
    con_flush();
}

static void efi_clear(int fg, int bg)
{
    int attr = (fg & 15) | ((bg & 7) << 4);
    con_flush();
    con_set_attr(attr);
    efi_st->con_out->clear_screen(efi_st->con_out);
    con.cur_x = con.cur_y = 0;
    for (int y = 0; y < con.rows; y++)
        for (int x = 0; x < con.cols; x++) con.cell[y][x] = (uint16_t)((attr << 8) | ' ');
}

static void efi_scroll(int fg, int bg)
{
    int attr = (fg & 15) | ((bg & 7) << 4);
    for (int y = 0; y + 1 < con.rows; y++) memcpy(con.cell[y], con.cell[y + 1], sizeof con.cell[y]);
    for (int x = 0; x < con.cols; x++) con.cell[con.rows - 1][x] = (uint16_t)((attr << 8) | ' ');
    redraw_all();
}

static void efi_cursor(int x, int y, int visible)
{
    con_flush();
    if (x == con.cols - 1 && y == con.rows - 1) x--;
    efi_st->con_out->set_cursor_position(efi_st->con_out, x, y);
    con.cur_x = x; con.cur_y = y;
    efi_st->con_out->enable_cursor(efi_st->con_out, visible);
}

static struct term_backend efi_term = { 80, 25, efi_put, efi_clear, efi_scroll, efi_cursor, NULL, con_flush };

/* ---- keyboard ------------------------------------------------------------------ */

int plat_key_poll(void)
{
    EFI_INPUT_KEY k;
    if (efi_st->con_in->read_key_stroke(efi_st->con_in, &k) != EFI_SUCCESS) return 0;
    if (k.unicode_char) {
        if (k.unicode_char == 127) return 8;
        if (k.unicode_char == '\n') return 13;
        return k.unicode_char < 256 ? k.unicode_char : 0;
    }
    switch (k.scan_code) {
    case SCAN_UP: return KEY_UP;       case SCAN_DOWN: return KEY_DOWN;
    case SCAN_LEFT: return KEY_LEFT;   case SCAN_RIGHT: return KEY_RIGHT;
    case SCAN_HOME: return KEY_HOME;   case SCAN_END: return KEY_END;
    case SCAN_PGUP: return KEY_PGUP;   case SCAN_PGDN: return KEY_PGDN;
    case SCAN_DELETE: return KEY_DEL;  case SCAN_INSERT: return KEY_INS;
    case SCAN_ESC: return 27;
    case SCAN_F11: return KEY_F1 + 10; case SCAN_F12: return KEY_F1 + 11;
    }
    if (k.scan_code >= SCAN_F1 && k.scan_code < SCAN_F1 + 10) return KEY_F1 + (k.scan_code - SCAN_F1);
    return 0;
}

/* ---- time ------------------------------------------------------------------------ */

static uint64_t tsc0, tsc_per_ms = 1;

static inline uint64_t rdtsc(void)
{
    uint32_t lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return ((uint64_t)hi << 32) | lo;
}

static void time_init(void)
{
    uint64_t a = rdtsc();
    efi_bs->stall(20000);
    uint64_t b = rdtsc();
    tsc_per_ms = (b - a) / 20;
    if (!tsc_per_ms) tsc_per_ms = 1;
    tsc0 = b;
}

uint64_t plat_ms(void) { return (rdtsc() - tsc0) / tsc_per_ms; }

void plat_sleep(unsigned ms)
{
    while (ms) {
        unsigned n = ms > 1000 ? 1000 : ms;
        efi_bs->stall(n * 1000);
        ms -= n;
    }
}

void plat_idle(void) { efi_bs->stall(2000); }

void plat_reboot(void)
{
    efi_st->runtime_services->reset_system(0, 0, 0, NULL);
    for (;;) __asm__ volatile("hlt");
}

void plat_shutdown(void)
{
    efi_st->runtime_services->reset_system(2, 0, 0, NULL);

    for (;;) {
        __asm__ volatile("cli; hlt");
    }
}

const char *plat_name(void) { return "uefi"; }

/* ---- memory ------------------------------------------------------------------------ */

static uint32_t convert_type(uint32_t t)
{
    switch (t) {
    case EfiLoaderCode: case EfiLoaderData: return PRISM_MEM_LOADER;
    case EfiBootServicesCode: case EfiBootServicesData:
    case EfiConventionalMemory: return PRISM_MEM_USABLE;
    case EfiUnusableMemory: return PRISM_MEM_BAD;
    case EfiACPIReclaimMemory: return PRISM_MEM_ACPI_RECL;
    case EfiACPIMemoryNVS: return PRISM_MEM_ACPI_NVS;
    default: return PRISM_MEM_RESERVED;
    }
}

/* convert the descriptors into entries, merging neighbours; returns the count */
static int convert_map(EFI_MEMORY_DESCRIPTOR *d, UINTN bytes, UINTN dsize, struct prism_mmap_entry *out, int max)
{
    int n = 0;
    for (UINTN off = 0; off + dsize <= bytes; off += dsize) {
        EFI_MEMORY_DESCRIPTOR *e = (EFI_MEMORY_DESCRIPTOR *)((char *)d + off);
        uint32_t t = convert_type(e->type);
        uint64_t len = e->number_of_pages * 4096;
        if (!len) continue;
        if (n && out[n - 1].type == t && out[n - 1].base + out[n - 1].length == e->physical_start) {
            out[n - 1].length += len;
            continue;
        }
        if (n >= max) break;
        out[n].base = e->physical_start; out[n].length = len; out[n].type = t; out[n].reserved = 0;
        n++;
    }
    return n;
}

int plat_memmap(struct prism_mmap_entry *buf, int max)
{
    UINTN size = 0, key, dsize;
    uint32_t dver;
    EFI_MEMORY_DESCRIPTOR *d = NULL;
    efi_bs->get_memory_map(&size, NULL, &key, &dsize, &dver);
    size += 16 * dsize;
    if (efi_bs->allocate_pool(EfiLoaderData, size, (void **)&d) != EFI_SUCCESS) return 0;
    int n = 0;
    if (efi_bs->get_memory_map(&size, d, &key, &dsize, &dver) == EFI_SUCCESS)
        n = convert_map(d, size, dsize, buf, max);
    efi_bs->free_pool(d);
    return n;
}

int plat_alloc(int kind, uint64_t size, uint64_t align, uint64_t *phys)
{
    /* everything the kernel needs before it sets up its own tables must sit
     * below 4 GiB, where the loader's page tables identity map it */
    uint64_t pages = (size + align + 4095) / 4096, addr = 0xFFFFFFFFULL;
    (void)kind;
    if (efi_bs->allocate_pages(AllocateMaxAddress, EfiLoaderData, pages, &addr) != EFI_SUCCESS) return -1;
    *phys = (addr + align - 1) & ~(align - 1);
    return 0;
}

int plat_exit(struct prism_mmap_entry *buf, int max)
{
    UINTN size = 0, key, dsize;
    uint32_t dver;
    EFI_MEMORY_DESCRIPTOR *d = NULL;

    efi_bs->get_memory_map(&size, NULL, &key, &dsize, &dver);
    size += 32 * dsize;
    if (efi_bs->allocate_pool(EfiLoaderData, size, (void **)&d) != EFI_SUCCESS) return -1;
    for (int tries = 0; tries < 8; tries++) {
        UINTN sz = size;
        if (efi_bs->get_memory_map(&sz, d, &key, &dsize, &dver) != EFI_SUCCESS) return -1;
        int n = convert_map(d, sz, dsize, buf, max);
        /* no allocation, no printing between the map and this call */
        if (efi_bs->exit_boot_services(efi_image, key) == EFI_SUCCESS) return n;
    }
    return -1;
}

/* ---- firmware tables ---------------------------------------------------------------------- */

void plat_fill_bootinfo(struct prism_bootinfo *bi)
{
    static const EFI_GUID acpi20 = GUID_ACPI20, acpi10 = GUID_ACPI10;
    static const EFI_GUID smbios = GUID_SMBIOS, smbios3 = GUID_SMBIOS3, gop_guid = GUID_GOP;

    bi->firmware = PRISM_FW_UEFI;
    bi->efi_system_table = (uint64_t)(uintptr_t)efi_st;

    for (UINTN i = 0; i < efi_st->number_of_table_entries; i++) {
        EFI_CONFIGURATION_TABLE *t = &efi_st->configuration_table[i];
        if (guid_eq(&t->guid, &acpi20)) bi->rsdp_phys = (uint64_t)(uintptr_t)t->table;
        else if (guid_eq(&t->guid, &acpi10) && !bi->rsdp_phys) bi->rsdp_phys = (uint64_t)(uintptr_t)t->table;
        else if (guid_eq(&t->guid, &smbios3)) bi->smbios_phys = (uint64_t)(uintptr_t)t->table;
        else if (guid_eq(&t->guid, &smbios) && !bi->smbios_phys) bi->smbios_phys = (uint64_t)(uintptr_t)t->table;
    }

    EFI_GRAPHICS_OUTPUT_PROTOCOL *gop = NULL;
    if (efi_bs->locate_protocol((EFI_GUID *)&gop_guid, NULL, (void **)&gop) != EFI_SUCCESS || !gop || !gop->mode)
        return;
    EFI_GRAPHICS_OUTPUT_MODE_INFORMATION *i = gop->mode->info;
    if (!i || i->pixel_format == PixelBltOnly || !gop->mode->frame_buffer_base) return;

    struct prism_framebuffer *fb = &bi->fb;
    fb->addr = gop->mode->frame_buffer_base;
    fb->width = i->horizontal_resolution;
    fb->height = i->vertical_resolution;
    fb->pitch = i->pixels_per_scan_line * 4;
    fb->bpp = 32;
    if (i->pixel_format == PixelRedGreenBlueReserved8BitPerColor) {
        fb->red_shift = 0; fb->green_shift = 8; fb->blue_shift = 16;
        fb->red_bits = fb->green_bits = fb->blue_bits = 8;
    } else if (i->pixel_format == PixelBlueGreenRedReserved8BitPerColor) {
        fb->blue_shift = 0; fb->green_shift = 8; fb->red_shift = 16;
        fb->red_bits = fb->green_bits = fb->blue_bits = 8;
    } else {
        uint32_t m[3] = { i->red_mask, i->green_mask, i->blue_mask };
        uint8_t sh[3], bits[3];
        for (int k = 0; k < 3; k++) {
            sh[k] = m[k] ? (uint8_t)__builtin_ctz(m[k]) : 0;
            bits[k] = (uint8_t)__builtin_popcount(m[k]);
        }
        fb->red_shift = sh[0]; fb->green_shift = sh[1]; fb->blue_shift = sh[2];
        fb->red_bits = bits[0]; fb->green_bits = bits[1]; fb->blue_bits = bits[2];
    }
    bi->flags |= PRISM_FLAG_FB;
}

/* ---- entry --------------------------------------------------------------------------------- */

EFI_STATUS efi_main(EFI_HANDLE image, EFI_SYSTEM_TABLE *st)
{
    EFI_PHYSICAL_ADDRESS heap = 0;
    const UINTN heap_pages = 16 * 256;           /* 16 MiB */

    efi_image = image;
    efi_st = st;
    efi_bs = st->boot_services;
    efi_bs->set_watchdog_timer(0, 0, 0, NULL);

    /* console: keep the current mode unless it is smaller than 80x25 */
    UINTN cols = 80, rows = 25;
    st->con_out->query_mode(st->con_out, st->con_out->mode->mode, &cols, &rows);
    if (cols < 80 || rows < 25) {
        st->con_out->set_mode(st->con_out, 0);
        st->con_out->query_mode(st->con_out, 0, &cols, &rows);
    }
    efi_term.cols = cols > MAXCOLS ? MAXCOLS : (int)cols;
    efi_term.rows = rows > MAXROWS ? MAXROWS : (int)rows;
    con.cur_attr = -1;
    console_init(&efi_term);

    if (efi_bs->allocate_pages(AllocateAnyPages, EfiLoaderData, heap_pages, &heap) != EFI_SUCCESS)
        console_panic("cannot allocate the loader heap");
    heap_init((void *)(uintptr_t)heap, heap_pages * 4096);

    time_init();
    if (fs_init() < 0)
        console_panic("cannot open the boot volume");

    loader_main();
    plat_reboot();
}
