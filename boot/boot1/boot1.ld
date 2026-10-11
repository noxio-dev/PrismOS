/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev */

OUTPUT_FORMAT(binary)
ENTRY(_start)

SECTIONS
{
    . = 0x7E00;

    .text : {
        *(.text .text.*)
        *(.rodata .rodata.*)
        *(.data .data.*)
    }

    . = 0x7E00 + 8192 - 1;
    .pad : { BYTE(0) }

    /DISCARD/ : {
        *(.bss) *(COMMON) *(.note*) *(.comment*) *(.eh_frame*)
    }
}
