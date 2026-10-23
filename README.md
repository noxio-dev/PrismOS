# PrismOS

A small experimental x86-64 operating system, written from scratch.
FreeBSD-style boot: a real **BIOS + UEFI bootloader** whose logic is written in
**C/Lua**, loading a **Rust** kernel (for now we have a shell).

## Status

Phase 1 (kernel foundations) is feature-complete: the machine boots on BIOS and
UEFI, the kernel manages memory and interrupts, talks to an AHCI disk and
mounts an ext4 volume that you can browse and modify from the kernel shell.
Phase 2.1 adds ring 3: the kernel can run a flat program in user mode with its
own page tables (see below). Phase 2.2 adds processes: a PCB, a private kernel
stack and a context switch per program.  Phase 2.3 makes the scheduler
preemptive: time slices on the LAPIC timer, strict priorities and
sleep/wakeup.  Phase 2.4 adds an ELF loader: compiled C programs (static PIE)
are loaded from the ext4 root, started with argv/envp/auxv, and can replace
themselves with `execve`; the FPU/SSE state is now saved per process.
Phase 2.5 adds the Unix trio: `fork` (with copy-on-write), `wait` (zombies,
blocking, WNOHANG, a Linux-style status word) and basic signals (handlers,
SIGCHLD, `kill`, a sigreturn trampoline that restores the interrupted context
register for register).
Phase 2.6 gives every process its own file descriptors on top of the VFS:
open file descriptions shared by `fork` and `dup`, pipes (bounded blocking,
EOF / EPIPE), a `/dev` directory (devfs) with `console`, `null` and `zero`,
and six new syscalls (`read`, `open`, `close`, `lseek`, `dup`, `pipe`; `write`
now routes through the table).
Phase 3.1 rounds out the POSIX surface a C library port needs: seventeen new
syscalls (`stat`, `fstat`, `brk`, `mmap`, `munmap`, `chdir`, `getcwd`, `dup2`,
`fcntl`, `getdents`, `ioctl`, `unlink`, `mkdir`, `rmdir`, `rename`, `link`,
`uname`), `O_NONBLOCK` / `O_CLOEXEC`, a working directory *per process*, the
POSIX SIGPIPE rule and the `isatty` ioctl pair -- the ABI the
[mlibc](https://github.com/managarm/mlibc) port and the `x86_64-prismos`
toolchain (phase 3.2) will build against.
Phase 3.2 has started: its first step -- the kernel prerequisites an mlibc
port needs beyond 3.1 -- is in.  `arch_prctl` gives a thread its FS/GS base
(TLS: the MSRs are switched with the process, `fork` copies them, `execve`
clears them); `futex` parks and wakes on a user word (WAIT / WAKE / the
REQUEUE pair / the BITSET pair, timespec timeouts against the uptime clock,
the bounded-blocking house rule); `sigaction` / `sigprocmask` make the signal
state a real POSIX one (per-handler masks, `SA_RESTORER` / `SA_NODEFER` /
`SA_RESETHAND` honoured, the interrupted mask saved on the delivery stack and
restored by `sigreturn`, SIGKILL unblockable, the mask kept across `fork` and
`execve`); `mprotect` re-protects mapped pages (W^X, COW-aware: a shared
read-only page is copied before it becomes writable, so a fork sibling never
sees the change); `poll` / `select` watch the descriptor table (readiness of
files, console input, pipes -- `POLLERR` / `POLLHUP` / `POLLNVAL` included);
and `clock_gettime` serves the uptime clock as `CLOCK_MONOTONIC`, the
RTC-anchored realtime as `CLOCK_REALTIME` and the charged ticks as the two
CPUTIME clocks -- millisecond precision, eight new numbers (36..43).  Its
second step is the port itself: `sysdeps/prismos` (ports/mlibc), the
mlibc layer that speaks those 44 syscalls -- one directory to drop into
an mlibc checkout, one patch to register it, verified against mlibc 7.0.0
(the whole `ansi` + `posix` build, and a hello world that links as the
static PIE the loader expects).  Its third step builds the real thing
(ports/toolchain): a `x86_64-prismos` cross toolchain (binutils 2.44 +
gcc 14.2.0, patched for the triplet, the ABI deliberately Linux-shaped),
mlibc built with Meson against it -- headers first, then the compiler,
then the library -- installed into a sysroot, and `/bin/hello` rebuilt
against it, passing the loader's own checks under `verify.sh`.  Its
fourth step ports the userspace itself (usr/mlibc): `/bin/hello`,
`/bin/fork`, `/bin/fd` and `/bin/posix` off the mini libc onto mlibc --
same scenarios, same checks, the C library's vocabulary -- each passing
`verify.sh`'s loader-contract checks with its own syscall set.  Its fifth
and last step completes the loader: `PT_INTERP` is honoured (the kernel
maps mlibc's `ld.so` at its own base, unrelocated, and starts it with
`AT_BASE` plus the program's `AT_PHDR` / `AT_PHNUM` / `AT_ENTRY`),
anonymous `PROT_NONE` mappings exist (the loader's reservation idiom),
the sysroot grows the shared half (`libc.so` + `ld.so`, cross-built with
`-Ddefault_library=both`), and a dynamically linked `/bin/hello-dyn`
joins the boot image with `/lib/ld.so` and `/lib/libc.so` beside it.

| Area                     | State   | Notes                                                                 |
|--------------------------|---------|-----------------------------------------------------------------------|
| BIOS + UEFI loader (C/Lua) | done    | 3-partition GPT image (ESP / BIOS boot / ext4 root), Lua menu and CLI, ELF64 kernel loader |
| Memory (PMM, VMM, heap)  | done    | bitmap PMM, own page tables with direct map and W^X, buddy + slab heap |
| CPU / interrupts         | done    | GDT/TSS, IDT, exception recovery, PIT                                  |
| ACPI / APIC              | done    | LAPIC + IOAPIC, 8259 masked, LAPIC timer calibrated                    |
| Input / output           | done    | PS/2 keyboard (us/fr), serial, VGA text and framebuffer console (Unicode, double-buffered in RAM); `snake` and `cube` demos draw pixels on the framebuffer (UEFI GOP, or a BIOS VBE linear mode chosen by the loader: `vbe_mode` in loader.conf) |
| PCI / AHCI               | done    | bus scan, SATA DMA read/write/flush; polled (no interrupts yet)       |
| VFS                      | done    | mounts, path walk, symlinks, descriptor API                            |
| ext4                     | partial | R/W, extents, checksums written; no journal, see limits below          |
| Ring 3                   | done    | user page tables per process, `syscall`/`sysret`, TSS stacks, fault containment (phase 2.1) |
| Processes / scheduler    | done    | PCB, kernel stacks, context switch, kernel threads (2.2); preemptive round-robin on the timer tick, 4 priority levels, `sleep`/`setprio` syscalls, CPU-time watchdog (2.3); FXSAVE state per process (2.4); parent/child links, `fork` + copy-on-write, blocking `wait`, wait-deadlock guard (2.5) |
| ELF loader / `execve`    | done    | static ELF64 (`ET_EXEC` / static PIE with RELATIVE relocations) and, since 3.2's last step, dynamically linked programs (`PT_INTERP`: the interpreter mapped unrelocated at its own base and started with `AT_BASE` / `AT_PHDR` / `AT_PHNUM` / `AT_ENTRY`), W^X segments, System V initial stack with argv / envp / auxv, `execve` syscall, `/bin/hello` demo built from `usr/` (2.4) |
| fork / wait / signals     | done    | `fork` clones the address space copy-on-write (refcounted shared pages, fault-time copies), `wait` reaps zombies (blocking, WNOHANG, by pid, Linux status word), SIGKILL/SIGTERM/SIGINT/SIGCHLD/SIGUSR1 with per-process handlers delivered at the syscall boundary and restored register-for-register by a sigreturn trampoline (2.5) |
| Descriptors / pipes / /dev | done   | a fd table per process (0/1/2 on the console), open file descriptions shared by fork / dup (shared offsets, POSIX), pipes with bounded blocking and EOF / EPIPE, devfs on /dev with `console` / `null` / `zero` served by the descriptor layer, syscalls read / open / close / lseek / dup / pipe (2.6) |
| POSIX syscalls (libc set) | done | 17 new syscalls (stat / fstat / brk / mmap / munmap / chdir / getcwd / dup2 / fcntl / getdents / ioctl / unlink / mkdir / rmdir / rename / link / uname), `O_NONBLOCK` / `O_CLOEXEC`, per-process cwd, the SIGPIPE rule, `uname` / the isatty ioctl pair; x86-64 Linux `struct stat` and `linux_dirent64` layouts (3.1) |
| mlibc prerequisites (3.2, first step) | done | `arch_prctl` (TLS: FS/GS base per process, switched with it), `futex` (WAIT / WAKE / REQUEUE / BITSET, bounded parking, per-address-space queues), `sigaction` / `sigprocmask` (per-handler masks, SA_RESTORER / SA_NODEFER / SA_RESETHAND, mask saved and restored by sigreturn, kept across fork / execve), `mprotect` (W^X, COW-aware), `poll` / `select` (readiness probe over files / console / pipes), `clock_gettime` (MONOTONIC from the tick, REALTIME anchored at the RTC, CPUTIME from the charged ticks) |
| mlibc sysdeps port (3.2, second step) | done | `ports/mlibc/sysdeps/prismos`: the tag layer over syscalls 0..43 (mlIBC 7.0.0's interface -- `Open`/`Read`/`Write`/`Stat`/`VmMap`/`FutexWait`/`Sigaction`/`Poll`/`ClockGet`/...), the raw `syscall` stubs (rax = nr, rdi/rsi/rdx/r10/r8/r9, -errno in rax), the x86-64 ABI headers (Linux layouts the kernel deliberately speaks, except the kernel's own O_* values in fcntl.h), the crt objects and the SA_RESTORER restorer; what the kernel lacks stays ENOSYS (threads / clone, sockets, *at() past AT_FDCWD, rusage) |
| x86_64-prismos toolchain + mlibc sysroot (3.2, third step) | done | `ports/toolchain`: binutils 2.44 + gcc 14.2.0 cross tools for the `x86_64-prismos` triplet (the two patches in `patches/` teach them the OS the way every new OS does), the ABI deliberately Linux-shaped (`glibc-stdint.h` fast types, `gnu-user.h`'s C++ feature-test cascade, `__cxa_atexit`, the crtbegin/crtend sandwich); mlibc built with Meson against it — headers first, then the compiler, then the library — installed into `build/toolchain/usr/x86_64-prismos` (libc.a + the four crt objects + 209 headers); `/bin/hello` rebuilt against it links as the static PIE the loader expects and passes `verify.sh`'s twenty loader-contract checks; the cross build also flushed out and fixed three latent port bugs (see ports/mlibc/README.md) |
| the four /bin programs on mlibc (3.2, fourth step) | done | `usr/mlibc`: `/bin/hello`, `/bin/fork`, `/bin/fd` and `/bin/posix` ported off the mini libc — same scenarios, same checks, the C library's vocabulary (stdio on the descriptor table, `fork` / `waitpid` / `kill` / `signal`, `opendir` / `readdir`, `tcgetattr`, `uname`, `errno`), the four PrismOS-only calls raw through `prism_raw.h` (`uptime_ms`, `yield`, the millisecond `sleep`, `brk`); each binary passes `verify.sh`'s loader-contract checks with its own syscall and symbol set — 130 checks across the four — via `make usr-mlibc` (`bootstrap.sh stage progs`; `stage hello` keeps the step-3 proof as a regression) |
| dynamic linking (3.2, fifth step) | done | the loader honours `PT_INTERP` (the interpreter read from the VFS with the program's own rules, mapped unrelocated at `USER_INTERP_BASE`, the process started at *its* entry with `AT_BASE` plus the program's `AT_PHDR` / `AT_PHNUM` / `AT_ENTRY`; the kernel relocates neither image — that is `ld.so`'s job now); anonymous `PROT_NONE` mappings (supervisor-only pages: the DSO-reservation idiom, upgradable by `mprotect`, replaceable by `MAP_FIXED`); mlibc rebuilt `-Ddefault_library=both` — `libc.so` + `ld.so` in the sysroot (two port fixes the shared build forced: `__do_syscall_ret` out of the rtld, the restorer hidden so its address is not GOT-shaped); the gcc patch now carries a Linux-shaped `LINK_SPEC` (the first version silently dropped `-shared`); `make usr-mlibc-dyn` builds the dynamically linked hello and `verify.sh`'s dyn mode checks the whole chain (47 checks); the boot image grows `/lib/ld.so` + `/lib/libc.so` + `/bin/hello-dyn` |
| Networking, USB, SMP     | todo    | not started                                                            |

**Testing.** `make fstest` (11 scenarios against `mke2fs` images, each validated
by `e2fsck -fn`) covers the VFS and ext4 on the host. `make test` boots the image
under QEMU on BIOS and UEFI and drives the shell over the serial line, including
the ext4 root partition (read-write, re-checked with `e2fsck` on the host), a
partition-layout assertion, a ring 3 process / context switch scenario
(phase 2.2) and an ELF scenario (phase 2.4: `/bin/hello` with argv/envp/auxv,
`execve`, its error paths, two float workloads interleaved by the scheduler,
fault containment, exit status), and a phase 2.5 scenario (`/bin/fork`:
copy-on-write isolation, fork pids and status round trips, blocking and WNOHANG
waits, wait-by-pid, a SIGCHLD handler, a SIGTERM kill, a self-signal whose
`kill` return value must survive the handler round trip, `execve` from a
forked child, orphan adoption, `kill` error paths), and a phase 2.6 scenario
(`/bin/fd`: stdio through the table, lseek with a shared description, the
`/dev` devices, pipes -- round trip, FIFO order across split reads, blocking
until a forked writer writes, EOF, EPIPE -- descriptor inheritance across
fork (the child reads where the parent left off and hands the bytes back
through an inherited pipe), dup (shared description, the close(1) + open
renumbering trick), a real read of fd 0 from the serial line, and fds
surviving `execve`).
Phase 3.1 adds a scenario of its own (`/bin/posix`): stat / fstat with the
x86-64 Linux layouts (file types, `rdev` of the devices, a fifo pipe end),
the program break, anonymous and file `mmap` (W^X, `MAP_FIXED`, `munmap`,
copy-on-write across fork, a private file mapping whose fill leaves the
fd offset alone), the per-process cwd (a relative `open` that lands in the
process's directory, not the shell's), `dup2` / `fcntl` / `F_DUPFD` / an
`O_CLOEXEC` fd checked from a re-`execve`d child, `O_NONBLOCK` pipes, the
SIGPIPE rule (a readerless writer dies by signal 13 unless it ignores the
signal), `getdents` over `/bin` and `/dev`, the name-space syscalls with
their error paths, and `uname` / `isatty`.

**Known limits of phase 1.4.** No journal (writes go straight to disk; a crash
mid-operation can need an `fsck`; journals that need recovery mount read-only),
metadata checksums are written but not verified on read, htree directories are
scanned linearly and lose their index when modified, no `inline_data` /
`bigalloc` / `meta_bg` / encryption, no permission checks (single user, ring 0),
no page cache for file data, whole-block I/O is synchronous and polled.

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
- [ ] Phase 3.3 — `init`, TTY layer with line discipline, `/proc` and `/tmp` (tmpfs)
- [ ] Phase 3.4 — BusyBox and a userspace shell
- [ ] Phase 3.5 — precise time (HPET/TSC behind the 3.2 `clock_gettime`), `/dev/random`, users and permissions

Phase 4 — toolchain
- [ ] Phase 4.1 — cross-binutils for `x86_64-prismos`
- [ ] Phase 4.2 — cross-gcc (C, then C++), libgcc, libstdc++
- [ ] Phase 4.3 — build the userspace (BusyBox, make, core utilities) with it

Phase 5 — self-hosting
- [ ] Phase 5.1 — native binutils + gcc running on PrismOS
- [ ] Phase 5.2 — native `make`, `sh`, `tar`, `git`-less source tree on the ext4 disk
- [ ] Phase 5.3 — rebuild the userspace natively, then the kernel's C parts (loader)
- [ ] Phase 5.4 — bootstrap: PrismOS builds PrismOS

Beyond (nice to have)
- [ ] networking: virtio-net or e1000, ARP/IP/UDP/TCP, a tiny `ping` and `wget`
- [ ] USB (xHCI) keyboard and mass storage, NVMe
- [ ] other file systems: FAT32 R/W in the kernel, tmpfs
- [ ] graphics: framebuffer userspace API, a minimal window system
- [ ] desktop (Xorg, then KDE): mlibc is the entry ticket, not the whole price -- also Unix domain sockets, `epoll`, shared memory (`MAP_SHARED` of files, `shm`), a display / input device interface (KMS-style), and a lot of the Linux surface (`/proc`, `/sys`, `udev`-like discovery)
- [ ] AArch64 port of the kernel and loader

*`One day, perhaps, PrismOS will be self-hosted lol`*

## Ring 3 (phase 2.1)

`sys/kern/src/user/usermode.rs` (+ `AddressSpace` in `mem/vmm.rs`, `swapgs` in
`arch/intr.rs`):

- **GDT**: `0x18` user data, `0x20` user code (RPL 3 -> `0x1b`/`0x23`), ordered so that
  `sysretq` lands on them (`STAR[63:48] = 0x10`). TSS has `RSP0` (set per run) and IST stacks
  for #DF, NMI and #MC.
- **Address spaces**: one PML4 per process. The kernel half and the kernel's low identity
  slot are shared (never `U/S`), user space is PML4 slots 1..127
  (`0x80_0000_0000` .. `0x4000_0000_0000`); code at the bottom, stack growing down from the top
  with an unmapped guard page. W^X is enforced, pages are freed on drop. The kernel reaches
  user memory only by walking the tables (`AddressSpace::read/write`), never by pointer.
  SMEP/SMAP/UMIP are enabled when the CPU has them.
- **Entry/exit**: `syscall`/`sysretq` (LSTAR, SFMASK) plus interrupts and exceptions through
  the IDT, with `swapgs` on every ring 3 <-> ring 0 transition and a per-CPU block in GS.
- **Containment**: any exception in ring 3 (or the watchdog) ends the program
  and returns to the scheduler; the kernel never panics because of user code.
- **Syscalls** (rax = number, args rdi/rsi/rdx, result in rax, `-errno` on failure):
  `0 exit`, `1 write(fd 1|2)`, `2 getpid`, `3 uptime_ms`, `4 yield` (phase 2.2),
  `5 sleep`, `6 setprio` (2.3), `7 execve` (2.4), `8 fork`, `9 wait`, `10 kill`,
  `11 signal`, `12 sigreturn` (2.5).

Shell: `user` lists the built-in test programs, `user <name>...` runs one or
more at once, `usertest` runs them all (outcome, fault type and page-leak check,
plus a two-process interleaving, a kernel-thread scenario and the 2.3
preemption / priority / wake-up scenarios) -- also part of
`make test`.

## Descriptors, pipes and /dev (phase 2.6)

`user/fd.rs` (the descriptor layer), `fs/devfs.rs` (the /dev directory) and
the `Opened::Dev` arm of `fs/vfs.rs`; the demo / test program is `/bin/fd`
(usr/fd).

- **Open file descriptions**: what one `open` produced -- what it is
  connected to (a slot of the VFS open-file table, a device, one end of a
  pipe) plus its flags -- is `Rc`-shared.  `fork` hands the child the *same*
  descriptions (not copies), so file offsets are shared; `dup` adds a second
  number for the same one; `execve` keeps the table as POSIX says.  Dropping
  the last clone is what closes the underlying thing, so death (which
  releases the whole table) closes every descriptor at once.
- **Per-process tables**: 64 numbers, fd 0/1/2 start on the console (three
  separate descriptions, like any unix boot), `open` installs at the lowest
  free number (`-EMFILE` past 63).  `write` (syscall 1) now routes through
  the table like everything else; the six new calls are 13 `read`, 14 `open`,
  15 `close`, 16 `lseek`, 17 `dup`, 18 `pipe`.
- **Files**: the offset and the permission checks live in the VFS slot
  (`fs/vfs.rs`), so a shared description is shared exactly; `lseek`
  SET/CUR/END works on any file fd, pipes and devices answer `-ESPIPE`.
- **/dev (devfs)**: a read-only in-memory file system mounted on /dev at
  boot, holding `console` (crw--w----), `null` and `zero`.  It only provides
  the *namespace* -- `open("/dev/console")` resolves to a device node
  (`FileType::Dev(id)`), and the descriptor layer turns the id into the
  device that does the I/O.  Console writes go through the sanitising path
  (no terminal escapes from ring 3), console reads park for a key or a
  serial byte; null reads EOF and swallows writes; zero produces zeroes.
- **Pipes**: one 16 KiB ring buffer shared by both ends, plus the count of
  live descriptions per end, so a reader can tell EOF (`writers == 0`) from
  would-block, and a writer gets `-EPIPE` (`readers == 0`) instead of
  blocking forever.  Reads drain in FIFO order, short counts are returned
  as is.
- **Blocking**: a pipe read on an empty buffer (writers alive) and a console
  read with nothing typed park the process -- ~20 ms at a time, re-checked
  on every wake -- for at most 10 s, then `-EAGAIN`.  The bound is
  deliberate: the shell is blocked while its children run, so nothing may
  wait forever (the same rule `sleep` follows).

Limits worth knowing, as phase 2.6 left them (the libc set of phase 3.1
lifted most -- see below): no `O_NONBLOCK` / `O_CLOEXEC`, no `dup2` /
`fcntl`, no per-process cwd (paths resolved against the shell's, like
`execve`'s), no `getdents` (a directory fd opened but could not be read),
no SIGPIPE delivery (a readerless write returned `-EPIPE`), and nothing
about permissions (single user, ring 0).

Shell: `ls /dev` and `mount` show the device directory (`c` type chars,
devfs read-only); `exec fd` runs the ten phase 2.6 scenarios and prints
`fd: all 10 passed` -- also part of `make test`.

## The libc syscall set (phase 3.1)

`user/sysc.rs` (the memory / process half: `brk`, `mmap`, `munmap`,
`chdir`, `getcwd`, `uname`) and the descriptor layer grown to POSIX size
(`user/fd.rs`: `stat`, `fstat`, `dup2`, `fcntl`, `getdents`, `ioctl`,
`unlink`, `mkdir`, `rmdir`, `rename`, `link`); the demo / test program is
`/bin/posix` (usr/posix).  The dispatch (user/syscall.rs) reads up to six
arguments now -- `mmap` needs r10 / r8 / r9, the Linux order.

- **stat / fstat** fill the x86-64 Linux `struct stat` layout (144 bytes)
  exactly -- the ABI the future mlibc port compiles against.  The file
  type lives in the mode's `S_IFMT` bits (`S_IFREG` / `S_IFDIR` / `S_IFLNK`),
  a devfs node is `S_IFCHR` with `st_rdev` = `(1 << 8) | id` (Linux's mem
  major), a pipe end fstats as a fifo.  `st_dev` is 0 (no device numbering
  yet), `st_blksize` is 4096.
- **brk** is sbrk's engine, the 1975 contract: one break per process,
  growing from the page after the loaded image (`brk_base`, a PCB field)
  to `USER_MMAP_BASE`, byte-granular in the value and page-granular on the
  pages.  `brk(0)` / any invalid address answers the current break
  (Linux's way of saying no); a fork child inherits the break with the
  copy-on-write pages, `execve` restarts it at the new image's end.
- **mmap / munmap** place everything first-fit above `USER_MMAP_BASE`
  (4 GiB into user space, above the trampoline and any image, below the
  stack) by probing the page tables -- there is no VMA list.
  `MAP_ANONYMOUS` hands out zero pages (`MAP_SHARED` included: it is just
  memory); a `MAP_PRIVATE` file mapping is filled eagerly through the new
  VFS `pread` (the description's offset never moves, bytes past EOF read
  as zero instead of the POSIX SIGBUS); `MAP_FIXED` first unmaps whatever
  was there.  W^X is enforced (`PROT_WRITE | PROT_EXEC` is `-EACCES`, the
  loader's rule); an anonymous `PROT_NONE` mapping is honoured since
  3.2's last step (pages present but supervisor-only, any ring-3 access
  SIGSEGVs, `mprotect` can upgrade them, `MAP_FIXED` replaces them -- the
  loader's reservation idiom), while `PROT_NONE` of a *file* and unknown
  bits are `-EINVAL`; mappings
  over the signal trampoline page are refused, and `MAP_SHARED` of a file
  is `-ENOSYS` (no page cache to write back through).  `munmap` releases
  pages through the COW table, so a fork sibling keeps its copy.
- **chdir / getcwd**: a cwd *per process* (a PCB string; the shell keeps
  its own in fsys.rs).  A fresh process starts from the shell's cwd,
  `fork` copies it, `execve` keeps it, and every path-taking syscall
  (`open`, `stat`, `unlink`, `execve`...) resolves against it -- a
  relative `open` lands where the process stands, not where the shell
  does.  `getcwd` returns the length including the NUL, `-ERANGE` when
  the buffer cannot hold it.
- **dup2 / fcntl / O_CLOEXEC**: `dup2` pins the description to an exact
  number (closing what occupied it -- the shell-redirection trick),
  `F_DUPFD` takes the lowest free number at or past its argument, and
  `F_GETFD` / `F_SETFD` manage `FD_CLOEXEC` -- a property of the *number*,
  kept in the per-process table, cleared by every dup, set by `open`
  with `O_CLOEXEC` and honoured by `execve` (which closes the marked
  numbers and keeps the rest, POSIX).  `F_GETFL` / `F_SETFL` read and
  rewrite the *description's* flags (shared by every dup, exactly POSIX):
  `O_NONBLOCK` and `O_APPEND` may change after the open.
- **O_NONBLOCK**: a pipe read with nothing buffered and writers alive, a
  full pipe write, and a console read with nothing typed all answer
  `-EAGAIN` at once instead of parking.
- **SIGPIPE** is the POSIX rule now: a write to a readerless pipe kills
  the writer by signal 13 (the `wait` status word says so) unless it
  installed a handler or `SIG_IGN` -- `signal` grew dispositions beyond
  a plain handler (`0` default, `1` ignore, the classic values) -- in
  which case the write answers `-EPIPE` and the program carries on.
- **getdents** walks a directory fd in the x86-64 Linux
  `linux_dirent64` record format (`d_ino`, `d_off`, `d_reclen`, `d_type`,
  name), so opendir / readdir wrappers are one loop away.  The VFS slot's
  offset doubles as the entry cursor (`lseek(fd, 0, SET)` rewinds),
  `d_off` is the next entry's index, records go back whole or not at
  all, and the call ends at 0 (the EOF).
- **ioctl** answers the `isatty` pair on the console: `TCGETS` succeeds
  (a zeroed termios -- nothing to configure yet) and `TIOCGWINSZ`
  reports the 80x25 text grid; every other request, and every
  non-console descriptor, is `-ENOTTY`.
- **unlink / mkdir / rmdir / rename / link** are the VFS name-space calls
  exposed to ring 3, with the errno surface a POSIX program expects
  (`-EEXIST`, `-ENOTEMPTY`, `-EISDIR` through unlink, `-ENOTDIR`,
  `-EXDEV` across mounts, `-EMLINK` at the link limit).  `mkdir`'s mode
  bits reach the inode (nothing checks them yet -- phase 3.5).
- **uname** fills the six 65-byte fields (`PrismOS` / `prismos` / `3.2` /
build string / `x86_64` / `(none)`).

Not here (yet): `MAP_SHARED` of a file, `fchmod` / `chmod`
and anything permission-shaped (phase 3.5), `wait4`-style rusage, and
`getdents` `d_type` for file systems that do not know one (ext4 and devfs
always do).  The former 3.1 gaps a library cares about -- `select` / `poll`,
`mprotect`, `clock_gettime`, TLS / `futex` / `sigprocmask` -- are phase 3.2's
first step, below; the `PROT_NONE` of the loader's reservations is 3.2's
last step, below that.

Shell: `exec posix` runs the ten phase 3.1 scenarios and prints
`posix: all 10 passed` -- also part of `make test` (the re-`execve`d
child reports `postexec: O_CLOEXEC fd closed, dupfd alive, stdout ok`).

## The mlibc prerequisites (phase 3.2, first step)

Eight syscalls (36..43) that close, kernel-side, the distance between the
3.1 ABI and what an [mlibc](https://github.com/managarm/mlibc) `sysdeps`
port needs before anything links: a thread pointer, a sleep/wake primitive,
real signal masks and dispositions, page re-protection, readiness
multiplexing and a clock.  They live where their phase-3.1 cousins do:
`user/sysc.rs` (`arch_prctl`, `mprotect`, `clock_gettime`), `user/futex.rs`
(the waiter table), `user/signal.rs` (the POSIX half) and `user/fd.rs`
(`poll` / `select` on a readiness probe).

- **arch_prctl** (36) is TLS: `ARCH_SET_FS` parks the thread pointer in the
  FS base MSR (live at once -- the caller is running), `ARCH_GET_FS` reads
  it back through a user word, and the GS pair goes through the swapgs
  shadow slot (`IA32_KERNEL_GS_BASE`).  The bases are PCB fields
  (`fs_base` / `gs_base`) published by the scheduler on every switch
  (`usermode::set_user_thread`, zeroed when the kernel context runs),
  copied by `fork` and cleared by `execve` -- a fresh image installs its own
  with the syscall, exactly what a mlibc `set_thread_area` does.  Bases must
  live in user space.
- **futex** (37, six arguments like `mmap`): `FUTEX_WAIT` quotes the 32-bit
  word (-`EAGAIN` on a mismatch, the lost-wakeup check) and parks; the waiter
  table is keyed by (address space, word address) -- the PML4's physical
  address, so after a `fork` parent and child wait on their own COW copies,
  Linux's per-mm rule.  Parking follows the bounded-blocking house rule
  (~20 ms naps, 10 s ceiling, then -`ETIMEDOUT`): the shell is blocked while
  its children run, so nothing may wait forever -- a library treats the
  timeout like a spurious wakeup and retries.  `FUTEX_WAKE` flips up to `val`
  matching waiters to ready; the BITSET pair narrows the match with `val3`'s
  bits; the REQUEUE pair wakes `val` waiters and moves up to `val2` more
  onto a second word (a condvar broadcast's second half),
  `FUTEX_CMP_REQUEUE` checking the word first.  A dying process drops its
  waiter entry on the way out.  Not here: `FUTEX_FD`, the PI variants, and
  cross-process shared mappings (no `MAP_SHARED` files yet).
- **sigaction / sigprocmask** (38 / 39) make the signal state POSIX.  An
  action is four u64 words -- handler, flags, restorer, mask -- the x86-64
  kernel layout a mlibc port copies into.  `SA_RESTORER` swaps the kernel
  trampoline for the caller's own return address (the restorer that makes
  the `sigreturn` syscall); `SA_NODEFER` and `SA_RESETHAND` are honoured at
  delivery; `SA_NOCLDSTOP` / `SA_NOCLDWAIT` / `SA_RESTART` are accepted and
  ignored (no stop/continue, no restart); `SA_SIGINFO` and `SA_ONSTACK` are
  refused -- no siginfo, no alternate stack (yet).  Delivery pushes the
  interrupted mask in front of the saved frame on the user stack and
  `sigreturn` restores it; the handler wears `blocked | sa_mask | sig`
  (minus SIGKILL, never blockable).  `sigprocmask` exchanges the 8-byte mask
  (`SIG_BLOCK` / `SIG_UNBLOCK` / `SIG_SETMASK`, a NULL set queries).  The
  mask survives `fork` and `execve` (POSIX); caught handlers reset on exec,
  `SIG_IGN` survives; the old `signal` syscall is now the plain wrapper.
- **mprotect** (40) re-protects the pages of a range: W^X applies
  (`-EACCES`), `PROT_NONE` and unknown bits are `-EINVAL`, a hole in the
  range is `-ENOMEM` (Linux), and the signal trampoline page is locked r-x
  forever.  The walk (vmm.rs) is COW-aware in both directions: making a COW
  page writable resolves the copy first, and making a *shared* read-only
  page writable (a code page a fork sibling also maps, refcount > 1) copies
  it, so the sibling never sees the write; clearing write just clears RW.
  This is what a libc's RELRO pass needs.
- **poll / select** (41 / 42) watch the descriptor table through one
  readiness probe: files, `/dev/null` and `/dev/zero` are always ready in
  the directions they were opened for (EOF is readiness -- a read answers
  0), the console is readable exactly when a decoded byte is pending (the
  keyboard and serial rings, peeked non-destructively), a pipe read end is
  readable with buffered bytes or at EOF (`POLLHUP`), a pipe write end
  writable with buffer space or `POLLERR` when nobody reads.
  `poll` is the Linux pollfd array (8-byte entries, `POLLNVAL` for closed
  numbers, negative fds skipped, `POLLERR` / `POLLHUP` / `POLLNVAL` always
  reported); `select` is the three fd_sets plus a timeval (`-EBADF` for a
  watched closed number, exceptfds accepted and always empty).  Both park
  the bounded way until something is ready or the timeout (negative / NULL =
  forever, house-capped) expires.
- **clock_gettime** (43): `CLOCK_MONOTONIC` (and raw / coarse / boottime)
  from the tick counter, `CLOCK_REALTIME` (and coarse) anchored at the
  RTC's read of boot -- a lazy once, the first caller pays the CMOS reads --
  and the two CPUTIME ids from the process's charged ticks.  Millisecond
  precision, the x86-64 `struct timespec` (i64 sec, nsec) written back.

Not here (yet): `SA_SIGINFO` / `SA_ONSTACK`, robust / PI futexes,
`FUTEX_CLOCK_REALTIME` (every clock is the uptime one), `sigtimedwait`,
`ppoll` / `pselect` (the signal-mask argument -- trivial once wanted), and
`gettimeofday` (a wrapper away).  The mlibc port that leans on all of it
is the phase's second step, below.

## The mlibc sysdeps port (phase 3.2, second step)

`ports/mlibc` -- `sysdeps/prismos`, a directory to drop into an
[mlibc](https://github.com/managarm/mlibc) checkout, plus the
seven-line `meson.build` patch that registers the `prismos` system.
mlibc's generic code calls *tags* (`Open`, `Read`, `Write`, `Stat`,
`VmMap`, `FutexWait`, `Sigaction`, `Poll`, `ClockGet`, ...); the port is
one implementation per tag, and every implementation is one syscall --
nothing in between (generic/sysdeps.cpp).  The raw convention lives in
generic/syscall.cpp: `rax` = number, `rdi` / `rsi` / `rdx` / `r10` / `r8` /
`r9` = arguments 1..6, the answer in `rax` with `[-4096, -1]` meaning
`-errno` -- the x86-64 Linux register order the kernel's dispatcher has
read since phase 2.1, which is why the stubs look like every other
Linux-ABI port's.

- **What the kernel has, the port speaks**: all 44 syscalls.  The
  layouts cross unchanged because the kernel made them Linux-shaped on
  purpose (3.1's decision): `struct stat` is the x86-64 Linux one, the
  sigaction exchanged with the kernel is the four-u64 (handler / flags /
  restorer / 8-byte mask) rt_sigaction shape, `sigprocmask` trades the
  low 8 bytes of a sigset_t, `poll` speaks the Linux pollfd, and the
  SA_* / CLOCK_* / errno values are the classic ones.  `TcbSet` parks
  the TCB in the FS base through `arch_prctl` (the x86-64 way); the
  SA_RESTORER half of the signal contract is x86_64/signal.S, a
  three-instruction restorer that makes the `sigreturn` syscall (12)
  when a handler returns.
- **What the kernel lacks stays ENOSYS** -- the rule of the phase.  Tags
  the kernel cannot serve are absent from the tag list
  (include/mlibc/sysdeps.hpp), and mlibc's generic layer answers
  `ENOSYS` on its own: `pthread_create` (no clone), sockets, `*at()`
  past `AT_FDCWD`, rusage accounting (wait4 gets honest zeroes),
  `fchmod` and the permission family (phase 3.5).  A few corners are
  emulated where the emulation is exact: `pipe2`'s flags ride `fcntl`
  (`O_CLOEXEC` -> `F_SETFD`, `O_NONBLOCK` -> `F_SETFL`), `opendir`
  checks `S_ISDIR` itself (the kernel has no `O_DIRECTORY` bit), and
  `pselect` refuses a signal mask rather than pretend -- plain
  `select` works.
- **The one ABI that is *not* Linux's**: the O_* flags.  The kernel's
  own values (`O_READ` 1, `O_WRITE` 2, `O_CREATE` 4, `O_TRUNC` 8,
  `O_APPEND` 16, `O_EXCL` 32, `O_NONBLOCK` 64, `O_CLOEXEC` 128 --
  fs/vfs.rs) are what the port's abi-bits/fcntl.h defines, and the
  Linux-only names (`O_DIRECTORY`, `O_TMPFILE`, ...) are reference
  constants so generic library code compiles -- the kernel answers
  `-EINVAL` if one is actually passed (it refuses unknown bits).  The
  rest of abi-bits/ is carried over from mlibc's own linux port (see
  PROVENANCE.txt): those headers *are* the ABI the kernel speaks.
- **Verified against mlibc 7.0.0** (meson 1.12, gcc 14): the whole
  `ansi` + `posix` options build, the headers and the four crt objects
  (crt1 / Scrt1 / crti / crtn -- a static-PIE-safe `_start`, `main`
  taken RIP-relative) install into a sysroot, and a `write()`-based
  hello world links as the static PIE the kernel's loader expects
  (`-pie --no-dynamic-linker -z separate-code -z max-page-size=4096`),
  `write` reaching `syscall 1` and the restorer reaching `syscall 12`
  under disassembly.  What cannot be tested off-target is the run
  itself: that is step 3's business (the cross compiler, `/bin/hello`
  rebuilt against mlibc, `make test`).

Next: phase 3.3 -- `init`, a TTY layer with line discipline, `/proc` and
`/tmp` (tmpfs).  Everything the phase set out to do is in: the sysdeps
port, the cross toolchain and sysroot, the four ports, and (the last
step, below) the ELF loader's dynamic linking.  What still waits for a
full dev box is the *run* of the dynamic chain -- QEMU booting the image,
`exec hello-dyn`, the summaries printed from the far side of `ld.so`.

## The cross toolchain and the mlibc sysroot (phase 3.2, third step)

`ports/toolchain` -- the phase's todo, verbatim: *build mlibc with Meson
against a cross C/C++ compiler (headers first, then the compiler, then the
library), install it into a sysroot*.  Two scripts drive the same pipeline
at two speeds.  `bootstrap.sh` (minutes): the cross compiler is a prefixed
pass-through around any C23/C++23 host compiler -- the arrangement the
step-2 verification used, because mlibc isolates its own includes and the
meson cross file is what makes the build *PrismOS*.  `build.sh` (the real
thing): binutils 2.44 and gcc 14.2.0 built for the `x86_64-prismos`
triplet, installed into the same prefix under the same names, and mlibc
rebuilt with them.  Neither toolchain knows a `prismos` OS out of the box,
so the two patches in `ports/toolchain/patches/` teach them the way every
new OS does (config.sub's OS list, the ELF target cases of bfd / ld / gas,
gcc's config.gcc and libgcc's config.host -- SerenityOS's recipe for
`x86_64-pc-serenity` being the family precedent).

- **The order is the todo's parenthetical.**  *Headers first*: mlibc's
  headers alone (`-Dheaders_only=true`) install into
  `build/toolchain/usr/x86_64-prismos/usr/include` -- the sysroot a cross
  compiler is built against (`--with-sysroot`).  *Then the compiler*:
  binutils, then gcc (`--without-headers` + `--with-newlib`: the
  compilers and libgcc, `inhibit_libc` set so libgcc includes no libc
  header -- mlibc proper does not exist yet at that point; no target
  runtime is built, because mlibc *is* the target runtime).  *Then the
  library*: the whole `ansi` + `posix` surface the kernel serves, static,
  plus the four crt objects, built with the real cross tools under the
  same cross file.  It lands as `libc.a` in the sysroot.
- **The cross gcc's ABI is deliberately Linux-shaped** -- the kernel made
  its layouts Linux-shaped on purpose (3.1's decision), and mlibc's x86_64
  headers assert the compiler agrees: `glibc-stdint.h` (not newlib's)
  gives the LP64 fast types those assertions check; `gnu-user.h` brings
  the GNU C++ feature-test cascade (`-D_GNU_SOURCE`, without which
  mlibc-config.h resolves `_DEFAULT_SOURCE` off and half the abi-bits,
  starting with `struct rusage`, never define); `default_use_cxa_atexit`
  gives mlibc's exit path its `__dso_handle` (it lives in `crtbeginS.o`
  only when that is on); and libgcc's `extra_parts` install the four
  crtbegin/crtend objects a static-PIE link sandwiches around the program.
  One more guard rides in the patch: libgcov's `mmap` include now respects
  `inhibit_libc` (the gcov runtime is meaningless without a libc, and the
  target libgcc configure -- with the sysroot visible -- had decided
  `HAVE_SYS_MMAN_H` while the build had no headers to serve it).
- **The proof, and what the proof caught.**  `/bin/hello` rebuilt against
  mlibc (ports/toolchain/hello.c: `write` through the Write tag, `printf`
  through stdio, auxv, a caught signal round-tripped through the
  SA_RESTORER restorer, `clock_gettime`) compiles with the cross gcc and
  links as the static PIE the loader expects -- the same crt sandwich a
  `-static-pie` driver assembles.  `verify.sh` then runs the loader's own
  checks, statically: ET_DYN, no `PT_INTERP`, a `PT_DYNAMIC` carrying
  `DT_RELA` (401 `R_X86_64_RELATIVE` relocations) and never `DT_NEEDED` /
  `DT_JMPREL` / IRELATIVE, no two segments sharing a page, and under
  disassembly `write` reaching syscall 1, the restorer reaching syscall 12,
  `clock_gettime` reaching syscall 43.  All twenty pass.  Building the
  library with a real cross compiler also flushed out three latent port
  bugs the step-2 host build had masked -- the missing abi-bits installs,
  mlibc's IFUNC strcmp (its `R_X86_64_IRELATIVE` lands in `DT_JMPREL`,
  which the kernel's loader refuses outright), and an incomplete
  `struct rusage` behind a feature-test cascade a linux-target g++
  satisfies implicitly; all three are fixed in the port (see
  ports/mlibc/README.md).
- **What still waits for a full dev box:** the *run*.  `verify.sh` checks
  everything the loader checks without booting; `make test` (QEMU, the
  image, the mlibc-built `/bin` in the ext4 root) is the remaining steps'
  business -- the fourth step's ports (below), then dynamic linking.
  The sandbox this step ran in has no QEMU, no clang, no cargo
  (the boot chain needs all three); `make mlibc` and `make usr-mlibc`
  carry the step's deliverables into any checkout.

See ports/toolchain/README.md for the scripts, the stages, and the ABI
decisions spelled out.

## The four /bin programs on mlibc (phase 3.2, fourth step)

`usr/mlibc` -- the phase's todo, verbatim: *port `/bin/hello`, `/bin/fork`,
`/bin/fd` and `/bin/posix` from the mini libc (`usr/lib`) to mlibc*.  The
scenarios are the phases' own, unchanged -- every check the mini-libc
originals printed, the same summary lines (`fork: all 10 passed`,
`fd: all 10 passed`, `posix: all 10 passed`) -- but the vocabulary is the
C library's now: what the originals asked the raw syscall interface for
through `usr/include/prism.h`, the ports ask `fork` / `waitpid` / `kill` /
`signal` / `opendir` / `tcgetattr` / `uname` / stdio, and mlibc's
sysdeps turn those into the same 44 syscalls through the tag layer.

- **A translation, not a rewrite.**  Each program keeps its phase's shape:
  `hello` (2.4) still runs `info` / `fpu` / `chain` / `errors` / `exit` /
  `fault` -- `printf` on stdio, `getpid` / `execve` from `<unistd.h>`,
  `getauxval` from `<sys/auxv.h>`, `strerror(errno)` for the failures;
  `fork` (2.5) still runs its ten scenarios with `fork` / `waitpid` / the
  `W*` macros of `<sys/wait.h>` and `signal`'s `SIG_ERR` refusals
  (`catching SIGKILL -> EINVAL` is now a matter of `errno`, not a raw
  word); `fd` (2.6) still runs its ten on `read` / `open` / `close` /
  `lseek` / `dup` / `pipe` with `O_*` flags out of `<fcntl.h>`; `posix`
  (3.1) still runs its ten on `stat` / `fstat`, `opendir` / `readdir`,
  `tcgetattr`, `uname`, the `mmap` / `munmap` of `<sys/mman.h>` and the
  directory calls (`mkdir` / `unlink` / `rename` / `link`).
- **The error convention, folded.**  The mini libc returned the kernel's
  negative errnos in one word; POSIX returns `-1` and sets `errno`.  Each
  port carries a two-line `ec()` that folds the POSIX rule back into the
  single word the error tables compare, so the scenario tables
  (`-ENOENT`, `-EFAULT`, `-ECHILD`, ...) read exactly as they did.
- **What stays raw, and why.**  Four calls are the kernel's own
  vocabulary, things POSIX simply has no word for, and they stay raw
  through `usr/mlibc/prism_raw.h` -- `uptime_ms` (3, the tick counter
  behind `CLOCK_MONOTONIC`), `yield` (4), the millisecond `sleep` (5,
  neither `sleep(3)`'s seconds nor `nanosleep`'s timespec) and `brk` (21,
  exact-return, which mlibc's mmap-based allocator never wraps).  The
  header is the only place the `syscall` instruction appears outside
  `libc.a`, and it speaks the same convention the dispatcher has read
  since phase 2.1 (rax = number, rdi / rsi / rdx = arguments).
- **The build is the proof's build, four times.**  `bootstrap.sh stage
  progs` (`make usr-mlibc`; `build.sh stage progs` on the real tools)
  compiles each program with the cross gcc against the sysroot and links
  the same crt sandwich the step-3 proof linked (`Scrt1 crti crtbeginS
  prog.o libc.a crtendS crtn`, `-pie --no-dynamic-linker -z
  separate-code -z max-page-size=4096`).  `verify.sh` then runs the
  loader's own checks per program: the ELF contract (ET_DYN, no
  `PT_INTERP`, `DT_RELA`, never `DT_NEEDED` / `DT_JMPREL` / IRELATIVE,
  one protection per page) plus *that program's* syscall set under
  disassembly -- every call it makes reaching its number through either
  the tag layer (`mov $nr,%edi ... call __do_syscallN`) or the raw shim
  (`mov $nr,%eax ... syscall`) -- and the symbols those paths live in
  (`fork`, `waitpid`, `opendir`, `tcgetattr`, `uname`, ...).  21 checks
  for hello, 27 for fork, 32 for fd, 50 for posix: 130 across the four.
- **What still waits for a full dev box:** the *run*, as in step 3.  The
  four ports land in `build/usr/bin-mlibc/` and the boot image keeps the
  mini-libc `/bin` -- but since the fifth step the image also carries the
  dynamic trio (`/lib/ld.so`, `/lib/libc.so`, `/bin/hello-dyn`), so a dev
  box with QEMU can `exec hello-dyn` and watch the whole chain go.

## The ELF loader's dynamic linking (phase 3.2, fifth step)

The phase's todo, verbatim: *ELF loader: dynamic linking, a requirement
and not an option.  Honour `PT_INTERP` ... `DT_NEEDED` resolution stays
in userspace ... shared libraries are `ET_DYN`.*  Three halves had to
move together: the kernel's loader, the library's shared build, and the
link that joins them.

- **The kernel's half** (sys/kern/src/user/elf.rs, refactored into a
  `parse` + `map_image` pair the program and the interpreter now share).
  A `PT_INTERP` is no longer refused: its path is read from the image
  (NUL-terminated, size-capped) and the interpreter file goes through
  the *same* checks as the program -- regular, executable, under the 4
  MiB cap -- before any address-space work, so a bad one fails the
  `execve` cleanly.  It is mapped at `USER_INTERP_BASE` (vmm.rs: 2 GiB
  above the mmap region -- the first-fit allocator simply routes around
  the image, and the `brk`, capped below the mmap region, can never grow
  into it), with the same placement rules as any image (W^X, one
  protection per page, span cap, the trampoline page untouched).  The
  kernel then relocates *neither* image: the interpreter relocates
  itself from its own `_DYNAMIC` (its `_start` runs `relocateSelf`
  first), and the program is the interpreter's to relocate from the
  phdrs the auxv hands it.  The process starts at the *interpreter's*
  entry, with `AT_BASE` (its load bias) joining the auxv -- and
  `AT_PHDR` / `AT_PHNUM` / `AT_ENTRY` still describing the *program*.
  The static-PIE relocation rules (RELATIVE-only, never `DT_NEEDED` /
  `DT_JMPREL`) now apply only when there is no interpreter, exactly as
  they always did.
- **The reservation idiom** (sysc.rs + vmm.rs): an anonymous `PROT_NONE`
  mapping is honoured -- pages present but supervisor-only (U/S clear,
  NX set), so any ring-3 access SIGSEGVs through the usual fault path,
  `mprotect` can upgrade the range later, and `MAP_FIXED` over it
  replaces it.  That is the loader's idiom: mlibc's `ld.so` reserves the
  span each shared object will occupy with an anonymous `PROT_NONE`
  `mmap` before mapping its segments `MAP_FIXED` over the reservation,
  and the libc's own allocator guards its arenas the same way (which the
  *static* programs' `malloc` needed all along -- a latent runtime bug
  no static check could see).  `PROT_NONE` of a file stays `-EINVAL`:
  a page nobody may touch has nothing to be filled with.
- **The library's half**: a second meson build of the same patched tree
  with `-Ddefault_library=both` (`build.sh stage dynlib`) -- the static
  `libc.a` and the four programs never notice, and the sysroot gains
  `libc.so` (the C library as an `ET_DYN` with its own `PT_LOAD` /
  `PT_TLS` / `PT_GNU_RELRO`) and `ld.so` (the rtld: linked `-nostdlib`,
  self-relocating, `SONAME ld.so`).  The shared build forced two port
  fixes the static one never asked for.  `__do_syscall_ret` compiled out
  of the rtld (linux's own rule): its `errno` store drags the libc's TLS
  symbol into a link against nothing, and nothing in the rtld calls it.
  And the restorer's address is hidden now (`.hidden` in signal.S, the
  attribute on the declaration): taken as data it would go through the
  GOT, and a `GLOB_DAT` is a symbol relocation -- `relocateSelf` accepts
  `R_X86_64_RELATIVE` and traps on anything else.  The linux alternative
  (compiling the whole `Sigaction` wrapper out of the rtld) breaks the
  static archive's member pull -- the libc's own twin member would come
  in for it and collide -- so the port keeps the wrapper everywhere and
  hides the reference instead.
- **The toolchain's half**: the first dynamic link failed on the *driver*,
  not the linker -- gcc's generic-elf `tm_file` carries no `LINK_SPEC`,
  so `x86_64-prismos-g++ -shared` silently dropped the flag (no static
  link ever noticed; the four programs link through `$LD` directly).
  The gcc patch now uses `x86_64-*-linux*`'s order (`gnu-user.h` +
  `i386/gnu-user-common.h` + `i386/gnu-user64.h`, plus the new
  `i386/prismos.h`: the link emulations, the `/lib/ld.so` dynamic
  linker, the `__prismos__` / unix / posix builtins) and libgcc grows
  `crtbeginT.o` -- the `-static` half of the sandwich.
- **The proof and the image.**  `build.sh stage dyn` (`make usr-mlibc-dyn`)
  links the same `usr/mlibc/hello/hello.c` against the sysroot's
  `libc.so`: `PT_INTERP /lib/ld.so`, `DT_NEEDED libc.so`, `DT_JMPREL`
  for the PLT the rtld resolves, and BIND NOW (`-z now`) because
  mlibc's rtld zeroes `GOT[1]/GOT[2]` -- lazy binding is not an option.
  `verify.sh`'s dyn mode then checks the whole chain, still statically:
  the executable's shape (the interpreter path, the `DT_NEEDED`, the
  JUMP_SLOT relocations, `__mlibc_entry` left undefined for `libc.so` to
  answer, the raw shim still raw in the binary), `ld.so`'s
  self-containment (an `ET_DYN` with no `PT_INTERP` of its own,
  `RELATIVE`-only relocations, no undefined symbols) and its exact
  syscall diet under disassembly -- `open` / `read` / `close` / `lseek` /
  `fstat` / `getpid` for the library files, `mmap` / `munmap` /
  `mprotect` for the reservations and the segment protections,
  `arch_prctl` for the FS base the TLS lives on -- and `libc.so`'s
  exports (`__mlibc_entry`, `printf`, `malloc`, `__do_syscall6`) with
  hello's library calls now living in it: 47 checks.  The boot image
  carries the trio (a stripped `/lib/ld.so` and `/lib/libc.so` beside
  `/bin/hello-dyn`; `make root`) -- the kernel reads whole files and the
  debug sections buy it nothing.  The run itself, as through the whole
  phase, is a dev-box matter.

## fork, wait and signals (phase 2.5)

`fork` (user/process.rs + mem/vmm.rs + mem/cow.rs), `wait` (user/process.rs),
signals (user/signal.rs); the demo / test program is `/bin/fork` (usr/fork).

- **`fork`**: the child gets a fresh kernel stack and PML4 whose *leaf*
  entries point at the parent's physical pages: no data page is copied up
  front.  Writable pages are turned read-only + COW (PTE software bit 9) in
  *both* spaces; already read-only pages (code, rodata) are shared as is.
  Every shared leaf gets a refcount in a kernel-side open-addressing table
  (mem/cow.rs), so whichever address space dies first only drops the count.
  The first write to a COW page faults (#PF, write, user); the handler copies
  the page -- or just re-arms RW when the other owner already exited, a
  self-healing path also taken when a fork fails halfway.  The child resumes
  in ring 3 through the standard trampoline with the parent's whole register
  frame and `rax = 0`; the parent's `fork` returns the child's pid.  It keeps
  the name, priority and signal handlers (execve resets those), gets a copy
  of the live x87/SSE state and a fresh watchdog budget.
- **`exit` / zombies**: every death funnels through one helper -- outcome,
  death order, address space freed (shared pages uncounted), children adopted
  by pid 0, a waiting parent woken, SIGCHLD raised in a listening parent.
  A dead-but-unreaped process keeps its kernel stack until `wait` or the
  shell's reaper frees it.
- **`wait(pid, &status, flags)`**: blocking (a new `Waiting` state, woken by
  the matching child's death), `WNOHANG`, by pid or any child; writes a
  Linux-style status word (`exit << 8`, or the signal that killed it) and
  frees the zombie.  `-ECHILD` without a matching child, `-EFAULT` on an
  unwritable status word.  A wait cycle (two processes waiting on each
  other) is detected by the scheduler's idle path, which terminates the
  waiters with a message instead of wedging the machine.
- **Signals (basic)**: per-process handlers (`signal(sig, handler)`, SIGKILL
  uncatchable), `kill(pid, sig)` (self-kills act at once; a handled signal on
  another process goes pending), SIGKILL/SIGTERM/SIGINT terminate by default,
  everything else is ignored, SIGCHLD is raised in a parent when a child
  dies.  Delivery happens at the syscall boundary: the kernel pushes the
  *whole* interrupted register frame on the user stack (below the red zone),
  enters the handler like a call (`rdi` = signal) and plants the return
  address on a one-page trampoline (`mov eax, 12; syscall`) mapped into every
  address space; when the handler returns, `sigreturn` restores the saved
  frame verbatim -- so the interrupted code keeps its registers *and* its
  syscall return value.  While a handler runs the signal is blocked; standard
  signals coalesce.  Not here: stop/continue, `SA_RESTART`,
  altstacks, real-time queues, and fault-signal delivery (a #PF kills even
  with a SIGSEGV handler: resuming the instruction would just fault again).
  The POSIX half -- `sigaction` masks and restorers, `sigprocmask`, the
  delivery-time mask save / restore -- is phase 3.2's first step, above.

Limits worth knowing: `wait` only looks at the process table snapshot (a pid
reused later is a different process), the wait-deadlock guard kills *all*
stranded waiters together, and signals are only delivered at syscall returns
(a tight loop without syscalls only meets SIGKILL).

Shell: `ps` shows pid, **ppid**, state (ready / running / sleeping / waiting /
zombie), priority, CPU ticks, preemptions and outcome (`signal 9` for a killed
child).  `/bin/fork` (`exec fork`) runs the ten phase 2.5 scenarios and prints
`fork: all 10 passed` -- also part of `make test`.

## Processes and scheduler (phases 2.2 and 2.3)

`sys/kern/src/user/process.rs`:

- **PCB**: `Process` holds the pid, name, state (ready / running / zombie),
  outcome, its `AddressSpace` and its saved kernel context; the process table
  is a Vec in pid order, and the kernel itself is the implicit pid 0 ("main"
  context, running on the boot stack).
- **Kernel stacks**: every process gets a private 16 KiB stack from the kernel
  heap (which lives in the shared kernel half, so it is reachable whichever
  CR3 is loaded).  `TSS.RSP0` and `gs:[0]` point at it, so every trap from
  ring 3 lands on it -- a suspended process keeps its whole syscall / interrupt
  frame intact and resumes exactly where it stopped.
- **Context switch** (`switch_ctx`, naked asm): callee-saved registers + RSP +
  RFLAGS, setjmp/longjmp style; the switcher republishes the CPU-side state
  (CR3, RSP0, current-process pointer) *before* the stack switch, which is safe
  because the kernel half is mapped in every address space.  A fresh process
  starts on a trampoline that either drops into ring 3 through a prepared
  `iretq` frame (user program) or calls a plain Rust function (kernel thread).
- **Scheduling** (phase 2.3) is preemptive, strict priority + round-robin:
  4 levels (0 = best, default 2); the CPU goes to the best ready process and
  processes of one level take turns in table order.  The timer tick
  (`pit::on_tick` -> `process::on_tick`) wakes sleepers, charges the running
  process one tick of CPU and, when it interrupted ring 3, switches away from
  inside the interrupt frame once the 20 ms slice is spent or a strictly better
  process became ready (a just-woken sleeper preempts at once).  The trap frame
  stays on the process's own kernel stack, so a preempted process resumes like
  after a `yield`.  The kernel itself is not preemptible: syscalls and
  exception handlers run with IF=0 and kernel threads never enable interrupts,
  which is what lets this single-CPU design go without kernel locks (SMP or
  kernel preemption will need them).  Syscalls: `yield` (to the same level or
  better), `sleep(ms)` (parks the process, max 10 s), `setprio(level)` (a
  process can only lower its own priority).  The watchdog now counts CPU ticks
  (3 s default), so sleeping is free; a process ends with `exit`, the watchdog,
  `kill` or a ring 3 exception.  `run_until_idle` drives the queue from the
  shell, idling with `sti; hlt` while only sleepers remain; dead processes are
  reaped once their outcome has been read.
  *Limits:* strict priority means a high priority process that never yields
  starves lower ones until its budget is gone (no aging yet).  x87/SSE state
  is saved per process since phase 2.4 (FXSAVE, eager switch; no AVX).

Shell: `ps` lists the process table, `kill <pid>` terminates a process,
`user ping pong` shows two ring 3 programs interleaving through the switch,
`user spin:3 spin:1` runs two CPU hogs at different priorities (`name:prio`),
`ps` shows priority, CPU ticks and preemption count.

## ELF programs (phase 2.4)

`sys/kern/src/user/elf.rs` (loader), `user/fpu.rs`, `execve` in
`user/syscall.rs`, shell command `exec`; user side in `usr/`.

- **Format**: ELF64 little endian x86-64, `ET_EXEC` linked inside user space or,
  the usual case, a **static PIE** (`ET_DYN`, `-pie --no-dynamic-linker`): user
  space starts at 512 GiB, out of reach of the small code model, so the image is
  loaded at `USER_CODE_BASE` and its `R_X86_64_RELATIVE` relocations are applied
  by the loader.  `PT_INTERP` / `DT_NEEDED` are refused (no dynamic linker yet).
- **Segments**: every `PT_LOAD` is mapped page by page with its own protection
  (a writable *and* executable segment is refused, segments sharing a page too,
  so link with `-z separate-code -z max-page-size=4096`); the file part is
  copied, `.bss` stays zero; the entry point must be in an executable segment.
- **Initial stack** (System V x86-64 process entry): 64 KiB, `rsp` 16-byte
  aligned on `argc`, then `argv[]`, NULL, `envp[]`, NULL and the auxiliary vector
  (`AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_ENTRY`, `AT_RANDOM`,
  `AT_EXECFN`, `AT_UID`..., `AT_NULL`); at most 64 + 64 strings, 24 KiB.
- **`execve(path, argv, envp)`** (syscall 7): builds the new address space first,
  so a failure (`-ENOENT`, `-EACCES` when not executable, `-ENOEXEC`, `-E2BIG`,
  `-EFAULT`, `-ENAMETOOLONG`, `-EISDIR`, `-ENOMEM`) leaves the caller untouched;
  on success the process keeps its pid, priority and CPU budget but gets a new
  image, name, stack and a pristine FPU state.
- **FPU / SSE**: the kernel is soft-float, user programs are not.  Each process
  has a 512-byte FXSAVE area, saved when it leaves the CPU and restored when it
  returns (eager; CR0.EM/TS clear, CR4.OSFXSR/OSXMMEXCPT set at boot).

Shell: `exec [-p prio] <program> [args...] [-- <program> [args...]]...` loads the
programs (a bare name is looked up in `/bin`), runs them as processes and reports
how they ended; several programs separated by `--` run concurrently.

`usr/` holds the user side: `usr/lib/crt0.S` (`_start`), `usr/lib/prism.c`
(freestanding mini libc: memcpy & co, `printf`, `getauxval`, ...),
`usr/include/prism.h` (syscall wrappers, signal numbers, wait macros) and one
directory per program.  `make` builds every program listed in `USR_PROGS`
(static PIE, `ld.lld`) and installs it as `/bin/<name>` in the ext4 root.
`/bin/hello` is the phase 2.4 demo and test:
`hello [info]`, `hello fpu <tag>` (x87 + SSE workload, run two at once to check the
per-process FPU state), `hello chain` (`execve` of itself), `hello errors` (all
the `execve` error paths), `hello exit <n>`, `hello fault` (NULL dereference,
contained by the kernel).  `/bin/fork` is the phase 2.5 one (see above).
`/bin/notelf` is an executable text file used to check
`-ENOEXEC`.

## Boot chain

One disk image boots both ways (GPT with a protective MBR, three partitions):

| Partition      | Type (GPT) | Content                                                          |
|----------------|------------|-------------------------------------------------------------------|
| `PrismOS ESP`  | EFI system | FAT32: `EFI/BOOT/BOOTX64.EFI` only - the UEFI stub                |
| `PrismOS boot` | BIOS boot | raw sectors: boot1 + the 64-bit BIOS loader (no filesystem)       |
| `PrismOS root` | Linux ext4 | `/boot/kernel`, `/boot/lua`, `loader.conf`, `/bin`, `/etc`, `/usr` |

The boot volume of both loaders is the ext4 root: the BIOS loader mounts it
with its own ext2/3/4 driver over `int 13h`, the UEFI stub (whose only job is
to be launched by the firmware) mounts it over the firmware block I/O
protocol, with a fallback to the volume the stub was started from (the ISO
keeps that legacy single-volume layout). The kernel mounts the same ext4
root partition on `/`, so the Lua scripts, the configuration and the kernel
are one set of files for everyone. A Linux-filesystem partition only counts
as the boot volume when it carries the boot tree (`/boot/loader.conf` or
`/boot/kernel/kernel`), so plain ext4 data disks are skipped.

## Build & run

Requirements: `clang` + `lld` (loader, `lld-link`), `gcc`/`binutils` (boot
sectors), `make`, `python3`, `mkfs.fat` + `mtools` (ESP), `mke2fs` (ext4 root),
Rust with the bare-metal
target (`rustup target add x86_64-unknown-none`). To run: `qemu-system-x86_64`
and, for UEFI, OVMF.

```sh
make              # build/prismos.img
make run          # boot it in QEMU (BIOS), serial on stdio
make run-uefi     # boot it in QEMU with OVMF (UEFI)
make test         # automated BIOS + UEFI boot tests through the serial console
make fstest       # VFS + ext4 tests on the host, checked with e2fsck
make data         # (re)create build/data.img, the extra ext4 disk `make run` attaches
make iso          # legacy single-volume ISO (UEFI)
make clean
```

Write it to a USB stick with `dd if=build/prismos.img of=/dev/sdX bs=1M`
(it boots on both legacy BIOS and UEFI machines).

## File systems (phase 1.4)

`make run` attaches the boot disk on AHCI port 0 and a second disk,
`build/data.img`, on port 1: the kernel mounts the ext4 root partition of the
boot disk (`disk 0 partition 3`) on `/` at boot, the extra volume stays
available for shell experiments. Everything is also reachable from the shell:

```
prism# mount -l              volumes found      prism# mount [-r] 0:2 /mnt
prism# ls -l /               prism# cat /hello.txt
prism# write /note hi\n      prism# mkdir -p /a/b   prism# cp, mv, rm -r, ln -s, ...
prism# df                    prism# sync            prism# umount /mnt
```

Layers (all in `sys/kern/src/`):

| File          | Role                                                              |
|---------------|-------------------------------------------------------------------|
| `vfs.rs`      | `FileSystem` trait, mount table, path walk (symlinks, `..`, mount points), open-file table (`open/read/write/seek/close`) |
| `ext4/`       | the driver: `mod.rs` (mount, block cache, inodes), `balloc.rs` (group descriptors, bitmaps, allocators), `extent.rs` (extent trees, file data), `dir.rs`, `ops.rs` (create, mkdir, rename, link, ...), `crc.rs` |
| `devfs.rs`    | the /dev device directory (phase 2.6): console, null, zero nodes |
| `fsys.rs`     | kernel glue: global VFS, cwd, GPT partition scan, boot-time mounts (ext4 root, devfs on /dev), RTC clock |
| `shfs.rs`     | shell commands                                                    |

ext4 support: extents (or classic block maps for reading), 32/64-bit
descriptors, flex_bg, uninit_bg and `metadata_csum` (checksums are written, not
yet verified on read), htree directories (scanned linearly; modifying one drops
the index). There is no journal: writes go straight to the disk at the end of
each operation, so a crash mid-operation can need an `fsck`. A volume that was
not cleanly unmounted or that needs journal recovery is mounted read-only, as
are volumes with unsupported ro_compat features. Refused: inline_data,
bigalloc, meta_bg, encryption. Run `sync` (or `reboot` / `halt`, which sync)
before powering off.

`make fstest` compiles `vfs.rs` and `ext4/` unchanged against image files made
by `mke2fs` (4k/1k blocks, with/without metadata_csum, 64bit, no journal,
128-byte inodes), runs read/write scenarios (fragmented files, full disk,
rename, links, many-entry directories, persistence across mounts) and checks
every result with `e2fsck -fn` and `debugfs`.

## The loader

`boot/loader/` is the common part: ELF64 kernel loader, memory map, page
tables, ANSI terminal, Lua bindings, plus the two read-only boot filesystems
of the BIOS loader (`fat.c` FAT32 and `ext4.c` ext2/3/4 -- extents or classic
block maps, symlinks, sparse files; checksums are not verified, and the same
features are refused as in the kernel driver). `boot/bios/` and `boot/efi/`
are the two platform backends (disk/console/memory/firmware tables).
`boot/libsa/` is a tiny freestanding libc (string, printf with floats,
malloc, setjmp, math...) so that stock Lua 5.4 (`contrib/lua`) builds
unmodified.

### Lua scripts (`boot/lua/`)

| File         | Role                                                          |
|--------------|---------------------------------------------------------------|
| `loader.lua` | entry point: read config, autoboot or menu                    |
| `config.lua` | `loader.conf` parser, module list, kernel loading, boot       |
| `menu.lua`   | boot menu with autoboot countdown                             |
| `cli.lua`    | `OK` prompt: `boot`, `set`, `show`, `ls`, `more`, `lsmem`, `lua`... |

The scripts only talk to the firmware through the `loader` table: `write`,
`getkey`, `time`, `delay`, `reboot`, `getenv`/`setenv`/`unsetenv`/`environ`,
`readfile`, `readdir`, `stat`, `exists`, `load_kernel`, `load_module`,
`kernel`, `memmap`, `boot`, plus `loader.platform` (`"bios"`/`"uefi"`) and
`loader.version`. Only the `base`, `table`, `string`, `math`, `utf8` and
`coroutine` libraries are available; `require` loads `/boot/lua/<name>.lua`.

### Configuration

`/boot/defaults/loader.conf` then `/boot/loader.conf` (FreeBSD syntax
`name="value"`). Useful variables: `kernel`, `kernel_options`,
`autoboot_delay` (seconds, `NO` = wait), `beastie_disable="YES"` (no menu),
`loader_menu_title`, and modules through `<name>_load="YES"`,
`<name>_name`, `<name>_type`.

The loader also works on a serial console (COM1, 115200 8N1) on BIOS; on UEFI
it uses whatever console the firmware provides.

## Boot protocol

The kernel is an ELF64 executable linked in the top 2 GiB (`KERNBASE =
0xFFFFFFFF80000000`, see `sys/kern/linker.ld`). It is entered in long mode with
paging on (identity map of 4 GiB + the kernel at its link address), interrupts
off, and `rdi` pointing to a `struct prism_bootinfo`: firmware type, memory map
(typed, with loader memory marked reclaimable), modules, command line,
environment, ACPI RSDP, SMBIOS, framebuffer. Definitions:
`include/prism/boot.h` (C) and `sys/kern/src/boot.rs` (Rust).

## Layout

```
boot/boot0, boot/boot1   BIOS sector code (assembly)
boot/bios, boot/efi      platform backends
boot/loader, boot/libsa  loader core, mini libc
boot/lua, boot/conf      Lua scripts and loader.conf
contrib/lua              Lua 5.4.8 (MIT)
include/                 prism.inc (shared constants), prism/boot.h
sys/kern                 Rust kernel (no_std, x86_64-unknown-none)
tools/mkimage.py         GPT disk image builder (ESP + BIOS boot + ext4 root)
usr/                     user space: crt0 + mini libc (usr/lib), headers, programs (usr/hello, usr/fork, usr/fd, usr/posix); the mlibc ports of the four (usr/mlibc)
tests/boot_test.py       QEMU boot tests (incl. ext4 root, layout assertion)
tests/fs_test.py         host VFS/ext4 tests (tests/fstest is the harness)
```

## License

GPL-3.0-or-later — see [LICENSE](LICENSE). Lua is MIT licensed, see
`contrib/lua/COPYRIGHT`. The framebuffer font is public domain.