/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <prism.inc>
#include "platform.h"
#include "fs.h"

#define MiB2        0x200000ULL
#define PAGE        4096ULL

struct elf64_hdr {
    uint8_t  ident[16];
    uint16_t type, machine;
    uint32_t version;
    uint64_t entry, phoff, shoff;
    uint32_t flags;
    uint16_t ehsize, phentsize, phnum, shentsize, shnum, shstrndx;
};

struct elf64_phdr {
    uint32_t type, flags;
    uint64_t offset, vaddr, paddr, filesz, memsz, align;
};

#define PT_LOAD     1
#define EM_X86_64   62

static struct loaded_kernel kern;
#define MAX_MODULES 16
static struct prism_module mods[MAX_MODULES];
static int nmods;

static uint64_t align_up(uint64_t v, uint64_t a) { return (v + a - 1) & ~(a - 1); }

const struct loaded_kernel *kernel_info(void) { return &kern; }

static int read_at(fs_file *f, uint64_t off, void *buf, size_t n)
{
    if (fs_seek(f, off) < 0) return -1;
    return fs_read(f, buf, n) == (long)n ? 0 : -1;
}

int kernel_load(const char *path, const char **err)
{
    static char msg[96];
    fs_file *f;
    struct elf64_hdr eh;
    struct elf64_phdr *ph = NULL;
    int r = fs_open(path, &f);

    kern.valid = 0;
    if (r < 0) { snprintf(msg, sizeof msg, "cannot open %s: %s", path, strerror(-r)); *err = msg; return -1; }
    if (read_at(f, 0, &eh, sizeof eh) < 0) { *err = "cannot read ELF header"; goto fail; }
    if (memcmp(eh.ident, "\177ELF", 4) || eh.ident[4] != 2 || eh.ident[5] != 1 ||
        eh.machine != EM_X86_64 || eh.type != 2) {
        *err = "not an x86-64 ELF64 executable";
        goto fail;
    }
    if (eh.phentsize != sizeof *ph || !eh.phnum || eh.phnum > 64) { *err = "bad program header table"; goto fail; }
    ph = malloc(eh.phnum * sizeof *ph);
    if (!ph || read_at(f, eh.phoff, ph, eh.phnum * sizeof *ph) < 0) { *err = "cannot read program headers"; goto fail; }

    uint64_t vmin = ~0ULL, vmax = 0;
    for (int i = 0; i < eh.phnum; i++) {
        if (ph[i].type != PT_LOAD || !ph[i].memsz) continue;
        if (ph[i].vaddr < vmin) vmin = ph[i].vaddr;
        if (ph[i].vaddr + ph[i].memsz > vmax) vmax = ph[i].vaddr + ph[i].memsz;
    }
    if (vmin == ~0ULL) { *err = "kernel has no loadable segment"; goto fail; }
    if (vmin < KERNBASE || vmax > KERNBASE + (1ULL << 30)) {
        *err = "kernel must be linked in the top 2 GiB (KERNBASE)";
        goto fail;
    }
    uint64_t vbase = vmin & ~(MiB2 - 1);
    uint64_t size = align_up(vmax - vbase, MiB2);
    uint64_t phys;
    if (plat_alloc(PLAT_ALLOC_KERNEL, size, MiB2, &phys) < 0) { *err = "out of memory for the kernel"; goto fail; }
    memset((void *)phys, 0, size);

    for (int i = 0; i < eh.phnum; i++) {
        if (ph[i].type != PT_LOAD || !ph[i].filesz) continue;
        if (read_at(f, ph[i].offset, (void *)(phys + (ph[i].vaddr - vbase)), ph[i].filesz) < 0) {
            *err = "short read while loading the kernel";
            goto fail;
        }
    }
    if (eh.entry < vmin || eh.entry >= vmax) { *err = "kernel entry point outside the image"; goto fail; }

    kern.valid = 1;
    snprintf(kern.path, sizeof kern.path, "%s", path);
    kern.phys = phys; kern.virt = vbase; kern.size = size; kern.entry = eh.entry;
    free(ph);
    fs_close(f);
    return 0;
fail:
    free(ph);
    fs_close(f);
    return -1;
}

void module_clear(void) { nmods = 0; }

int module_load(const char *path, const char *type, const char *name, const char **err)
{
    static char msg[96];
    fs_file *f;
    int r = fs_open(path, &f);
    if (r < 0) { snprintf(msg, sizeof msg, "cannot open %s: %s", path, strerror(-r)); *err = msg; return -1; }
    if (nmods >= MAX_MODULES) { fs_close(f); *err = "too many modules"; return -1; }
    uint64_t size = fs_size(f), phys;
    if (plat_alloc(PLAT_ALLOC_KERNEL, align_up(size ? size : 1, PAGE), PAGE, &phys) < 0) {
        fs_close(f); *err = "out of memory for the module"; return -1;
    }
    if (fs_read(f, (void *)phys, size) != (long)size) { fs_close(f); *err = "short read"; return -1; }
    fs_close(f);
    struct prism_module *m = &mods[nmods++];
    memset(m, 0, sizeof *m);
    m->phys = phys; m->size = size;
    snprintf(m->name, sizeof m->name, "%s", name);
    snprintf(m->type, sizeof m->type, "%s", type);
    return 0;
}

