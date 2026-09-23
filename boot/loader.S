/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 */

#include <prism.inc>

    .text
    .code16
    .globl _start
_start:
    cli
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $LOADER_ADDR, %sp

    movb %dl, boot_drive

    movw $msg_alive, %si
    call bios_print

    movw $KERNEL_SEG, %ax
    movw %ax, %es
    xorw %bx, %bx
    movb $0x02, %ah
    movb $KERNEL_SECTORS, %al
    movb $0x00, %ch
    movb $0x00, %dh
    movb $KERNEL_SECTOR, %cl
    movb boot_drive, %dl
    int  $0x13
    jc   disk_error
    xorw %ax, %ax
    movw %ax, %es

    movw $msg_kernel_ok, %si
    call bios_print

    call enable_a20

    lgdt gdt_descriptor

    movl %cr0, %eax
    orl  $1, %eax
    movl %eax, %cr0

    ljmp $0x08, $protected_start

disk_error:
    movw $msg_disk_err, %si
    call bios_print
1:  hlt
    jmp  1b

bios_print:
    lodsb
    orb  %al, %al
    jz   2f
    movb $0x0E, %ah
    movb $0x00, %bh
    movb $0x0F, %bl
    int  $0x10
    jmp  bios_print
2:  ret

enable_a20:
    inb  $0x92, %al
    testb $2, %al
    jnz  1f
    orb  $2, %al
    andb $0xFE, %al
    outb %al, $0x92
1:  ret

boot_drive: .byte 0

msg_alive:     .asciz "PrismOS loader: alive, chargement du noyau...\r\n"
msg_kernel_ok: .asciz "Noyau charge, passage en mode protege...\r\n"
msg_disk_err:  .asciz "Erreur disque (noyau) -- halte.\r\n"

    .align 8
gdt_start:
    .quad 0
gdt_code32:
    .word 0xFFFF, 0
    .byte 0, 0x9A, 0xCF, 0
gdt_data32:
    .word 0xFFFF, 0
    .byte 0, 0x92, 0xCF, 0
gdt_end:

gdt_descriptor:
    .word gdt_end - gdt_start - 1
    .long gdt_start

    .code32
protected_start:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs
    movl $0x90000, %esp

    movl $KERNEL_ADDR, %eax
    jmp  *%eax
