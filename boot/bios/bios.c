/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <prism.inc>
#include "../loader/platform.h"
#include "../loader/fs.h"
#include "bios.h"

void loader_main(void);

uint32_t bios_drive;
static void (*thunk)(void);

void bios_call(struct bios_regs *r)
{
    memcpy((void *)BIOS_REGS, r, sizeof *r);
    thunk();
    memcpy(r, (void *)BIOS_REGS, sizeof *r);
}

/* ---- VGA text console --------------------------------------------------- */

static volatile uint16_t *const vga = (volatile uint16_t *)0xB8000;

static uint16_t cellval(char c, int fg, int bg) { return (uint8_t)c | ((bg << 4 | fg) << 8); }

static void vga_put(int x, int y, char c, int fg, int bg) { vga[y * 80 + x] = cellval(c, fg, bg & 7); }

static void vga_clear(int fg, int bg)
{
    for (int i = 0; i < 80 * 25; i++) vga[i] = cellval(' ', fg, bg & 7);
}

static void vga_scroll(int fg, int bg)
{
    for (int i = 0; i < 80 * 24; i++) vga[i] = vga[i + 80];
    for (int i = 80 * 24; i < 80 * 25; i++) vga[i] = cellval(' ', fg, bg & 7);
}

static void vga_cursor(int x, int y, int visible)
{
    uint16_t pos = y * 80 + x;
    outb(0x3D4, 0x0F); outb(0x3D5, pos & 0xFF);
    outb(0x3D4, 0x0E); outb(0x3D5, pos >> 8);
    outb(0x3D4, 0x0A);
    uint8_t v = inb(0x3D5);
    outb(0x3D5, visible ? (v & ~0x20) : (v | 0x20));
}

static struct term_backend vga_term = {
    80, 25, vga_put, vga_clear, vga_scroll, vga_cursor, serial_raw
};

/* ---- time, keyboard, reboot ---------------------------------------------- */

uint64_t plat_ms(void)
{
    return (uint64_t)*(volatile uint32_t *)0x46C * 54925 / 1000;
}

void plat_sleep(unsigned ms)
{
    while (ms) {
        unsigned n = ms > 1000 ? 1000 : ms;
        struct bios_regs r = { 0 };
        r.intno = 0x15; r.eax = 0x8600;
        r.ecx = (n * 1000) >> 16; r.edx = (n * 1000) & 0xFFFF;
        bios_call(&r);
        ms -= n;
    }
}

void plat_idle(void)
{
    struct bios_regs r = { 0 };
    r.intno = 0x15; r.eax = 0x8600; r.ecx = 0; r.edx = 2000;
    bios_call(&r);
}

int plat_key_poll(void)
{
    int k = serial_key();
    if (k) return k;
    struct bios_regs r = { 0 };
    r.intno = 0x16; r.eax = 0x0100;
    bios_call(&r);
    if (r.eflags & FLAG_ZF) return 0;
    r.intno = 0x16; r.eax = 0x0000;
    bios_call(&r);
    int ascii = r.eax & 0xFF, scan = (r.eax >> 8) & 0xFF;
    if (ascii && ascii != 0xE0) return ascii;
    switch (scan) {
    case 0x48: return KEY_UP;   case 0x50: return KEY_DOWN;
    case 0x4B: return KEY_LEFT; case 0x4D: return KEY_RIGHT;
    case 0x47: return KEY_HOME; case 0x4F: return KEY_END;
    case 0x49: return KEY_PGUP; case 0x51: return KEY_PGDN;
    case 0x53: return KEY_DEL;  case 0x52: return KEY_INS;
    case 0x85: return KEY_F1 + 10; case 0x86: return KEY_F1 + 11;
    }
    if (scan >= 0x3B && scan <= 0x44) return KEY_F1 + (scan - 0x3B);
    return 0;
}

void plat_reboot(void)
{
    for (int i = 0; i < 0x10000 && (inb(0x64) & 2); i++) ;
    outb(0x64, 0xFE);
    for (int i = 0; i < 100000; i++) __asm__ volatile("pause");
    static const uint8_t nullidt[10] = { 0 };       /* triple fault */
    __asm__ volatile("lidt %0; int3" : : "m"(nullidt));
    for (;;) __asm__ volatile("hlt");
}

