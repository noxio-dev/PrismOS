/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * PrismOS boot0 -- real-mode boot sector (MBR), hello world
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
    movw $0x7C00, %sp
    sti

    movw $msg_hello, %si
    call bios_print
    movw $msg_halt, %si
    call bios_print

1:  hlt
    jmp 1b

bios_print:
    lodsb
    orb %al, %al
    jz 2f
    movb $0x0E, %ah
    int $0x10
    jmp bios_print
2:  ret

msg_hello: .asciz "PrismOS boot0: hello, world!\r\n"
msg_halt:  .asciz "No stage2 yet -- halting.\r\n"

    .section .sig, "a", @progbits
    .short 0xAA55