# BusyBox and a userspace shell (phase 3.4)

```
ports/busybox/
├── build.sh                          the port: fetch → the prism-cc driver →
│                                     config → build → stage (stripped, the
│                                     cross strip) → verify
└── busybox-1.37.0-prismos.config     the committed minimal config -- the port
```

Phase 3.4 of the roadmap, verbatim: *BusyBox port: minimal config (`ash`,
`ls`, `cat`, `echo`, `mkdir`, `rm`, `cp`, `mv`), static build against the
mlibc sysroot, `verify.sh` extended*.  Each word is a piece of this port:

**the port.**  busybox 1.37.0, fetched once into `build/toolchain-src`
(the directory the toolchain and binutils ports already share), built by
its own Kbuild through `prism-cc` -- ports/binutils' driver with one
busybox-specific answer (`-lm` maps to nothing: mlibc's libc.a carries the
musl-derived math, and busybox's makefile adds `-lm` unconditionally "for
dc, awk, ntpd"; the `-lcrypt` / `-lrt` probes fail honestly through the
same resolve rule).  Kbuild's partial links (`built-in.o`) ride a second
answer: busybox spells `LD` as `$(CC) -nostdlib`, so the driver recognises
its own name with `-r` and hands the relocatable link to the raw cross ld
-- no sandwich, there is no `_start` to build.  One binary comes out,
**1.3 MiB stripped** (the cross strip's job, under the kernel's 4 MiB
whole-file cap), staged into `build/busybox-out`.

**minimal config.**  The committed `busybox-1.37.0-prismos.config` is the
port's decision record: an allnoconfig turned up to exactly the roadmap's
applets -- 3.4.1's `ash ls cat echo mkdir rm cp mv`, 3.4.4's `ps mount df
grep sed awk tar vi` -- plus the small utilities a session leans on
(`pwd true false test printf env sleep uname ln touch rmdir chmod wc head
tail id whoami basename dirname readlink time`), ash's job control and line
editing on, and `FEATURE_SH_STANDALONE` / `NOFORK` **off** on purpose: the
honest fork + execve path is the one this phase is testing.  (`time` earns
its place: its report of a child's user / system ticks is the wait4 /
rusage pair's own proof, syscall 66 answering at runtime.)  Committed
rather than regenerated: same tree, same config, same binary -- the same
determinism a pinned tarball buys.

**static build against the mlibc sysroot.**  Every object compiles through
`x86_64-prismos-gcc` with the do_prog discipline (`-nostdinc`, the
compiler's own headers, the mlibc sysroot) and the one link assembles the
static-PIE sandwich the kernel's loader expects.  What the port asked the
libc for:

* the **linux option** (`'linux': true` in the sysdeps' meson.build since
  this phase): `mount(2)` / `umount(2)`, `statfs(2)` and the mntent
  family live behind it, and they are the calls the `mount` and `df`
  applets parse their flags through -- the sysdeps.hpp comment had
  promised the tags would "stay raw through prism_raw.h until that
  option lands with the busybox step"; this is that step.  The kernel
  serves the three syscalls (48 / 55 / 56) since phase 3.3; the tags
  joined them, and init's mounts go through the libc's `mount(2)` now.
  The option needs the kernel UAPI headers mlibc's own
  sysdep-signatures includes: the twenty-two-header closure the port
  carries in `ports/mlibc/linux-headers/` (PROVENANCE.txt tells the
  whole story), installed into the sysroot the way a real cross toolchain
  ships kernel headers.  The **bsd option** rides along (managarm's own
  arrangement): the linux option's `utmp.cpp` includes `<utmp.h>`, and
  that header lives in options/bsd.
* `getopt_long`, `error()` -- the glibc option phase 4.1 already turned
  on, what busybox's `libbb` parses its flags through;
* `statvfs` -- busybox's `df` asks the POSIX twin, so the port converts
  the kernel's 120-byte `statfs` answer into `statvfs`'s field order in
  the sysdep, the conversion glibc does in its own userspace;
* `getgroups` -- one group identity per process (the gid the kernel
  answers), so `id` prints its `groups=` word honestly.

**verify.sh extended.**  The port's verify stage runs verify.sh's custom
mode on the unstripped binary: the loader's whole contract (static PIE,
page-exclusive segments, RELATIVE relocations only) plus the syscall diet
traced under the emulator -- write / read / open / close / lseek, the
stat family and lstat, the directory walk (`getdents`), `fchdir` /
`fsync` / `chmod` / `utimensat` / `wait4` (the new numbers of the phase,
62..68) -- and the symbols `ash_main` / `applet_main`.  The same
`PRISM_VERIFY_SYSCALLS` / `PRISM_VERIFY_SYMBOLS` door binutils' port uses.

## The kernel's seven numbers

The gaps the port revealed (roadmap 3.4.2, the port's own finding): a
shell's `cd` wants `fchdir`, a `cp` that flushes wants `fsync`, `touch`
and `cp -p` want `utimensat`, `chmod` / `fchmod` wanted the syscalls the
kernel shell's command already spoke, and `wait4` with rusage (plus
`getrusage` behind it) is what a shell reports children in.  Numbers
62..68, all riding machinery that already existed -- the VFS's chmod, the
write-back cache's flush, the inode time setters -- the syscall layer
only had to spell them:

```
 62 fchdir(fd)                          the directory an fd is open on
 63 fsync(fd)                           flush the open file's mount
 64 chmod(path, mode)                   the permission bits, journal-written
 65 fchmod(fd, mode)                    the same, through an open file
 66 wait4(pid, &status, flags, &rusage) wait with the child's rusage
 67 getrusage(who, &rusage)             SELF / CHILDREN / THREAD
 68 utimensat(dirfd, path, &times, flags)  atime / mtime, UTIME_NOW /
                                          UTIME_OMIT, the futimens spelling
```

The rusage pair is honest accounting, not a stub: the tick knows which
side of the ring boundary it caught a process on (user vs system, the
`on_tick(from_user)` split), the PCB accumulates its own pair plus its
reaped children's, and `wait4` reports exactly what the child leaves
behind -- `ru_nvcsw` / `ru_nivcsw` included, straight from the scheduler's
switch and preempt counters.

## The proof without a machine

`tests/busybox_test.py` (`make busyboxtest`) compiles `prism_emu.c` --
the ptrace runner that maps a static PIE at `USER_BASE`, applies its
`R_X86_64_RELATIVE` relocations, builds the SysV stack and answers the
PrismOS syscall ABI, rewriting to the Linux numbers -- and drives the
applets through real work on this host, with a **host busybox of the same
tarball and the same committed config** as the judge (glibc underneath:
the only difference between the two sides is the libc), the arrangement
binutils_test.py has with the host's binutils:

* the text paths: echo, printf, cat, wc, head, tail, grep, sed, awk
  (math included), uname, id / whoami, basename / dirname / pwd -- text
  for text;
* the file paths: mkdir / rmdir / rm / touch / chmod, cp and mv (content
  byte for byte), ln -s + readlink, `ls` (the sorted walk) and `ls -l`
  after a chmod -- the whole line, dates and all;
* tar: archives **byte for byte** (both sides deterministic: same
  members, same order, same mtimes), and `tar tf` listings text for text;
* the mounts: `mount`'s listing text for text (mlibc's mntent over
  /proc/mounts), `df`'s row shape for this filesystem (the numbers drift
  with the live disk -- the shape is the check);
