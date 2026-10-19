// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// User space (phase 2): ring 3 entry/exit (usermode.rs), the process layer
// -- PCB, kernel stacks, context switch, preemptive scheduler
// (process.rs) -- and the system call dispatch (syscall.rs).
//
// Phase 2.4: elf.rs loads executables from the VFS (ELF64, static / static
// PIE, System V initial stack with argv / envp / auxv), `execve` lives in
// syscall.rs, fpu.rs keeps the x87 / SSE state per process.
//
// Phase 2.5: signal.rs (handlers, delivery at the syscall boundary, the
// sigreturn trampoline) and, in process.rs, `fork` with a copy-on-write
// address space clone and the blocking `wait`.
//
// Phase 2.6: fd.rs -- the descriptor layer.  A fd table per process
// (0/1/2 on the console), open file descriptions shared by fork and dup,
// pipes with bounded blocking, the /dev devices, and the six syscalls
// read / open / close / lseek / dup / pipe (plus `write`, rerouted).
//
// Phase 3.1: the newlib syscall set.  sysc.rs carries the memory / process
// half (brk, mmap, munmap, chdir, getcwd, uname); fd.rs grows the
// descriptor half (stat, fstat, dup2, fcntl, getdents, ioctl, unlink,
// mkdir, rmdir, rename, link) plus O_NONBLOCK / O_CLOEXEC, a per-process
// cwd (process.rs) and the SIGPIPE rule.
//
// usermode.rs also carries the built-in flat test programs; shell/shuser.rs
// exposes them as `user` / `usertest` / `ps` / `kill`.

pub mod elf;
pub mod fd;
pub mod fpu;
pub mod process;
pub mod signal;
pub mod sysc;
pub mod syscall;
pub mod usermode;
