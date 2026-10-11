/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
/*
 * PrismOS boot protocol: what the loader hands to the kernel.
 *
 * The kernel is an ELF64 executable linked in the top 2 GiB of the address
 * space (KERNBASE).  The loader enters it in 64-bit long mode with:
 *
 *   rdi = physical address of struct prism_bootinfo
 *   cs  = 0x08 (64-bit code), ds/es/ss = 0x10, interrupts disabled
 *   paging enabled: identity map of the first 4 GiB (2 MiB pages) plus the
 *     kernel image mapped at its link address
 *   rsp = a 64 KiB stack inside boot data (reclaimable memory)
 *
 * Every pointer in these structures is a physical address; all of them are
 * reachable through the identity map.  Memory typed PRISM_MEM_LOADER holds
 * the boot info, page tables and stack and may be reclaimed once the kernel
 * has copied what it needs and switched to its own page tables.
 *
 * The Rust mirror of these definitions is sys/kern/src/boot.rs.
 */
#ifndef PRISM_BOOT_H
#define PRISM_BOOT_H

#include <stdint.h>

#define PRISM_BOOT_MAGIC    0x4F4F424D53495250ULL      /* "PRISMBOO" */
#define PRISM_BOOT_VERSION  1

#define PRISM_FW_BIOS       1
#define PRISM_FW_UEFI       2

#define PRISM_FLAG_FB       (1u << 0)   /* linear framebuffer valid */
#define PRISM_FLAG_VGATEXT  (1u << 1)   /* VGA text mode at 0xB8000 */

#define PRISM_MEM_USABLE    1
#define PRISM_MEM_RESERVED  2
#define PRISM_MEM_ACPI_RECL 3
#define PRISM_MEM_ACPI_NVS  4
#define PRISM_MEM_BAD       5
#define PRISM_MEM_LOADER    6           /* reclaimable: loader, boot data */
#define PRISM_MEM_KERNEL    7           /* kernel image and modules */

struct prism_mmap_entry {
    uint64_t base;
    uint64_t length;
    uint32_t type;
    uint32_t reserved;
};

struct prism_module {
    uint64_t phys;
    uint64_t size;
    char     name[32];
    char     type[16];
};

struct prism_framebuffer {
    uint64_t addr;
    uint32_t width;
    uint32_t height;
    uint32_t pitch;                     /* bytes per scan line */
    uint32_t bpp;
    uint8_t  red_shift, red_bits;
    uint8_t  green_shift, green_bits;
    uint8_t  blue_shift, blue_bits;
    uint16_t reserved;
};

struct prism_bootinfo {
    uint64_t magic;
    uint32_t version;
    uint32_t size;                      /* sizeof(struct prism_bootinfo) */
    uint32_t firmware;                  /* PRISM_FW_* */
    uint32_t flags;                     /* PRISM_FLAG_* */
    uint64_t kernel_phys;               /* physical base of the kernel image */
    uint64_t kernel_virt;               /* virtual base it is mapped at */
    uint64_t kernel_size;
    uint64_t mmap_phys;                 /* struct prism_mmap_entry[] */
    uint32_t mmap_count;
    uint32_t mmap_entry_size;
    uint64_t modules_phys;              /* struct prism_module[] */
    uint32_t module_count;
    uint32_t module_entry_size;
    uint64_t cmdline_phys;              /* NUL terminated kernel arguments */
    uint64_t env_phys;                  /* "name=value\0...\0\0" */
    uint64_t rsdp_phys;                 /* ACPI RSDP, 0 if none */
    uint64_t smbios_phys;               /* SMBIOS entry point, 0 if none */
    uint64_t efi_system_table;          /* EFI system table, 0 on BIOS */
    uint64_t pml4_phys;                 /* page table root in use */
    struct prism_framebuffer fb;
};

#endif
