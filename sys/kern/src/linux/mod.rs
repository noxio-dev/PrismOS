// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The Linux layer: the parts of Linux that PrismOS re-implements so that
// programs built for Linux can one day run here.
//
// Ground rules
//
//   * Same behaviour, our own code.  Everything below is written from the
//     System V / x86-64 ABI documents and from what Linux does as seen from
//     user space; no Linux source is copied (Linux is GPL-2.0-only, PrismOS
//     is GPL-3.0-or-later: the two cannot be mixed).
//   * The tree mirrors Linux's, so that a reader who knows one finds the
//     other:
//
//         linux/uapi/                include/uapi/linux/   what user space sees
//         linux/fs/binfmt_elf/       fs/binfmt_elf.c       the ELF loader
//
//     New subsystems get the directory their Linux counterpart has.
//   * Numbers are Linux's: the errno values in uapi/errno.rs, the auxv keys
//     in uapi/elf.rs.  PrismOS's own system calls keep their own numbering.
//   * Pure logic (parsing, layout, byte images) lives in files that need
//     nothing from the rest of the kernel, so the host tests include them
//     as they are (tests/linux_elf_host.rs).  Only the glue that touches
//     address spaces and the VFS is kernel-only.
//
// Status
//
//   The ELF loader is complete for what it covers (see fs/binfmt_elf/mod.rs)
//   but nothing calls it yet: a Linux program also needs Linux's system
//   call numbers and semantics, which are the next layer.  Until then
//   user/abi.rs keeps sending Linux programs to /usr/sbin/linux-compat.

#![allow(dead_code)]

pub mod fs;
pub mod uapi;
