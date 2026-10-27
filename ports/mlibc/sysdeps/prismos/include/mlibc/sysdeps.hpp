#pragma once

#include <mlibc/sysdep-signatures.hpp>

/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * The PrismOS sysdeps tag set (phase 3.2, step 2): every tag the kernel has
 * a syscall for.  A tag absent from this list is *missing on purpose* --
 * mlibc's generic layer then answers ENOSYS (pthread_create, sockets,
 * *at() beyond AT_FDCWD, rusage, ...) -- which is the port's contract:
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
        OpenDir,
        ReadEntries,
        Tcgetattr,
        Tcgetwinsize,

        /* the phase 3.3 step: the tty's write side, the identity pair and
         * the creation mask -- what an init and a shell lean on.  (The
         * mount pair and statfs are kernel-side syscalls 48 / 55 / 56,
         * but their tags live behind mlibc's linux option and its
         * kernel headers; they stay raw through apps/include/prism_raw.h
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
        GetPgid
{};

struct SysdepTraits {
        static constexpr bool usesRtNetlink = false;
};

template<typename Tag>
using Sysdeps = SysdepOf<PrismOSSysdepTags, Tag>;

} // namespace mlibc
