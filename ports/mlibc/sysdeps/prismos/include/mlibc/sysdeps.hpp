#pragma once

#include <mlibc/sysdep-signatures.hpp>

/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * The PrismOS sysdeps tag set (phase 3.2, step 2): every tag the kernel has
 * a syscall for.  A tag absent from this list is *missing on purpose* --
 * mlibc's generic layer then answers ENOSYS (*at() beyond AT_FDCWD, ...) -- which is the port's contract:
 * what the kernel lacks stays ENOSYS.
 */

namespace mlibc {

struct PrismOSSysdepTags :
        LibcPanic,
        LibcLog,

        /* the mandatory sixteen (all-sysdeps.hpp asserts these) */
        Exit,
        Write,
        Read,
        Open,
        Seek,
        Close,
        ClockGet,
        FutexWait,
        FutexWake,
        AnonAllocate,
        AnonFree,
        VmMap,
        VmUnmap,
        TcbSet,

        /* the rest of what the kernel serves */
        Isatty,
        Stat,
        /* the file-test pair the stat-backed check serves (ar and the
         * other binutils tools ask access() on every invocation); the
         * Faccessat half stays AT_FDCWD-only, the port's *at() contract */
        Access,
        Faccessat,
        VmProtect,
        GetPid,
        GetTid,
        FutexTid,
        Yield,
        Sleep,
        Kill,
        Fork,
        Waitpid,
        Execve,
        Dup,
        Dup2,
        Fcntl,
        Pipe,
        GetCwd,
        Chdir,
        Mkdir,
        Rmdir,
        Unlinkat,
        Rename,
        Link,
        Symlink,
        Readlink,
        Uname,
        Ioctl,
        Poll,
        Pselect,
        Sigaction,
        Sigprocmask,
        ThreadSigmask,
        Sigtimedwait,
        OpenDir,
        ReadEntries,
        Tcgetattr,
        Tcgetwinsize,
        Ttyname,
        SetUid,
        SetEuid,
        SetGid,
        SetEgid,
        SetGroups,
        Tcflush,

        /* the phase 3.3 step: the tty's write side, the identity pair and
         * the creation mask -- what an init and a shell lean on.  (The
         * mount pair and statfs are kernel-side syscalls 48 / 55 / 56,
         * but their tags live behind mlibc's linux option and its
         * kernel headers; they stay raw through pkg/include/prism_raw.h
         * until that option lands with the busybox step.) */
        Tcsetattr,
        GetPpid,
        GetUid,
        GetEuid,
        GetGid,
        GetEgid,
        Umask,

        /* the phase 3.3.4 step: sessions and process groups -- what a
         * getty / shell does before claiming a controlling terminal
         * (lstat rides on the Stat tag's AT_SYMLINK_NOFOLLOW flag). */
        SetSid,
        SetPgid,
        GetPgid,

        GetGroups,

        /* the query half of sbrk: libiberty's xmalloc asks sbrk(0) at
         * startup (its allocation tally), and the kernel's brk (21)
         * answers the current break -- found by relinking binutils
         * against the phase 3.4 library. */
        Brk,

        /* the phase 3.4 step (the busybox port): the syscall gaps the
         * shell and its applets found -- fchdir for cd, fsync for a cp
         * that flushes, the chmod pair, utimensat for touch / cp -p /
         * tar, wait4's rusage (and getrusage behind it), and the mount
         * pair + statfs the linux option carries: the tags stay raw
         * through prism_raw.h no longer, busybox's own applets are the
         * libc's callers now.  (Fdatasync rides Fsync: the kernel has
         * one flush; fchmodat and the other *at() names stay absent --
         * ENOSYS, the port's contract.) */
        Fchdir,
        Fsync,
        Fdatasync,
        Sync,
        Chmod,
        Fchmod,
        Fchownat,
        Utimensat,
        GetRusage,
        Mount,
        Umount2,
        Statfs,
        Statvfs,
        SetHostname,
        SetItimer,
        GetItimer,

