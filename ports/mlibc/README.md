# The mlibc port for PrismOS (phase 3.2, step 2)

`sysdeps/prismos` — the layer an [mlibc](https://github.com/managarm/mlibc)
build needs to speak PrismOS: one directory to drop into an mlibc checkout,
one patch to register it.  It is the second step of phase 3.2 (the first
gave the kernel the eight prerequisite syscalls 36..43); the third will
build it with Meson against a cross compiler and install it into a sysroot
this port is already shaped for.

```
ports/mlibc/
├── mlibc-prismos.patch          the seven-line registration in mlibc's meson.build
└── sysdeps/prismos/
    ├── meson.build              sources, headers, the crt objects (the sysdeps/demo shape)
    ├── include/
    │   ├── mlibc/sysdeps.hpp    the tag list: every sysdep the kernel serves
    │   ├── prismos/syscall.h    syscall numbers 0..43, the raw call convention
    │   └── abi-bits/…           the x86-64 ABI headers (see PROVENANCE.txt)
    ├── generic/
    │   ├── syscall.cpp          __do_syscall0..6 — rax = nr, rdi/rsi/rdx/r10/r8/r9, -errno in rax
    │   ├── sysdeps.cpp          the tags, one syscall each
    │   └── entry.cpp            __mlibc_entry: the initial stack to main
    └── x86_64/
        ├── crt-src/crt1.S, Scrt1.S, crti.S, crtn.S
        └── signal.S             the SA_RESTORER restorer (syscall 12, sigreturn)
```

## The contract

**Everything the kernel has, the port speaks.**  mlibc's generic layer
calls *tags* (`sys_open`-shaped functions, `include/mlibc/sysdeps.hpp`);
each one maps to exactly one PrismOS syscall: `Read`→13, `Write`→1,
`Open`→14, `Stat`→19/20, `VmMap`→22, `FutexWait`/`FutexWake`→37,
`Sigaction`→38, `Poll`→41, `Pselect`→42, `ClockGet`→43, and so on through
the whole table (see `include/prismos/syscall.h`).  The layouts cross
unchanged because the kernel made them Linux-shaped on purpose: `struct
stat` is the x86-64 Linux one, the sigaction exchanged with the kernel is
the four-u64 (handler / flags / restorer / 8-byte mask) rt_sigaction
shape, `sigprocmask` trades the low 8 bytes of a sigset_t, `poll` speaks
the Linux pollfd, the SA_* / CLOCK_* / errno values are the classic ones.
`TcbSet` parks the TCB in the FS base through `arch_prctl` — the x86-64
way the kernel switched with the process since the first step of this
phase.

**What the kernel lacks stays ENOSYS** — the rule of the phase.  Tags the
kernel cannot serve are simply absent from the tag list, and mlibc's
generic layer answers `ENOSYS` on its own: `pthread_create` (no clone),
sockets, `*at()` past `AT_FDCWD`, rusage accounting (wait4 gets honest
zeroes), `fchmod` and the whole permission family (phase 3.5).  A few
corners are emulated where the emulation is exact: `pipe2`'s flags ride
`fcntl` (`O_CLOEXEC`→`F_SETFD`, `O_NONBLOCK`→`F_SETFL`), `opendir` checks
`S_ISDIR` itself (the kernel has no `O_DIRECTORY` bit), and `pselect`
refuses a signal mask rather than pretend (`select` works).

Two ABI notes worth remembering.  The **O_* flags are the kernel's own**
(`O_READ` 1, `O_WRITE` 2, `O_CREATE` 4, `O_TRUNC` 8, `O_APPEND` 16,
`O_EXCL` 32, `O_NONBLOCK` 64, `O_CLOEXEC` 128 — `fs/vfs.rs`), not Linux's
octal encodings, so this port's `abi-bits/fcntl.h` defines them; the
Linux-only names (`O_DIRECTORY`, `O_TMPFILE`, ...) are reference constants
so generic library code compiles, and the kernel answers `-EINVAL` if one
is actually passed.  And **`open()` takes no mode** — `mkdir` carries it —
so `Open` drops the argument on the floor.

## Applying it

```sh
git clone https://github.com/managarm/mlibc
cd mlibc
git apply /path/to/PrismOS/ports/mlibc/mlibc-prismos.patch
cp -r /path/to/PrismOS/ports/mlibc/sysdeps/prismos sysdeps/
```

## Building it (until the cross toolchain of step 3 exists)

The port needs Meson ≥ 1.3 and any C++23-capable x86-64 compiler; the
cross file is what makes it *PrismOS* rather than the host system:

```ini
# prismos-cross.txt
[binaries]
c = 'x86_64-linux-clang'     # whatever compiles freestanding x86-64 for you
cpp = 'x86_64-linux-clang++'

[host_machine]
system = 'prismos'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
```

```sh
meson setup build --cross-file prismos-cross.txt
ninja -C build
meson install -C build --destdir sysroot
```

This step was verified against mlibc 7.0.0 (meson 1.12, gcc 14): the full
`ansi` + `posix` options build, the headers and the four crt objects
install into a sysroot, and a `write()`-based hello world links as the
static PIE the kernel's loader expects (`-pie --no-dynamic-linker -z
separate-code -z max-page-size=4096`), with `write` reaching `syscall 1`
and the restorer reaching `syscall 12` under disassembly.  What cannot be
tested off-target is the run itself: that is step 3's business (the cross
compiler, `/bin/hello` rebuilt against mlibc, `make test`).

## What comes next (the rest of phase 3.2)

* build mlibc with Meson against a cross C/C++ compiler — headers first,
  then the compiler, then the library — and install it into a sysroot;
* port `/bin/hello`, `/bin/fork`, `/bin/fd` and `/bin/posix` off the mini
  libc (`usr/lib`) onto this one;
* the ELF loader learns dynamic linking: `PT_INTERP`, mlibc's `ld.so`,
  `DT_NEEDED` resolution in `/lib` and `/usr/lib`, RELRO through
  `mprotect`.
