/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#ifndef BIOS_H
#define BIOS_H

#include <stdint.h>
#include <prism/boot.h>

/* Layout mirrors REGS_* in include/prism.inc */
struct bios_regs {
    uint32_t eax, ebx, ecx, edx, esi, edi, ebp, eflags;
    uint16_t ds, es;
    uint8_t  intno;
    uint8_t  pad[3];
};

#define FLAG_CF 0x0001
#define FLAG_ZF 0x0040

/* handed over by boot1 */
struct boot1_info {
    uint32_t magic;
    uint32_t drive;
    uint64_t thunk;
};

void bios_call(struct bios_regs *r);
extern uint32_t bios_drive;

int  blk_read(uint64_t lba, uint32_t count, void *buf);
int  bios_disk_init(void);
int  e820_read(struct prism_mmap_entry *m, int max);

#endif
