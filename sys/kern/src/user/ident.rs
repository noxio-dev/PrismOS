// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Process identities (phase 3.5.3): real / effective / saved user and group
// ids, supplementary groups, and the syscalls that read and change them with
// the POSIX / Linux rules:
//
//   74 setuid     75 setgid     76 geteuid    77 getegid
//   78 setreuid   79 setregid   80 setresuid  81 setresgid
//   82 getresuid  83 getresgid  84 getgroups  85 setgroups
//   86 chown      87 fchown     88 lchown     89 access
//
// (50 getuid / 51 getgid keep their numbers.)  A process with euid 0 may set
// any id; anyone else may only shuffle between the ids it already holds.

use crate::fs::vfs::{Cred, MAX_GROUPS};
use crate::user::fd::{copy_in, copy_out, fs_errno, user_path};
use crate::user::ident_rules::{rule_set, rule_setre, rule_setres, NONE};
use crate::user::process;

const EPERM: i64 = 1;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;

/// What the VFS asks for: the current process's credentials.
pub fn current_cred() -> Cred {
    process::current().map_or(Cred::ROOT, |p| p.cred)
}

/// Install the VFS hook (called once at boot).
pub fn init() {
    crate::fs::vfs::set_cred_hook(current_cred);
}

fn id_arg(v: u64) -> u32 {
    v as u32 // the ABI passes uid_t in a 64-bit register; -1 arrives as 0xFFFFFFFF
}

pub fn sys_getuid() -> i64 {
    process::current().map_or(0, |p| p.cred.uid as i64)
}
pub fn sys_getgid() -> i64 {
    process::current().map_or(0, |p| p.cred.gid as i64)
}
pub fn sys_geteuid() -> i64 {
    process::current().map_or(0, |p| p.cred.euid as i64)
}
pub fn sys_getegid() -> i64 {
    process::current().map_or(0, |p| p.cred.egid as i64)
}

fn uids(p: &process::Process) -> (u32, u32, u32) {
    (p.cred.uid, p.cred.euid, p.suid)
}
fn gids(p: &process::Process) -> (u32, u32, u32) {
    (p.cred.gid, p.cred.egid, p.sgid)
}
fn put_u(p: &mut process::Process, t: (u32, u32, u32)) {
    p.cred.uid = t.0;
    p.cred.euid = t.1;
    p.suid = t.2;
}
fn put_g(p: &mut process::Process, t: (u32, u32, u32)) {
    p.cred.gid = t.0;
    p.cred.egid = t.1;
    p.sgid = t.2;
}

pub fn sys_setuid(v: u64) -> i64 {
    let Some(p) = process::current() else { return -EFAULT };
    match rule_set(p.cred.euid == 0, uids(p), id_arg(v)) {
        Some(t) if id_arg(v) != NONE => {
            put_u(p, t);
            0
        }
        _ => -EPERM,
    }
}
pub fn sys_setgid(v: u64) -> i64 {
    let Some(p) = process::current() else { return -EFAULT };
    match rule_set(p.cred.euid == 0, gids(p), id_arg(v)) {
        Some(t) if id_arg(v) != NONE => {
            put_g(p, t);
            0
        }
        _ => -EPERM,
    }
}
pub fn sys_setreuid(r: u64, e: u64) -> i64 {
    let Some(p) = process::current() else { return -EFAULT };
    match rule_setre(p.cred.euid == 0, uids(p), id_arg(r), id_arg(e)) {
        Some(t) => {
            put_u(p, t);
            0
        }
        None => -EPERM,
    }
}
pub fn sys_setregid(r: u64, e: u64) -> i64 {
    let Some(p) = process::current() else { return -EFAULT };
    match rule_setre(p.cred.euid == 0, gids(p), id_arg(r), id_arg(e)) {
        Some(t) => {
            put_g(p, t);
            0
        }
        None => -EPERM,
    }
}
pub fn sys_setresuid(r: u64, e: u64, s: u64) -> i64 {
    let Some(p) = process::current() else { return -EFAULT };
    match rule_setres(p.cred.euid == 0, uids(p), id_arg(r), id_arg(e), id_arg(s)) {
        Some(t) => {
            put_u(p, t);
            0
        }
        None => -EPERM,
    }
}
pub fn sys_setresgid(r: u64, e: u64, s: u64) -> i64 {
    let Some(p) = process::current() else { return -EFAULT };
    match rule_setres(p.cred.euid == 0, gids(p), id_arg(r), id_arg(e), id_arg(s)) {
        Some(t) => {
            put_g(p, t);
            0
        }
        None => -EPERM,
    }
}

