/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * The syscalls the C library has no word for (phase 3.2, step 4).
 *
 * Everything else the ported programs need, mlibc wraps: its sysdeps port
 * (ports/mlibc/sysdeps/prismos) speaks the whole 44-call table through the
 * tag layer, so fork / waitpid / sigaction / opendir / tcgetwinsize and
 * the rest are plain library calls now.  These four are different: they
 * are the kernel's own vocabulary, calls POSIX simply does not define --
 * so the mlibc userspace keeps this one small raw-syscall header, and it
 * is the only place the `syscall` instruction appears outside libc.a:
 *
 *   uptime_ms (3)   the ticks since boot -- the test programs time things
 *                   in exactly these; clock_gettime is the POSIX clock,
 *                   and there is no POSIX word for the raw tick counter
 *   yield (4)       hand the CPU to the next process; sched_yield would
 *                   need a syscall the kernel never grew
 *   sleep (5)       milliseconds, bounded at 10 s -- neither sleep(3)'s
 *                   seconds nor nanosleep's timespec
 *   brk (21)        the program break, exact-return; mlibc's allocator
 *                   is mmap-based and its interface carries no brk tag,
 *                   so the brk scenario of /bin/posix speaks it raw
 *
 * The convention is the one the kernel's dispatcher has read since
 * phase 2.1 and the port's own stubs speak (ports/mlibc/sysdeps/prismos/
 * generic/syscall.cpp): rax = number, rdi / rsi / rdx = the arguments,
 * the answer in rax -- for these four, never an error.
 */
#ifndef PRISM_RAW_H
#define PRISM_RAW_H

#include <stddef.h>
#include <stdint.h>

enum {
	PRISM_SYS_UPTIME_MS = 3,
	PRISM_SYS_YIELD = 4,
	PRISM_SYS_SLEEP = 5,
	PRISM_SYS_BRK = 21,
};

static inline long prism_raw3(long n, long a, long b, long c)
{
	long r;
	__asm__ volatile("syscall"
	    : "=a"(r)
	    : "a"(n), "D"(a), "S"(b), "d"(c)
	    : "rcx", "r11", "memory");
	return r;
}

/* The milliseconds since boot. */
static inline long prism_uptime_ms(void)
{
	return prism_raw3(PRISM_SYS_UPTIME_MS, 0, 0, 0);
}

/* Hand the CPU to the next process. */
static inline void prism_yield(void)
{
	prism_raw3(PRISM_SYS_YIELD, 0, 0, 0);
}

/* Sleep for `ms` milliseconds (the kernel bounds it at 10 s). */
static inline void prism_sleep_ms(long ms)
{
	prism_raw3(PRISM_SYS_SLEEP, ms, 0, 0);
}

/*
 * The program break.  An `addr` of 0 (or one below the initial break) is
 * a query that moves nothing; any other address sets it.  The answer is
 * always the (new) current break -- page-aligned by construction -- and
 * the call never fails.
 */
static inline unsigned long prism_brk(unsigned long addr)
{
	return (unsigned long)prism_raw3(PRISM_SYS_BRK, (long)addr, 0, 0);
}

#endif /* PRISM_RAW_H */
