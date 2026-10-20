#ifndef _PRISMOS_SYSCALL_H
#define _PRISMOS_SYSCALL_H

#include <stddef.h>
#include <stdint.h>

/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * PrismOS syscall numbers and the raw call convention (phase 3.2, step 2):
 * rax = number, rdi / rsi / rdx / r10 / r8 / r9 = arguments 1..6 (the
 * x86-64 Linux register order the kernel's dispatcher reads), result in
 * rax -- a value in [-4096, -1] is -errno, anything else is the answer.
 * The list mirrors sys/kern/src/user/syscall.rs exactly.
 */

#ifdef __cplusplus
extern "C" {
#endif

enum {
	PRISM_SYS_EXIT = 0,          /* exit(status) [noreturn] */
	PRISM_SYS_WRITE = 1,         /* write(fd, buf, len) */
	PRISM_SYS_GETPID = 2,        /* getpid() */
	PRISM_SYS_UPTIME_MS = 3,     /* uptime_ms() */
	PRISM_SYS_YIELD = 4,         /* yield() */
	PRISM_SYS_SLEEP = 5,         /* sleep(ms) */
	PRISM_SYS_SETPRIO = 6,       /* setprio(level) */
	PRISM_SYS_EXECVE = 7,        /* execve(path, argv, envp) */
	PRISM_SYS_FORK = 8,          /* fork() */
	PRISM_SYS_WAIT = 9,          /* wait(pid, &status, flags) */
	PRISM_SYS_KILL = 10,         /* kill(pid, sig) */
	PRISM_SYS_SIGNAL = 11,       /* signal(sig, handler) -- plain wrapper */
	PRISM_SYS_SIGRETURN = 12,    /* sigreturn() [restorer only] */
	PRISM_SYS_READ = 13,         /* read(fd, buf, len) */
	PRISM_SYS_OPEN = 14,         /* open(path, flags) -- 2 args, no mode */
	PRISM_SYS_CLOSE = 15,        /* close(fd) */
	PRISM_SYS_LSEEK = 16,        /* lseek(fd, off, whence) */
	PRISM_SYS_DUP = 17,          /* dup(fd) */
	PRISM_SYS_PIPE = 18,         /* pipe(&fds[2]) */
	PRISM_SYS_STAT = 19,         /* stat(path, &statbuf) */
	PRISM_SYS_FSTAT = 20,        /* fstat(fd, &statbuf) */
	PRISM_SYS_BRK = 21,          /* brk(addr) */
	PRISM_SYS_MMAP = 22,         /* mmap(addr, len, prot, flags, fd, off) */
	PRISM_SYS_MUNMAP = 23,       /* munmap(addr, len) */
	PRISM_SYS_CHDIR = 24,        /* chdir(path) */
	PRISM_SYS_GETCWD = 25,       /* getcwd(buf, size) */
	PRISM_SYS_DUP2 = 26,         /* dup2(oldfd, newfd) */
	PRISM_SYS_FCNTL = 27,        /* fcntl(fd, cmd, arg) */
	PRISM_SYS_GETDENTS = 28,     /* getdents(fd, buf, len) */
	PRISM_SYS_IOCTL = 29,        /* ioctl(fd, req, arg) */
	PRISM_SYS_UNLINK = 30,       /* unlink(path) */
	PRISM_SYS_MKDIR = 31,        /* mkdir(path, mode) */
	PRISM_SYS_RMDIR = 32,        /* rmdir(path) */
	PRISM_SYS_RENAME = 33,       /* rename(old, new) */
	PRISM_SYS_LINK = 34,         /* link(old, new) */
	PRISM_SYS_UNAME = 35,        /* uname(&utsname) */
	PRISM_SYS_ARCH_PRCTL = 36,   /* arch_prctl(code, addr) -- TLS */
	PRISM_SYS_FUTEX = 37,        /* futex(addr, op, val, ts, addr2, val3) */
	PRISM_SYS_SIGACTION = 38,    /* sigaction(sig, &act, &oldact) */
	PRISM_SYS_SIGPROCMASK = 39,  /* sigprocmask(how, &set, &oldset) */
	PRISM_SYS_MPROTECT = 40,     /* mprotect(addr, len, prot) */
	PRISM_SYS_POLL = 41,         /* poll(&pollfd[], nfds, timeout_ms) */
	PRISM_SYS_SELECT = 42,       /* select(nfds, r, w, e, &timeval) */
	PRISM_SYS_CLOCK_GETTIME = 43, /* clock_gettime(clk, &ts) */
};

/* arch_prctl() codes -- the Linux x86-64 values the kernel expects. */
#define PRISM_ARCH_SET_GS 0x1001
#define PRISM_ARCH_SET_FS 0x1002
#define PRISM_ARCH_GET_FS 0x1003
#define PRISM_ARCH_GET_GS 0x1004

/* futex() operations -- the Linux values the kernel implements. */
#define PRISM_FUTEX_WAIT 0
#define PRISM_FUTEX_WAKE 1
#define PRISM_FUTEX_REQUEUE 3
#define PRISM_FUTEX_CMP_REQUEUE 4
#define PRISM_FUTEX_WAIT_BITSET 9
#define PRISM_FUTEX_WAKE_BITSET 10

/* The raw stubs, implemented in generic/syscall.cpp.  They return the raw
 * kernel answer (possibly -errno); __do_syscall_ret turns an error answer
 * into errno + -1, the way a plain syscall() helper behaves. */
typedef long prism_sc_word_t;

prism_sc_word_t __do_syscall0(long sc);
prism_sc_word_t __do_syscall1(long sc, prism_sc_word_t arg1);
prism_sc_word_t __do_syscall2(long sc, prism_sc_word_t arg1, prism_sc_word_t arg2);
prism_sc_word_t __do_syscall3(long sc, prism_sc_word_t arg1, prism_sc_word_t arg2,
		prism_sc_word_t arg3);
prism_sc_word_t __do_syscall4(long sc, prism_sc_word_t arg1, prism_sc_word_t arg2,
		prism_sc_word_t arg3, prism_sc_word_t arg4);
prism_sc_word_t __do_syscall5(long sc, prism_sc_word_t arg1, prism_sc_word_t arg2,
		prism_sc_word_t arg3, prism_sc_word_t arg4, prism_sc_word_t arg5);
prism_sc_word_t __do_syscall6(long sc, prism_sc_word_t arg1, prism_sc_word_t arg2,
		prism_sc_word_t arg3, prism_sc_word_t arg4, prism_sc_word_t arg5,
		prism_sc_word_t arg6);
long __do_syscall_ret(unsigned long ret);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* _PRISMOS_SYSCALL_H */
