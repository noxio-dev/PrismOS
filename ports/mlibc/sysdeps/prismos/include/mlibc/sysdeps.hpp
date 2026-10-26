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
        Tcgetwinsize
{};

struct SysdepTraits {
        static constexpr bool usesRtNetlink = false;
};

template<typename Tag>
using Sysdeps = SysdepOf<PrismOSSysdepTags, Tag>;

} // namespace mlibc