void plat_shutdown(void)
{
    __asm__ volatile("cli");

    outw(0x604, 0x2000);

    outw(0xB004, 0x2000);

    for (;;) {
        __asm__ volatile("hlt");
    }
}

/* ---- memory -------------------------------------------------------------- */

int e820_read(struct prism_mmap_entry *m, int max)
{
    uint32_t cont = 0;
    int n = 0;
    do {
        struct bios_regs r = { 0 };
        uint8_t *buf = (uint8_t *)(BIOS_SCRATCH + 0x40);
        memset(buf, 0, 24);
        buf[20] = 1;                                   /* ACPI 3.0 "enabled" default */
        r.intno = 0x15; r.eax = 0xE820; r.ebx = cont; r.ecx = 24; r.edx = 0x534D4150;
        r.es = 0; r.edi = BIOS_SCRATCH + 0x40;
        bios_call(&r);
        if ((r.eflags & FLAG_CF) || r.eax != 0x534D4150) break;
        cont = r.ebx;
        if (r.ecx >= 24 && !(buf[20] & 1)) continue;
        if (n >= max) break;
        uint32_t t = *(uint32_t *)(buf + 16);
        m[n].base = *(uint64_t *)buf;
        m[n].length = *(uint64_t *)(buf + 8);
        m[n].type = t == 1 ? PRISM_MEM_USABLE : t == 3 ? PRISM_MEM_ACPI_RECL :
                    t == 4 ? PRISM_MEM_ACPI_NVS : t == 5 ? PRISM_MEM_BAD : PRISM_MEM_RESERVED;
        m[n].reserved = 0;
        if (m[n].length) n++;
    } while (cont);
    return n;
}

int plat_memmap(struct prism_mmap_entry *buf, int max) { return e820_read(buf, max); }

static uint64_t kern_next = KERNEL_LOAD_BASE, bd_next = BOOTDATA_BASE;

int plat_alloc(int kind, uint64_t size, uint64_t align, uint64_t *phys)
{
    uint64_t *next = kind == PLAT_ALLOC_KERNEL ? &kern_next : &bd_next;
    uint64_t limit = kind == PLAT_ALLOC_KERNEL ? KERNEL_LOAD_LIMIT : BOOTDATA_BASE + BOOTDATA_SIZE;
    uint64_t a = (*next + align - 1) & ~(align - 1);
    if (a + size > limit) return -1;
    *next = a + size;
    *phys = a;
    return 0;
}

static void vbe_enter(void);

int plat_exit(struct prism_mmap_entry *buf, int max)
{
    int n = e820_read(buf, max);
    if (n <= 0) return -1;
    n = mmap_carve(buf, n, max, 0, 0x100000, PRISM_MEM_RESERVED);
    n = mmap_carve(buf, n, max, LOADER_ADDR, LOADER_STACK_TOP - LOADER_ADDR, PRISM_MEM_LOADER);
    vbe_enter();                                       /* last BIOS call: no way back from here */
    return n;
}

static int checksum_ok(const uint8_t *p, int len)
{
    uint8_t s = 0;
    for (int i = 0; i < len; i++) s += p[i];
    return s == 0;
}

static uint64_t scan_sig(uint64_t from, uint64_t to, const char *sig, int siglen, int (*ok)(const uint8_t *))
{
    for (uint64_t a = from; a + 16 <= to; a += 16)
        if (!memcmp((void *)a, sig, siglen) && ok((const uint8_t *)a)) return a;
    return 0;
}

static int rsdp_ok(const uint8_t *p)
{
    if (!checksum_ok(p, 20)) return 0;
    return p[15] < 2 || checksum_ok(p, *(const uint32_t *)(p + 20));
}
static int sm_ok(const uint8_t *p) { return checksum_ok(p, p[5]); }
static int sm3_ok(const uint8_t *p) { return checksum_ok(p, p[6]); }

