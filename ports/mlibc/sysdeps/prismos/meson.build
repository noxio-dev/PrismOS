#-
# SPDX-License-Identifier: GPL-3.0-or-later
#
# The PrismOS sysdeps port (phase 3.2, step 2): sources, headers and the
# crt objects, wired the way mlibc's own ports are (sysdeps/demo is the
# skeleton this follows).  x86_64 only -- the architecture PrismOS runs.

sysdep_supported_options = {
        'posix': true,
}

# The kernel's ELF loader links statically, full stop: it refuses DT_JMPREL
# (sys/kern/src/user/elf.rs -- "PLT relocations (link statically)"), and an
# IFUNC's R_X86_64_IRELATIVE lands exactly there.  Turning mlibc's IFUNC
# support off makes the generic strcmp (string.cpp's alias of
# __mlibc_strcmp_default) build instead of the x86_64 resolver -- the port's
# contract, same rule as pthread_create and sockets: what the kernel lacks
# stays out.  Found by step 3's deeper check (a program that includes
# <signal.h> and calls strcmp): the step-2 hello world never hit either.
internal_conf.set10('MLIBC_IFUNCS_SUPPORTED', false)

prismos_include_dirs = [
        include_directories('include'),
]

rtld_include_dirs += prismos_include_dirs
libc_include_dirs += prismos_include_dirs

rtld_sources += files(
        'generic/sysdeps.cpp',
        'generic/syscall.cpp',
        'x86_64/signal.S',
)

libc_sources += files(
        'generic/entry.cpp',
        'generic/sysdeps.cpp',
        'generic/syscall.cpp',
        'x86_64/signal.S',
)

if not no_headers
        install_headers(
                'include/abi-bits/access.h',
                'include/abi-bits/aio.h',
                'include/abi-bits/auxv.h',
                'include/abi-bits/blkcnt_t.h',
                'include/abi-bits/blksize_t.h',
                'include/abi-bits/clockid_t.h',
                'include/abi-bits/clone-flags.h',
                'include/abi-bits/dev_t.h',
                'include/abi-bits/epoll.h',
                'include/abi-bits/errno.h',
                'include/abi-bits/fcntl.h',
                'include/abi-bits/fd_set.h',
                'include/abi-bits/fsblkcnt_t.h',
                'include/abi-bits/fsfilcnt_t.h',
                'include/abi-bits/gid_t.h',
                'include/abi-bits/in.h',
                'include/abi-bits/ino_t.h',
                'include/abi-bits/inotify.h',
                'include/abi-bits/ioctls.h',
                'include/abi-bits/ipc.h',
                'include/abi-bits/limits.h',
                'include/abi-bits/mode_t.h',
                'include/abi-bits/mqueue.h',
                'include/abi-bits/msg.h',
                'include/abi-bits/nlink_t.h',
                'include/abi-bits/packet.h',
                'include/abi-bits/pid_t.h',
                'include/abi-bits/poll.h',
                'include/abi-bits/ptrace.h',
                'include/abi-bits/random.h',
                'include/abi-bits/reboot.h',
                'include/abi-bits/resource.h',
                'include/abi-bits/rlim_t.h',
                'include/abi-bits/route.h',
                'include/abi-bits/sa_family_t.h',
                'include/abi-bits/sched_param.h',
                'include/abi-bits/seek-whence.h',
                'include/abi-bits/sem.h',
                'include/abi-bits/shm.h',
                'include/abi-bits/sig-limits.h',
                'include/abi-bits/sigevent.h',
                'include/abi-bits/signal.h',
                'include/abi-bits/sigset_t.h',
                'include/abi-bits/sigval.h',
                'include/abi-bits/sockaddr_storage.h',
                'include/abi-bits/socklen_t.h',
                'include/abi-bits/socket.h',
                'include/abi-bits/stat.h',
                'include/abi-bits/statfs.h',
                'include/abi-bits/statvfs.h',
                'include/abi-bits/statx.h',
                'include/abi-bits/suseconds_t.h',
                'include/abi-bits/termios.h',
                'include/abi-bits/time.h',
                'include/abi-bits/uid_t.h',
                'include/abi-bits/utmp-defines.h',
                'include/abi-bits/utmpx.h',
                'include/abi-bits/utsname.h',
                'include/abi-bits/vm-flags.h',
                'include/abi-bits/vt.h',
                'include/abi-bits/wait.h',
                'include/abi-bits/xattr.h',
                subdir: 'abi-bits',
                follow_symlinks: true
        )

        install_headers(
                'include/prismos/syscall.h',
                subdir: 'prismos',
        )
endif

if not headers_only
        crt = custom_target('crt1',
                build_by_default: true,
                command: c_compiler.cmd_array() + ['-c', '-o', '@OUTPUT@', '@INPUT@'],
                input: 'x86_64/crt-src/crt1.S',
                output: 'crt1.o',
                install: true,
                install_dir: get_option('libdir')
        )

        custom_target('Scrt1',
                build_by_default: true,
                command: c_compiler.cmd_array() + ['-c', '-o', '@OUTPUT@', '@INPUT@'],
                input: 'x86_64/crt-src/Scrt1.S',
                output: 'Scrt1.o',
                install: true,
                install_dir: get_option('libdir')
        )

        custom_target('crti',
                build_by_default: true,
                command: c_compiler.cmd_array() + ['-c', '-o', '@OUTPUT@', '@INPUT@'],
                input: 'x86_64/crt-src/crti.S',
                output: 'crti.o',
                install: true,
                install_dir: get_option('libdir')
        )

        custom_target('crtn',
                build_by_default: true,
                command: c_compiler.cmd_array() + ['-c', '-o', '@OUTPUT@', '@INPUT@'],
                input: 'x86_64/crt-src/crtn.S',
                output: 'crtn.o',
                install: true,
                install_dir: get_option('libdir')
        )
endif
