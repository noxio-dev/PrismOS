/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev */

OUTPUT_FORMAT(binary)
ENTRY(_start)

SECTIONS
{
    . = 0x7C00;

    .text : {
        *(.text .text.*)
        *(.rodata .rodata.*)
        *(.data .data.*)
    }

    . = 0x7C00 + 0x1A8;
    .dap : { *(.dap) }

    . = 0x7C00 + 0x1FE;
    .sig : { *(.sig) }

    /DISCARD/ : {
        *(.bss) *(COMMON) *(.note*) *(.comment*) *(.eh_frame*)
    }
}
