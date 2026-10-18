// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// User space (phase 2): ring 3 entry/exit (usermode.rs), the process layer
// -- PCB, kernel stacks, context switch, cooperative round-robin
// (process.rs) -- and the system call dispatch (syscall.rs).
//
// usermode.rs also carries the built-in flat test programs; shell/shuser.rs
// exposes them as `user` / `usertest` / `ps` / `kill`.

pub mod process;
pub mod syscall;
pub mod usermode;
