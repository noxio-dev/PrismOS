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
sleep/wakeup.

| Area                     | State   | Notes                                                                 |
|--------------------------|---------|-----------------------------------------------------------------------|
| BIOS + UEFI loader (C/Lua) | done    | GPT/FAT32 image, Lua menu and CLI, ELF64 kernel loader                |
| Memory (PMM, VMM, heap)  | done    | bitmap PMM, own page tables with direct map and W^X, buddy + slab heap |
| CPU / interrupts         | done    | GDT/TSS, IDT, exception recovery, PIT                                  |
| ACPI / APIC              | done    | LAPIC + IOAPIC, 8259 masked, LAPIC timer calibrated                    |
| Input / output           | done    | PS/2 keyboard (us/fr), serial, VGA text and framebuffer console (Unicode, double-buffered in RAM); `snake` and `cube` demos draw pixels on the framebuffer (UEFI GOP, or a BIOS VBE linear mode chosen by the loader: `vbe_mode` in loader.conf) |
| PCI / AHCI               | done    | bus scan, SATA DMA read/write/flush; polled (no interrupts yet)       |
| VFS                      | done    | mounts, path walk, symlinks, descriptor API                            |
| ext4                     | partial | R/W, extents, checksums written; no journal, see limits below          |
| Ring 3                   | done    | user page tables per process, `syscall`/`sysret`, TSS stacks, fault containment (phase 2.1) |
| Processes / scheduler    | done    | PCB, kernel stacks, context switch, kernel threads (2.2); preemptive round-robin on the timer tick, 4 priority levels, `sleep`/`setprio` syscalls, CPU-time watchdog (2.3); no FPU state saved yet |
| Networking, USB, SMP     | todo    | not started                                                            |

**Testing.** `make fstest` (11 scenarios against `mke2fs` images, each validated
by `e2fsck -fn`) covers the VFS and ext4 on the host. `make test` boots the image
under QEMU on BIOS and UEFI and drives the shell over the serial line, including
an ext4-on-AHCI scenario and a ring 3 process / context switch scenario (phase
2.2). The phase 1.4 code was type-checked and host-tested,
but its QEMU scenario had not been run when it was written: run `make test`
before relying on it.

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
- [ ] Phase 1.5 — hardening before userspace
  - [ ] run `make test` on BIOS + UEFI with the ext4 data disk, fix what it finds
  - [ ] ext4: replay a dirty journal at mount, then a minimal jbd2 writer
  - [ ] ext4: verify metadata checksums on read, htree insert, `fallocate`-style preallocation
  - [ ] block cache for file data (write-back, `sync` / periodic flush)
  - [ ] AHCI: interrupt-driven completion and NCQ instead of polling
  - [ ] boot from the ext4 volume (loader ext4 driver) instead of the FAT32 ESP only
  - [ ] kernel logging (`dmesg` ring buffer), proper `panic` backtrace, unit tests in-kernel
  - [ ] SMP bring-up (AP startup via the LAPIC), per-CPU data, spinlocks

Phase 2 — ring 3 and processes
- [x] Phase 2.1 — ring 3: user page tables per process, `syscall`/`sysret`, TSS stacks
- [x] Phase 2.2 — processes and threads: PCB, kernel stacks, context switch
- [x] Phase 2.3 — scheduler: preemptive round-robin on the LAPIC timer, then priorities, sleep/wakeup
- [ ] Phase 2.4 — ELF loader: `execve` from the VFS, argv/envp, auxv
- [ ] Phase 2.5 — `fork`, `wait`, `exit`, signals (basic), copy-on-write
- [ ] Phase 2.6 — file descriptors per process on top of the VFS, pipes, `/dev` (`console`, `null`, `zero`)

