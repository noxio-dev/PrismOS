// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// User space (phase 2): ring 3 entry/exit (usermode.rs), the process layer
// -- PCB, kernel stacks, context switch, cooperative round-robin
// (process.rs) -- and the system call dispatch (syscall.rs).
//
// Phase 2.4: elf.rs loads executables from the VFS (ELF64, static / static
// PIE, System V initial stack with argv / envp / auxv), `execve` lives in
// syscall.rs, fpu.rs keeps the x87 / SSE state per process.
//
// usermode.rs also carries the built-in flat test programs; shell/shuser.rs
// exposes them as `user` / `usertest` / `ps` / `kill`.

pub mod elf;
pub mod fpu;
pub mod process;
pub mod syscall;
pub mod usermode;
