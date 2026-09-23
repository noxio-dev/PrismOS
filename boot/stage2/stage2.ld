/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * PrismOS stage2 linker script -- flat binary loaded at 0x7E00.
 */

OUTPUT_FORMAT(binary)
OUTPUT_ARCH(i386)

SECTIONS
{
    . = 0x7E00;

    .text : {
        *(.text)
        *(.rodata)
        *(.data)
    }

    /DISCARD/ : {
        *(.bss)
        *(COMMON)
        *(.note*)
        *(.comment*)
    }
}