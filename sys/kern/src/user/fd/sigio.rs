// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// SIGIO: fcntl F_SETOWN / F_GETOWN / F_SETSIG / F_GETSIG and the O_ASYNC
// flag (the pure rules are user/sigio_core.rs, host tested by
// tests/sigio_host.rs).
//
// Every description carries an `Aio` (owner, signal, last readiness seen).
// A description with O_ASYNC set joins a registry of weak references; `scan`
// probes each one with the same `desc_ready` poll uses and sends the signal
// on the not-ready -> ready edge.  `scan` only ever runs in process context
// -- at the end of a syscall (signal::deliver) and after a park -- never from
// an interrupt handler, because the probe may take RefCell borrows and the
// tty pump has side effects.  Consequences, stated plainly:
//   * a process that makes no syscall and never parks hears nothing until
//     it next does (an X server always sits in poll / read);
//   * a description that stays readable raises one signal per edge, not one
//     per arriving byte: the handler must drain until EAGAIN (what Xorg's
//     input thread does anyway).

use alloc::rc::{Rc, Weak};
use alloc::vec::Vec;
use core::cell::Cell;
use core::ptr::addr_of_mut;

use super::{desc_ready, FileDesc};
use crate::user::process;
use crate::user::signal::{self, SigInfoRec};
use crate::user::sigio_core::*;
use crate::user::usermode::EINVAL;

/// The kernel's own O_ASYNC bit (the value of the port's abi-bits/fcntl.h).
pub const O_ASYNC: u32 = 0x400;
/// Linux's spelling (020000), accepted by F_SETFL from a Linux binary.
const O_ASYNC_LINUX: u32 = 0x2000;

pub fn wants_async(arg: u32) -> bool {
    arg & (O_ASYNC | O_ASYNC_LINUX) != 0
}

/// Per-description SIGIO state, shared by every dup like the flags.
pub struct Aio {
    owner: Cell<i32>,
    sig: Cell<u32>,
    last: Cell<bool>,
}

impl Aio {
    pub fn new() -> Aio {
        Aio { owner: Cell::new(0), sig: Cell::new(0), last: Cell::new(false) }
    }
}

static mut REG: Vec<Weak<FileDesc>> = Vec::new();

fn with_reg<R>(f: impl FnOnce(&mut Vec<Weak<FileDesc>>) -> R) -> R {
    let flags = crate::arch::io::cli_save();
    // SAFETY: single CPU, interrupts masked while the list is touched.
    let r = unsafe { f(&mut *addr_of_mut!(REG)) };
    // SAFETY: restores what cli_save returned.
    unsafe { crate::arch::io::restore_flags(flags) };
    r
}

/// Called after F_SETFL: join (or leave) the registry, and take the current
/// readiness as the baseline so data already queued does not count as new.
pub fn flags_changed(desc: &Rc<FileDesc>) {
    let on = desc.flags() & O_ASYNC != 0;
    let me = Rc::downgrade(desc);
    if on {
        desc.aio.last.set(desc_ready(desc).read);
        with_reg(|r| {
            if !r.iter().any(|w| w.ptr_eq(&me)) {
                r.push(me);
            }
        });
    } else {
        with_reg(|r| r.retain(|w| !w.ptr_eq(&me)));
    }
}

/// F_SETOWN / F_GETOWN / F_SETSIG / F_GETSIG.
pub fn fcntl(desc: &Rc<FileDesc>, cmd: u64, arg: u64) -> i64 {
    match cmd {
        8 => match decode_owner(arg as u32 as i32) {
            Some(o) => {
                desc.aio.owner.set(encode_owner(o));
                desc.aio.last.set(desc_ready(desc).read);
                0
            }
            None => -EINVAL,
        },
        9 => desc.aio.owner.get() as i64,
        10 => {
            let n = arg as u32;
            if !valid_setsig(n) {
                return -EINVAL;
            }
            desc.aio.sig.set(n);
            0
        }
        _ => desc.aio.sig.get() as i64,
    }
}

/// Probe every O_ASYNC description and signal the owners of the ones that
/// just became readable.  Cheap when nothing is registered.
pub fn scan() {
    let live: Vec<Rc<FileDesc>> = with_reg(|r| {
        r.retain(|w| w.strong_count() > 0);
        r.iter().filter_map(|w| w.upgrade()).collect()
    });
    for d in live {
        let async_on = d.flags() & O_ASYNC != 0;
        let owner = decode_owner(d.aio.owner.get()).unwrap_or(Owner::None);
        let r = desc_ready(&d);
        let now = r.read || r.err || r.hup;
        let was = d.aio.last.replace(now);
        if !should_fire(async_on, owner, was, now) {
            continue;
        }
        let sig = signal_for(d.aio.sig.get()) as u64;
        match owner {
            Owner::Pid(p) => {
                let rec = SigInfoRec { code: SI_SIGIO, pid: 0, uid: 0 };
                process::post_signal_from(p as u64, sig, rec);
            }
            Owner::Pgrp(g) => {
                signal::signal_pgrp(g as u64, sig);
            }
            Owner::None => {}
        }
    }
}
