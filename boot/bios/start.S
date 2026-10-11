/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev */

    .section .text.start, "ax"
    .globl _start
_start:
    cld
    movq    %rdi, %rbx
    movq    $__bss_start, %rdi
    movq    $__bss_end, %rcx
    subq    %rdi, %rcx
    xorl    %eax, %eax
    rep     stosb
    andq    $-16, %rsp
    movq    %rbx, %rdi
    call    bios_main
1:  cli
    hlt
    jmp     1b

    .text
    .macro  stub n, err
exc_stub_\n:
    .if \err == 0
    pushq   $0
    .endif
    pushq   $\n
    jmp     exc_common
    .endm

    stub 0, 0
    stub 1, 0
    stub 2, 0
    stub 3, 0
    stub 4, 0
    stub 5, 0
    stub 6, 0
    stub 7, 0
    stub 8, 1
    stub 9, 0
    stub 10, 1
    stub 11, 1
    stub 12, 1
    stub 13, 1
    stub 14, 1
    stub 15, 0
    stub 16, 0
    stub 17, 1
    stub 18, 0
    stub 19, 0
    stub 20, 0
    stub 21, 1
    stub 22, 0
    stub 23, 0
    stub 24, 0
    stub 25, 0
    stub 26, 0
    stub 27, 0
    stub 28, 0
    stub 29, 1
    stub 30, 1
    stub 31, 0

exc_common:
    pushq   %rax
    pushq   %rcx
    pushq   %rdx
    pushq   %rbx
    pushq   %rbp
    pushq   %rsi
    pushq   %rdi
    pushq   %r8
    pushq   %r9
    pushq   %r10
    pushq   %r11
    pushq   %r12
    pushq   %r13
    pushq   %r14
    pushq   %r15
    movq    %rsp, %rdi
    andq    $-16, %rsp
    cld
    call    exception_handler
1:  cli
    hlt
    jmp     1b

    .section .rodata
    .globl exc_stub_table
    .balign 8
exc_stub_table:
    .irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
    .quad exc_stub_\n
    .endr
