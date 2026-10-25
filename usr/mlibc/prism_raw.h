/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * The syscalls the C library has no word for (phase 3.2, step 4).
 *
 * Everything else the ported programs need, mlibc wraps: its sysdeps port
 * (ports/mlibc/sysdeps/prismos) speaks the whole 44-call table through the
 * tag layer, so fork / waitpid / sigaction / opendir / tcgetwinsize and
 * the rest are plain library calls now.  These are different: they
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
 *   fb_info (44)    the console framebuffer's geometry and pixel format
 *                   (phase 3.3, the cube port): 32 bpp, and the shift /
 *                   bits of each colour channel, so the caller can pack
 *                   its own pixels
 *   fb_blit (45)    copy a rectangle of those packed pixels onto the
 *                   screen, clipped -- the whole graphics API; there is
 *                   no /dev/fb to mmap through yet (no page cache)
 *
 * The convention is the one the kernel's dispatcher has read since
 * phase 2.1 and the port's own stubs speak (ports/mlibc/sysdeps/prismos/
 * generic/syscall.cpp): rax = number, rdi / rsi / rdx = the arguments,
 * the answer in rax -- for the first four, never an error; for the fb
 * pair, 0 or the kernel's negative errno (44 answers -ENODEV on a VGA
 * text boot, 45 answers -EFAULT on a source page that will not read).
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
	PRISM_SYS_FB_INFO = 44,
	PRISM_SYS_FB_BLIT = 45,
};

/* errno values the fb pair answers with (the kernel's own numbering). */
#define PRISM_ENODEV 19
#define PRISM_EFAULT 14
#define PRISM_EINVAL 22

static inline long prism_raw3(long n, long a, long b, long c)
{
	long r;
	__asm__ volatile("syscall"
	    : "=a"(r)
	    : "a"(n), "D"(a), "S"(b), "d"(c)
	    : "rcx", "r11", "memory");
	return r;
}

/* Six-argument call (fb_blit): r10 / r8 / r9 carry arguments 4..6, the
 * same register order the kernel's dispatcher reads (mmap, futex ...). */
static inline long prism_raw6(long n, long a, long b, long c, long d, long e, long f)
{
	long r;
	register long r10 __asm__("r10") = d;
	register long r8 __asm__("r8") = e;
	register long r9 __asm__("r9") = f;
	__asm__ volatile("syscall"
	    : "=a"(r)
	    : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9)
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

/*
 * The screen the kernel console draws on, as prism_fb_info() reports it:
 * 32 bpp, the classic linear layout.  The shift / bits pairs say where
 * each 8-bit channel lands in a pixel (a QEMU UEFI frame is usually
 * bgrx: r at 16/8, g at 8/8, b at 0/8); pack a pixel exactly like the
 * kernel's Surface::rgb does:
 *
 *   ch(v, shift, bits) = (v >> (8 - bits)) << shift   (bits 0 or > 8: 0)
 */
struct prism_fb_info {
	uint32_t w, h;		    /* pixels */
	uint32_t bpp;		    /* always 32 today */
	uint32_t r_shift, r_bits;
	uint32_t g_shift, g_bits;
	uint32_t b_shift, b_bits;
};

/* Fill *info: 0, or the kernel's negative errno (-ENODEV on VGA text). */
static inline long prism_fb_info(struct prism_fb_info *info)
{
	return prism_raw3(PRISM_SYS_FB_INFO, (long)info, 0, 0);
}

/*
 * Copy a w x h rectangle of pixels -- already packed in the format
 * fb_info() described, `stride` bytes per source row -- onto the screen
 * at (x, y), clipped on every side.  0 (a rectangle entirely off screen
 * is a no-op, not an error), or the kernel's negative errno.
 */
static inline long prism_fb_blit(const uint32_t *src, uint32_t w, uint32_t h,
	uint32_t stride, int32_t x, int32_t y)
{
	return prism_raw6(PRISM_SYS_FB_BLIT, (long)src, (long)w, (long)h,
	    (long)stride, (long)x, (long)y);
}

#endif /* PRISM_RAW_H */
