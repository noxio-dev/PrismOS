/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev */
OUTPUT_FORMAT(binary)
ENTRY(_start)

SECTIONS
{
    . = 0x01000000;

    .text : {
        *(.text.start)
        *(.text .text.*)
    }
    .rodata : ALIGN(16) { *(.rodata .rodata.*) }
    .data : ALIGN(16) { *(.data .data.*) }
    .bss : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*) *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }
    __end = .;

    /DISCARD/ : { *(.note*) *(.comment*) *(.eh_frame*) *(.got*) }
}