        /* the init step's shutdown half: reboot / poweroff ride the
         * kernel's 57, the cmd passed raw (the linux-reboot option's
         * tag, phase 3.4's option lands with it). */
        Reboot,

        /* AF_UNIX stream sockets (kernel syscalls 90..102): what an X
         * server and its clients, or any local IPC, stand on.  send / recv /
         * sendto / recvfrom are mlibc's own wrappers over MsgSend / MsgRecv.
         * Datagram sockets and other families answer the kernel's errno. */
        Socket,
        Socketpair,
        Bind,
        Listen,
        Accept,
        Connect,
        MsgSend,
        MsgRecv,
        Shutdown,
        Sockname,
        Peername,
        GetSockopt,
        SetSockopt,

        /* phase 4.2: threads.  Clone / PrepareStack / ThreadExit live in
         * generic/thread.cpp (libc only: ld.so has no use for them);
         * pthread_create, pthread_join and friends are mlibc's own over
         * them, on top of FutexWait / FutexWake, which were already here.
         * Tgkill is pthread_kill's; cancellation (SIGCANCEL) stays
         * unsupported: its signal number (32) is past the 31 the kernel
         * tracks (SA_SIGINFO itself is served since the Xorg step). */
        Clone,
        PrepareStack,
        ThreadExit,
        Tgkill,
        /* epoll (kernel user/fd/epoll.rs, syscalls 109..111).  mlibc's
         * epoll_create / epoll_wait are built on these three. */
        EpollCreate,
        EpollCtl,
        EpollPwait,
        /* sigaltstack (kernel syscall 112): the alternate signal stack an
         * SA_ONSTACK handler runs on -- Xorg's crash handler asks for one. */
        Sigaltstack,
        /* ftruncate (kernel syscall 113): what shm_open'ed files and
         * memfds are sized with before a MAP_SHARED mmap (MIT-SHM, shm
         * clients).  truncate / fallocate / memfd_create (114..116) follow
         * below, with the rest of the Xorg step's syscalls. */
        Ftruncate,
        Truncate,
        Fallocate,
        MemfdCreate,

        /* limits, locking, CPU affinity (kernel syscalls 117..123).  The
         * kernel stores and reports the limits without enforcing them and
         * has one CPU, so these answer honestly and pin nothing.  There is
         * no prlimit tag in mlibc (getrlimit / setrlimit ride 117 / 118),
         * no mlockall (no kernel call), and the *_thread* affinity tags stay
         * absent: the kernel's affinity calls take a pid, not a tid. */
        GetRlimit,
        SetRlimit,
        Mlock,
        Munlock,
        GetAffinity,
        SetAffinity,

        /* event descriptors (kernel syscalls 124, 126..129; the options
         * linux-eventfd / -timerfd / -signalfd default to the linux option,
         * already on, so the wrappers were built and answered ENOSYS until
         * these tags existed).  What an X server's main loop and its
         * signal / timer handling stand on. */
        EventfdCreate,
        TimerfdCreate,
        TimerfdSettime,
        TimerfdGettime,
        SignalfdCreate,

        /* the *at() family (kernel syscalls 130..140, user/fd/at.rs): a
         * relative path starts at a directory descriptor.  Stat's fd_path
         * form, Faccessat, Fchownat and Unlinkat already existed (AT_FDCWD
         * only) and now send any other dirfd to the kernel.  Utimensat
         * (68) has always taken a dirfd and still serves AT_FDCWD only. */
        Openat,
        Mkdirat,
        Readlinkat,
        Renameat,
        Symlinkat,
        Linkat,
        Fchmodat
{};

struct SysdepTraits {
        static constexpr bool usesRtNetlink = false;
};

template<typename Tag>
using Sysdeps = SysdepOf<PrismOSSysdepTags, Tag>;

} // namespace mlibc