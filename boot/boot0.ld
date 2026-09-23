/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 */

OUTPUT_FORMAT(binary)
OUTPUT_ARCH(i386)

SECTIONS
{
    . = 0x7C00;

    .text : {
        *(.text)
        *(.rodata)
        *(.data)
    }

    . = 0x7C00 + 510;
    .sig : {
        *(.sig)
    }

    /DISCARD/ : {
        *(.bss)
        *(.note*)
        *(.comment*)
    }
}