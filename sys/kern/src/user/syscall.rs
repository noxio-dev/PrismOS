// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// System call dispatch (phase 2): the Rust side of the `syscall` entry stub
// in user/usermode.rs.  The frame (registers of the caller, including the
// syscall number in rax and the arguments in rdi/rsi/rdx) is already built
// on the kernel stack of the running process; the result goes back in
// frame.rax and the stub returns to ring 3 with `sysretq`.
//
// System call ABI (Linux-like):
//   rax = number, rdi/rsi/rdx/r10/r8/r9 = arguments, result in rax
//   (negative errno on failure); rcx and r11 are clobbered by the CPU.
//     0 exit(code)            1 write(fd, buf, len)       2 getpid()
//     3 uptime_ms()           4 yield()
//     5 sleep(ms)             6 setprio(level)   (phase 2.3)
//     7 execve(path, argv, envp)                    (phase 2.4)
//     8 fork()                9 wait(pid, status_ptr, flags)  (phase 2.5)
//    10 kill(pid, sig)       11 signal(sig, handler)
//    12 sigreturn()
//    13 read(fd, buf, len)   14 open(path, flags)      15 close(fd)
//    16 lseek(fd, off, whence)  17 dup(fd)            18 pipe(int *fds)
//                                                          (phase 2.6)
//    19 stat(path, st)       20 fstat(fd, st)          21 brk(addr)
//    22 mmap(a,len,prot,fl,fd,off)  23 munmap(addr, len)
//    24 chdir(path)          25 getcwd(buf, size)      (phase 3.1:
//    26 dup2(old, new)       27 fcntl(fd, cmd, arg)     the newlib
//    28 getdents(fd, buf, len)  29 ioctl(fd, req, arg)   syscall set)
//    30 unlink(path)         31 mkdir(path, mode)      32 rmdir(path)
//    33 rename(old, new)     34 link(old, new)         35 uname(buf)
//
// Phase 3.2 (the mlibc prerequisites, first step):
//    36 arch_prctl(code, addr)          TLS: the FS/GS base pair
//    37 futex(uaddr, op, val, timeout, uaddr2, val3) -- six arguments
//                                      (r10 / r8 / r9 carry 4..6, like
//                                      mmap)
//    38 sigaction(sig, act, oldact)    39 sigprocmask(how, set, oldset)
//    40 mprotect(addr, len, prot)     41 poll(fds, nfds, timeout_ms)
//    42 select(nfds, r, w, e, tv)     43 clock_gettime(clk, ts)
//                                      (select's 4th and 5th ride in
//                                      r10 / r8)
//
// Phase 3.3 (the user-space framebuffer, for the mlibc cube port):
//    44 fb_info(struct prism_fb_info *)  the screen's geometry + format
//    45 fb_blit(src, w, h, stride, x, y) copy caller-packed pixels onto
//                                      the console surface -- six arguments
//                                      (r10 / r8 / r9 carry 4..6, like
//                                      mmap)
//
// `exit` never returns to the caller.  `yield` and `sleep` switch to another
// process and come back here only when the process is scheduled again (the
// trap frame stayed on its kernel stack).  `yield` only hands the CPU to a
// process of the same priority level or a better one.  `sleep` parks the
// process for at least `ms` milliseconds (capped at 10 s) and returns 0.
// `setprio` lowers the caller's priority (levels 0 = best .. 3); it returns
// the previous level, -EINVAL for a level out of range and -EPERM when the
// request would raise the priority.
//
// `execve` replaces the calling process's image by an ELF executable of the
// VFS (user/elf.rs): new address space, fresh System V stack (argc, argv,
// envp, auxv), fresh FPU state and clean signal handlers; the pid, priority,
// CPU budget and descriptor table stay (POSIX keeps the fds).  It returns
// only on failure (-ENOENT, -ENOEXEC, -EACCES, -E2BIG, -ENOMEM, -EFAULT ...)
// and then the caller is untouched.
//
// Phase 2.5: `fork` clones the caller copy-on-write and returns the child's
// pid (the child resumes in ring 3 with rax = 0).  `wait` reaps a dead
// child (-ECHILD when there is none, WNOHANG returns 0 instead of
// blocking) and writes its Linux-style status word.  `kill` and `signal`
// are the basic signal API (user/signal.rs); `sigreturn` is what a handler
// returns into (through the trampoline page) -- it restores the frame
// pushed at delivery, so the interrupted code keeps its registers and its
// syscall return value.  A pending signal is delivered between the return
// value and `sysretq`.
//
// Phase 2.6: the descriptor layer (user/fd.rs) sits in front of the VFS:
// every process owns a fd table (0/1/2 on the console), `fork` shares the
// open descriptions with the child, `write`/`read`/`open`/`close`/
// `lseek`/`dup`/`pipe` are the six syscalls above, and the device nodes of
// /dev (devfs) resolve through it.
//
// Phase 3.1 (the newlib set): stat / fstat / brk / mmap / munmap /
// chdir / getcwd / uname (user/sysc.rs) and dup2 / fcntl / getdents /
// ioctl / unlink / mkdir / rmdir / rename / link (user/fd.rs) -- 17 new
// numbers, plus O_NONBLOCK / O_CLOEXEC, a per-process cwd and the SIGPIPE
// rule inside the old ones.  Arguments beyond the third arrive in
// r10 / r8 / r9 (mmap needs six).

