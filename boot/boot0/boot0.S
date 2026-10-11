/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev */

#include <prism.inc>

    .text
    .code16
    .globl _start
_start:
    cli
    xorw    %ax, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movw    $BOOT0_ADDR, %sp
    sti
    cld
    movb    %dl, drive

    movw    $msg_boot, %si
    call    puts

    /* INT 13h extensions are mandatory */
    movb    $0x41, %ah
    movw    $0x55AA, %bx
    int     $0x13
    jc      fail
    cmpw    $0xAA55, %bx
    jne     fail
    testb   $1, %cl
    jz      fail

    movw    $dap, %si
    movb    $0x42, %ah
    movb    drive, %dl
    int     $0x13
    jc      fail

    cmpl    $BOOT1_MAGIC, BOOT1_ADDR
    jne     fail

    movb    drive, %dl
    ljmp    $0x0000, $(BOOT1_ADDR + BOOT1_HDR)

fail:
    movw    $msg_fail, %si
    call    puts
1:  hlt
    jmp     1b

puts:
    lodsb
    testb   %al, %al
    jz      1f
    movb    $0x0E, %ah
    movw    $0x0007, %bx
    int     $0x10
    jmp     puts
1:  ret

drive:      .byte 0
msg_boot:   .asciz "boot0 "
msg_fail:   .asciz "ERR"

/* Disk address packet; the LBA (qword at +8) is patched by mkimage. */
    .section .dap, "a", @progbits
dap:
    .byte   0x10, 0
    .word   BOOT1_SECTORS
    .word   BOOT1_ADDR, 0
    .quad   0

    .section .sig, "a", @progbits
    .byte   0x55, 0xAA