Phase 3 — POSIX userspace
- [ ] Phase 3.1 — syscall set for newlib: `open/read/write/lseek/close/stat/dup/pipe/mmap/brk/getcwd/...`
- [ ] Phase 3.2 — newlib port and a `x86_64-prismos` toolchain target
- [ ] Phase 3.3 — `init`, TTY layer with line discipline, `/proc` and `/tmp` (tmpfs)
- [ ] Phase 3.4 — BusyBox and a userspace shell
- [ ] Phase 3.5 — time (`clock_gettime`, HPET/TSC), `/dev/random`, users and permissions

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
- [ ] other file systems: FAT32 R/W in the kernel, tmpfs, devfs
- [ ] graphics: framebuffer userspace API, a minimal window system
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
  `0 exit`, `1 write(fd 1|2)`, `2 getpid`, `3 uptime_ms`, `4 yield` (phase 2.2).

Shell: `user` lists the built-in test programs, `user <name>...` runs one or
more at once, `usertest` runs them all (outcome, fault type and page-leak check,
plus a two-process interleaving, a kernel-thread scenario and the 2.3
preemption / priority / wake-up scenarios) -- also part of
`make test`.

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
  starves lower ones until its budget is gone (no aging yet), and FPU/SSE state
  is not saved on a switch (needed before the ELF loader, phase 2.4).

Shell: `ps` lists the process table, `kill <pid>` terminates a process,
`user ping pong` shows two ring 3 programs interleaving through the switch,
`user spin:3 spin:1` runs two CPU hogs at different priorities (`name:prio`),
`ps` shows priority, CPU ticks and preemption count.

## Boot chain

One disk image boots both ways (GPT with a protective MBR):

| Partition       | Content                                                       |
|-----------------|---------------------------------------------------------------|
| `PrismOS boot`  | raw: boot1 + the 64-bit BIOS loader                           |
| `PrismOS ESP`   | FAT32: `EFI/BOOT/BOOTX64.EFI`, `boot/` (Lua, conf, kernel)    |

The BIOS loader mounts the ESP with its own FAT32 driver, the UEFI loader uses
the firmware's. The Lua scripts and the kernel are therefore the same files.

## Build & run

Requirements: `clang` + `lld` (loader, `lld-link`), `gcc`/`binutils` (boot
sectors), `make`, `python3`, `mkfs.fat` + `mtools`, Rust with the bare-metal
target (`rustup target add x86_64-unknown-none`). To run: `qemu-system-x86_64`
and, for UEFI, OVMF.

```sh
make              # build/prismos.img
make run          # boot it in QEMU (BIOS), serial on stdio
make run-uefi     # boot it in QEMU with OVMF (UEFI)
make test         # automated BIOS + UEFI boot tests through the serial console
make fstest       # VFS + ext4 tests on the host, checked with e2fsck
make data         # (re)create build/data.img, the ext4 disk `make run` attaches
make clean
```

Write it to a USB stick with `dd if=build/prismos.img of=/dev/sdX bs=1M`
(it boots on both legacy BIOS and UEFI machines).

## File systems (phase 1.4)

`make run` attaches a second disk, `build/data.img`, on an AHCI controller; the
kernel mounts the first ext2/3/4 volume it finds (a whole disk or a GPT
partition) on `/` at boot. Everything is also reachable from the shell:

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
| `fsys.rs`     | kernel glue: global VFS, cwd, GPT partition scan, boot-time mount, RTC clock |
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
tables, ANSI terminal, Lua bindings. `boot/bios/` and `boot/efi/` are the two
platform backends (disk/console/memory/firmware tables). `boot/libsa/` is a
tiny freestanding libc (string, printf with floats, malloc, setjmp, math...)
so that stock Lua 5.4 (`contrib/lua`) builds unmodified.

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
tools/mkimage.py         GPT/FAT32 disk image builder
tests/boot_test.py       QEMU boot tests (incl. ext4 on AHCI)
tests/fs_test.py         host VFS/ext4 tests (tests/fstest is the harness)
```

## License

GPL-3.0-or-later — see [LICENSE](LICENSE). Lua is MIT licensed, see
`contrib/lua/COPYRIGHT`. The framebuffer font is public domain.