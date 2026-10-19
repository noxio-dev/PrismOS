/* SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * PrismOS user space: system call wrappers and the tiny freestanding libc
 * (usr/lib/prism.c).  Programs are static PIE executables started by the
 * kernel's ELF loader (sys/kern/src/user/elf.rs); `_start`
 * (usr/lib/crt0.S) hands the System V initial stack to `main`.
 */
#ifndef PRISM_H
#define PRISM_H

#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>

/* ---- system calls (kernel: sys/kern/src/user/syscall.rs) -------------- */

enum {
	SYS_EXIT = 0, SYS_WRITE = 1, SYS_GETPID = 2, SYS_UPTIME_MS = 3,
	SYS_YIELD = 4, SYS_SLEEP = 5, SYS_SETPRIO = 6, SYS_EXECVE = 7,
	SYS_FORK = 8, SYS_WAIT = 9, SYS_KILL = 10, SYS_SIGNAL = 11,
	SYS_SIGRETURN = 12,
};

/* errno values returned (negated) by the kernel */
enum {
	PRISM_EPERM = 1, PRISM_ENOENT = 2, PRISM_ESRCH = 3, PRISM_E2BIG = 7,
	PRISM_ENOEXEC = 8, PRISM_EBADF = 9, PRISM_ECHILD = 10,
	PRISM_ENOMEM = 12, PRISM_EACCES = 13, PRISM_EFAULT = 14,
	PRISM_EISDIR = 21, PRISM_EINVAL = 22, PRISM_ENAMETOOLONG = 36,
	PRISM_ENOSYS = 38,
};

/* signals (the basic set the kernel implements) */
enum {
	PRISM_SIGHUP = 1, PRISM_SIGINT = 2, PRISM_SIGQUIT = 3, PRISM_SIGILL = 4,
	PRISM_SIGTRAP = 5, PRISM_SIGABRT = 6, PRISM_SIGBUS = 7, PRISM_SIGFPE = 8,
	PRISM_SIGKILL = 9, PRISM_SIGUSR1 = 10, PRISM_SIGSEGV = 11,
	PRISM_SIGUSR2 = 12, PRISM_SIGPIPE = 13, PRISM_SIGALRM = 14,
	PRISM_SIGTERM = 15, PRISM_SIGCHLD = 17,
};

/* wait() flags and status decoding (Linux encoding) */
#define PRISM_WNOHANG 1
#define PRISM_WEXITSTATUS(s) (((s) >> 8) & 0xff)
#define PRISM_WTERMSIG(s) ((s) & 0x7f)
#define PRISM_WIFEXITED(s) (((s) & 0x7f) == 0)
#define PRISM_WIFSIGNALED(s) (((s) & 0x7f) != 0)
/* wait "any child" (also 0); a specific pid waits for that child */
#define PRISM_WAIT_ANY (-1L)

static inline long syscall3(long n, long a, long b, long c)
{
	long r;
	__asm__ volatile("syscall"
	    : "=a"(r)
	    : "a"(n), "D"(a), "S"(b), "d"(c)
	    : "rcx", "r11", "memory");
	return r;
}

__attribute__((noreturn)) static inline void sys_exit(long code)
{
	syscall3(SYS_EXIT, code, 0, 0);
	for (;;)
		;
}
static inline long sys_write(int fd, const void *buf, size_t len)
{
	return syscall3(SYS_WRITE, fd, (long)buf, (long)len);
}
static inline long sys_getpid(void) { return syscall3(SYS_GETPID, 0, 0, 0); }
static inline long sys_uptime_ms(void) { return syscall3(SYS_UPTIME_MS, 0, 0, 0); }
static inline void sys_yield(void) { syscall3(SYS_YIELD, 0, 0, 0); }
static inline void sys_sleep(long ms) { syscall3(SYS_SLEEP, ms, 0, 0); }
static inline long sys_setprio(long level) { return syscall3(SYS_SETPRIO, level, 0, 0); }
static inline long sys_execve(const char *path, char *const argv[], char *const envp[])
{
	return syscall3(SYS_EXECVE, (long)path, (long)argv, (long)envp);
}
static inline long sys_fork(void) { return syscall3(SYS_FORK, 0, 0, 0); }
static inline long sys_wait(long pid, int *status, long flags)
{
	return syscall3(SYS_WAIT, pid, (long)status, flags);
}
static inline long sys_kill(long pid, long sig) { return syscall3(SYS_KILL, pid, sig, 0); }
static inline long sys_signal(long sig, void (*handler)(int))
{
	return syscall3(SYS_SIGNAL, sig, (long)handler, 0);
}
/* The PRISM_W* macros above decode the int *status word filled by sys_wait. */

/* ---- auxiliary vector ------------------------------------------------- */

enum {
	AT_NULL = 0, AT_PHDR = 3, AT_PHENT = 4, AT_PHNUM = 5, AT_PAGESZ = 6,
	AT_BASE = 7, AT_FLAGS = 8, AT_ENTRY = 9, AT_UID = 11, AT_EUID = 12,
	AT_GID = 13, AT_EGID = 14, AT_CLKTCK = 17, AT_SECURE = 23,
	AT_RANDOM = 25, AT_EXECFN = 31,
};

extern char **environ;
/* Value of auxiliary vector entry `type`, or 0 when absent. */
unsigned long getauxval(unsigned long type);

/* ---- libc ------------------------------------------------------------- */

void *memcpy(void *d, const void *s, size_t n);
void *memmove(void *d, const void *s, size_t n);
void *memset(void *d, int c, size_t n);
int memcmp(const void *a, const void *b, size_t n);
size_t strlen(const char *s);
int strcmp(const char *a, const char *b);

/* Writes to fd 1.  Supports %s %c %d %i %u %x %X %p %% with the l / ll
 * length modifiers and a width / 0 flag.  Each call is one write(), so
 * lines of concurrent processes do not interleave mid-line. */
int printf(const char *fmt, ...);
int vsnprintf(char *buf, size_t size, const char *fmt, va_list ap);
unsigned long strtoul(const char *s, char **end, int base);

#endif
