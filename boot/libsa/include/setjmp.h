/* SPDX-License-Identifier: GPL-3.0-or-later */
#ifndef _LIBSA_SETJMP_H
#define _LIBSA_SETJMP_H
/* rbx rbp r12-r15 rsp rip rdi rsi + xmm6-xmm15 (Win64 only) */
typedef unsigned long long jmp_buf[32];
int setjmp(jmp_buf) __attribute__((returns_twice));
void longjmp(jmp_buf, int) __attribute__((noreturn));
#endif
