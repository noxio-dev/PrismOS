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
// envp, auxv), fresh FPU state and clean signal handlers; the pid, priority
// and CPU budget stay.  It returns only on failure (-ENOENT, -ENOEXEC,
// -EACCES, -E2BIG, -ENOMEM, -EFAULT ...) and then the caller is untouched.
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
// The descriptor API of fs/vfs.rs (open / read / write / seek / close) is
// meant to sit behind this table later (phase 2.6).

use crate::arch::intr::IsrFrame;
use crate::mem::vmm::AddressSpace;
use crate::user::elf;
use crate::user::process;
use crate::user::signal;
use crate::user::usermode::{EBADF, EFAULT, ENOSYS};
use alloc::string::String;
use alloc::vec::Vec;

const ENOENT: i64 = 2;
const E2BIG: i64 = 7;
const ENAMETOOLONG: i64 = 36;

/// Copy `len` user bytes starting at `buf` to the console, in chunks.
/// Returns the number of bytes written or -EFAULT when nothing was readable.
fn sys_write(fd: u64, buf: u64, len: u64) -> i64 {
    if fd != 1 && fd != 2 {
        return -EBADF;
    }
    let len = len.min(4096) as usize;
    let Some(cur) = process::current() else { return -EFAULT };
    let Some(aspace) = cur.aspace.as_ref() else { return -EFAULT };
    let mut chunk = [0u8; 256];
    let mut done = 0usize;
    while done < len {
        let n = (len - done).min(chunk.len());
        if !aspace.read(buf + done as u64, &mut chunk[..n]) {
            break;
        }
        put_user_bytes(&chunk[..n]);
        done += n;
    }
    if done == 0 && len > 0 {
        -EFAULT
    } else {
        done as i64
    }
}

/// Print user-supplied bytes: valid UTF-8 as is, control characters (other
/// than newline / tab) and invalid sequences as '?', so a program cannot
/// inject terminal escapes into the console.
fn put_user_bytes(mut b: &[u8]) {
    while !b.is_empty() {
        let (valid, skip, bad) = match core::str::from_utf8(b) {
            Ok(s) => (s, b.len(), false),
            Err(e) => {
                let v = e.valid_up_to();
                // SAFETY: from_utf8 vouches for the prefix.
                (unsafe { core::str::from_utf8_unchecked(&b[..v]) }, v + e.error_len().unwrap_or(b.len() - v), true)
            }
        };
        for ch in valid.chars() {
            if ch.is_control() && ch != '\n' && ch != '\t' {
                crate::kprint!("?");
            } else {
                crate::kprint!("{}", ch);
            }
        }
        if bad {
            crate::kprint!("?");
        }
        b = &b[skip.min(b.len())..];
    }
}

/// NUL-terminated string at the user address `ptr` (without the NUL).
fn read_user_cstr(aspace: &AddressSpace, ptr: u64, max: usize) -> Result<Vec<u8>, i64> {
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
        let abs = crate::fs::fsys::abs(&path);
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
    cur.signals.reset(); // POSIX: execve resets caught signal handlers
    // SAFETY: CR0/CR4 were set up by usermode::init; the image is pristine.
    unsafe { cur.fpu.restore() };
    *frame = crate::user::usermode::user_frame(new.entry, new.rsp);
    0
}

/// The dispatcher: called from `syscall_entry` (usermode.rs) with the trap
/// frame on the current process's kernel stack.  `exit` never returns.
pub extern "C" fn dispatch(frame: &mut IsrFrame) {
    let (a0, a1, a2) = (frame.rdi, frame.rsi, frame.rdx);
    let ret: i64 = match frame.rax {
        0 => process::exit_current(process::Outcome::Exited(a0)),
        1 => sys_write(a0, a1, a2),
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
