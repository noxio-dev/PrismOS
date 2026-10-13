/*
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * PrismOS kernel image.  The loader reads the program headers, copies each
 * PT_LOAD segment to a 2 MiB aligned physical block and maps it at the
 * virtual addresses below (top 2 GiB of the address space, KERNBASE in
 * include/prism.inc).
 */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

KERNBASE = 0xFFFFFFFF80000000;

PHDRS
{
    text   PT_LOAD FLAGS(5);      /* r-x */
    rodata PT_LOAD FLAGS(4);      /* r-- */
    data   PT_LOAD FLAGS(6);      /* rw- */
}

SECTIONS
{
    . = KERNBASE + 0x200000;

    .text : ALIGN(4K) {
        __text_start = .;
        *(.text._start)
        *(.text .text.*)
        __text_end = .;
    } :text

    .rodata : ALIGN(4K) {
        __rodata_start = .;
        *(.rodata .rodata.*)
        __rodata_end = .;
    } :rodata

    .data : ALIGN(4K) {
        __data_start = .;
        *(.data .data.*)
    } :data

    .bss : ALIGN(4K) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    } :data

    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note*)
        *(.comment*)
    }
}