/* ---- VESA / VBE linear framebuffer ----------------------------------------
 *
 * The loader menu stays on the VGA text screen.  Just before the kernel is
 * entered we look for a 32 bpp linear-framebuffer mode (VBE 2.0+) and, at the
 * point of no return (plat_exit), switch to it and describe it to the kernel
 * in struct prism_framebuffer, exactly like the UEFI GOP path does.  When no
 * usable mode exists the kernel keeps getting VGA text as before.
 *
 * loader.conf: vbe_mode="auto" (default), "off", or an exact "1024x768".
 */

#define VBE_INFO        (BIOS_SCRATCH + 0x100)      /* 512-byte controller block */
#define VBE_MODEINFO    (BIOS_SCRATCH + 0x400)      /* 256-byte mode block */
#define VBE_AUTO_MAX_W  1920                        /* auto never goes beyond 1080p ... */
#define VBE_AUTO_MAX_H  1080                        /* ... unless nothing smaller exists */

static struct prism_bootinfo *vbe_bi;               /* set while a mode is pending */
static uint16_t vbe_mode_id;

static int vbe_call(uint32_t ax, uint32_t bx, uint32_t cx, uint32_t di)
{
    struct bios_regs r = { 0 };
    r.intno = 0x10; r.eax = ax; r.ebx = bx; r.ecx = cx; r.es = 0; r.edi = di;
    bios_call(&r);
    return (r.eax & 0xFFFF) == 0x004F;              /* AL = supported, AH = success */
}

/* Fill *fb from the mode block of `mode` (already in VBE_MODEINFO). */
static void vbe_describe(struct prism_framebuffer *fb, uint16_t vbe_version)
{
    const uint8_t *mi = (const uint8_t *)VBE_MODEINFO;
    uint16_t pitch = *(const uint16_t *)(mi + 16);
    if (vbe_version >= 0x0300 && *(const uint16_t *)(mi + 50))
        pitch = *(const uint16_t *)(mi + 50);       /* LinBytesPerScanLine */
    fb->addr = *(const uint32_t *)(mi + 40);        /* PhysBasePtr */
    fb->width = *(const uint16_t *)(mi + 18);
    fb->height = *(const uint16_t *)(mi + 20);
    fb->pitch = pitch;
    fb->bpp = mi[25];
    fb->red_bits = mi[31];   fb->red_shift = mi[32];
    fb->green_bits = mi[33]; fb->green_shift = mi[34];
    fb->blue_bits = mi[35];  fb->blue_shift = mi[36];
    fb->reserved = 0;
}

/* "1024x768" -> 1, anything else -> 0 */
static int parse_wxh(const char *s, unsigned *w, unsigned *h)
{
    unsigned a = 0, b = 0;
    int digits = 0;
    for (; *s >= '0' && *s <= '9'; s++, digits++) a = a * 10 + (*s - '0');
    if (!digits || (*s != 'x' && *s != 'X')) return 0;
    s++;
    for (digits = 0; *s >= '0' && *s <= '9'; s++, digits++) b = b * 10 + (*s - '0');
    if (!digits || *s) return 0;
    *w = a; *h = b;
    return a > 0 && b > 0;
}

