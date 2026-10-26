# The mlibc port for PrismOS (phase 3.2, step 2)

`sysdeps/prismos` — the layer an [mlibc](https://github.com/managarm/mlibc)
build needs to speak PrismOS: one directory to drop into an mlibc checkout,
one patch to register it.  It is the second step of phase 3.2 (the first
gave the kernel the eight prerequisite syscalls 36..43); the third
(ports/toolchain) built the `x86_64-prismos` cross toolchain against it and
this port into a sysroot — and, building the library with a real cross
compiler rather than a host one, caught the three latent bugs noted below.
The fifth step (the shared half: `-Ddefault_library=both`, `libc.so` +
`ld.so`) forced two more, noted below those three.

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

## Building it

The port needs Meson ≥ 1.3 and any C++23-capable x86-64 compiler; the cross
file is what makes it *PrismOS* rather than the host system:

```ini
# prismos-cross.txt
[binaries]
c = 'x86_64-prismos-gcc'     # whatever compiles freestanding x86-64 for you
cpp = 'x86_64-prismos-g++'

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

**The full pipeline lives in ports/toolchain** (phase 3.2, step 3):
`bootstrap.sh` drives wrappers → headers → library → `/bin/hello` →
`verify.sh` with any host compiler behind the prefixed names, and
`build.sh` swaps in the real `x86_64-prismos` cross tools (binutils 2.44 +
gcc 14.2.0) before rebuilding the library.  `make mlibc` and
`make usr-mlibc` run it from the PrismOS root.

This step was verified against mlibc 7.0.0 (meson 1.12, gcc 14): the full
`ansi` + `posix` options build, the headers and the four crt objects
install into a sysroot, and a `write()`-based hello world links as the
static PIE the kernel's loader expects (`-pie --no-dynamic-linker -z
separate-code -z max-page-size=4096`), with `write` reaching `syscall 1`
and the restorer reaching `syscall 12` under disassembly.  Step 3's
`verify.sh` (ports/toolchain) checks the same contract, twenty ways,
against the cross-built `/bin/hello`.

## Three latent bugs the cross build caught (fixed in this port)

Building the library with the real cross compiler — whose include paths
match the sysroot exactly, with no host headers leaking in — exposed what
the step-2 host verification could not:

1. **The abi-bits install list missed three headers the library's own
   headers need**: `signal.h` (any program including `<signal.h>` failed —
   the step-2 hello world never did), `epoll.h` and `clone-flags.h`.  The
   files were in the port's tree but not in `install_headers`; they are
   now.
2. **mlibc 7.0.0's x86_64 `strcmp` is an IFUNC** (an AVX2/SSE4.2/SSE2
   resolver picked by CPUID).  Its `R_X86_64_IRELATIVE` relocation lands in
   `DT_JMPREL` — which the kernel's loader refuses outright ("PLT
   relocations (link statically)", sys/kern/src/user/elf.rs).  The port
   now sets `MLIBC_IFUNCS_SUPPORTED` off through the mechanism mlibc's own
   build provides for exactly this ("sysdeps might want to disable
   unsupported options"), so the generic `strcmp` builds instead.  The
   step-2 hello world linked because it never called `strcmp`.
3. **`struct rusage` was incomplete where the port used it** (`sizeof` in
   the Waitpid tag): the definition sits behind a feature-test cascade
   (`_DEFAULT_SOURCE` / `__MLIBC_XOPEN`) that a linux-target g++ satisfies
   implicitly through its predefined `_GNU_SOURCE` — a host (or any
   GNU-userland) compiler papered over the missing include.  The port now
   includes `<abi-bits/resource.h>` itself, and the cross gcc's
   `gnu-user.h` tm (ports/toolchain/patches) gives PrismOS the same C++
   cascade, so user programs see the same headers the library did.

## Two more the shared build caught (step 5, fixed in this port)

Building the library *shared* — `ld.so` linked `-nostdlib` against nothing,
`libc.so` behind it — exposed what every static link papered over:

1. **`__do_syscall_ret` pulled the libc's TLS into the rtld.**  The helper
   stores `errno`, and `errno` is `__mlibc_errno` — a libc TLS symbol a
   `-nostdlib` link against nothing cannot resolve.  Nothing in the rtld
   calls it (the sysdep wrappers read the raw answer through `sc_error()`),
   so it is now compiled out of the rtld — linux's own guard.  It stays in
   the libc, and stays dead weight in the static archive: nothing pulls
   the libc's own twin member for it.
2. **The restorer's address was GOT-shaped.**  `Sigaction` plants
   `&__mlibc_signal_restore` in the kernel's sigaction word; a
   default-visibility reference goes through the GOT, and the resulting
   `R_X86_64_GLOB_DAT` carries a symbol — `relocateSelf` accepts
   `R_X86_64_RELATIVE` and traps on anything else, so `ld.so` could never
   have started.  The symbol is hidden now (`.hidden` in `x86_64/signal.S`,
   the attribute on the declaration).  The linux alternative — compiling
   the whole `Sigaction` wrapper out of the rtld — breaks the static
   archive's member pull (the libc's twin member would come in for it and
   collide on every wrapper they share), so the port keeps the wrapper
   everywhere and hides the reference instead.

## What comes next

Both remaining steps of the phase are done — the four ports on this
library (step 4, `apps/` — usr-merged into `/usr/bin` now) and the ELF loader's dynamic linking (step 5:
`PT_INTERP`, this port's `ld.so` + `libc.so` built `-Ddefault_library=both`,
`DT_NEEDED` resolution in `/lib` and `/usr/lib`, RELRO through `mprotect`).
What is left is the *run*: a dev box booting the image and executing
`/bin/hello-dyn` from the far side of `ld.so`.
