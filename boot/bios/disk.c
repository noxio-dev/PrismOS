/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <string.h>
#include <prism.inc>
#include "../loader/fs.h"
#include "../loader/fsback.h"
#include "../loader/platform.h"
#include "bios.h"

/*
 * BIOS block layer and boot volume discovery: the GPT/MBR is scanned for
 * a mountable partition and the fs.h calls are dispatched to the backend
 * that mounted (fat.c or ext4.c).
 *
 * Candidate order: a Linux filesystem partition first (that is where the
 * boot tree lives in the standard three-partition layout: ESP / BIOS boot /
 * ext4 root), then the EFI System Partition (legacy single-volume layouts
 * put the whole boot tree on it), then anything else.  Volumes that are not
 * an ESP must carry the boot tree (/boot/loader.conf or /boot/kernel/kernel)
 * to be picked -- that rules out plain ext4 data disks.
 */

struct dap {
    uint8_t size, zero;
    uint16_t count, off, seg;
    uint64_t lba;
} __attribute__((packed));

static int read_chunk(uint64_t lba, uint32_t count)
{
    for (int tries = 0; tries < 3; tries++) {
        struct dap *d = (struct dap *)BIOS_SCRATCH;
        struct bios_regs r = { 0 };
        d->size = 16; d->zero = 0; d->count = count;
        d->off = BOUNCE_ADDR & 0xF; d->seg = BOUNCE_ADDR >> 4; d->lba = lba;
        r.intno = 0x13; r.eax = 0x4200; r.edx = bios_drive; r.esi = BIOS_SCRATCH;
        bios_call(&r);
        if (!(r.eflags & FLAG_CF)) return 0;
        r.eax = 0; r.edx = bios_drive; r.intno = 0x13;       /* reset and retry */
        bios_call(&r);
    }
    return -1;
}

int blk_read(uint64_t lba, uint32_t count, void *buf)
{
    uint8_t *dst = buf;
    while (count) {
        uint32_t n = count > 64 ? 64 : count;
        if (read_chunk(lba, n) < 0) return -1;
        memcpy(dst, (void *)BOUNCE_ADDR, n * 512);
        dst += n * 512; lba += n; count -= n;
    }
    return 0;
}

/* ---- boot volume discovery -------------------------------------------- */

static const uint8_t esp_guid[16] = {
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11,
    0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
};
static const uint8_t linux_guid[16] = {        /* Linux filesystem / data */
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47,
    0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
};

/* which backend mounted (1 = fat, 2 = ext4) */
static int mounted;

static int try_mount(uint64_t lba)
{
    if (ext4_mount(lba) == 0) { mounted = 2; return 0; }
    if (fat_mount(lba) == 0) { mounted = 1; return 0; }
    return -1;
}

/* does the mounted volume carry the loader boot tree? */
static int have_boot_tree(void)
{
    int isdir;
    uint64_t size;
    if (mounted == 1)
        return fat_stat("/boot/loader.conf", &isdir, &size) == 0 ||
               fat_stat("/boot/kernel/kernel", &isdir, &size) == 0;
    return ext4_stat("/boot/loader.conf", &isdir, &size) == 0 ||
           ext4_stat("/boot/kernel/kernel", &isdir, &size) == 0;
}

/* mount a candidate; an ESP is a boot volume by definition, the rest must
 * prove it with the boot tree */
static int try_candidate(uint64_t lba, int is_esp)
{
    if (try_mount(lba) < 0) return -1;
    return (is_esp || have_boot_tree()) ? 0 : -1;
}

/* candidate partitions, kept in scan order and tried by priority */
#define MAX_CANDS 16
struct cand { uint64_t lba; int prio; };

/* 0 = Linux filesystem (the ext4 root), 1 = EFI System Partition, 2 = anything else */
static int gpt_prio(const uint8_t *type)
{
    int i, zero = 1;
    for (i = 0; i < 16; i++)
        if (type[i]) { zero = 0; break; }
    if (zero) return -1;                        /* not a partition */
    if (!memcmp(type, linux_guid, 16)) return 0;
    if (!memcmp(type, esp_guid, 16)) return 1;
    return 2;
}

static int scan_gpt(void)
{
    uint8_t s[512], e[512];
    if (blk_read(1, 1, s) < 0 || memcmp(s, "EFI PART", 8)) return -1;
    uint64_t elba = *(uint64_t *)(s + 72);
    uint32_t n = *(uint32_t *)(s + 80), esz = *(uint32_t *)(s + 84);
    if (esz < 128 || esz > 512) return -1;
    struct cand c[MAX_CANDS];
    int nc = 0;
    for (uint32_t i = 0; i < n && nc < MAX_CANDS; i++) {
        uint64_t off = (uint64_t)i * esz;
        if (blk_read(elba + off / 512, 1, e) < 0) return -1;
        int prio = gpt_prio(e + off % 512);
        if (prio < 0) continue;
        c[nc].lba = *(uint64_t *)(e + off % 512 + 32);
        c[nc].prio = prio;
        nc++;
    }
    for (int prio = 0; prio < 3; prio++)
        for (int i = 0; i < nc; i++)
            if (c[i].prio == prio && try_candidate(c[i].lba, prio == 1) == 0)
                return 0;
    return -1;
}

static int mbr_prio(uint8_t type)
{
    if (!type) return -1;                                         /* empty */
    if (type == 0x83) return 0;                                   /* Linux */
    if (type == 0x0B || type == 0x0C || type == 0xEF) return 1;   /* FAT32/EFI */
    if (type == 0x05 || type == 0x0F || type == 0xEE) return -1;  /* chained/protective */
    return 2;
}

static int scan_mbr(void)
{
    uint8_t s[512];
    if (blk_read(0, 1, s) < 0 || s[510] != 0x55 || s[511] != 0xAA) return -1;
    for (int prio = 0; prio < 3; prio++)
        for (int i = 0; i < 4; i++) {
            const uint8_t *p = s + 0x1BE + i * 16;
            if (mbr_prio(p[4]) == prio && try_candidate(*(uint32_t *)(p + 8), prio == 1) == 0)
                return 0;
        }
    return -1;
}

int fs_init(void)
{
    if (scan_gpt() == 0) return 0;
    if (scan_mbr() == 0) return 0;
    return try_candidate(0, 0) == 0 ? 0 : -1;    /* bare, unpartitioned volume */
}

/* ---- fs.h dispatch ------------------------------------------------------ */

int fs_stat(const char *path, int *isdir, uint64_t *size)
{
    return mounted == 1 ? fat_stat(path, isdir, size)
                        : ext4_stat(path, isdir, size);
}

int fs_open(const char *path, fs_file **out)
{
    return mounted == 1 ? fat_open(path, out) : ext4_open(path, out);
}

long fs_read(fs_file *f, void *buf, size_t n)
{
    return mounted == 1 ? fat_read(f, buf, n) : ext4_read(f, buf, n);
}

int fs_seek(fs_file *f, uint64_t off)
{
    return mounted == 1 ? fat_seek(f, off) : ext4_seek(f, off);
}

uint64_t fs_size(fs_file *f)
{
    return mounted == 1 ? fat_size(f) : ext4_size(f);
}

void fs_close(fs_file *f)
{
    if (mounted == 1) fat_close(f);
    else ext4_close(f);
}

int fs_readdir(const char *path, fs_dir_cb cb, void *arg)
{
    return mounted == 1 ? fat_readdir(path, cb, arg) : ext4_readdir(path, cb, arg);
}