/* Pick a mode and fill *fb.  Returns the VBE mode number, or 0 for none. */
static uint16_t vbe_pick(struct prism_framebuffer *fb)
{
    const char *opt = env_get("vbe_mode");
    unsigned want_w = 0, want_h = 0;
    if (opt && (!strcmp(opt, "off") || !strcmp(opt, "no") || !strcmp(opt, "0") ||
                !strcmp(opt, "text") || !strcmp(opt, "NO") || !strcmp(opt, "OFF")))
        return 0;
    if (opt) parse_wxh(opt, &want_w, &want_h);       /* anything else means "auto" */

    uint8_t *info = (uint8_t *)VBE_INFO;
    memset(info, 0, 512);
    memcpy(info, "VBE2", 4);                         /* ask for the VBE 2.0+ block */
    if (!vbe_call(0x4F00, 0, 0, VBE_INFO) || memcmp(info, "VESA", 4)) return 0;
    uint16_t version = *(uint16_t *)(info + 4);
    if (version < 0x0200) return 0;                  /* no linear framebuffer before 2.0 */

    uint32_t mp = *(uint32_t *)(info + 14);          /* far pointer to the mode list */
    uint32_t list_at = (mp >> 16) * 16 + (mp & 0xFFFF);
    if (!list_at || list_at >= 0x100000) return 0;
    const uint16_t *list = (const uint16_t *)(uintptr_t)list_at;

    /* exact: the mode asked for; best: largest up to 1080p; small: smallest above it */
    uint16_t exact = 0, best = 0, small = 0;
    uint64_t best_area = 0, small_area = ~0ULL;
    uint8_t *mi = (uint8_t *)VBE_MODEINFO;
    for (int i = 0; i < 512 && list[i] != 0xFFFF; i++) {
        uint16_t mode = list[i];
        memset(mi, 0, 256);
        if (!vbe_call(0x4F01, 0, mode, VBE_MODEINFO)) continue;
        uint16_t attr = *(uint16_t *)mi;
        if ((attr & 0x91) != 0x91) continue;         /* supported + graphics + linear fb */
        if (mi[25] != 32 || mi[27] != 6) continue;   /* 32 bpp direct colour only */
        struct prism_framebuffer c;
        vbe_describe(&c, version);
        if (!c.addr || c.pitch < c.width * 4 || c.width < 640 || c.height < 400) continue;
        uint64_t area = (uint64_t)c.width * c.height;
        if (want_w && c.width == want_w && c.height == want_h) exact = mode;
        if (c.width <= VBE_AUTO_MAX_W && c.height <= VBE_AUTO_MAX_H) {
            if (area > best_area) { best = mode; best_area = area; }
        } else if (area < small_area) {
            small = mode; small_area = area;
        }
    }
    uint16_t pick = exact ? exact : best ? best : small;   /* a mode that is not there: auto */
    if (!pick) return 0;

    memset(mi, 0, 256);
    if (!vbe_call(0x4F01, 0, pick, VBE_MODEINFO)) return 0;
    vbe_describe(fb, version);
    return pick;
}

/* Called at the point of no return: switch the display to the chosen mode. */
static void vbe_enter(void)
{
    struct prism_bootinfo *bi = vbe_bi;
    if (!bi) return;
    vbe_bi = NULL;
    if (vbe_call(0x4F02, vbe_mode_id | 0x4000, 0, 0)) {   /* bit 14: linear fb, bit 15 clear: wipe it */
        /* The pitch can change once the mode is really set: ask again. */
        struct bios_regs r = { 0 };
        r.intno = 0x10; r.eax = 0x4F06; r.ebx = 1;        /* get scan line length */
        bios_call(&r);
        if ((r.eax & 0xFFFF) == 0x004F && (r.ebx & 0xFFFF) >= bi->fb.width * 4)
            bi->fb.pitch = r.ebx & 0xFFFF;
        return;
    }
    /* The BIOS refused after all: back to text mode and tell the kernel. */
    struct bios_regs r = { 0 };
    r.intno = 0x10; r.eax = 0x0003;
    bios_call(&r);
    memset(&bi->fb, 0, sizeof bi->fb);
    bi->flags = (bi->flags & ~PRISM_FLAG_FB) | PRISM_FLAG_VGATEXT;
}

/* Choose the kernel's display and describe it in the boot info. */
static void vbe_setup(struct prism_bootinfo *bi)
{
    vbe_mode_id = vbe_pick(&bi->fb);
    if (vbe_mode_id) {
        bi->flags |= PRISM_FLAG_FB;                   /* no VGA text: it is invisible in a graphics mode */
        vbe_bi = bi;
    } else {
        memset(&bi->fb, 0, sizeof bi->fb);
        bi->flags |= PRISM_FLAG_VGATEXT;
    }
}

