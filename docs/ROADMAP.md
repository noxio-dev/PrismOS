# PrismOS roadmap

The plan to make PrismOS a usable, self-hosting UNIX. Back to the [README](../README.md).

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
- [x] Phase 3.4 — BusyBox and a userspace shell
  - [x] 3.4.1 BusyBox port: minimal config (`ash`, `ls`, `cat`, `echo`, `mkdir`, `rm`, `cp`, `mv`), static build against the mlibc sysroot, `verify.sh` extended
    - [x] busybox 1.37.0 (`ports/busybox`): the committed config IS the port -- an allnoconfig turned up to the roadmap's own applets plus the small utilities a session leans on (`pwd true false test printf env sleep uname ln touch rmdir chmod wc head tail id whoami basename dirname readlink`), ash's job control and line editing on and `FEATURE_SH_STANDALONE` off (the honest fork + execve path is this phase's own test); busybox's Kbuild drives `prism-cc` (ports/binutils' driver with two answers: `-lm` is nothing -- mlibc's libc.a carries the musl-derived math -- and Kbuild's `-r` partial links go to the raw cross ld, `LD` being `$(CC) -nostdlib` in busybox's makefile); the one binary links as the static PIE the loader expects, 1.3 MiB stripped under the 4 MiB cap, and `verify.sh`'s custom mode checks the diet traced under the emulator, the new numbers 62..68 included -- `make busybox`
  - [x] 3.4.2 fill the gaps the port reveals: `fchdir`, `fsync`, `chmod` / `fchmod` (stubs), `wait4` with rusage, `utimensat`, `readlink` / `symlink` from userspace, ...
    - [x] the seven syscalls, numbers 62..68 (`sys/kern/src/user/{fd,process,syscall}.rs`): fchdir, fsync, chmod, fchmod, wait4, getrusage, utimensat (readlink / symlink already served since the tmpfs step) -- all riding machinery that already existed, the VFS's chmod, the write-back cache's flush, the inode time setters; utimensat covers both POSIX spellings (the *at walk and futimens through an empty path, UTIME_NOW / UTIME_OMIT included); rusage is honest accounting, not a stub -- the tick splits user / system at the ring boundary, the PCB carries its own pair plus its reaped children's, and wait4 reports exactly what the child leaves behind
    - [x] the mlibc port's **linux option** turns on (`'linux': true`, the bsd option riding along -- managarm's own arrangement, the linux option's utmp.cpp needs options/bsd's `<utmp.h>`): mount / umount / statfs / mntent land in the libc, the tags the sysdeps.hpp comment had promised would "stay raw through prism_raw.h until that option lands with the busybox step" -- init's mounts go through `mount(2)` now; the option's kernel-UAPI need is the twenty-two-header closure carried in `ports/mlibc/linux-headers` (PROVENANCE.txt tells why exactly that small), installed into the sysroot the way a real cross toolchain ships kernel headers; `df`'s statvfs converts from the kernel's statfs in the sysdep, `id`'s getgroups answers one group (the gid itself)
  - [x] 3.4.3 interactive `ash`: pipes, redirections (`dup2`), job control (builds on 3.3.4), `PATH`, `/etc/profile`
    - [x] ash runs on what 3.3 built -- fork / exec / wait4, pipes, `dup2` redirections, sigaction, the tty's raw mode for its line editor, TIOCGPGRP / TIOCSPGRP for the job control on 3.3.4's process groups; the session gets its PATH and `prism$` prompt from `/etc/profile` (init's `argv[0]` is `-sh`, the login-shell spelling)
  - [x] 3.4.4 more applets: `ps`, `mount`, `df`, `grep`, `sed`, `awk`, `tar`, `vi`
    - [x] all in the committed config: `ps` reads /proc, `mount` and `df` speak the libc's mount(2) / statvfs (the mntent listing over /proc/mounts), awk's math links out of mlibc's musl-derived libm, vi edits on the tty layer, and `time` rides along as wait4 / rusage's own proof (its report of a child's user / sys ticks is syscall 66 answering) -- `make busyboxtest` judges the single-process half of them against a host busybox of the same tarball and config; ps, vi and time are the QEMU side (the emulator's honest no-fork rule)
  - [x] 3.4.5 `init` spawns `ash`; the kernel shell becomes rescue only
    - [x] `/usr/bin/sh` is the busybox symlink (ash) and init spawns it as a login shell reading `/etc/profile`; pkg/sh stays installed as `/usr/bin/minish`, the phase 3.3 test rig; the kernel shell keeps `prism# ` for `init=` rescue exactly where 3.3.7 left it
  - [x] 3.4.6 tests: `sh` scripts run in QEMU (pipes, loops, redirections), checked by `make test`
    - [x] `boot_test.py`'s busybox scenario (`make test`): pipes into `wc`, `> /tmp` redirections, for loops, `$(( ))` arithmetic, `ash -c` subshells, a tar round trip, `touch` / `chmod` / `ls -l` through the new syscalls, and `time echo timed` -- wait4 answering with the child's rusage; `tests/busybox_test.py` (`make busyboxtest`) is the host-side proof -- every single-process applet under prism_emu against a host busybox of the same tarball and the same committed config, 44 checks: text for text, files and archives byte for byte, the mounts listed text for text; `init_drive.py` and `tmpfs_drive.py` speak ash now (its line editing, its error paths, its prompt from the profile)
- [ ] Phase 3.5 — precise time, randomness, users and permissions
  - [x] 3.5.1 time: TSC (or HPET) calibration, nanosecond `clock_gettime`, accurate `nanosleep`, optionally a tickless timer
    - [x] `hw/hpet.rs` + `hw/clock.rs`: the TSC measured against the HPET (three 10 ms windows, median) or PIT channel 2 (three 20 ms), `lfence; rdtsc` converted with a 32.32 fixed-point multiplier; fallbacks: a 64-bit HPET counter, then the 10 ms tick count; `now_ns` is clamped monotonic; `clock=pit` on the command line restores the old clock
    - [x] `clock_gettime` / `gettimeofday` in nanoseconds / microseconds (realtime = RTC anchored at the first read + the monotonic clock; the CPUTIME clocks stay at tick granularity)
    - [x] `nanosleep` on a clock deadline, uncapped (loops over bounded parks), `-EINTR` + `rem` when a signal with a handler or a fatal default action arrives, ignored signals do not end it
    - [x] LAPIC one-shot deadline mode (`apic::arm_at`): the timer is re-armed for the earlier of the next 10 ms scheduler tick and the next sleeper's deadline, so sleeps wake within microseconds; the 100 Hz tick keeps driving slices, accounting and itimers (not fully tickless: an idle CPU still takes its tick); `clock=periodic` turns it off; `clock` shell command and boot log lines report the source
    - [x] still to run under QEMU: the kernel compiles and `cargo check` is clean, but no boot test has exercised the one-shot timer yet (the build environment had neither QEMU nor the bare-metal target)
  - [ ] 3.5.2 randomness: entropy (RDRAND / RDSEED, TSC jitter), ChaCha20 CSPRNG, `/dev/random`, `/dev/urandom`, `getrandom`, a real `AT_RANDOM`
  - [ ] 3.5.3 identities: uid / gid / euid in the PCB, `setuid` / `setgid`, ext4 inode owners actually read and written
  - [ ] 3.5.4 permissions: access checks in the VFS (rwx for owner / group / other), `chmod` / `chown`, `umask`, setuid bit at `execve`
  - [ ] 3.5.5 accounts: `/etc/passwd`, `/etc/group`, `login`, `su`, `id`, `whoami`
  - [ ] 3.5.6 tests: access denied to a non-root user, setuid, non-constant `getrandom`

Phase 4 — toolchain (phase 3.2 already ships cross binutils 2.44 and gcc 14.2.0, so this phase is about completing and using them)
- [x] Phase 4.1 — complete binutils in the image: cross-built `ar`, `ranlib`, `strip`, `objdump`, `nm`, installed and used by the builds
  - [x] the five utilities as PrismOS programs: binutils 2.44 configured `--host=x86_64-prismos --target=x86_64-prismos` (the same tarball and triplet patch the toolchain already fetches), compiled against the mlibc sysroot and linked as the static PIEs the loader expects — `ports/binutils`, whose `prism-cc` driver turns the do_prog discipline into a compiler shape so binutils' own configure and make drive both toolchain paths (bootstrap wrappers or real cross tools) unchanged; libc.a leads the link group, libiberty's always-built `getopt.o`/`fnmatch.o` never colliding with the static library's
  - [x] the libc gap the port revealed: the **glibc option** joins the mlibc port (`'glibc': true` — `getopt_long`, the GNU long options the utilities parse their flags with; the option set finally matching the deliberately Linux-shaped ABI the gcc patch chose), while what the kernel lacks stays `ENOSYS` and degrades honestly (libiberty's `getrlimit` probe)
  - [x] installed into the image: `/usr/bin/{ar,ranlib,nm,objdump,strip}`, stripped with the cross strip under the 4 MiB whole-file cap (1.9–3.2 MiB each, ~11 MiB together — the root partition 64→96 MiB); `as` and `ld` stay phase 5.1's business
  - [x] used by the builds: the port's own AR / RANLIB archive libbfd, libopcodes, libsframe and zlib, the cross strip stages the five; `verify.sh` disassembles every program it checks through the prefixed `objdump` / `nm` / `readelf` (bootstrap.sh's wrapper set completed with `objdump` + `readelf`); the Makefile grew `make binutils` and `make root` installs the five
  - [x] the kernel's hidden prerequisite: `ELF_STACK_SIZE` 16→256 pages — `objdump -d` of a full 1.9 MiB binary faulted eight bytes below the 64 KiB stack, measured under `prism_emu` running on the kernel's own stack size
  - [x] the proof without a machine: `prism_emu` grew the full argv and the utilities' file surface (the stat pair + lstat, unlink / rename, umask, munmap / mprotect, fcntl, a real lseek, the PrismOS `O_*` bits translated, `O_CREATE` files at the kernel's 0o644) — `tests/binutils_test.py` (`make bintest`, 17 checks) drives the five against the host's own binutils 2.44: archives and stripped objects **byte for byte**, `nm` and `objdump -d` text for text (367,021 lines on the stress run), the errno paths honest
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
