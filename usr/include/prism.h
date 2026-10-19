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
        SYS_READ = 13, SYS_OPEN = 14, SYS_CLOSE = 15, SYS_LSEEK = 16,
        SYS_DUP = 17, SYS_PIPE = 18,
        /* phase 3.1: the newlib syscall set */
        SYS_STAT = 19, SYS_FSTAT = 20, SYS_BRK = 21, SYS_MMAP = 22,
        SYS_MUNMAP = 23, SYS_CHDIR = 24, SYS_GETCWD = 25, SYS_DUP2 = 26,
        SYS_FCNTL = 27, SYS_GETDENTS = 28, SYS_IOCTL = 29, SYS_UNLINK = 30,
        SYS_MKDIR = 31, SYS_RMDIR = 32, SYS_RENAME = 33, SYS_LINK = 34,
        SYS_UNAME = 35,
};

/* errno values returned (negated) by the kernel */
enum {
        PRISM_EPERM = 1, PRISM_ENOENT = 2, PRISM_ESRCH = 3, PRISM_EIO = 5,
        PRISM_E2BIG = 7, PRISM_ENOEXEC = 8, PRISM_EBADF = 9,
        PRISM_ECHILD = 10, PRISM_EAGAIN = 11, PRISM_ENOMEM = 12,
        PRISM_EACCES = 13, PRISM_EFAULT = 14, PRISM_EBUSY = 16,
        PRISM_EEXIST = 17, PRISM_EXDEV = 18, PRISM_ENODEV = 19,
        PRISM_ENOTDIR = 20, PRISM_EISDIR = 21, PRISM_EINVAL = 22,
        PRISM_EMFILE = 24, PRISM_ENOTTY = 25, PRISM_EFBIG = 27,
        PRISM_ENOSPC = 28, PRISM_ESPIPE = 29, PRISM_EROFS = 30,
        PRISM_EMLINK = 31, PRISM_EPIPE = 32, PRISM_ENAMETOOLONG = 36,
        PRISM_ERANGE = 34, PRISM_ENOSYS = 38, PRISM_ENOTEMPTY = 39,
};

/* open() flags: the kernel's own values (fs/vfs.rs), not Linux's O_* */
enum {
        PRISM_O_READ = 1, PRISM_O_WRITE = 2, PRISM_O_CREATE = 4,
        PRISM_O_TRUNC = 8, PRISM_O_APPEND = 16, PRISM_O_EXCL = 32,
        PRISM_O_NONBLOCK = 64, PRISM_O_CLOEXEC = 128,
};

/* lseek() whence */
#define PRISM_SEEK_SET 0
#define PRISM_SEEK_CUR 1
#define PRISM_SEEK_END 2

/* signals (the basic set the kernel implements) */
enum {
        PRISM_SIGHUP = 1, PRISM_SIGINT = 2, PRISM_SIGQUIT = 3, PRISM_SIGILL = 4,
        PRISM_SIGTRAP = 5, PRISM_SIGABRT = 6, PRISM_SIGBUS = 7, PRISM_SIGFPE = 8,
        PRISM_SIGKILL = 9, PRISM_SIGUSR1 = 10, PRISM_SIGSEGV = 11,
        PRISM_SIGUSR2 = 12, PRISM_SIGPIPE = 13, PRISM_SIGALRM = 14,
        PRISM_SIGTERM = 15, PRISM_SIGCHLD = 17,
};

/* signal() dispositions: 0 = default, 1 = ignore (phase 3.1) */
#define PRISM_SIG_DFL ((void *)0)
#define PRISM_SIG_IGN ((void *)1)

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

/* ---- descriptors, pipes, /dev (phase 2.6) ----------------------------- */

static inline long sys_read(int fd, void *buf, size_t len)
{
        return syscall3(SYS_READ, fd, (long)buf, (long)len);
}
static inline long sys_open(const char *path, unsigned long flags)
{
        return syscall3(SYS_OPEN, (long)path, flags, 0);
}
static inline long sys_close(int fd) { return syscall3(SYS_CLOSE, fd, 0, 0); }
static inline long sys_lseek(int fd, long off, long whence)
{
        return syscall3(SYS_LSEEK, fd, off, whence);
}
static inline long sys_dup(int fd) { return syscall3(SYS_DUP, fd, 0, 0); }
static inline long sys_pipe(int *fds) { return syscall3(SYS_PIPE, (long)fds, 0, 0); }

/* ---- the newlib syscall set (phase 3.1) ------------------------------- */

/* six-argument call (mmap): r10 / r8 / r9 carry arguments 4..6, the same
 * register order Linux uses -- the kernel reads the trap frame's copies. */
static inline long syscall6(long n, long a, long b, long c, long d, long e, long f)
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

