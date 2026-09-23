/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 */

    .text
    .code32
    .globl kstart
kstart:
    cli
    movl $0x90000, %esp
    movl %esp, %ebp

    call kmain

    cli
1:  hlt
    jmp  1b
