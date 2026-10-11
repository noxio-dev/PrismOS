/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev */

#include <prism.inc>

#define CHUNK_SECTORS   64

    .text
    .globl _start
    .code16
_start:
hdr_magic:      .long BOOT1_MAGIC
hdr_lba_lo:     .long 0
hdr_lba_hi:     .long 0
hdr_sectors:    .long 0

/* ------------------------------------------------------------------ */
/* Entry, 16-bit real mode.  DL = boot drive.                          */
/* ------------------------------------------------------------------ */
boot1_entry:
    cli
    xorw    %ax, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movw    $BOOT0_ADDR, %sp
    cld
    movb    %dl, boot_drive
    sti

    movw    $msg_boot1, %si
    call    puts

    call    enable_a20
    jc      a20_fail

    lgdt    gdtr

    /* ---- read the loader into high memory ---- */
    movl    hdr_sectors, %eax
    testl   %eax, %eax
    jz      no_loader
    movl    %eax, remaining
    movl    $LOADER_ADDR, dest
    movl    hdr_lba_lo, %eax
    movl    %eax, dap_lba_lo
    movl    hdr_lba_hi, %eax
    movl    %eax, dap_lba_hi

1:  movl    remaining, %ecx
    testl   %ecx, %ecx
    jz      2f
    cmpl    $CHUNK_SECTORS, %ecx
    jbe     3f
    movl    $CHUNK_SECTORS, %ecx
3:  movl    %ecx, chunk
    movw    %cx, dap_count
    movb    boot_drive, %dl
    movw    $dap, %si
    movb    $0x42, %ah
    int     $0x13
    jc      disk_fail

    movl    chunk, %ecx
    shll    $9, %ecx
    movl    $BOUNCE_ADDR, %esi
    movl    dest, %edi
    call    copy_high

    movl    chunk, %eax
    subl    %eax, remaining
    addl    %eax, dap_lba_lo
    adcl    $0, dap_lba_hi
    shll    $9, %eax
    addl    %eax, dest

    movb    $'.', %al
    call    putc
    jmp     1b

2:  movw    $msg_ok, %si
    call    puts

    /* ---- enter protected mode, then long mode ---- */
    cli
    movl    %cr0, %eax
    orl     $1, %eax
    movl    %eax, %cr0
    ljmp    $SEL_CODE32, $pm32_entry

a20_fail:
    movw    $msg_a20, %si
    jmp     die
disk_fail:
    movw    $msg_disk, %si
    jmp     die
no_loader:
    movw    $msg_noldr, %si
die:
    call    puts
1:  hlt
    jmp     1b

/* ------------------------------------------------------------------ */
/* 16-bit helpers                                                      */
/* ------------------------------------------------------------------ */
puts:
    lodsb
    testb   %al, %al
    jz      1f
    call    putc
    jmp     puts
1:  ret

putc:
    pushw   %bx
    movb    $0x0E, %ah
    movw    $0x0007, %bx
    int     $0x10
    popw    %bx
    ret

/*
 * Copy ECX bytes (multiple of 4) from ESI to EDI anywhere in the 4 GiB
 * address space.  Switches to flat 32-bit protected mode and back.
 */
copy_high:
    cli
    movl    %cr0, %eax
    orl     $1, %eax
    movl    %eax, %cr0
    ljmp    $SEL_CODE32, $1f
    .code32
1:  movw    $SEL_DATA, %ax
    movw    %ax, %ds
    movw    %ax, %es
    cld
    shrl    $2, %ecx
    rep     movsl
    ljmp    $SEL_CODE16, $2f
    .code16
2:  movw    $SEL_DATA16, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movl    %cr0, %eax
    andl    $0xFFFFFFFE, %eax
    movl    %eax, %cr0
    ljmp    $0x0000, $3f
3:  xorw    %ax, %ax
    movw    %ax, %ds
    movw    %ax, %es
    sti
    ret

/* A20 gate: BIOS, fast gate, then keyboard controller.  CF set on failure. */
enable_a20:
    call    a20_check
    jnz     a20_ok
    movw    $0x2401, %ax
    int     $0x15
    call    a20_check
    jnz     a20_ok
    inb     $0x92, %al
    orb     $0x02, %al
    andb    $0xFE, %al
    outb    %al, $0x92
    call    a20_check
    jnz     a20_ok
    call    kbc_wait
    movb    $0xD1, %al
    outb    %al, $0x64
    call    kbc_wait
    movb    $0xDF, %al
    outb    %al, $0x60
    call    kbc_wait
    movw    $0x8000, %cx
1:  call    a20_check
    jnz     a20_ok
    loop    1b
    stc
    ret
a20_ok:
    clc
    ret

kbc_wait:
    movw    $0xFFFF, %cx
1:  inb     $0x64, %al
    testb   $0x02, %al
    jz      2f
    loop    1b
2:  ret

