#include <errno.h>
#include <limits.h>
#include <mlibc-config.h>
#include <string.h>

#include <abi-bits/fd_set.h>
#include <abi-bits/fcntl.h>
#include <abi-bits/ioctls.h>
#include <abi-bits/resource.h>
#include <abi-bits/stat.h>
#include <abi-bits/vm-flags.h>

#include <bits/ansi/timespec.h>
#include <bits/ensure.h>
#include <bits/timeval.h>
#include <bits/winsize.h>

#include <mlibc/all-sysdeps.hpp>
#include <prismos/syscall.h>

#include <poll.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/resource.h>
#include <termios.h>

/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * The PrismOS sysdeps (phase 3.2, step 2): every mlibc tag the kernel has
 * a syscall for, spoken through the raw convention of generic/syscall.cpp.
 * Everything else -- threads / clone, sockets, *at() past AT_FDCWD, rusage
 * accounting, fchmod and friends -- is deliberately absent from the tag
 * list (include/mlibc/sysdeps.hpp), so mlibc's generic layer answers
 * ENOSYS: what the kernel lacks stays ENOSYS, the port's contract.
 *
 * Kernel-side references: sys/kern/src/user/{syscall,fd,sysc,signal,
 * futex,process}.rs.  Layout notes worth remembering while reading:
 *   * struct stat is the x86-64 Linux one (144 bytes) the kernel fills;
 *   * the sigaction the kernel exchanges is four u64 words -- handler,
 *     flags, restorer, 8-byte mask -- exactly this port's ksigaction;
 *   * sigprocmask exchanges the low 8 bytes of a sigset_t;
 *   * select takes a struct timeval, poll a millisecond timeout;
 *   * open() takes no mode (mkdir does), and the O_* values are the
 *     kernel's own, defined in this port's abi-bits/fcntl.h.
 */

namespace {

using sc_word_t = prism_sc_word_t;

/* The kernel answers -errno in rax; anything in [-4096, -1] is an error. */
inline int sc_error(sc_word_t ret) {
        auto v = static_cast<unsigned long>(ret);
        if(v > -4096UL)
                return -static_cast<long>(v);
        return 0;
}

/* Argument adapters: every word a syscall takes is either an integer or a
 * pointer; the C++ template below needs one conversion per shape. */
inline sc_word_t scw(int x) { return x; }
inline sc_word_t scw(unsigned int x) { return x; }
inline sc_word_t scw(long x) { return x; }
inline sc_word_t scw(unsigned long x) { return x; }
inline sc_word_t scw(const void *x) { return reinterpret_cast<sc_word_t>(x); }

/* One variadic front end for __do_syscallN.  Shorter calls simply see
 * zero-padded r10 / r8 / r9 -- registers the kernel reads only for the
 * six-argument ones (mmap, futex, select). */
template<typename... T>
sc_word_t sys(long nr, T... args) {
        sc_word_t words[] = {scw(args)..., 0, 0, 0, 0, 0, 0};
        return __do_syscall6(nr, words[0], words[1], words[2], words[3], words[4], words[5]);
}

/* The SA_RESTORER half of the signal contract: the restorer mlibc plants
 * in every action (x86_64/signal.S) -- it makes the sigreturn syscall (12)
 * when a handler returns.  Hidden on purpose: the address is planted as
 * data in the kernel's sigaction word, and a default-visibility reference
 * would go through the GOT -- ld.so's self-relocator (relocateSelf)
 * accepts R_X86_64_RELATIVE and nothing else, and a GLOB_DAT carries a
 * symbol.  (The linux port instead compiles the whole Sigaction wrapper
 * out of the rtld; that breaks the static archive's member pull -- the
 * libc's own twin would come in for Sigaction and collide -- so the port
 * keeps the wrapper everywhere and hides the reference instead.) */
extern "C" void __mlibc_signal_restore(void) __attribute__((visibility("hidden")));

/* The four-u64 sigaction the kernel copies in and out (user/signal.rs). */
struct ksigaction {
        uint64_t handler;
        uint64_t flags;
        uint64_t restorer;
        uint64_t mask;
};

/* The kernel's sigprocmask exchanges the low 8 bytes of the sigset_t. */
int prism_sigmask(int how, const sigset_t *__restrict set,
                sigset_t *__restrict retrieve) {
        uint64_t kset = 0;
        if(set)
                memcpy(&kset, set, sizeof(kset));
        uint64_t kold = 0;
        auto ret = sys(PRISM_SYS_SIGPROCMASK, how, set ? &kset : nullptr,
                        retrieve ? &kold : nullptr);
        if(int e = sc_error(ret); e)
                return e;
        if(retrieve) {
                memset(retrieve, 0, sizeof(*retrieve));
                memcpy(retrieve, &kold, sizeof(kold));
        }
        return 0;
}

} // namespace