/* ---- memory map ------------------------------------------------------ */

static void sort_map(struct prism_mmap_entry *m, int n)
{
    for (int i = 1; i < n; i++) {
        struct prism_mmap_entry t = m[i];
        int j = i - 1;
        while (j >= 0 && m[j].base > t.base) { m[j + 1] = m[j]; j--; }
        m[j + 1] = t;
    }
}

int mmap_carve(struct prism_mmap_entry *m, int n, int max, uint64_t b, uint64_t l, uint32_t type)
{
    uint64_t e = b + l;
    struct prism_mmap_entry *t = malloc(max * sizeof *t);
    int o = 0;
    if (!t) return n;
    for (int i = 0; i < n; i++) {
        uint64_t mb = m[i].base, me = m[i].base + m[i].length;
        if (me <= b || mb >= e) { if (o < max) t[o++] = m[i]; continue; }
        if (mb < b && o < max) { t[o] = m[i]; t[o].length = b - mb; o++; }
        if (me > e && o < max) { t[o] = m[i]; t[o].base = e; t[o].length = me - e; o++; }
    }
    if (o < max) { t[o].base = b; t[o].length = l; t[o].type = type; t[o].reserved = 0; o++; }
    sort_map(t, o);
    memcpy(m, t, o * sizeof *t);
    free(t);
    return o;
}

static int merge_map(struct prism_mmap_entry *m, int n)
{
    int o = 0;
    for (int i = 0; i < n; i++) {
        if (!m[i].length) continue;
        if (o && m[o - 1].type == m[i].type && m[o - 1].base + m[o - 1].length == m[i].base)
            m[o - 1].length += m[i].length;
        else
            m[o++] = m[i];
    }
    return o;
}

/* ---- trampoline ------------------------------------------------------- */

/*
 * Position independent stub copied into boot data (identity mapped in both
 * the old and the new page tables).  In: rax = PML4, rdi = boot info,
 * rsi = kernel entry, rdx = stack top.  Loads a minimal GDT, switches CR3,
 * reloads CS/SS and jumps to the kernel.
 */
extern const char tramp_start[], tramp_end[];
__asm__(
    ".text\n"
    ".globl tramp_start\n"
    "tramp_start:\n"
    "   cli\n"
    "   leaq .Ltgdt(%rip), %rcx\n"
    "   movq %rcx, .Ltgdtr+2(%rip)\n"
    "   lgdt .Ltgdtr(%rip)\n"
    "   movq %rax, %cr3\n"
    "   movq %rdx, %rsp\n"
    "   pushq $0x08\n"
    "   leaq .Ltcs(%rip), %rcx\n"
    "   pushq %rcx\n"
    "   lretq\n"
    ".Ltcs:\n"
    "   movl $0x10, %ecx\n"
    "   movw %cx, %ds\n"
    "   movw %cx, %es\n"
    "   movw %cx, %ss\n"
    "   movw %cx, %fs\n"
    "   movw %cx, %gs\n"
    "   xorl %ebp, %ebp\n"
    "   pushq $0\n"             /* fake return address: ABI alignment */
    "   jmpq *%rsi\n"
    "   .balign 16\n"
    ".Ltgdt:\n"
    "   .quad 0\n"
    "   .quad 0x00AF9A000000FFFF\n"
    "   .quad 0x00CF92000000FFFF\n"
    ".Ltgdtr:\n"
    "   .word 23\n"
    "   .quad 0\n"
    "   .globl tramp_end\n"
    "tramp_end:\n"
);

/* ---- hand-off ---------------------------------------------------------- */

#define BD_SIZE     0x40000
#define BD_BOOTINFO 0x0000
#define BD_CMDLINE  0x1000
#define BD_MODULES  0x2000
#define BD_TRAMP    0x3000
#define BD_ENV      0x4000
#define BD_ENV_MAX  0x8000
#define BD_PML4     0xC000
#define BD_PDPT_LO  0xD000
#define BD_PD_LO    0xE000          /* 4 pages */
#define BD_PDPT_HI  0x12000
#define BD_PD_K     0x13000
#define BD_PD_X     0x14000         /* up to 8 extra identity PDs */
#define BD_MMAP     0x20000
#define BD_STACK    0x30000
#define MMAP_MAX    (0x10000 / sizeof(struct prism_mmap_entry))

struct env_ctx { char *p, *end; };

static void env_pack(const char *n, const char *v, void *arg)
{
    struct env_ctx *c = arg;
    size_t l = strlen(n) + strlen(v) + 2;
    if (c->p + l + 1 > c->end) return;
    c->p += snprintf(c->p, l, "%s=%s", n, v) + 1;
}

static int extra_pds;

