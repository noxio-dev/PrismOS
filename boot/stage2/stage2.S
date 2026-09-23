/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * PrismOS stage2 -- loaded at 0x7E00 by boot0
 */

    .text
    .code16
    .globl _start
_start:
    cli                         /* pas d'interruptions pendant qu'on s'installe */
    xorw %ax, %ax
    movw %ax, %ds               /* boot0 a saute avec CS=0, mais on n'est */
    movw %ax, %es               /* pas garanti de DS/ES -- on les remet a 0 */
    movw %ax, %ss
    movw $0x7C00, %sp           /* pile juste sous le secteur de boot */

    movw $msg, %si
    call bios_print

1:  hlt
    jmp 1b

/* --- bios_print : affiche la chaine NUL-terminnee pointee par SI --- */
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

msg: .asciz "PrismOS stage2: alive! PM switch next.\r\n"