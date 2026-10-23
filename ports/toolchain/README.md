# The x86_64-prismos toolchain, the mlibc sysroot, the ports and the dynamic half (phase 3.2, steps 3 + 4 + 5)

`ports/toolchain` is the phase's third, fourth and fifth steps: *build mlibc
with Meson against a cross C/C++ compiler (headers first, then the compiler,
then the library), install it into a sysroot* -- and rebuild `/bin/hello`
against the result as the static PIE the kernel's loader expects (step 3),
then port `/bin/hello`, `/bin/fork`, `/bin/fd` and `/bin/posix` off the mini
libc onto it (step 4, the sources in `usr/mlibc`), and finally teach the
loader dynamic linking (step 5): the sysroot's shared half (`libc.so` +
`ld.so`) and a dynamically linked `hello` whose `PT_INTERP` the kernel
honours.

```
ports/toolchain/
├── bootstrap.sh               the pipeline: wrappers → mlibc (headers, library) → progs → verify
├── build.sh                   the real thing: binutils + GCC for x86_64-prismos, then mlibc with them,
├──                              then the shared half (dynlib) and the dynamic hello (dyn)
├── cross.txt                  the meson cross file (system = prismos)
├── verify.sh                  the loader's own checks, statically (per program, and `dyn` for the
├──                              dynamic chain: the executable, ld.so, libc.so)
├── hello.c                    /bin/hello rebuilt against mlibc -- step 3's proof
└── patches/
    ├── binutils-2.44-prismos.patch   the triplet: config.sub + bfd + ld + gas
    └── gcc-14.2.0-prismos.patch      the triplet + the ABI: config.sub + config.gcc (linux's
                                       tm_file order + i386/prismos.h) + libgcc

usr/mlibc/                     the step-4 ports: hello/, fork/, fd/, posix/ + prism_raw.h
                               (hello.c doubles as the step-5 dynamic proof: build.sh stage dyn)
```

## The order (the phase's todo, verbatim)

**headers first.**  `bootstrap.sh stage headers` installs mlibc's headers
alone (`-Dheaders_only=true`) into `build/toolchain/usr/x86_64-prismos/usr/include`
-- the sysroot a cross compiler is built against (`--with-sysroot`).

**then the compiler.**  `build.sh stage binutils` and `stage gcc` build the
cross tools for the `x86_64-prismos` triplet: `as`, `ld`, `ar`, `ranlib`,
`strip`, `objdump`, ... then `gcc` / `g++` 14.2.0 (`--without-headers`: the
compilers and libgcc, no target runtime -- mlibc *is* the target runtime).
Neither binutils nor GCC know a `prismos` OS out of the box, so `patches/`
teaches them the way every new OS does (SerenityOS's `x86_64-pc-serenity`
being the family recipe): `config.sub`'s OS list, BFD's / ld's / gas's ELF
target cases, GCC's `config.gcc` and libgcc's `config.host`.

**then the library.**  `build.sh stage mlibc` builds mlibc proper -- the
whole `ansi` + `posix` surface the kernel serves, static, plus the four crt
objects -- with the real cross compiler, under the same meson cross file the
bootstrap used.  `libc.a` lands in the sysroot.

**the proof.**  `bootstrap.sh stage hello` (build.sh calls the same stage)
compiles `hello.c` with the cross compiler and links it as the loader's
contract spells out: `-pie --no-dynamic-linker -z separate-code -z
max-page-size=4096 -z noexecstack -z norelro`, the crt sandwich a
`-static-pie` driver assembles (`Scrt1.o crti.o crtbeginS.o … libc.a
crtendS.o crtn.o`).  `verify.sh` then checks, statically, everything the
kernel's ELF loader (`sys/kern/src/user/elf.rs`) checks.

