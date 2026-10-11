/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * Constants shared between the BIOS boot chain (assembly), the loader (C)
 * and the disk image builder.  Only plain #defines live here so the file
 * can be included from assembly and from C alike.
 */

#ifndef PRISM_INC
#define PRISM_INC

/*
 * BIOS boot chain: low memory map (real mode addresses)
 *
 *   0x00000500 - 0x00004FFF   free
 *   0x00005000 - 0x00006FFF   real mode stack used by the BIOS thunk
 *   0x00007000 - 0x000070FF   struct bios_regs exchange area
 *   0x00007100 - 0x00007BFF   scratch (disk address packet, E820 buffer)
 *   0x00007C00 - 0x00007DFF   boot0 (MBR)
 *   0x00007E00 - 0x00009DFF   boot1 (stage 1.5: A20, long mode, BIOS thunk)
 *   0x00010000 - 0x00016FFF   initial page tables (identity map of 4 GiB)
 *   0x00020000 - 0x0002FFFF   bounce buffer for BIOS disk reads
 */
#define BOOT0_ADDR          0x7C00
#define BOOT0_DAP_OFF       0x1A8       /* disk address packet inside boot0 */
#define BOOT0_LBA_OFF       0x1B0       /* qword patched by mkimage: LBA of boot1 */

#define BOOT1_ADDR          0x7E00
#define BOOT1_MAGIC         0x31425250  /* "PRB1" */
#define BOOT1_HDR           16          /* magic, loader lba (2), loader sectors */
#define BOOT1_SECTORS       16          /* 8 KiB reserved for boot1 */
#define BOOT1_INFO_MAGIC    0x464E4231  /* "1BNF" */

#define RM_STACK            0x7000
#define BIOS_REGS           0x7000
#define BIOS_SCRATCH        0x7100
#define PT_BASE             0x10000
#define BOUNCE_ADDR         0x20000
#define BOUNCE_SIZE         0x10000

/* struct bios_regs layout (see boot/bios/bios.h) */
#define REGS_EAX            0x00
#define REGS_EBX            0x04
#define REGS_ECX            0x08
#define REGS_EDX            0x0C
#define REGS_ESI            0x10
#define REGS_EDI            0x14
#define REGS_EBP            0x18
#define REGS_EFLAGS         0x1C
#define REGS_DS             0x20
#define REGS_ES             0x22
#define REGS_INT            0x24

/* GDT selectors shared by boot1 and the loader */
#define SEL_CODE64          0x08
#define SEL_DATA            0x10
#define SEL_CODE32          0x18
#define SEL_CODE16          0x20
#define SEL_DATA16          0x28

/*
 * BIOS boot chain: extended memory map (physical addresses)
 *
 *   0x00100000 - 0x001FFFFF   boot data (kernel page tables, boot info...)
 *   0x00200000 - 0x00FFFFFF   kernel and modules
 *   0x01000000 - 0x013FFFFF   loader image (text, data, bss)
 *   0x01400000 - 0x023FFFFF   loader heap
 *   0x02400000 - 0x024FFFFF   loader stack (grows down from 0x02500000)
 */
#define LOADER_ADDR         0x01000000
#define LOADER_MAX_SIZE     0x00400000
#define LOADER_HEAP_BASE    0x01400000
#define LOADER_HEAP_SIZE    0x01000000
#define LOADER_STACK_TOP    0x02500000

#define BOOTDATA_BASE       0x00100000
#define BOOTDATA_SIZE       0x00100000
#define KERNEL_LOAD_BASE    0x00200000
#define KERNEL_LOAD_LIMIT   0x01000000

/* Virtual address the kernel image must be linked at (top 2 GiB) */
#define KERNBASE            0xFFFFFFFF80000000

#endif /* PRISM_INC */
