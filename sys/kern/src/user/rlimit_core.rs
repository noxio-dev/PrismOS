// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Resource limits (getrlimit / setrlimit / prlimit64), the pure part: the
// per-process table and the rules of setrlimit(2).  No kernel types in here,
// so tests/rlimit_host.rs can include this file as it is.
//
// What is enforced is up to the kernel: this table stores and reports.  The
// kernel passes the two limits that really exist in the system when it builds
// a table (the fixed fd table size and the fixed user stack size), so the
// numbers a program reads are the true ones rather than Linux's defaults.

pub const RLIM_INFINITY: u64 = u64::MAX;

pub const RLIMIT_CPU: u64 = 0;
pub const RLIMIT_FSIZE: u64 = 1;
pub const RLIMIT_DATA: u64 = 2;
pub const RLIMIT_STACK: u64 = 3;
pub const RLIMIT_CORE: u64 = 4;
pub const RLIMIT_RSS: u64 = 5;
pub const RLIMIT_NPROC: u64 = 6;
pub const RLIMIT_NOFILE: u64 = 7;
pub const RLIMIT_MEMLOCK: u64 = 8;
pub const RLIMIT_AS: u64 = 9;
pub const RLIMIT_LOCKS: u64 = 10;
pub const RLIMIT_SIGPENDING: u64 = 11;
pub const RLIMIT_MSGQUEUE: u64 = 12;
pub const RLIMIT_NICE: u64 = 13;
pub const RLIMIT_RTPRIO: u64 = 14;
pub const RLIMIT_RTTIME: u64 = 15;
pub const RLIM_NLIMITS: usize = 16;

pub const EPERM: i64 = 1;
pub const EINVAL: i64 = 22;

/// `struct rlimit`: soft limit (`cur`) and hard ceiling (`max`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rlimit {
    pub cur: u64,
    pub max: u64,
}

impl Rlimit {
    pub const INFINITE: Rlimit = Rlimit { cur: RLIM_INFINITY, max: RLIM_INFINITY };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RlimitTable {
    lim: [Rlimit; RLIM_NLIMITS],
    /// The largest value RLIMIT_NOFILE can ever take: the size of the kernel's
    /// descriptor table (Linux's `nr_open`).  Asking for more is `-EPERM`.
    nofile_cap: u64,
}

impl RlimitTable {
    /// A fresh table.  `stack_bytes` is the real user stack size (a fixed
    /// size here, so soft and hard are both that), `nofile_cap` the size of
    /// the descriptor table.  Everything else starts unlimited, except the
    /// core dump size, which starts at 0 like on Linux (no dumps are written).
    pub const fn new(stack_bytes: u64, nofile_cap: u64) -> RlimitTable {
        let mut lim = [Rlimit::INFINITE; RLIM_NLIMITS];
        lim[RLIMIT_STACK as usize] = Rlimit { cur: stack_bytes, max: stack_bytes };
        lim[RLIMIT_CORE as usize] = Rlimit { cur: 0, max: RLIM_INFINITY };
        lim[RLIMIT_NOFILE as usize] = Rlimit { cur: nofile_cap, max: nofile_cap };
        RlimitTable { lim, nofile_cap }
    }

    /// `-EINVAL` for a resource number that does not exist.
    pub fn get(&self, res: u64) -> Result<Rlimit, i64> {
        if res >= RLIM_NLIMITS as u64 {
            return Err(-EINVAL);
        }
        Ok(self.lim[res as usize])
    }

    /// setrlimit(2) rules, in the order Linux checks them:
    /// `-EINVAL` for a bad resource or a soft limit above the hard one;
    /// `-EPERM` when an unprivileged caller raises the hard limit, or when
    /// RLIMIT_NOFILE's hard limit would pass the descriptor table's size.
    /// Lowering is always allowed; a privileged caller may raise.
    pub fn set(&mut self, res: u64, new: Rlimit, privileged: bool) -> Result<(), i64> {
        let old = self.get(res)?;
        if new.cur > new.max {
            return Err(-EINVAL);
        }
        if new.max > old.max && !privileged {
            return Err(-EPERM);
        }
        if res == RLIMIT_NOFILE && new.max > self.nofile_cap {
            return Err(-EPERM);
        }
        self.lim[res as usize] = new;
        Ok(())
    }
}
