/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 */

    .text
    .code16
    .globl _start
_start:
    cli
    xorw %ax, %ax
    movw %ax, %DS
    movw %ax, %es
    movw %ax, %ss
    movw $0x7C00, %sp

    movw $msg, %si
    call bios_print

    call enable_a20
    call setup_paging

    lgdt gdt_descriptor

    movl %cr4, %eax
    orl  $0x20, %eax
    movl %eax, %cr4

    movl $0x1000, %eax
    movl %eax, %cr3

    movl $0xC0000080, %ecx
    rdmsr
    orl  $0x100, %eax
    wrmsr

    movl %cr0, %eax
    orl  $0x80000001, %eax
    movl %eax, %cr0

    ljmp $0x08, $long_mode_entry

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

enable_a20:
    inb  $0x92, %al
    testb $2, %al
    jnz  1f
    orb  $2, %al
    andb $0xFE, %al
    outb %al, $0x92
1:  ret

setup_paging:
    movw $0x1000, %di
    movl $3072, %ecx
    xorl %eax, %eax
    cld
    rep  stosl

    movw $0x1000, %di
    movl $0x2003, (%di)

    movw $0x2000, %di
    movl $0x3003, (%di)

    movw $0x3000, %di
    xorl %ebx, %ebx
3:
    movl %ebx, %eax
    shll $21, %eax
    orl  $0x83, %eax
    movl %eax, (%di)
    addw $8, %di
    incl %ebx
    cmpl $512, %ebx
    jl   3b
    ret

    .align 8
gdt_start:
    .quad 0
gdt_code64:
    .quad (1<<43) | (1<<44) | (1<<47) | (1<<53)
gdt_end:

gdt_descriptor:
    .word gdt_end - gdt_start - 1
    .long gdt_start

msg: .asciz "PrismOS stage2: alive! Passage en long mode...\r\n"

    .code64
long_mode_entry:
    movq $0x90000, %rsp

    leaq msg64(%rip), %rsi
    movq $0xB8000, %rdi
1:
    movb (%rsi), %al
    testb %al, %al
    jz   2f
    movb %al, (%rdi)
    movb $0x0F, 1(%rdi)
    addq $2, %rdi
    incq %rsi
    jmp  1b
2:
    cli
3:  hlt
    jmp  3b

msg64: .asciz "Hello from long mode (x86_64)! stage2 OK."