static void map_identity_gib(uint64_t bd, uint64_t gib)
{
    uint64_t *pdpt = (uint64_t *)(bd + BD_PDPT_LO);
    if (gib < 4 || gib >= 512 || (pdpt[gib] & 1) || extra_pds >= 8) return;
    uint64_t pdp = bd + BD_PD_X + (uint64_t)extra_pds++ * PAGE;
    uint64_t *pd = (uint64_t *)pdp;
    for (int i = 0; i < 512; i++) pd[i] = ((gib << 30) + ((uint64_t)i << 21)) | 0x83;
    pdpt[gib] = pdp | 3;
}

static void build_tables(uint64_t bd, struct prism_bootinfo *bi)
{
    uint64_t *pml4 = (uint64_t *)(bd + BD_PML4), *pdpt = (uint64_t *)(bd + BD_PDPT_LO);
    uint64_t *pdl = (uint64_t *)(bd + BD_PD_LO), *pdh = (uint64_t *)(bd + BD_PDPT_HI);
    uint64_t *pdk = (uint64_t *)(bd + BD_PD_K);

    pml4[0] = (bd + BD_PDPT_LO) | 3;
    pml4[511] = (bd + BD_PDPT_HI) | 3;
    for (int g = 0; g < 4; g++) pdpt[g] = (bd + BD_PD_LO + g * PAGE) | 3;
    for (uint64_t i = 0; i < 2048; i++) pdl[i] = (i << 21) | 0x83;

    pdh[510] = (bd + BD_PD_K) | 3;
    for (uint64_t off = 0; off < kern.size; off += MiB2) {
        uint64_t v = kern.virt + off;
        pdk[(v - KERNBASE) >> 21] = (kern.phys + off) | 0x83;
    }

    extra_pds = 0;
    uint64_t ptrs[4] = { bi->rsdp_phys, bi->smbios_phys, bi->efi_system_table, bi->fb.addr };
    for (int i = 0; i < 4; i++) if (ptrs[i]) map_identity_gib(bd, ptrs[i] >> 30);
    if (bi->fb.addr) map_identity_gib(bd, (bi->fb.addr + (uint64_t)bi->fb.pitch * bi->fb.height) >> 30);
    bi->pml4_phys = bd + BD_PML4;
}

int boot_kernel(const char *cmdline, const char **err)
{
    uint64_t bd;
    if (!kern.valid) { *err = "no kernel loaded"; return -1; }
    if (plat_alloc(PLAT_ALLOC_BOOTDATA, BD_SIZE, PAGE, &bd) < 0) { *err = "out of boot data memory"; return -1; }
    memset((void *)bd, 0, BD_SIZE);

    struct prism_bootinfo *bi = (struct prism_bootinfo *)(bd + BD_BOOTINFO);
    bi->magic = PRISM_BOOT_MAGIC;
    bi->version = PRISM_BOOT_VERSION;
    bi->size = sizeof *bi;
    bi->kernel_phys = kern.phys;
    bi->kernel_virt = kern.virt;
    bi->kernel_size = kern.size;

    snprintf((char *)(bd + BD_CMDLINE), PAGE, "%s", cmdline ? cmdline : "");
    bi->cmdline_phys = bd + BD_CMDLINE;

    struct env_ctx ec = { (char *)(bd + BD_ENV), (char *)(bd + BD_ENV) + BD_ENV_MAX - 1 };
    env_each(env_pack, &ec);
    *ec.p = 0;
    bi->env_phys = bd + BD_ENV;

    memcpy((void *)(bd + BD_MODULES), mods, nmods * sizeof mods[0]);
    bi->modules_phys = bd + BD_MODULES;
    bi->module_count = nmods;
    bi->module_entry_size = sizeof(struct prism_module);

    plat_fill_bootinfo(bi);
    build_tables(bd, bi);

    memcpy((void *)(bd + BD_TRAMP), tramp_start, tramp_end - tramp_start);

    struct prism_mmap_entry *map = (struct prism_mmap_entry *)(bd + BD_MMAP);
    int n = plat_exit(map, MMAP_MAX);
    if (n <= 0) { *err = "cannot obtain the memory map"; return -1; }   /* firmware still alive */

    n = mmap_carve(map, n, MMAP_MAX, bd, BD_SIZE, PRISM_MEM_LOADER);
    n = mmap_carve(map, n, MMAP_MAX, kern.phys, kern.size, PRISM_MEM_KERNEL);
    for (int i = 0; i < nmods; i++)
        n = mmap_carve(map, n, MMAP_MAX, mods[i].phys, align_up(mods[i].size ? mods[i].size : 1, PAGE), PRISM_MEM_KERNEL);
    n = merge_map(map, n);
    bi->mmap_phys = bd + BD_MMAP;
    bi->mmap_count = n;
    bi->mmap_entry_size = sizeof *map;

    __asm__ volatile("jmpq *%%rcx"
                     : : "a"(bi->pml4_phys), "D"((uint64_t)bi), "S"(kern.entry),
                         "d"(bd + BD_STACK + 0x10000), "c"(bd + BD_TRAMP)
                     : "memory");
    __builtin_unreachable();
}
