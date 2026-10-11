/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <stdio.h>
#include <string.h>
#include <prism.inc>
#include "../loader/fs.h"
#include "../loader/platform.h"
#include "bios.h"

int fat_mount(uint64_t part_lba);

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

static const uint8_t esp_guid[16] = {
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11,
    0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
};

/* find the EFI System Partition (GPT) or an MBR FAT32/EFI partition */
static int find_data_partition(uint64_t *lba)
{
    uint8_t s[512];
    if (blk_read(1, 1, s) < 0) return -1;
    if (!memcmp(s, "EFI PART", 8)) {
        uint64_t elba = *(uint64_t *)(s + 72);
        uint32_t n = *(uint32_t *)(s + 80), esz = *(uint32_t *)(s + 84);
        uint8_t e[512];
        for (uint32_t i = 0; i < n && esz >= 128 && esz <= 512; i++) {
            uint64_t off = (uint64_t)i * esz;
            if (blk_read(elba + off / 512, 1, e) < 0) return -1;
            const uint8_t *ent = e + off % 512;
            if (!memcmp(ent, esp_guid, 16)) { *lba = *(uint64_t *)(ent + 32); return 0; }
        }
        return -1;
    }
    if (blk_read(0, 1, s) < 0 || s[510] != 0x55 || s[511] != 0xAA) return -1;
    for (int i = 0; i < 4; i++) {
        const uint8_t *p = s + 0x1BE + i * 16;
        if (p[4] == 0x0B || p[4] == 0x0C || p[4] == 0xEF) { *lba = *(uint32_t *)(p + 8); return 0; }
    }
    *lba = 0;                                   /* maybe a bare FAT volume */
    return 0;
}

int fs_init(void)
{
    uint64_t lba;
    if (find_data_partition(&lba) < 0) return -1;
    return fat_mount(lba);
}
