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
        /* SMP real-mode trampoline template (hw/smp.rs): copied to
           physical 0xA000 at boot, never executed in place.  Kept INSIDE
           the rodata range so the kernel's own page tables map it. */
        KEEP(*(.trampoline))
        __rodata_end = .;
    } :rodata

    /* Kernel symbol table (kallsyms): FIXED 128 KiB so that filling it in a
       second build pass never moves any other symbol (see tools/mksyms.py
       and the `kernel` rule in the Makefile). */
    .kallsyms : ALIGN(4K) {
        __kallsyms_start = .;
        KEEP(*(.kallsyms))
        . = __kallsyms_start + 0x20000;
        __kallsyms_end = .;
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
