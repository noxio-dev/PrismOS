# apps/ — the PrismOS packages

A userland program on PrismOS is a **package**: a directory under `apps/`
carrying a `prism.toml` manifest.  The mini libc is gone — **mlibc is the
only libc** — and `tools/prism.py` (the `./prism` shim at the repo root) is
the builder.  Eight packages live here: `hello`, `fork`, `fd`, `posix`
(stat, mmap, directories, uname, termios and the phase 3.3.4 session
scenario: setsid, setpgid, `kill(-pgrp)`, lstat on a symlink), `cube`
(the kernel's spinning cube), `tmpfs` (the phase 3.3 RAM file system,
exercised from ring 3), `init` (the session keeper the kernel starts as
pid 1: it mounts `/proc` and `/tmp`, then respawns a shell on the
console and a getty on `/dev/ttyS0`) and `sh` (the shell each of those
terminals gets: builtins, PATH exec, and the session / foreground-group
setup of 3.3.4 before its first prompt).

## The five-minute tour

```
$ ./prism init demo            # scaffold apps/demo (manifest + src/main.c)
$ ./prism build demo           # compile + link + verify + stage, stripped
$ make                         # the image picks it up: /usr/bin/demo
$ ./prism run                  # boot it (make run-uefi-kvm)
prism$ demo
hello from demo (uptime 1437 ms)
```

(The session prompt is `prism$ ` now — init + the userland shell own
the console; the kernel shell's `prism# ` is the rescue mode,
`kernel_options="init=kshell"` in loader.conf.)

No Makefile wiring, no editing anywhere else: drop a directory with a
manifest under `apps/` and the next `make` builds it, checks it against the
loader's contract, strips it under the kernel's 4 MiB whole-file read, and
installs it at the manifest's `[install]` path.

## The manifest

```toml
[package]
name = "hello"              # must match the directory name
version = "0.1.0"           # informational
description = "..."         # informational

[build]
sources = ["src/hello.c"]   # app-relative; glob patterns allowed
cflags = []                 # extra compile flags (the base set is fixed)
ldflags = []                # extra link flags
output = "build/hello"      # app-relative; where the binary lands
strip = true                # the kernel reads whole files (4 MiB cap)
verify = true               # verify.sh's loader-contract checks

[install]
path = "/usr/bin/hello"     # where it lands in the image's root

[verify]                     # optional: the program's own ABI
syscalls = [1, 7, 3, 4, 5]  # every syscall it makes, by number
symbols = ["write", "getauxval"]
```

### Fields

| Field | Default | Notes |
|---|---|---|
| `[package] name` | — | required; `[a-z0-9-]`, matches the directory |
| `[package] version` | — | required |
| `[build] sources` | — | required in builtin mode; globs allowed |
| `[build] makefile` | — | switches to **external mode** (below) |
| `[build] target` | `all` | the make target, in external mode |
| `[build] output` | `build/<name>` | app-relative; must not escape the app |
| `[build] cflags` | `[]` | appended to the fixed freestanding set |
| `[build] ldflags` | `[]` | appended to the fixed static-PIE link |
| `[build] strip` | `true` | strip at staging (see below) |
| `[build] verify` | `true` | run verify.sh after every build |
| `[install] path` | — | required; absolute, in-image |
| `[verify] syscalls` | `[]` | the diet, checked under disassembly |
| `[verify] symbols` | `[]` | symbols those paths live in |

### The two build modes

**builtin** (no `makefile`): prism compiles `sources` itself with the
cross tools of `build/toolchain/usr` — the exact `do_prog` discipline of
`ports/toolchain/bootstrap.sh`: freestanding `-fPIE -nostdinc` against the
mlibc sysroot (`-isystem` the compiler's own headers, then the sysroot,
then `-I apps/include` for the raw shim) — and links the static PIE the
kernel's loader expects: `Scrt1 / crti / crtbeginS / the objects /
--start-group libc.a --end-group / crtendS / crtn`, `-z separate-code -z
max-page-size=4096`, `-e _start`.  If the link misses compiler intrinsics
it retries with libgcc inside the group, the way a `-static-pie` driver
would.

**external** (`makefile = "Makefile"`): prism runs `make -f <makefile>
<target>` in the app directory and picks up `output`.  For builds too
specific for the builtin mode.  The loader contract still gets checked —
the binary has to be the same static PIE either way.

### What build does, in order

1. **compile + link** (or `make`) into `<app>/build/<name>`, unstripped —
   the symbols are what `[verify]` checks against;
2. **verify** with `ports/toolchain/verify.sh`: the loader-contract shape
   (ET_DYN, no PT_INTERP, no DT_NEEDED, DT_RELA of R_X86_64_RELATIVE,
   page-exclusive segments, non-exec stack) plus the manifest's syscall
   and symbol diet under disassembly — a broken binary fails the build;
3. **stage** the stripped binary into `build/apps-out/<name>`: the kernel
   reads whole files (`MAX_ELF_FILE` is 4 MiB) and the meson debug
   sections buy the loader nothing (a static mlibc binary is ~16 MiB
   unstripped, ~0.9 MiB stripped).

`prism install` then copies the staged binary into a root tree (the
Makefile's `root.stamp` calls `install --all --dest build/root`), and the
manifest's `[install] path` decides where it lands — `/usr/bin/hello`,
`/usr/bin/cube`, ... (the image is usr-merged: `/bin` is a symlink to
`usr/bin`, so the shell's `exec hello` spells it either way)

### The diet, honestly

`[verify]` declares what the program actually does.  The scaffold's says
`syscalls = [1, 3]`, `symbols = ["printf"]` — write (at flush time) and
the raw uptime clock, through printf.  Rewrite `main.c`, rewrite the
diet: it is the program's ABI, and `verify.sh` checks every number of it
under disassembly (the tag layer's `mov $nr,%edi ... call
__do_syscallN`, the raw shim's `mov $nr,%eax ... syscall`).

### PrismOS-only calls

Everything POSIX the kernel serves, mlibc wraps (the sysdeps port in
`ports/mlibc/sysdeps/prismos`).  The PrismOS-only calls — `uptime_ms`,
`yield`, the millisecond `sleep`, `brk`, and the cube's framebuffer pair
`fb_info` / `fb_blit` — stay raw, through `apps/include/prism_raw.h`: the
only place the `syscall` instruction appears outside `libc.a`.

## The commands

```
prism init <name>     scaffold apps/<name>
prism build [name]     one package, or all of them (also: make apps)
prism install [name]   into a root tree (--dest DIR; default build/root)
prism verify [name]    the contract checks, alone
prism list             the packages
prism run              make the image and boot it (--bios, --tcg select)
prism clean [name]     a package's outputs, or all of them (--all)
```

## Not packages (yet)

`/usr/bin/hello-dyn` — the dynamically linked proof of phase 3.2's last step
(`make hello-dyn`, `ports/toolchain/build.sh stage dyn`) — and `/lib`'s
`ld.so` + `libc.so` stay toolchain-phase artifacts.  A `dynamic = true`
manifest mode is the natural next step.

## Testing without a machine

`prism_emu.c` (in the delivery; also `scripts/prism_emu.c` upstream of
it) runs any static PIE of this shape on the host under ptrace: it maps
the segments at USER_BASE, applies the RELATIVE relocations, builds the
SysV stack + auxv, and answers the PrismOS syscall ABI (rewriting to the
Linux numbers).  It is how `cube`'s frames and `demo`'s printf were
checked here without QEMU.