**the ports (step 4).**  `bootstrap.sh stage progs` (or `stage prog <name>`
for one; `make usr-mlibc`; `build.sh stage progs` on the real tools)
compiles the four programs of `usr/mlibc` the same way against the same
sysroot and links the same sandwich four times -- `hello`, `fork`, `fd`
and `posix` land in `build/usr/bin-mlibc/`.  `verify.sh <elf> <sysroot>
<program>` checks each one with *its own* expectations: every syscall the
program makes (the tag layer's `mov $nr,%edi … call __do_syscallN`, the
raw shim's `mov $nr,%eax … syscall`) and the symbols those paths live in
-- 21 checks for hello, 27 for fork, 32 for fd, 50 for posix, 130 across
the four.  With no program name the step-3 proof set applies
(write / the restorer / clock_gettime).

**the dynamic half (step 5).**  `build.sh stage dynlib` re-runs meson on
the same patched mlibc tree with `-Ddefault_library=both`: the static
`libc.a` and the four programs never notice, and the sysroot gains
`libc.so` (the C library as an `ET_DYN`, `SONAME libc.so`) and `ld.so`
(the rtld: `-nostdlib`, `SONAME ld.so`, self-relocating).  `build.sh
stage dyn` (`make usr-mlibc-dyn`) then links the *same* `usr/mlibc/hello`
sources against `libc.so` instead of `libc.a` -- `--dynamic-linker
/lib/ld.so`, `-z now` (mlibc's rtld zeroes `GOT[1]/GOT[2]`: BIND NOW or
nothing) -- and `stage dynverify` runs `verify.sh <elf> <sysroot> dyn`:
the executable's dynamic shape (PT_INTERP `/lib/ld.so`, `DT_NEEDED
libc.so`, `DT_JMPREL`, JUMP_SLOT relocations, `__mlibc_entry` undefined
for the library to answer), `ld.so`'s self-containment (`RELATIVE`-only
relocations, no undefined symbols) and its exact syscall diet (open /
read / close / lseek / fstat, mmap / munmap / mprotect, arch_prctl,
getpid -- every one confirmed under disassembly), and `libc.so`'s exports
with hello's library calls now living in it: 47 checks.  The boot image
carries the trio (a stripped `/lib/ld.so` + `/lib/libc.so` +
`/bin/hello-dyn`; `make root`).

## The two paths

**`bootstrap.sh`** (fast, minutes): the cross compiler is a *prefixed
pass-through* around any C23/C++23 host compiler, exactly the arrangement
the port's step-2 verification used (meson 1.12, gcc 14) -- mlibc isolates
its own includes (`-nostdinc` for the C++ sources, `-ffreestanding`, its
internal include dirs first), and the cross file is what makes the build
*PrismOS* rather than the host system.  Everything downstream of the
compiler names works identically.

**`build.sh`** (the real thing, an hour or two): binutils + GCC built for
`x86_64-prismos`, installed into the same prefix under the same names --
the wrappers are simply answered by real binaries from then on -- and mlibc
rebuilt with them.  Both paths end at the same `verify.sh`, and only this
path exposes the port bugs a host compiler masks (three were found and
fixed; see ports/mlibc/README.md).

## The gcc patch's ABI decisions

The `x86_64-*-prismos` target config.gcc adds is `x86_64-*-linux*`'s
header cascade plus a target header of our own, each piece forced by
something PrismOS already decided:

- **`glibc-stdint.h`** (not newlib's): PrismOS's ABI is Linux-shaped on
  purpose -- the kernel speaks the x86-64 Linux `struct stat`, the Linux
  pollfd, the Linux O_*-shaped rt_sigaction.  mlibc's x86_64 headers assert
  that (`__INT_FAST8_TYPE__` = `signed char`, fast16/32 = `long int`), and
  only the glibc stdint layout gives those.
- **`gnu-user.h`** (the top-level one): the GNU C++ feature-test cascade
  (`CPLUSPLUS_CPP_SPEC -D_GNU_SOURCE`) g++ itself has on GNU systems --
  without it mlibc's `mlibc-config.h` resolves `_DEFAULT_SOURCE` off and
  half the abi-bits (starting with `struct rusage`) never define.  It also
  brings the crtbegin/crtend link sandwich that matches mlibc's sysroot.
- **`i386/gnu-user-common.h` + `i386/gnu-user64.h`** (linux's order): the
  LINK_SPEC -- without it the driver has no words for `-shared` /
  `-static` / `-pie` / `-dynamic-linker` and *silently drops them*.  The
  step-3 patch's generic-elf order never noticed (the four programs link
  through `$LD` directly); the first `-shared` driver link -- mlibc's
  `ld.so`, through meson -- failed on a missing `_DYNAMIC` for exactly
  this reason.
- **`i386/prismos.h`** (new): the link emulations (`elf_x86_64` & co), the
  dynamic linker path (`/lib/ld.so`, mlibc's rtld -- what the kernel's
  `PT_INTERP` handling reads), and the `__prismos__` / unix / posix
  preprocessor builtins `i386/gnu-user-common.h` asks every GNU-user
  target for.
- **`default_use_cxa_atexit=yes`**: mlibc's exit path registers destructors
  through `__cxa_atexit`, whose `__dso_handle` lives in `crtbeginS.o` only
  when this is on (the default for GNU-userland targets).
- **`libgcc/config.host`'s `extra_parts`**: the five crt objects
  (`crtbegin.o crtend.o crtbeginS.o crtendS.o crtbeginT.o`) -- the S pair
  the shared and PIE links need, the T one `-static` asks for; the bare
  ELF target installs none of them.

And one build-side decision: `--with-newlib --without-headers` sets
`inhibit_libc` (libgcc must not include libc headers -- it builds before
mlibc proper exists), `--disable-fixincludes` keeps fixincludes from
shadowing a partial copy of the sysroot's headers into `gcc/include-fixed`,
and the one-line libgcov.h guard (`HAVE_SYS_MMAN_H && !inhibit_libc`) keeps
the gcov runtime -- meaningless without a libc -- out of that build.

## What `make test` still is

Everything above verifies statically -- the ELF shape the loader maps, the
dynamic section it reads (static: `DT_RELA`, never `DT_NEEDED` /
`DT_JMPREL`; dynamic: `PT_INTERP` / `DT_NEEDED` / BIND NOW exactly because
the rtld resolves them), the syscalls the disassembly reaches (per
program: write, fork, wait, stat, mmap, uname, ... plus the restorer's 12
and, in the step-3 proof, clock_gettime; in the dyn mode, the loader's own
diet on top).  The *run* is what is left of the phase: `make test` boots
the image under QEMU -- which since step 5 carries `/lib/ld.so`,
`/lib/libc.so` and `/bin/hello-dyn`, so the first `exec hello-dyn` runs
the whole dynamic chain on the target -- and needs a box with QEMU, clang
and cargo (the sandbox this step ran in has none of the three).  The
scripts here are what that box runs.