fn put3(t: (u32, u32, u32), a: u64, b: u64, c: u64) -> i64 {
    for (v, ptr) in [(t.0, a), (t.1, b), (t.2, c)] {
        if !copy_out(ptr, 0, &v.to_le_bytes()) {
            return -EFAULT;
        }
    }
    0
}
pub fn sys_getresuid(a: u64, b: u64, c: u64) -> i64 {
    let Some(p) = process::current() else { return -EFAULT };
    put3(uids(p), a, b, c)
}
pub fn sys_getresgid(a: u64, b: u64, c: u64) -> i64 {
    let Some(p) = process::current() else { return -EFAULT };
    put3(gids(p), a, b, c)
}

/// `getgroups(size, list)`: size 0 only counts.
pub fn sys_getgroups(size: u64, list: u64) -> i64 {
    let Some(p) = process::current() else { return -EFAULT };
    let n = p.cred.ngroups as usize;
    if size == 0 {
        return n as i64;
    }
    if (size as usize) < n {
        return -EINVAL;
    }
    for i in 0..n {
        if !copy_out(list, i * 4, &p.cred.groups[i].to_le_bytes()) {
            return -EFAULT;
        }
    }
    n as i64
}

/// `setgroups(size, list)`: root only.
pub fn sys_setgroups(size: u64, list: u64) -> i64 {
    let Some(p) = process::current() else { return -EFAULT };
    if p.cred.euid != 0 {
        return -EPERM;
    }
    let n = size as usize;
    if n > MAX_GROUPS {
        return -EINVAL;
    }
    let mut g = [0u32; MAX_GROUPS];
    for (i, slot) in g.iter_mut().enumerate().take(n) {
        let mut b = [0u8; 4];
        if !copy_in(list, i * 4, &mut b) {
            return -EFAULT;
        }
        *slot = u32::from_le_bytes(b);
    }
    p.cred.groups = g;
    p.cred.ngroups = n as u8;
    0
}

fn opt(v: u64) -> Option<u32> {
    let v = id_arg(v);
    if v == NONE { None } else { Some(v) }
}

fn chown_path(path_p: u64, uid: u64, gid: u64, follow: bool) -> i64 {
    let abs = match user_path(path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    chown_abs(&abs, uid, gid, follow)
}
/// The chown of an already resolved path (fchownat shares it).
pub fn chown_abs(abs: &str, uid: u64, gid: u64, follow: bool) -> i64 {
    crate::fs::fsys::vfs().chown(abs, follow, opt(uid), opt(gid)).map_or_else(|e| -fs_errno(e), |_| 0)
}
pub fn sys_chown(path_p: u64, uid: u64, gid: u64) -> i64 {
    chown_path(path_p, uid, gid, true)
}
pub fn sys_lchown(path_p: u64, uid: u64, gid: u64) -> i64 {
    chown_path(path_p, uid, gid, false)
}
pub use crate::user::fd::sys_fchown;

/// `access(path, mode)`: real ids; mode 0 (F_OK) only checks existence.
pub fn sys_access(path_p: u64, mode: u64) -> i64 {
    if mode & !7 != 0 {
        return -EINVAL;
    }
    let abs = match user_path(path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::fs::fsys::vfs().access(&abs, mode as u8).map_or_else(|e| -fs_errno(e), |_| 0)
}