/* ZF = 0 if A20 is enabled (0000:0500 and FFFF:0510 are distinct). */
a20_check:
    pushw   %ds
    pushw   %es
    pushw   %si
    pushw   %di
    xorw    %ax, %ax
    movw    %ax, %ds
    notw    %ax
    movw    %ax, %es
    movw    $0x0500, %si
    movw    $0x0510, %di
    movb    (%si), %al
    pushw   %ax
    movb    %es:(%di), %al
    pushw   %ax
    movb    $0x00, (%si)
    movb    $0xFF, %es:(%di)
    cmpb    $0xFF, (%si)
    popw    %ax
    movb    %al, %es:(%di)
    popw    %ax
    movb    %al, (%si)
    popw    %di
    popw    %si
    popw    %es
    popw    %ds
    ret

/* ------------------------------------------------------------------ */
/* 32-bit protected mode: build page tables, switch to long mode       */
/* ------------------------------------------------------------------ */
    .code32
pm32_entry:
    movw    $SEL_DATA, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movw    %ax, %fs
    movw    %ax, %gs
    movl    $BOOT0_ADDR, %esp
    cld

    /* PML4, PDPT and 4 page directories, 6 pages in all */
    movl    $PT_BASE, %edi
    xorl    %eax, %eax
    movl    $(6 * 4096 / 4), %ecx
    rep     stosl

    movl    $(PT_BASE + 0x1000 + 3), PT_BASE            /* PML4[0] -> PDPT  */
    movl    $(PT_BASE + 0x2000 + 3), PT_BASE + 0x1000   /* PDPT[0] -> PD0   */
    movl    $(PT_BASE + 0x3000 + 3), PT_BASE + 0x1008   /* PDPT[1] -> PD1   */
    movl    $(PT_BASE + 0x4000 + 3), PT_BASE + 0x1010   /* PDPT[2] -> PD2   */
    movl    $(PT_BASE + 0x5000 + 3), PT_BASE + 0x1018   /* PDPT[3] -> PD3   */

    /* 2048 x 2 MiB pages: identity map of 0 - 4 GiB (present|rw|ps) */
    movl    $(PT_BASE + 0x2000), %edi
    movl    $0x83, %eax
    movl    $2048, %ecx
1:  movl    %eax, (%edi)
    addl    $0x200000, %eax
    addl    $8, %edi
    decl    %ecx
    jnz     1b

    movl    %cr4, %eax
    orl     $0x620, %eax            /* PAE | OSFXSR | OSXMMEXCPT */
    movl    %eax, %cr4
    movl    $PT_BASE, %eax
    movl    %eax, %cr3
    movl    $0xC0000080, %ecx       /* EFER */
    rdmsr
    orl     $0x100, %eax            /* LME */
    wrmsr
    movl    %cr0, %eax
    andl    $0xFFFFFFF3, %eax       /* clear EM, TS */
    orl     $0x80000023, %eax       /* PG | NE | MP | PE */
    movl    %eax, %cr0
    ljmp    $SEL_CODE64, $long_entry

/* ------------------------------------------------------------------ */
/* 64-bit: call the loader (rdi = struct boot1_info *)                 */
/* ------------------------------------------------------------------ */
    .code64
long_entry:
    movl    $SEL_DATA, %eax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movw    %ax, %fs
    movw    %ax, %gs
    movq    $LOADER_STACK_TOP, %rsp
    cld
    leaq    boot1_info(%rip), %rdi
    movq    $LOADER_ADDR, %rax
    callq   *%rax
1:  cli
    hlt
    jmp     1b

/* ------------------------------------------------------------------ */
/* bios_call_thunk: void thunk(void), operates on struct bios_regs at  */
/* BIOS_REGS.  Callable from 64-bit code (SysV ABI).                   */
/* ------------------------------------------------------------------ */
    .globl bios_call_thunk
bios_call_thunk:
    pushq   %rbx
    pushq   %rbp
    pushq   %r12
    pushq   %r13
    pushq   %r14
    pushq   %r15
    movq    %rsp, saved_rsp(%rip)
    sidt    saved_idtr(%rip)
    cli
    pushq   $SEL_CODE32
    leaq    thunk32(%rip), %rax
    pushq   %rax
    lretq

    .code32
thunk32:
    movw    $SEL_DATA, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movw    %ax, %fs
    movw    %ax, %gs
    movl    $BOOT0_ADDR, %esp
    movl    %cr0, %eax
    andl    $0x7FFFFFFF, %eax       /* paging off: leaves long mode */
    movl    %eax, %cr0
    movl    $0xC0000080, %ecx
    rdmsr
    andl    $0xFFFFFEFF, %eax       /* LME off */
    wrmsr
    movl    %cr4, %eax
    andl    $0xFFFFFFDF, %eax       /* PAE off */
    movl    %eax, %cr4
    ljmp    $SEL_CODE16, $thunk16

    .code16