* the failure paths: a missing file exits 1 with the errno text.

44 checks.  The emulator's syscall surface grew with the phase --
getdents (the directory walk), mkdir / rmdir / chdir / fchdir, the chmod
pair, utimensat, symlink / readlink, pipe, uname, nanosleep, getrusage,
statfs -- every one a register-for-register rewrite to the Linux call the
kernel's own answer mirrors.  What the emulator **refuses** to do is just
as deliberate: `fork` / `execve` / `wait` stay ENOSYS under it, so the
shell itself is the QEMU scenario's business (`make test`'s "ash pipes,
redirections, loops" step -- the roadmap's 3.4.6 sentence, `sh` scripts
with pipes and loops through fork + pipe + dup2 + execve, `echo > /tmp/x`
redirections, `$(( ))` arithmetic, `time echo timed` for wait4's rusage
report, and the new syscalls through `touch` / `chmod` / `ls -l`).

## The session

`make root` installs the one binary at `/usr/bin/busybox` plus the applet
symlinks (`sh` among them: busybox's ash owns the name -- pkg/sh stays
installed as `/usr/bin/minish`, the phase 3.3 test rig).  `init` spawns
`/usr/bin/sh` with `argv[0] = "-sh"` -- the classic getty spelling of a
*login shell*, so ash reads `/etc/profile` (the PATH and the `prism$`
prompt the whole session inherits).  The kernel shell keeps `prism# ` as
the rescue mode, exactly where 3.3.7 left it.

## Usage

```
make busybox                        # or: sh ports/busybox/build.sh
sh ports/busybox/build.sh list      # fetch cc config build stage verify
make busyboxtest                    # the emulated proof, 44 checks
make test                           # the QEMU scenarios (needs qemu)
```

Requirements: whatever `make mlibc` already needed, plus the busybox
tarball (stage fetch shares `build/toolchain-src` with `ports/toolchain`,
downloaded once).  A full run is a couple of minutes here (two cores);
the binary lands in `build/busybox-out`, verified against the loader's
contract by `ports/toolchain/verify.sh`.
