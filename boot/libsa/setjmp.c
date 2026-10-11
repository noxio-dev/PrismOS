/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Copyright (c) 2026 noxio-dev */
#include <setjmp.h>

#if defined(_WIN32)
/* Win64 ABI: callee saved rbx rbp rdi rsi r12-r15 xmm6-xmm15 */
__attribute__((naked)) int setjmp(jmp_buf b)
{
    __asm__ volatile(
        "movq %rbx, 0(%rcx)\n movq %rbp, 8(%rcx)\n movq %r12, 16(%rcx)\n"
        "movq %r13, 24(%rcx)\n movq %r14, 32(%rcx)\n movq %r15, 40(%rcx)\n"
        "movq %rdi, 64(%rcx)\n movq %rsi, 72(%rcx)\n"
        "leaq 8(%rsp), %rdx\n movq %rdx, 48(%rcx)\n"
        "movq (%rsp), %rdx\n movq %rdx, 56(%rcx)\n"
        "movups %xmm6, 96(%rcx)\n movups %xmm7, 112(%rcx)\n movups %xmm8, 128(%rcx)\n"
        "movups %xmm9, 144(%rcx)\n movups %xmm10, 160(%rcx)\n movups %xmm11, 176(%rcx)\n"
        "movups %xmm12, 192(%rcx)\n movups %xmm13, 208(%rcx)\n movups %xmm14, 224(%rcx)\n"
        "movups %xmm15, 240(%rcx)\n"
        "xorl %eax, %eax\n ret\n");
}

__attribute__((naked, noreturn)) void longjmp(jmp_buf b, int v)
{
    __asm__ volatile(
        "movl %edx, %eax\n testl %eax, %eax\n jnz 1f\n movl $1, %eax\n1:\n"
        "movq 0(%rcx), %rbx\n movq 8(%rcx), %rbp\n movq 16(%rcx), %r12\n"
        "movq 24(%rcx), %r13\n movq 32(%rcx), %r14\n movq 40(%rcx), %r15\n"
        "movq 64(%rcx), %rdi\n movq 72(%rcx), %rsi\n"
        "movups 96(%rcx), %xmm6\n movups 112(%rcx), %xmm7\n movups 128(%rcx), %xmm8\n"
        "movups 144(%rcx), %xmm9\n movups 160(%rcx), %xmm10\n movups 176(%rcx), %xmm11\n"
        "movups 192(%rcx), %xmm12\n movups 208(%rcx), %xmm13\n movups 224(%rcx), %xmm14\n"
        "movups 240(%rcx), %xmm15\n"
        "movq 48(%rcx), %rsp\n jmpq *56(%rcx)\n");
}
#else
/* System V AMD64 ABI: callee saved rbx rbp r12-r15 */
__attribute__((naked)) int setjmp(jmp_buf b)
{
    __asm__ volatile(
        "movq %rbx, 0(%rdi)\n movq %rbp, 8(%rdi)\n movq %r12, 16(%rdi)\n"
        "movq %r13, 24(%rdi)\n movq %r14, 32(%rdi)\n movq %r15, 40(%rdi)\n"
        "leaq 8(%rsp), %rdx\n movq %rdx, 48(%rdi)\n"
        "movq (%rsp), %rdx\n movq %rdx, 56(%rdi)\n"
        "xorl %eax, %eax\n ret\n");
}

__attribute__((naked, noreturn)) void longjmp(jmp_buf b, int v)
{
    __asm__ volatile(
        "movl %esi, %eax\n testl %eax, %eax\n jnz 1f\n movl $1, %eax\n1:\n"
        "movq 0(%rdi), %rbx\n movq 8(%rdi), %rbp\n movq 16(%rdi), %r12\n"
        "movq 24(%rdi), %r13\n movq 32(%rdi), %r14\n movq 40(%rdi), %r15\n"
        "movq 48(%rdi), %rsp\n jmpq *56(%rdi)\n");
}
#endif
