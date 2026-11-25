// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// signalfd, the pure part: the mask rules, which pending signal a read takes
// and the 128-byte `struct signalfd_siginfo` it hands out.  No kernel types
// in here, so tests/signalfd_host.rs can include this file as it is.  The
// descriptor, the pending set of the process and the user memory live in
// user/fd/signalfd.rs.
//
// The rules (Linux's):
//   mask      the signals the descriptor accepts.  SIGKILL and SIGSTOP never
//             stick (as in sigprocmask), nor does bit 0 or anything past the
//             signals this kernel tracks.  `signalfd` is only useful for
//             signals the caller blocked: an unblocked one with a handler is
//             delivered to the handler, and one with the default action is
//             acted on, before a read could see it.
//   read      takes pending signals of the mask, lowest number first, one
//             128-byte record each, as many as the buffer holds (at least
//             one); a buffer under 128 bytes is -EINVAL.  Nothing pending
//             blocks (or -EAGAIN with O_NONBLOCK).  The signal is consumed:
//             it is not delivered afterwards.
//   ready     readable when a signal of the mask is pending.  Never writable.

pub const NSIG: u32 = 32;
pub const SIGKILL: u32 = 9;
pub const SIGSTOP: u32 = 19;

/// `sizeof(struct signalfd_siginfo)`.
pub const SIGINFO_SIZE: usize = 128;

pub const EINVAL: i64 = 22;

/// The signal set of a `signalfd` call (a user `sigset_t` word) as the mask
/// the descriptor keeps: bit n = signal n, without bit 0, SIGKILL, SIGSTOP
/// and the bits past `NSIG`.
pub fn normalize_mask(raw: u64) -> u32 {
    let all = (1u64 << NSIG) - 1;
    (raw & all & !1 & !(1u64 << SIGKILL) & !(1u64 << SIGSTOP)) as u32
}

/// `signalfd4`'s `sizemask` argument: the kernel's `sigset_t` is 8 bytes.
pub fn check_sigsetsize(sz: u64) -> Result<(), i64> {
    if sz == 8 {
        Ok(())
    } else {
        Err(-EINVAL)
    }
}

/// The pending signal a read takes next: the lowest one in `pending & mask`.
pub fn next_signal(pending: u32, mask: u32) -> Option<u32> {
    let hit = pending & mask & !1;
    if hit == 0 {
        None
    } else {
        Some(hit.trailing_zeros())
    }
}

/// How many records a buffer of `len` bytes holds.
pub fn records_that_fit(len: usize) -> usize {
    len / SIGINFO_SIZE
}

/// One `struct signalfd_siginfo`: `ssi_signo` @0, `ssi_errno` @4,
/// `ssi_code` @8, `ssi_pid` @12, `ssi_uid` @16, the rest (fd, tid, band,
/// overrun, trapno, status, int, ptr, times, addr) zero.
pub fn record(sig: u32, code: i32, pid: u32, uid: u32) -> [u8; SIGINFO_SIZE] {
    let mut r = [0u8; SIGINFO_SIZE];
    r[0..4].copy_from_slice(&sig.to_le_bytes());
    // ssi_errno (4..8) stays 0.
    r[8..12].copy_from_slice(&code.to_le_bytes());
    r[12..16].copy_from_slice(&pid.to_le_bytes());
    r[16..20].copy_from_slice(&uid.to_le_bytes());
    r
}
