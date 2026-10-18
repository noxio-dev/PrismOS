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
// The descriptor API of fs/vfs.rs (open / read / write / seek / close) is
// meant to sit behind this table later (phase 2.6).

use crate::arch::intr::IsrFrame;
use crate::user::process;
use crate::user::usermode::{EBADF, EFAULT, ENOSYS};

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
        _ => -ENOSYS,
    };
    frame.rax = ret as u64;
    // A user program can never reach non-canonical code addresses, so
    // `sysretq` is safe by construction; nothing else to check.
}
