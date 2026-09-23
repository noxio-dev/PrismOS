/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * PrismOS boot0 -- real-mode boot sector (MBR)
 */

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

    movb %dl, boot_drive

    movw $msg_hello, %si
    call bios_print

    movw $0x7E00, %bx
    movb $0x02, %ah
    movb $0x01, %al
    movb $0x00, %ch
    movb $0x00, %dh
    movb $0x02, %cl
    movb boot_drive, %dl
    int  $0x13
    jc   disk_error

    movw $msg_stage2, %si
    call bios_print

    ljmp $0x0000, $0x7E00

disk_error:
    movw $msg_err, %si
    call bios_print
1:  hlt
    jmp 1b

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

    .data

msg_hello:  .asciz "PrismOS boot0: hello, world!\r\n"
msg_stage2: .asciz "loading stage2...\r\n"
msg_err:    .asciz "DISK ERROR -- halting.\r\n"

boot_drive: .byte 0

    .section .sig, "a", @progbits
    .byte 0x55
    .byte 0xAA