// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// SIGIO rules, pure (host tested by tests/sigio_host.rs): how F_SETOWN's
// argument reads, which signal an async description raises, and when.
//
// The kernel half (user/fd/sigio.rs) keeps, per open file description, an
// owner (F_SETOWN), a signal (F_SETSIG) and the last readiness it saw, and
// asks `should_fire` after every probe: a signal goes out on the not-ready ->
// ready edge, never while the descriptor merely stays ready.

pub const SIGIO: u32 = 29;

/// `si_code` of a SIGIO raised by I/O readiness (Linux: SI_SIGIO).
pub const SI_SIGIO: i32 = -5;

/// Who F_SETOWN names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// Nobody: no signal is sent.
    None,
    /// One process (positive argument).
    Pid(u32),
    /// A process group (negative argument).
    Pgrp(u32),
}

/// Decode the 32-bit F_SETOWN argument (a negative value names a group).
/// `i32::MIN` has no positive twin and is refused, like Linux (-EINVAL).
pub fn decode_owner(arg: i32) -> Option<Owner> {
    match arg {
        0 => Some(Owner::None),
        i32::MIN => None,
        n if n > 0 => Some(Owner::Pid(n as u32)),
        n => Some(Owner::Pgrp((-n) as u32)),
    }
}

/// F_GETOWN's answer: the argument F_SETOWN would take for this owner.
pub fn encode_owner(o: Owner) -> i32 {
    match o {
        Owner::None => 0,
        Owner::Pid(p) => p as i32,
        Owner::Pgrp(g) => -(g as i32),
    }
}

/// F_SETSIG's argument: 0 means plain SIGIO, anything else must be a signal
/// the kernel tracks (1..=31; real-time numbers are not supported).
pub fn valid_setsig(n: u32) -> bool {
    n <= 31
}

/// The signal an async description raises: its F_SETSIG, or SIGIO.
pub fn signal_for(setsig: u32) -> u32 {
    if setsig == 0 {
        SIGIO
    } else {
        setsig
    }
}

/// The edge rule: fire only when readiness turned on, the description is
/// O_ASYNC and somebody owns it.
pub fn should_fire(async_on: bool, owner: Owner, was_ready: bool, now_ready: bool) -> bool {
    async_on && owner != Owner::None && !was_ready && now_ready
}
