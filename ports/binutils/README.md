# The binutils utilities as PrismOS programs (phase 4.1)

```
ports/binutils/
└── build.sh     the port: fetch → the prism-cc driver → configure → build →
                 stage (stripped, the cross strip) → verify
```

Phase 4.1 of the roadmap, verbatim: *complete binutils in the image:
cross-built `ar`, `ranlib`, `strip`, `objdump`, `nm`, installed and used
by the builds*.  Each word is a piece of this port:

**complete binutils in the image.**  The five utilities become PrismOS
executables — `ET_DYN` static PIEs with the segment geometry the kernel's
ELF loader (`sys/kern/src/user/elf.rs`) demands, linked against mlibc,
stripped under the 4 MiB `MAX_ELF_FILE` cap — and land in the image's
`/usr/bin` (`make root` copies `build/binutils-out`).  They are the first
real software the system carries for *itself*: the seed of phase 5's
self-hosting, the tools `as` and `ld` will sit next to when 5.1 builds
them.  Sizes, stripped: `ar` 1.9 MiB, `ranlib` 1.9 MiB, `nm` 1.9 MiB,
`strip` 2.0 MiB, `objdump` 3.2 MiB (the disassembler tables), ~11 MiB
together — the reason `mkimage.py`'s root grew from 64 to 96 MiB.

**cross-built.**  binutils 2.44 — the same tarball and the same triplet
patch `ports/toolchain/patches/` already fetches for the cross tools —
configured here with `--host=x86_64-prismos --target=x86_64-prismos`,
build being whatever machine runs the script.  Host and target are
PrismOS, so the utilities are compiled against the mlibc sysroot and
linked as the loader expects; the libraries they need (libiberty, bfd,
opcodes, libsframe, the in-tree zlib) are archived by the **cross**
`x86_64-prismos-ar` / `x86_64-prismos-ranlib` — this port's build is
itself the "used by the builds" of the phase's sentence.  gas and ld are
deliberately absent (`--without-gnu-as --without-gnu-ld`): they are
phase 5.1's "native binutils", not this phase's.

**the driver.**  binutils' own configure and make drive everything
through `prism-cc` (generated at `build/binutils-native/prism-cc`), a
mini compiler driver that answers to the `CC` name and does what the
two toolchain paths both need:

* a *compile* goes through `x86_64-prismos-gcc` with the do_prog
  discipline — `-nostdinc`, the compiler's own headers, the mlibc
  sysroot, `-ffreestanding -fPIE -fno-stack-protector
  -fno-asynchronous-unwind-tables -fno-math-errno` — the same command
  `tools/prism.py` builds every app with;
* a *link* assembles the static-PIE sandwich: `Scrt1 / crti /
  crtbeginS / the objects and archives / --start-group libc.a ... --
  end-group / crtendS / crtn`, `-pie --no-dynamic-linker -z
  separate-code -z max-page-size=4096 -z noexecstack -z norelro`.

Because it calls the *prefixed* names behind everything, the bootstrap
wrappers (the fast path, `make mlibc`) and `build.sh`'s real cross tools
work identically — the port never knows which path it is on.  One detail
the driver owns: **libc.a leads the link group**.  libiberty's `getopt.o`
and `fnmatch.o` are `REQUIRED_OFILES` — always in the archive — and a
static libc that also defines them would collide the moment both members
get pulled (a shared libc never extracts members, which is why native
builds never see this).  With mlibc scanned first, `getopt_long` and
friends resolve from the library and libiberty's copies are never
pulled; the group keeps bfd↔zlib↔libiberty's mutual dependencies
satisfiable in both directions.

**what the port asked the libc for.**  The utilities parse their flags
with GNU `getopt_long`, which mlibc only builds with its **glibc-compat
option on** (`options/glibc/generic/getopt.cpp`).  The PrismOS sysdeps
port now declares `'glibc': true` — the option set finally matching the
deliberately Linux-shaped ABI the gcc patch already chose
(`glibc-stdint.h`, `gnu-user.h`).  What the kernel lacks stays `ENOSYS`
by the port's contract, and degrades honestly: libiberty's `stack-limit`
asks `getrlimit` at startup, mlibc warns, the tool carries on with its
default.

