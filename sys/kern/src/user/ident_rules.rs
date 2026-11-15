// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The pure set*id rules (phase 3.5.3), split out so the host tests can run them.

pub const NONE: u32 = u32::MAX; // (uid_t)-1: "leave unchanged"

/// The pure rule of setuid(2) on (real, effective, saved); None = EPERM.
pub fn rule_set(privileged: bool, ids: (u32, u32, u32), v: u32) -> Option<(u32, u32, u32)> {
    if privileged {
        Some((v, v, v))
    } else if v == ids.0 || v == ids.2 {
        Some((ids.0, v, ids.2))
    } else {
        None
    }
}

/// setreuid(2) / setregid(2) on (real, effective, saved).
pub fn rule_setre(privileged: bool, ids: (u32, u32, u32), r: u32, e: u32) -> Option<(u32, u32, u32)> {
    let (mut ru, mut eu, mut su) = ids;
    if r != NONE {
        if !privileged && r != ids.0 && r != ids.1 {
            return None;
        }
        ru = r;
    }
    if e != NONE {
        if !privileged && e != ids.0 && e != ids.1 && e != ids.2 {
            return None;
        }
        eu = e;
    }
    if r != NONE || (e != NONE && e != ids.0) {
        su = eu;
    }
    Some((ru, eu, su))
}

/// setresuid(2) / setresgid(2).
pub fn rule_setres(privileged: bool, ids: (u32, u32, u32), r: u32, e: u32, s: u32) -> Option<(u32, u32, u32)> {
    let held = |v: u32| v == ids.0 || v == ids.1 || v == ids.2;
    for v in [r, e, s] {
        if v != NONE && !privileged && !held(v) {
            return None;
        }
    }
    Some((
        if r == NONE { ids.0 } else { r },
        if e == NONE { ids.1 } else { e },
        if s == NONE { ids.2 } else { s },
    ))
}