thunk16:
    movw    $SEL_DATA16, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movw    %ax, %fs
    movw    %ax, %gs
    movl    %cr0, %eax
    andl    $0xFFFFFFFE, %eax      /* PE off: real mode */
    movl    %eax, %cr0
    ljmp    $0x0000, $thunk_real

thunk_real:
    xorw    %ax, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movw    %ax, %fs
    movw    %ax, %gs
    movw    $RM_STACK, %sp
    lidt    rm_idtr

    movb    BIOS_REGS + REGS_INT, %al
    movb    %al, patch_int + 1          /* self-modifying: INT n */
    movw    BIOS_REGS + REGS_ES, %ax
    movw    %ax, %es
    movl    BIOS_REGS + REGS_EBX, %ebx
    movl    BIOS_REGS + REGS_ECX, %ecx
    movl    BIOS_REGS + REGS_EDX, %edx
    movl    BIOS_REGS + REGS_ESI, %esi
    movl    BIOS_REGS + REGS_EDI, %edi
    movl    BIOS_REGS + REGS_EBP, %ebp
    pushl   BIOS_REGS + REGS_EAX
    movw    BIOS_REGS + REGS_DS, %ds
    popl    %eax
    sti
patch_int:
    int     $0
    pushfl
    movl    %eax, %ss:BIOS_REGS + REGS_EAX
    movl    %ebx, %ss:BIOS_REGS + REGS_EBX
    movl    %ecx, %ss:BIOS_REGS + REGS_ECX
    movl    %edx, %ss:BIOS_REGS + REGS_EDX
    movl    %esi, %ss:BIOS_REGS + REGS_ESI
    movl    %edi, %ss:BIOS_REGS + REGS_EDI
    movl    %ebp, %ss:BIOS_REGS + REGS_EBP
    popl    %eax
    movl    %eax, %ss:BIOS_REGS + REGS_EFLAGS
    movw    %ds, %ax
    movw    %ax, %ss:BIOS_REGS + REGS_DS
    movw    %es, %ax
    movw    %ax, %ss:BIOS_REGS + REGS_ES
    cli
    xorw    %ax, %ax
    movw    %ax, %ds
    movw    %ax, %es
    lgdt    gdtr
    movl    %cr0, %eax
    orl     $1, %eax
    movl    %eax, %cr0
    ljmp    $SEL_CODE32, $thunk_back32

    .code32
thunk_back32:
    movw    $SEL_DATA, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movw    %ax, %fs
    movw    %ax, %gs
    movl    $BOOT0_ADDR, %esp
    movl    %cr4, %eax
    orl     $0x20, %eax             /* PAE */
    movl    %eax, %cr4
    movl    $PT_BASE, %eax
    movl    %eax, %cr3
    movl    $0xC0000080, %ecx
    rdmsr
    orl     $0x100, %eax            /* LME */
    wrmsr
    movl    %cr0, %eax
    orl     $0x80000000, %eax       /* PG */
    movl    %eax, %cr0
    ljmp    $SEL_CODE64, $thunk_back64

    .code64
thunk_back64:
    movl    $SEL_DATA, %eax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %ss
    movw    %ax, %fs
    movw    %ax, %gs
    cld
    lidt    saved_idtr(%rip)
    movq    saved_rsp(%rip), %rsp
    popq    %r15
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %rbp
    popq    %rbx
    retq

/* ------------------------------------------------------------------ */
/* Data                                                                */
/* ------------------------------------------------------------------ */
    .align 8
gdt:
    .quad   0x0000000000000000
    .quad   0x00AF9A000000FFFF      /* 0x08 code64 */
    .quad   0x00CF92000000FFFF      /* 0x10 data (flat 4 GiB) */
    .quad   0x00CF9A000000FFFF      /* 0x18 code32 (flat 4 GiB) */
    .quad   0x00009A000000FFFF      /* 0x20 code16 (64 KiB) */
    .quad   0x000092000000FFFF      /* 0x28 data16 (64 KiB) */
gdt_end:

gdtr:
    .word   gdt_end - gdt - 1
    .long   gdt
    .word   0

rm_idtr:
    .word   0x03FF
    .long   0
    .word   0

    .align 8
saved_idtr: .quad 0, 0
saved_rsp:  .quad 0

/* handed to the loader in %rdi */
    .align 8
boot1_info:
    .long   BOOT1_INFO_MAGIC
boot_drive: .long 0
    .quad   bios_call_thunk

remaining:  .long 0
dest:       .long 0
chunk:      .long 0

dap:
    .byte   0x10, 0
dap_count:  .word 0
    .word   0, (BOUNCE_ADDR >> 4)
dap_lba_lo: .long 0
dap_lba_hi: .long 0

msg_boot1:  .asciz "boot1 "
msg_ok:     .asciz " ok\r\n"
msg_a20:    .asciz "A20 failed"
msg_disk:   .asciz "disk error"
msg_noldr:  .asciz "no loader"