use crate::arch::intr::IsrFrame;
use crate::mem::vmm::AddressSpace;
use crate::user::elf;
use crate::user::fd;
use crate::user::futex;
use crate::user::process;
use crate::user::signal;
use crate::user::sysc;
use crate::user::usermode::{EFAULT, ENOSYS};
use alloc::string::String;
use alloc::vec::Vec;

const ENOENT: i64 = 2;
const E2BIG: i64 = 7;
const ENAMETOOLONG: i64 = 36;

/// NUL-terminated string at the user address `ptr` (without the NUL).
/// Shared by execve and the descriptor layer's `open` (phase 2.6).
pub(crate) fn read_user_cstr(aspace: &AddressSpace, ptr: u64, max: usize) -> Result<Vec<u8>, i64> {
    let mut out = Vec::new();
    let mut p = ptr;
    loop {
        // Never read past the end of the page in one go: the next one may
        // be unmapped even though the string ends before it.
        let n = ((4096 - (p & 4095)) as usize).min(64);
        let mut buf = [0u8; 64];
        if !aspace.read(p, &mut buf[..n]) {
            return Err(-EFAULT);
        }
        if let Some(z) = buf[..n].iter().position(|&b| b == 0) {
            out.extend_from_slice(&buf[..z]);
            return if out.len() > max { Err(-E2BIG) } else { Ok(out) };
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > max {
            return Err(-E2BIG);
        }
        p += n as u64;
    }
}

/// A NULL-terminated array of string pointers (argv / envp); 0 = empty.
fn read_user_strv(aspace: &AddressSpace, ptr: u64) -> Result<Vec<Vec<u8>>, i64> {
    let mut v = Vec::new();
    if ptr == 0 {
        return Ok(v);
    }
    let mut total = 0usize;
    for i in 0..=elf::MAX_ARGS as u64 {
        let mut w = [0u8; 8];
        if !aspace.read(ptr + i * 8, &mut w) {
            return Err(-EFAULT);
        }
        let sp = u64::from_le_bytes(w);
        if sp == 0 {
            return Ok(v);
        }
        if v.len() == elf::MAX_ARGS {
            return Err(-E2BIG);
        }
        let s = read_user_cstr(aspace, sp, elf::MAX_STRING)?;
        total += s.len() + 1;
        if total > elf::MAX_ARG_BYTES {
            return Err(-E2BIG);
        }
        v.push(s);
    }
    Err(-E2BIG)
}

/// execve(path, argv, envp): see the header comment.
fn sys_execve(frame: &mut IsrFrame, path_p: u64, argv_p: u64, envp_p: u64) -> i64 {
    let Some(cur) = process::current() else { return -EFAULT };
    let loaded = {
        let Some(aspace) = cur.aspace.as_ref() else { return -EFAULT };
        let path = match read_user_cstr(aspace, path_p, 255) {
            Ok(p) => p,
            Err(e) if e == -E2BIG => return -ENAMETOOLONG,
            Err(e) => return e,
        };
        let Ok(path) = String::from_utf8(path) else { return -ENOENT };
        if path.is_empty() {
            return -ENOENT;
        }
        let argv = match read_user_strv(aspace, argv_p) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let envp = match read_user_strv(aspace, envp_p) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let abs = process::abs_current(&path);
        // A program started without argv still gets argv[0].
        let argv = if argv.is_empty() { alloc::vec![path.clone().into_bytes()] } else { argv };
        match elf::load_path(&abs, &argv, &envp) {
            Ok(l) => (l, abs),
            Err(e) => return -e.errno(),
        }
    };
    let (new, abs) = loaded;
    // Point of no return: swap the address space (the new tables go live
    // before the old ones are freed), rename the process, restart in ring 3.
    let old = cur.aspace.replace(new.aspace);
    // SAFETY: the new tables share the kernel half, so the running kernel
    // code stays mapped; the old ones are dropped only after the switch.
    unsafe { cur.aspace.as_ref().unwrap().activate() };
    drop(old);
    cur.name = elf::base_name(&abs);
    cur.fpu.reset();
    cur.signals.exec_reset(); // POSIX: caught -> default, SIG_IGN kept, mask kept
    cur.brk_base = new.image_end; // the new image's heap bounds (3.1)
    cur.brk = new.image_end;
    cur.fds.close_exec(); // POSIX: the O_CLOEXEC numbers go (phase 3.1)
    // The fresh image has no TLS of its own yet (phase 3.2): zero the
    // thread registers -- arch_prctl installs the new ones.
    cur.fs_base = 0;
    cur.gs_base = 0;
    crate::user::usermode::set_user_thread(0, 0);
    // The cwd, priority, CPU budget and the surviving descriptors stay.
    // SAFETY: CR0/CR4 were set up by usermode::init; the image is pristine.
    unsafe { cur.fpu.restore() };
    *frame = crate::user::usermode::user_frame(new.entry, new.rsp);
    0
}

/// The dispatcher: called from `syscall_entry` (usermode.rs) with the trap
/// frame on the current process's kernel stack.  `exit` never returns.
pub extern "C" fn dispatch(frame: &mut IsrFrame) {
    let (a0, a1, a2) = (frame.rdi, frame.rsi, frame.rdx);
    // r10 / r8 / r9 carry arguments 4..6 (the Linux order; mmap is the
    // only six-argument call so far).
    let ret: i64 = match frame.rax {
        0 => process::exit_current(process::Outcome::Exited(a0)),
        2 => process::getpid() as i64,
        3 => crate::hw::pit::uptime_ms() as i64,
        4 => {
            process::yield_current();
            0
        }
        5 => {
            process::sleep_current(a0);
            0
        }
        6 => process::setprio_current(a0),
        7 => sys_execve(frame, a0, a1, a2),
        // ---- phase 2.5 --------------------------------------------------
        8 => process::fork_current(frame),
        9 => process::wait_current(a0, a1, a2),
        10 => signal::sys_kill(a0, a1),
        11 => signal::sys_signal(a0, a1),
        12 => {
            // The frame is the one pushed at delivery: restore it verbatim
            // and skip the rax overwrite below (the saved rax is the
            // interrupted syscall's return value).
            signal::sys_sigreturn(frame);
            return;
        }
        // ---- phase 2.6: the descriptor layer ----------------------------
        1 => fd::sys_write(a0, a1, a2),
        13 => fd::sys_read(a0, a1, a2),
        14 => fd::sys_open(a0, a1),
        15 => fd::sys_close(a0),
        16 => fd::sys_lseek(a0, a1, a2),
        17 => fd::sys_dup(a0),
        18 => fd::sys_pipe(a0),
        // ---- phase 3.1: the newlib set ---------------------------------
        19 => fd::sys_stat(a0, a1),
        20 => fd::sys_fstat(a0, a1),
        21 => sysc::sys_brk(a0),
        22 => sysc::sys_mmap([a0, a1, a2, frame.r10, frame.r8, frame.r9]),
        23 => sysc::sys_munmap(a0, a1),
        24 => sysc::sys_chdir(a0),
        25 => sysc::sys_getcwd(a0, a1),
        26 => fd::sys_dup2(a0, a1),
        27 => fd::sys_fcntl(a0, a1, a2),
        28 => fd::sys_getdents(a0, a1, a2),
        29 => fd::sys_ioctl(a0, a1, a2),
        30 => fd::sys_unlink(a0),
        31 => fd::sys_mkdir(a0, a1),
        32 => fd::sys_rmdir(a0),
        33 => fd::sys_rename(a0, a1),
        34 => fd::sys_link(a0, a1),
        35 => sysc::sys_uname(a0),
        // ---- phase 3.2: the mlibc prerequisites ------------------------
        36 => sysc::sys_arch_prctl(a0, a1),
        37 => futex::sys_futex([a0, a1, a2, frame.r10, frame.r8, frame.r9]),
        38 => signal::sys_sigaction(a0, a1, a2),
        39 => signal::sys_sigprocmask(a0, a1, a2),
        40 => sysc::sys_mprotect(a0, a1, a2),
        41 => fd::sys_poll(a0, a1, a2),
        42 => fd::sys_select(a0, a1, a2, frame.r10, frame.r8),
        43 => sysc::sys_clock_gettime(a0, a1),
        // ---- phase 3.3: the user-space framebuffer ---------------------
        44 => sysc::sys_fb_info(a0),
        45 => sysc::sys_fb_blit([a0, a1, a2, frame.r10, frame.r8, frame.r9]),
        // ---- phase 3.3: /tmp tmpfs + the usr-merge userland ------------
        46 => fd::sys_symlink(a0, a1),
        47 => fd::sys_readlink(a0, a1, a2),
        _ => -ENOSYS,
    };
    frame.rax = ret as u64;
    // A pending signal is delivered between the syscall's return value and
    // the drop into ring 3 (phase 2.5).  After a successful execve there is
    // never anything pending (handlers were reset); after a fork the child
    // starts with an empty mask too.
    signal::deliver(frame);
    // A user program can never reach non-canonical code addresses, so
    // `sysretq` is safe by construction; nothing else to check.
}