**installed.**  `stage stage` strips the five with the cross
`x86_64-prismos-strip` into `build/binutils-out` (the kernel reads whole
files: 18 MiB unstripped would not fit the 4 MiB cap), and the
Makefile's root rule installs them at `/usr/bin/{ar,ranlib,nm,objdump,
strip}`.  `readelf`, `objcopy`, `size`, `strings` and `addr2line` build
in the same tree and would stage the same way — they are one line each
when a later phase wants them; the roadmap's five are this phase's.

**and used by the builds.**  Three wirings make the cross binutils the
one every build runs on, in both toolchain paths:

* `verify.sh` now defaults its `READELF` / `OBJDUMP` / `NM` to the
  prefixed tools when the toolchain has them — every loader-contract and
  syscall-diet check of every app and of this port disassembles through
  binutils built for the triplet (bootstrap wrappers on the fast path,
  the real cross tools on `build.sh`'s);
* `bootstrap.sh`'s wrapper set gained `objdump` and `readelf` — the fast
  path's prefixed set was incomplete;
* this port's own build, as above: `AR`, `RANLIB` archive the libraries,
  the cross strip stages the binaries.

## The kernel's one number

`objdump -d` on a full utility walks more stack than a hello world ever
asked for: under `prism_emu` (which runs on the kernel's own stack
size) the phase 2.4 value — 16 pages, 64 KiB — faulted eight bytes
*below* the stack on a 1.9 MiB input.  `ELF_STACK_SIZE` is 256 pages
(1 MiB) since; the emulator matched, so what fits here fits there.

## The proof without a machine

`tests/binutils_test.py` (`make bintest`) compiles `prism_emu.c` — the
ptrace runner that maps a static PIE at `USER_BASE`, applies its
`R_X86_64_RELATIVE` relocations, builds the SysV stack and answers the
PrismOS syscall ABI, rewriting to the Linux numbers — and drives the
five through real work on this host, with the host's own binutils 2.44
(the same release the port builds) as the judge, the way `fs_test.py`
uses `e2fsck`:

* `ar rc` + `ranlib`: the PrismOS archive and the host's are **the same
  file, byte for byte** (both deterministic: timestamps, uid, gid, mode
  zeroed);
* `nm` listings and `objdump -d` disassembly: text for text — including
  the stress run, a full `objdump -d` of the 1.9 MiB `ar` itself,
  367,021 lines out, identical;
* `strip -o`: byte for byte, and `nm` on the result honestly says "no
  symbols";
* the failure paths: a missing file exits 1 with the errno text, no
  arguments prints the usage and exits 2.

The emulator's file surface grew with the phase — `stat` / `fstat` /
`lstat`, `unlink` / `rename` (the archive and rename dance), `umask`,
`munmap` / `mprotect`, a real `lseek`, `fcntl`, the clock pair — all
rewritten to the Linux calls the kernel's own answers mirror, and the
PrismOS `O_*` bits (the kernel's own values, not Linux's) translated on
the way through.  `O_CREATE` files get 0o644, exactly what the kernel's
`vfs.rs` hardcodes.

## Usage

```
make binutils                        # or: sh ports/binutils/build.sh
sh ports/binutils/build.sh list      # fetch cc configure build stage verify
make bintest                         # the emulated proof, 17 checks
```

Requirements: whatever `make mlibc` already needed, plus the binutils
tarball (stage fetch shares `build/toolchain-src` with
`ports/toolchain`, tarball and patch downloaded once).  A full run is a
couple of minutes here (two cores); the five binaries land in
`build/binutils-out`, each verified against the loader's contract by
`ports/toolchain/verify.sh` — the same checks every app passes, with
the diet each tool actually speaks, traced under the emulator:
write / read / open / close / lseek, the stat pair and lstat, mmap with
munmap, fcntl's close-on-exec, ioctl's isatty probe, getpid /
arch_prctl from the library's own startup, and dup / unlink for ar's
temp-file dance.
