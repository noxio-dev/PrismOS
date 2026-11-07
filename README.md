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
build PrismOS itself.

## Status

| Phase | Topic | State |
|-------|-------|-------|
| 1 | Kernel foundations (heap, ACPI/APIC, PCI/AHCI, VFS + ext4, SMP bring-up) | done |
| 2 | Ring 3 and processes (scheduler, ELF loader, fork, signals, pipes) | done |
| 3.1 – 3.4 | POSIX userspace (syscalls, mlibc, init/TTY/procfs, BusyBox + ash) | done |
| 3.5 | Precise time, randomness, users and permissions | todo |
| 4.1 | Cross binutils in the image (`ar`, `ranlib`, `nm`, `objdump`, `strip`) | done |
| 4.2 – 4.5 | Threads, C++, building userspace with the PrismOS toolchain, dev image | todo |
| 5 | Self-hosting (native binutils, gcc, build tools, full bootstrap) | todo |

The detailed checklist, with the notes behind every step, lives in
[docs/ROADMAP.md](docs/ROADMAP.md).

## Build and run

```sh
make              # build build/prismos.img
make run          # boot it in QEMU (BIOS)
make run-kvm      # boot it in QEMU/KVM (BIOS)
make run-uefi     # boot it in QEMU with OVMF
make run-kvm-uefi # boot it in QEMU/KVM with OVMF
make test         # automated boot tests
```

Host-side tests that need no QEMU: `make fstest` (VFS + ext4 checked with
`e2fsck`), `make bintest` (binutils port) and `make busyboxtest` (BusyBox port).

## License

GPL-3.0-or-later -- see [LICENSE](LICENSE). Lua is MIT licensed, see
`contrib/lua/COPYRIGHT`. The framebuffer font is public domain.