static inline long sys_stat(const char *path, void *st)
{
        return syscall3(SYS_STAT, (long)path, (long)st, 0);
}
static inline long sys_fstat(int fd, void *st)
{
        return syscall3(SYS_FSTAT, fd, (long)st, 0);
}
static inline long sys_brk(unsigned long addr) { return syscall3(SYS_BRK, addr, 0, 0); }
static inline long sys_mmap(unsigned long addr, unsigned long len, int prot,
    int flags, int fd, unsigned long off)
{
        return syscall6(SYS_MMAP, (long)addr, (long)len, prot, flags, fd, (long)off);
}
static inline long sys_munmap(unsigned long addr, unsigned long len)
{
        return syscall3(SYS_MUNMAP, (long)addr, (long)len, 0);
}
static inline long sys_chdir(const char *path) { return syscall3(SYS_CHDIR, (long)path, 0, 0); }
static inline long sys_getcwd(char *buf, unsigned long size)
{
        return syscall3(SYS_GETCWD, (long)buf, size, 0);
}
static inline long sys_dup2(int old, int new) { return syscall3(SYS_DUP2, old, new, 0); }
static inline long sys_fcntl(int fd, int cmd, unsigned long arg)
{
        return syscall3(SYS_FCNTL, fd, cmd, arg);
}
static inline long sys_getdents(int fd, void *buf, unsigned long len)
{
        return syscall3(SYS_GETDENTS, fd, (long)buf, len);
}
static inline long sys_ioctl(int fd, unsigned long req, void *arg)
{
        return syscall3(SYS_IOCTL, fd, req, (long)arg);
}
static inline long sys_unlink(const char *path)
{
        return syscall3(SYS_UNLINK, (long)path, 0, 0);
}
static inline long sys_mkdir(const char *path, int mode)
{
        return syscall3(SYS_MKDIR, (long)path, mode, 0);
}
static inline long sys_rmdir(const char *path) { return syscall3(SYS_RMDIR, (long)path, 0, 0); }
static inline long sys_rename(const char *old, const char *new)
{
        return syscall3(SYS_RENAME, (long)old, (long)new, 0);
}
static inline long sys_link(const char *old, const char *new)
{
        return syscall3(SYS_LINK, (long)old, (long)new, 0);
}
static inline long sys_uname(void *buf) { return syscall3(SYS_UNAME, (long)buf, 0, 0); }

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

/* ---- POSIX structures (phase 3.1) ------------------------------------- */

/* fcntl() commands; FD_CLOEXEC is the only fd flag (F_GETFD / F_SETFD). */
enum {
        PRISM_F_DUPFD = 0, PRISM_F_GETFD = 1, PRISM_F_SETFD = 2,
        PRISM_F_GETFL = 3, PRISM_F_SETFL = 4,
};
#define PRISM_FD_CLOEXEC 1

/* mmap(): protection and flags (the Linux values). */
enum {
        PRISM_PROT_READ = 1, PRISM_PROT_WRITE = 2, PRISM_PROT_EXEC = 4,
        PRISM_MAP_SHARED = 1, PRISM_MAP_PRIVATE = 2, PRISM_MAP_FIXED = 0x10,
        PRISM_MAP_ANONYMOUS = 0x20,
};

/* ioctl() requests: the tty pair a libc's isatty() needs. */
#define PRISM_TCGETS 0x5401
#define PRISM_TIOCGWINSZ 0x5413

/* The x86-64 Linux `struct stat` layout (144 bytes) -- what the kernel
 * fills and what the future newlib port (phase 3.2) compiles against. */
struct prism_stat {
        uint64_t st_dev;
        uint64_t st_ino;
        uint64_t st_nlink;
        uint32_t st_mode;
        uint32_t st_uid;
        uint32_t st_gid;
        uint32_t pad0;
        uint64_t st_rdev;
        int64_t st_size;
        int64_t st_blksize;
        int64_t st_blocks;
        int64_t st_atime;
        int64_t st_mtime;
        int64_t st_ctime;
        int64_t reserved[3];
};

/* st_mode file types (the classic S_IF* bits). */
#define PRISM_S_IFMT 0xF000
#define PRISM_S_IFREG 0x8000
#define PRISM_S_IFDIR 0x4000
#define PRISM_S_IFCHR 0x2000
#define PRISM_S_IFIFO 0x1000
#define PRISM_S_IFLNK 0xA000
#define PRISM_S_ISREG(m) (((m) & PRISM_S_IFMT) == PRISM_S_IFREG)
#define PRISM_S_ISDIR(m) (((m) & PRISM_S_IFMT) == PRISM_S_IFDIR)
#define PRISM_S_ISCHR(m) (((m) & PRISM_S_IFMT) == PRISM_S_IFCHR)
#define PRISM_S_ISFIFO(m) (((m) & PRISM_S_IFMT) == PRISM_S_IFIFO)

/* The x86-64 Linux `struct linux_dirent64` -- one getdents() record. */
struct prism_dirent {
        uint64_t d_ino;
        int64_t d_off;
        uint16_t d_reclen;
        uint8_t d_type;
        char d_name[];
};
enum {
        PRISM_DT_UNKNOWN = 0, PRISM_DT_FIFO = 1, PRISM_DT_CHR = 2,
        PRISM_DT_DIR = 4, PRISM_DT_REG = 8, PRISM_DT_LNK = 10,
};

/* struct utsname: six NUL-padded 65-byte fields. */
struct prism_utsname {
        char sysname[65];
        char nodename[65];
        char release[65];
        char version[65];
        char machine[65];
        char domainname[65];
};

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