namespace mlibc {

void Sysdeps<LibcLog>::operator()(const char *message) {
        size_t len = strlen(message);
        ssize_t unused;
        sysdep<Write>(2, message, len, &unused);
}

[[noreturn]] void Sysdeps<LibcPanic>::operator()() {
        sysdep<LibcLog>("mlibc: panic");
        sysdep<Exit>(-1);
        __builtin_trap();
}

// ---------------------------------------------------------------- ansi core

[[noreturn]] void Sysdeps<Exit>::operator()(int status) {
        sys(PRISM_SYS_EXIT, status);
        __builtin_trap();
}

int Sysdeps<Read>::operator()(int fd, void *buf, size_t count, ssize_t *bytes_read) {
        auto ret = sys(PRISM_SYS_READ, fd, buf, count);
        if(int e = sc_error(ret); e)
                return e;
        *bytes_read = static_cast<ssize_t>(ret);
        return 0;
}

int Sysdeps<Write>::operator()(int fd, const void *buf, size_t count, ssize_t *bytes_written) {
        auto ret = sys(PRISM_SYS_WRITE, fd, buf, count);
        if(int e = sc_error(ret); e)
                return e;
        *bytes_written = static_cast<ssize_t>(ret);
        return 0;
}

int Sysdeps<Open>::operator()(const char *pathname, int flags, mode_t, int *fd) {
        /* The kernel's open() takes two arguments -- no mode (mkdir carries
         * it); the O_* bits here are already the kernel's own values. */
        auto ret = sys(PRISM_SYS_OPEN, pathname, flags);
        if(int e = sc_error(ret); e)
                return e;
        *fd = static_cast<int>(ret);
        return 0;
}

int Sysdeps<Close>::operator()(int fd) {
        auto ret = sys(PRISM_SYS_CLOSE, fd);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Seek>::operator()(int fd, off_t offset, int whence, off_t *new_offset) {
        auto ret = sys(PRISM_SYS_LSEEK, fd, offset, whence);
        if(int e = sc_error(ret); e)
                return e;
        *new_offset = static_cast<off_t>(ret);
        return 0;
}

// ------------------------------------------------------------- the vm tags

int Sysdeps<VmMap>::operator()(void *hint, size_t size, int prot, int flags,
                int fd, off_t offset, void **window) {
        if(offset % 4096)
                return EINVAL;
        if(size >= PTRDIFF_MAX)
                return ENOMEM;
        auto ret = sys(PRISM_SYS_MMAP, hint, size, prot, flags, fd, offset);
        if(int e = sc_error(ret); e)
                return e;
        *window = reinterpret_cast<void *>(static_cast<uintptr_t>(ret));
        return 0;
}

int Sysdeps<VmUnmap>::operator()(void *pointer, size_t size) {
        auto ret = sys(PRISM_SYS_MUNMAP, pointer, size);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<VmProtect>::operator()(void *pointer, size_t size, int prot) {
        auto ret = sys(PRISM_SYS_MPROTECT, pointer, size, prot);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<AnonAllocate>::operator()(size_t size, void **pointer) {
        return sysdep<VmMap>(nullptr, size, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0, pointer);
}

int Sysdeps<AnonFree>::operator()(void *pointer, size_t size) {
        return sysdep<VmUnmap>(pointer, size);
}

// -------------------------------------------------------------------- time

int Sysdeps<ClockGet>::operator()(int clock, time_t *secs, long *nanos) {
        struct timespec tp = {};
        auto ret = sys(PRISM_SYS_CLOCK_GETTIME, clock, &tp);
        if(int e = sc_error(ret); e)
                return e;
        *secs = tp.tv_sec;
        *nanos = tp.tv_nsec;
        return 0;
}

int Sysdeps<Sleep>::operator()(time_t *secs, long *nanos) {
        /* PrismOS sleep() takes milliseconds, bounded at 10 s (the house
         * rule); it is not interruptible, so no remainder is reported. */
        if(*nanos < 0 || *nanos >= 1'000'000'000)
                return EINVAL;
        long ms = static_cast<long>(*secs) * 1000 + (*nanos + 999'999) / 1'000'000;
        if(ms > 0) {
                auto ret = sys(PRISM_SYS_SLEEP, ms);
                if(int e = sc_error(ret); e)
                        return e;
        }
        *secs = 0;
        *nanos = 0;
        return 0;
}

// ---------------------------------------------------- futex, tcb, thread id

int Sysdeps<FutexWait>::operator()(int *pointer, int expected, const struct timespec *time) {
        /* The kernel's futex timeouts run against the uptime clock; a NULL
         * timespec parks the bounded way (10 s ceiling) and answers
         * ETIMEDOUT -- a library treats that as a spurious wakeup, retries. */
        auto ret = sys(PRISM_SYS_FUTEX, pointer, PRISM_FUTEX_WAIT, expected, time, 0, 0);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<FutexWake>::operator()(int *pointer, bool all) {
        auto ret = sys(PRISM_SYS_FUTEX, pointer, PRISM_FUTEX_WAKE,
                        all ? INT_MAX : 1, 0, 0, 0);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

pid_t Sysdeps<FutexTid>::operator()() {
        /* Single-threaded: the tid is the pid. */
        return static_cast<pid_t>(sys(PRISM_SYS_GETPID));
}

pid_t Sysdeps<GetTid>::operator()() {
        return static_cast<pid_t>(sys(PRISM_SYS_GETPID));
}

int Sysdeps<TcbSet>::operator()(void *pointer) {
        /* TLS, the x86-64 way: the FS base *is* the TCB address (the Linux
         * x86-64 convention mlibc's own code was written against).  The
         * kernel parks it in the MSR, switched with the process. */
        auto ret = sys(PRISM_SYS_ARCH_PRCTL, PRISM_ARCH_SET_FS, pointer);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

// ------------------------------------------------------------ fd machinery

int Sysdeps<Dup>::operator()(int fd, int, int *newfd) {
        auto ret = sys(PRISM_SYS_DUP, fd);
        if(int e = sc_error(ret); e)
                return e;
        *newfd = static_cast<int>(ret);
        return 0;
}

int Sysdeps<Dup2>::operator()(int fd, int, int newfd) {
        auto ret = sys(PRISM_SYS_DUP2, fd, newfd);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Fcntl>::operator()(int fd, int request, va_list args, int *result) {
        auto arg = va_arg(args, unsigned long);
        auto ret = sys(PRISM_SYS_FCNTL, fd, request, arg);
        if(int e = sc_error(ret); e)
                return e;
        *result = static_cast<int>(ret);
        return 0;
}

int Sysdeps<Pipe>::operator()(int *fds, int flags) {
        auto ret = sys(PRISM_SYS_PIPE, fds);
        if(int e = sc_error(ret); e)
                return e;
        /* pipe2()-style flags are emulated on the two ends: O_CLOEXEC is a
         * property of the numbers (F_SETFD), O_NONBLOCK of the descriptions
         * (F_SETFL). */
        if(flags & O_CLOEXEC) {
                for(int i = 0; i < 2; i++) {
                        auto r = sys(PRISM_SYS_FCNTL, fds[i], F_SETFD, FD_CLOEXEC);
                        if(int e = sc_error(r); e)
                                return e;
                }
        }
        if(flags & O_NONBLOCK) {
                for(int i = 0; i < 2; i++) {
                        auto r = sys(PRISM_SYS_FCNTL, fds[i], F_SETFL, O_NONBLOCK);
                        if(int e = sc_error(r); e)
                                return e;
                }
        }
        return 0;
}

int Sysdeps<Ioctl>::operator()(int fd, unsigned long request, void *arg, int *result) {
        auto ret = sys(PRISM_SYS_IOCTL, fd, request, arg);
        if(int e = sc_error(ret); e)
                return e;
        *result = static_cast<int>(ret);
        return 0;
}

int Sysdeps<Isatty>::operator()(int fd) {
        struct termios attrs;
        auto ret = sys(PRISM_SYS_IOCTL, fd, TCGETS, &attrs);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Tcgetattr>::operator()(int fd, struct termios *attr) {
        auto ret = sys(PRISM_SYS_IOCTL, fd, TCGETS, attr);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Tcgetwinsize>::operator()(int fd, struct winsize *winsz) {
        auto ret = sys(PRISM_SYS_IOCTL, fd, TIOCGWINSZ, winsz);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Poll>::operator()(struct pollfd *fds, nfds_t count, int timeout, int *num_events) {
        auto ret = sys(PRISM_SYS_POLL, fds, count, timeout);
        if(int e = sc_error(ret); e)
                return e;
        *num_events = static_cast<int>(ret);
        return 0;
}

int Sysdeps<Pselect>::operator()(int num_fds, fd_set *read_set, fd_set *write_set,
                fd_set *except_set, const struct timespec *timeout, const sigset_t *sigmask,
                int *num_events) {
        /* pselect's signal-mask argument is not something the kernel keeps
         * yet (README: "trivial once wanted") -- refuse it rather than
         * pretend.  plain select() passes a null mask and works. */
        if(sigmask)
                return ENOSYS;

        /* The kernel's select takes a timeval; translate.  NULL means wait
         * (bounded, the house rule), like everywhere here. */
        struct timeval tv = {};
        struct timeval *tvp = nullptr;
        if(timeout) {
                if(timeout->tv_nsec < 0 || timeout->tv_nsec >= 1'000'000'000)
                        return EINVAL;
                tv.tv_sec = timeout->tv_sec;
                tv.tv_usec = timeout->tv_nsec / 1000;
                tvp = &tv;
        }

        auto ret = sys(PRISM_SYS_SELECT, num_fds, read_set, write_set, except_set, tvp);
        if(int e = sc_error(ret); e)
                return e;
        /* Nothing exceptional ever happens on PrismOS, and the kernel does
         * not write the except set back -- clear it ourselves (POSIX: only
         * ready descriptors stay set). */
        if(except_set)
                memset(except_set, 0, sizeof(fd_set));
        *num_events = static_cast<int>(ret);
        return 0;
}

// ------------------------------------------------------------ the namespace

int Sysdeps<Stat>::operator()(fsfd_target fsfdt, int fd, const char *path, int,
                struct stat *statbuf) {
        switch(fsfdt) {
        case fsfd_target::path: {
                auto ret = sys(PRISM_SYS_STAT, path, statbuf);
                if(int e = sc_error(ret); e)
                        return e;
                return 0;
        }
        case fsfd_target::fd: {
                auto ret = sys(PRISM_SYS_FSTAT, fd, statbuf);
                if(int e = sc_error(ret); e)
                        return e;
                return 0;
        }
        default:
                /* fd_path would be fstatat(); the kernel has no *at calls. */
                return ENOSYS;
        }
}

int Sysdeps<OpenDir>::operator()(const char *path, int *handle) {
        /* No O_DIRECTORY in the kernel's flag set: open read-only, then let
         * a stat decide -- POSIX wants opendir to fail with ENOTDIR. */
        int fd;
        if(int e = sysdep<Open>(path, O_RDONLY, 0, &fd); e)
                return e;
        struct stat st;
        if(int e = sysdep<Stat>(fsfd_target::fd, fd, nullptr, 0, &st); e) {
                sysdep<Close>(fd);
                return e;
        }
        if(!S_ISDIR(st.st_mode)) {
                sysdep<Close>(fd);
                return ENOTDIR;
        }
        *handle = fd;
        return 0;
}

int Sysdeps<ReadEntries>::operator()(int handle, void *buffer, size_t max_size,
                size_t *bytes_read) {
        auto ret = sys(PRISM_SYS_GETDENTS, handle, buffer, max_size);
        if(int e = sc_error(ret); e)
                return e;
        *bytes_read = static_cast<size_t>(ret);
        return 0;
}

int Sysdeps<GetCwd>::operator()(char *buffer, size_t size) {
        auto ret = sys(PRISM_SYS_GETCWD, buffer, size);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Chdir>::operator()(const char *path) {
        auto ret = sys(PRISM_SYS_CHDIR, path);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Mkdir>::operator()(const char *path, mode_t mode) {
        auto ret = sys(PRISM_SYS_MKDIR, path, mode);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Rmdir>::operator()(const char *path) {
        auto ret = sys(PRISM_SYS_RMDIR, path);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Unlinkat>::operator()(int dirfd, const char *path, int flags) {
        /* No *at syscalls in the kernel: only AT_FDCWD resolves, and of the
         * flags only AT_REMOVEDIR changes the call. */
        if(dirfd != AT_FDCWD)
                return ENOSYS;
        if(flags & ~AT_REMOVEDIR)
                return EINVAL;
        auto ret = flags & AT_REMOVEDIR
                        ? sys(PRISM_SYS_RMDIR, path)
                        : sys(PRISM_SYS_UNLINK, path);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Rename>::operator()(const char *path, const char *new_path) {
        auto ret = sys(PRISM_SYS_RENAME, path, new_path);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Link>::operator()(const char *old_path, const char *new_path) {
        auto ret = sys(PRISM_SYS_LINK, old_path, new_path);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Symlink>::operator()(const char *target_path, const char *link_path) {
        // The target travels verbatim -- the kernel stores relative targets
        // relative (the VFS resolves them at walk time), so no *at() shape
        // and no cwd joining on this argument.
        auto ret = sys(PRISM_SYS_SYMLINK, target_path, link_path);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Readlink>::operator()(const char *path, void *buffer, size_t max_size,
                ssize_t *length) {
        auto ret = sys(PRISM_SYS_READLINK, path, buffer, max_size);
        if(int e = sc_error(ret); e)
                return e;
        // The kernel's answer is the byte count it placed (no NUL, POSIX).
        *length = static_cast<ssize_t>(ret);
        return 0;
}

int Sysdeps<Uname>::operator()(struct utsname *buf) {
        auto ret = sys(PRISM_SYS_UNAME, buf);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

// ---------------------------------------------------------------- processes

pid_t Sysdeps<GetPid>::operator()() {
        return static_cast<pid_t>(sys(PRISM_SYS_GETPID));
}

void Sysdeps<Yield>::operator()() {
        sys(PRISM_SYS_YIELD);
}

int Sysdeps<Fork>::operator()(pid_t *child) {
        auto ret = sys(PRISM_SYS_FORK);
        if(int e = sc_error(ret); e)
                return e;
        *child = static_cast<pid_t>(ret);
        return 0;
}

int Sysdeps<Execve>::operator()(const char *path, char *const argv[], char *const envp[]) {
        auto ret = sys(PRISM_SYS_EXECVE, path, argv, envp);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

int Sysdeps<Waitpid>::operator()(pid_t pid, int *status, int flags, struct rusage *ru,
                pid_t *ret_pid) {
        /* pid semantics match: -1 (and 0) wait on any child, a positive pid
         * on that child; WNOHANG is the flag the kernel knows.  It keeps no
         * rusage accounting -- wait4() gets honest zeroes. */
        if(ru)
                memset(ru, 0, sizeof(struct rusage));
        auto ret = sys(PRISM_SYS_WAIT, pid, status, flags);
        if(int e = sc_error(ret); e)
                return e;
        *ret_pid = static_cast<pid_t>(ret);
        return 0;
}

int Sysdeps<Kill>::operator()(pid_t pid, int sig) {
        auto ret = sys(PRISM_SYS_KILL, pid, sig);
        if(int e = sc_error(ret); e)
                return e;
        return 0;
}

// ------------------------------------------------------------------ signals

int Sysdeps<Sigaction>::operator()(int signum, const struct sigaction *act,
                struct sigaction *oldact) {
        /* The kernel exchanges four u64 words -- handler, flags, restorer,
         * 8-byte mask (the x86-64 Linux rt_sigaction shape); SA_RESTORER
         * plants our restorer so a handler's `ret` lands in x86_64/signal.S.
         * SA_SIGINFO / SA_ONSTACK the kernel refuses; those errors ride
         * through untouched. */
#ifdef SIGCANCEL
        /* Thread cancellation rides on SIGCANCEL with SA_SIGINFO and tgkill; the
         * kernel has neither (user/signal.rs refuses SA_SIGINFO).  ENOSYS is the
         * answer mlibc's PthreadSignalInstaller accepts as "cancellation not
         * supported" -- any other error trips its __ensure at load time. */
        if(signum == SIGCANCEL)
                return ENOSYS;
#endif
        ksigaction kact = {}, kold = {};
        if(act) {
                kact.handler = reinterpret_cast<uintptr_t>(act->sa_handler);
                kact.flags = static_cast<uint64_t>(
                                static_cast<unsigned long>(static_cast<unsigned int>(act->sa_flags)));
                kact.flags |= SA_RESTORER;
                kact.restorer = reinterpret_cast<uintptr_t>(&__mlibc_signal_restore);
                memcpy(&kact.mask, &act->sa_mask, sizeof(kact.mask));
        }

        auto ret = sys(PRISM_SYS_SIGACTION, signum, act ? &kact : nullptr,
                        oldact ? &kold : nullptr);
        if(int e = sc_error(ret); e)
                return e;

        if(oldact) {
                oldact->sa_handler = reinterpret_cast<void (*)(int)>(kold.handler);
                oldact->sa_flags = static_cast<int>(kold.flags);
                oldact->sa_restorer = reinterpret_cast<void (*)(void)>(kold.restorer);
                memset(&oldact->sa_mask, 0, sizeof(oldact->sa_mask));
                memcpy(&oldact->sa_mask, &kold.mask, sizeof(kold.mask));
        }
        return 0;
}

int Sysdeps<Sigprocmask>::operator()(int how, const sigset_t *__restrict set,
                sigset_t *__restrict retrieve) {
        return prism_sigmask(how, set, retrieve);
}

int Sysdeps<ThreadSigmask>::operator()(int how, const sigset_t *__restrict set,
                sigset_t *__restrict retrieve) {
        /* One process, one thread: the two calls are the same word. */
        return prism_sigmask(how, set, retrieve);
}

} // namespace mlibc
