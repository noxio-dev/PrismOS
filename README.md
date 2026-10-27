# PrismOS

A small experimental x86-64 operating system, written from scratch.

## What is PrismOS

PrismOS is a UNIX-like operating system built from the ground up:

- **Boot**: a FreeBSD-style loader for both **BIOS and UEFI**, whose logic is
  written in **C/Lua**, loading the kernel from an ext4 root partition.
- **Kernel**: written in **Rust** (`no_std`), with its own memory manager, a
  preemptive priority scheduler, SMP, ring 3 processes (`fork`, `execve`,
  signals, pipes), a VFS with ext4 (read/write) and tmpfs.
- **Userspace**: a POSIX-flavoured ABI (Linux-shaped syscalls) with a port of
  [mlibc](https://github.com/managarm/mlibc), a cross toolchain
  (`x86_64-prismos`: binutils + gcc) and dynamic linking (`ld.so`, `libc.so`).

## Goal

Make PrismOS a **usable, self-hosting UNIX**: a system you can boot, log into,
and work on with a real shell and real software (BusyBox first, then a native
toolchain and, in time, scripting languages such as Python), until PrismOS can
build PrismOS itself. The roadmap below is the plan.

Build and run: `make` builds `build/prismos.img`, `make run` boots it in QEMU
(BIOS), `make run-uefi` with OVMF, `make test` runs the automated boot tests.

## Todo

Phase 1 — kernel foundations
- [x] Phase 1.1 — heap (slab/buddy)
- [x] Phase 1.2 — ACPI/APIC (LAPIC + IOAPIC + 8259 masked)
- [x] Phase 1.3 — PCI/AHCI
- [x] Phase 1.4 — VFS + ext4 R/W
- [x] Phase 1.5 — hardening before userspace
  - [x] run `make test` on BIOS + UEFI with the ext4 data disk, fix what it finds
  - [x] ext4: replay a dirty journal at mount, then a minimal jbd2 writer
  - [x] ext4: verify metadata checksums on read, htree insert, `fallocate`-style preallocation
  - [x] block cache for file data (write-back, `sync` / periodic flush)
  - [x] AHCI: interrupt-driven completion and NCQ instead of polling (MSI, FPDMA reads, per-slot bounce buffers)
  - [x] boot from the ext4 volume (loader ext4 driver) instead of the FAT32 ESP only (BIOS loader; UEFI still needs a FAT32 ESP)
  - [x] three-partition layout: minimal ESP (UEFI stub only), BIOS boot partition, ext4 root read by both the loader and the kernel
  - [x] kernel logging (`dmesg` ring buffer), proper `panic` backtrace, unit tests in-kernel
  - [x] SMP bring-up (AP startup via the LAPIC), per-CPU data, spinlocks (APs idle on their own LAPIC timer and answer IPIs; the scheduler stays on the BSP until phase 2)

Phase 2 — ring 3 and processes
- [x] Phase 2.1 — ring 3: user page tables per process, `syscall`/`sysret`, TSS stacks
- [x] Phase 2.2 — processes and threads: PCB, kernel stacks, context switch
- [x] Phase 2.3 — scheduler: preemptive round-robin on the LAPIC timer, then priorities, sleep/wakeup
- [x] Phase 2.4 — ELF loader: `execve` from the VFS, argv/envp, auxv
- [x] Phase 2.5 — `fork`, `wait`, `exit`, signals (basic), copy-on-write
- [x] Phase 2.6 — file descriptors per process on top of the VFS, pipes, `/dev` (`console`, `null`, `zero`)

Phase 3 — POSIX userspace
- [x] Phase 3.1 — syscall set for a C library: `open/read/write/lseek/close/stat/dup/pipe/mmap/brk/getcwd/...`
- [x] Phase 3.2 — mlibc port and a `x86_64-prismos` toolchain target
  - [x] kernel prerequisites mlibc needs beyond 3.1: TLS (`FS` base through `arch_prctl`), `futex`, `sigprocmask` / `sigaction`, `mprotect`, `poll` / `select`, `clock_gettime` (pulled forward from 3.5) — syscalls 36..43
  - [x] `sysdeps/prismos` in mlibc (`sys_open`, `sys_read`, `sys_vm_map`, `sys_fork`, `sys_execve`, `sys_waitpid`, ...); sysdeps the kernel lacks stay `ENOSYS` — `ports/mlibc`, verified against mlibc 7.0.0
  - [x] build mlibc with Meson against a cross C/C++ compiler (headers first, then the compiler, then the library), install it into a sysroot — `ports/toolchain`: the bootstrap pipeline (wrappers → headers → library → `/bin/hello` + `verify.sh`), and the real `x86_64-prismos` cross tools (binutils 2.44 + gcc 14.2.0, patched for the triplet, the ABI deliberately Linux-shaped: `glibc-stdint.h`, `gnu-user.h`, `__cxa_atexit`, the crtbegin/crtend sandwich) — `make mlibc`, `make usr-mlibc`
  - [x] port `/bin/hello`, `/bin/fork`, `/bin/fd` and `/bin/posix` from the mini libc (`usr/lib`) to mlibc — `usr/mlibc`: same scenarios, same summary lines, the C library's vocabulary (`fork` / `waitpid` / the `W*` macros, `signal` with its `SIG_ERR` refusals, `opendir` / `readdir`, `tcgetattr`, `uname`, stdio buffered on the descriptor table, `-1` + `errno` folded back for the error tables); the four PrismOS-only calls stay raw through `prism_raw.h` (`uptime_ms`, `yield`, the millisecond `sleep`, `brk`); `verify.sh` grew a per-program mode — every syscall each program makes and the symbols those paths live in, 130 checks across the four — `make usr-mlibc`
  - [x] ELF loader: dynamic linking, a requirement and not an option. Honour `PT_INTERP` (load the interpreter, mlibc's `ld.so`, at its own base and start it with `AT_BASE` plus `AT_PHDR` / `AT_PHNUM` / `AT_ENTRY` describing the main program); `DT_NEEDED` resolution stays in userspace (`ld.so` finds libraries in `/lib` and `/usr/lib` and maps them with file `mmap`); add `mprotect` for RELRO and relocation fix-ups; shared libraries are `ET_DYN` with their own `PT_LOAD` / `PT_TLS` / `PT_GNU_RELRO` handling — the loader's `PT_INTERP` path (sys/kern/src/user/elf.rs) + anonymous `PROT_NONE` mmap (sysc.rs / vmm.rs, the reservation idiom) on the kernel side; `-Ddefault_library=both` putting `libc.so` + `ld.so` in the sysroot on the library side; the proof is a dynamically linked `/bin/hello-dyn` (PT_INTERP `/lib/ld.so`, `DT_NEEDED libc.so`, BIND_NOW) with `/lib/ld.so` + `/lib/libc.so` in the boot image — `make usr-mlibc-dyn`, `verify.sh`'s dyn mode
- [x] Phase 3.3 — `init`, TTY layer with line discipline, `/proc` and `/tmp` (tmpfs)
  - [x] 3.3.1 tmpfs: an in-memory `FileSystem` (create, unlink, rename, readdir, size quota) mounted on `/tmp` at boot; the `fstest` scenarios are reused against it
  - [x] 3.3.2 missing syscalls: `getppid`, `getuid` / `getgid` (fixed values for now), `umask`, `gettimeofday`, `nanosleep`, `ppoll` / `pselect`, `mount` / `umount`, `reboot`
  - [x] 3.3.3 TTY layer: a `tty` object with a line discipline (canonical mode, echo, erase, kill-line, `^C` / `^D` / `^\`), real `TCGETS` / `TCSETS` / `TCSETSW`, raw mode, `TIOCGWINSZ`; the console and the serial port sit on top of it
  - [x] 3.3.4 sessions and process groups: `setsid` / `setpgid` / `getpgid` (syscalls 59-61), the controlling-terminal claim (`TIOCSCTTY`, what `/dev/tty` resolves to), the foreground group (`TIOCSPGRP` exchanging a pgrp, validated against the session), `kill(-pgrp)` and a `^C` delivered to the group, `SIGHUP` to the foreground group when a session leader dies — pgid / sid / ctty on the PCB (process.rs), the group walk in signal.rs, the posix package's session scenario
  - [x] 3.3.5 devfs additions: `/dev/tty` (the session's controlling terminal, `ENXIO` without one), `/dev/tty0` (the console terminal: keyboard in, text screen out), `/dev/ttyS0` (the serial terminal: COM1's own receive queue and line discipline, the queue of its own it was promised — init puts a getty on it), `/dev/full`; `/dev/console` stays the kernel's mirror (text screen + serial)
  - [x] 3.3.6 procfs: `/proc/self`, `/proc/<pid>/{stat,status,cmdline,cwd,exe,fd}` (cwd and fd/N are symlinks, lstat-backed; stat/status carry the pgrp/session pair), `/proc/{meminfo,uptime,mounts,version,cpuinfo,cmdline}`; read-only, generated on read
  - [x] 3.3.7 `init`: the kernel starts `/sbin/init` (pid 1) instead of the kernel shell; `init` mounts `/proc` and `/tmp` through `mount(2)` (the kernel keeps `/` and `/dev`, the devtmpfs split), reaps orphans (replacing adoption by pid 0), respawns a shell on the console and a getty on `/dev/ttyS0`, handles clean shutdown; the kernel shell stays as a rescue mode (`init=` in `loader.conf`, a missing or dead init falls back to it)
  - [x] 3.3.8 tests: `tests/init_drive.py` drives the *serial* session end to end (init's mounts, `/proc`'s files and symlinks, the line edits, `/dev/full`, `^C` on a blocked child, `^D` and the respawn); `tests/tmpfs_drive.py` and `boot_test.py`'s userland scenarios answer for `/tmp`, the usr-merge and the boot, and the rescue scenarios keep the kernel shell honest
- [ ] Phase 3.4 — BusyBox and a userspace shell
  - [ ] 3.4.1 BusyBox port: minimal config (`ash`, `ls`, `cat`, `echo`, `mkdir`, `rm`, `cp`, `mv`), static build against the mlibc sysroot, `verify.sh` extended
  - [ ] 3.4.2 fill the gaps the port reveals: `fchdir`, `fsync`, `chmod` / `fchmod` (stubs), `wait4` with rusage, `utimensat`, `readlink` / `symlink` from userspace, ...
  - [ ] 3.4.3 interactive `ash`: pipes, redirections (`dup2`), job control (builds on 3.3.4), `PATH`, `/etc/profile`
  - [ ] 3.4.4 more applets: `ps`, `mount`, `df`, `grep`, `sed`, `awk`, `tar`, `vi`
  - [ ] 3.4.5 `init` spawns `ash`; the kernel shell becomes rescue only
  - [ ] 3.4.6 tests: `sh` scripts run in QEMU (pipes, loops, redirections), checked by `make test`
- [ ] Phase 3.5 — precise time, randomness, users and permissions
  - [ ] 3.5.1 time: TSC (or HPET) calibration, nanosecond `clock_gettime`, accurate `nanosleep`, optionally a tickless timer
  - [ ] 3.5.2 randomness: entropy (RDRAND / RDSEED, TSC jitter), ChaCha20 CSPRNG, `/dev/random`, `/dev/urandom`, `getrandom`, a real `AT_RANDOM`
  - [ ] 3.5.3 identities: uid / gid / euid in the PCB, `setuid` / `setgid`, ext4 inode owners actually read and written
  - [ ] 3.5.4 permissions: access checks in the VFS (rwx for owner / group / other), `chmod` / `chown`, `umask`, setuid bit at `execve`
  - [ ] 3.5.5 accounts: `/etc/passwd`, `/etc/group`, `login`, `su`, `id`, `whoami`
  - [ ] 3.5.6 tests: access denied to a non-root user, setuid, non-constant `getrandom`

Phase 4 — toolchain (phase 3.2 already ships cross binutils 2.44 and gcc 14.2.0, so this phase is about completing and using them)
- [ ] Phase 4.1 — complete binutils in the image: cross-built `ar`, `ranlib`, `strip`, `objdump`, `nm`, installed and used by the builds
- [ ] Phase 4.2 — threads: `clone` (`CLONE_VM`, `CLONE_THREAD`, `set_tid_address`), per-thread TLS, `pthread_create` in the mlibc port (`futex` is already there)
- [ ] Phase 4.3 — C++: cross-built libstdc++ (with threads), a `hello.cpp` test (iostream, exceptions, `std::thread`), shared libgcc
- [ ] Phase 4.4 — build the userspace with the PrismOS toolchain: `make`, coreutils, diffutils, findutils, pkg-config
- [ ] Phase 4.5 — a "dev" image: the sysroot embedded in the boot image, preparing phase 5

Phase 5 — self-hosting
- [ ] Phase 5.1 — native binutils: `as`, `ld`, `ar` cross-built, then run on PrismOS to assemble and link a program
- [ ] Phase 5.2 — native gcc: `cc1`, `cc1plus`, `gcc`, `g++` running on PrismOS (needs plenty of RAM, a solid `mmap`, `fork` / `pipe`, a bearable build time)
- [ ] Phase 5.3 — native build tools: `make`, `sh`, `tar`, `gzip`, a source tree on the ext4 disk (ideally with real `MAP_SHARED` and a page cache)
- [ ] Phase 5.4 — rebuild the userspace natively, then the kernel's C parts (loader)
- [ ] Phase 5.5 — the Rust kernel: native `rustc` / `cargo` (the hardest; keep for last or defer)
- [ ] Phase 5.6 — full bootstrap: PrismOS builds PrismOS, with a test comparing the produced binaries

Beyond (nice to have)
- [ ] hidden kernel prerequisites that the phases above keep running into: file-backed `MAP_SHARED` with a page cache, Unix domain sockets, `epoll` -- move them before 4.2 / 5.2 when they block
- [ ] networking: virtio-net or e1000, ARP/IP/UDP/TCP, a tiny `ping` and `wget`
- [ ] USB (xHCI) keyboard and mass storage, NVMe
- [ ] other file systems: FAT32 R/W in the kernel
- [ ] graphics: framebuffer userspace API, a minimal window system
- [ ] desktop (Xorg, then KDE): mlibc is the entry ticket, not the whole price -- also Unix domain sockets, `epoll`, shared memory (`MAP_SHARED` of files, `shm`), a display / input device interface (KMS-style), and a lot of the Linux surface (`/proc`, `/sys`, `udev`-like discovery)
- [ ] AArch64 port of the kernel and loader

*`One day, perhaps, PrismOS will be self-hosted lol`*

## License

GPL-3.0-or-later -- see [LICENSE](LICENSE). Lua is MIT licensed, see
`contrib/lua/COPYRIGHT`. The framebuffer font is public domain.