void plat_fill_bootinfo(struct prism_bootinfo *bi)
{
    bi->firmware = PRISM_FW_BIOS;
    vbe_setup(bi);
    uint64_t ebda = (uint64_t)*(volatile uint16_t *)0x40E << 4;
    uint64_t rsdp = 0;
    if (ebda >= 0x80000 && ebda < 0xA0000) rsdp = scan_sig(ebda, ebda + 1024, "RSD PTR ", 8, rsdp_ok);
    if (!rsdp) rsdp = scan_sig(0xE0000, 0x100000, "RSD PTR ", 8, rsdp_ok);
    bi->rsdp_phys = rsdp;
    uint64_t sm = scan_sig(0xF0000, 0x100000, "_SM3_", 5, sm3_ok);
    if (!sm) sm = scan_sig(0xF0000, 0x100000, "_SM_", 4, sm_ok);
    bi->smbios_phys = sm;
}

/* ---- exceptions ----------------------------------------------------------- */

struct exc_frame {
    uint64_t r15, r14, r13, r12, r11, r10, r9, r8, rdi, rsi, rbp, rbx, rdx, rcx, rax;
    uint64_t vec, err, rip, cs, rflags, rsp, ss;
};

void exception_handler(struct exc_frame *f)
{
    uint64_t cr2;
    __asm__ volatile("movq %%cr2, %0" : "=r"(cr2));
    console_puts("\x1b[0m\x1b[41;37m");
    printf("\nEXCEPTION %d err=%llx rip=%llx cs=%llx rflags=%llx rsp=%llx cr2=%llx\n",
           (int)f->vec, (unsigned long long)f->err, (unsigned long long)f->rip,
           (unsigned long long)f->cs, (unsigned long long)f->rflags,
           (unsigned long long)f->rsp, (unsigned long long)cr2);
    printf("rax=%llx rbx=%llx rcx=%llx rdx=%llx rsi=%llx rdi=%llx\n",
           (unsigned long long)f->rax, (unsigned long long)f->rbx, (unsigned long long)f->rcx,
           (unsigned long long)f->rdx, (unsigned long long)f->rsi, (unsigned long long)f->rdi);
    for (;;) __asm__ volatile("cli; hlt");
}

extern const uint64_t exc_stub_table[32];
static struct { uint64_t lo, hi; } idt[32] __attribute__((aligned(16)));

static void idt_init(void)
{
    for (int i = 0; i < 32; i++) {
        uint64_t a = exc_stub_table[i];
        idt[i].lo = (a & 0xFFFF) | ((uint64_t)SEL_CODE64 << 16) | (0x8EULL << 40) | ((a & 0xFFFF0000ULL) << 32);
        idt[i].hi = a >> 32;
    }
    struct { uint16_t limit; uint64_t base; } __attribute__((packed)) d = { sizeof idt - 1, (uint64_t)idt };
    __asm__ volatile("lidt %0" : : "m"(d));
}

/* ---- entry ----------------------------------------------------------------- */

static int usable(const struct prism_mmap_entry *m, int n, uint64_t b, uint64_t l)
{
    for (int i = 0; i < n; i++)
        if (m[i].type == PRISM_MEM_USABLE && m[i].base <= b && m[i].base + m[i].length >= b + l) return 1;
    return 0;
}

const char *plat_name(void) { return "bios"; }

void bios_main(struct boot1_info *info)
{
    bios_drive = info->drive;
    thunk = (void (*)(void))info->thunk;
    serial_init();
    idt_init();

    struct bios_regs r = { 0 };
    r.intno = 0x10; r.eax = 0x0003;                   /* 80x25 text mode */
    bios_call(&r);
    console_init(&vga_term);

    heap_init((void *)LOADER_HEAP_BASE, LOADER_HEAP_SIZE);

    struct prism_mmap_entry m[64];
    int n = e820_read(m, 64);
    if (!usable(m, n, 0x100000, LOADER_STACK_TOP - 0x100000))
        console_panic("not enough memory: PrismOS needs at least 40 MiB of RAM");

    if (fs_init() < 0)
        console_panic("cannot mount the boot volume (FAT32 EFI System Partition)");

    loader_main();
    for (;;) __asm__ volatile("hlt");
}
