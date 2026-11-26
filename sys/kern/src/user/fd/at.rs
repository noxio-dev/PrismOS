// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The *at() family: a path resolved against an open directory instead of
// the cwd.  One helper does the resolving (`user_path_at`); the calls are
// thin wrappers over the same VFS operations the cwd-relative syscalls use.
//
//   130 openat(dirfd, path, flags)
//   131 fstatat(dirfd, path, buf, flags)            AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH
//   132 mkdirat(dirfd, path, mode)
//   133 unlinkat(dirfd, path, flags)                AT_REMOVEDIR
//   134 readlinkat(dirfd, path, buf, size)
//   135 renameat(olddirfd, old, newdirfd, new)
//   136 symlinkat(target, newdirfd, linkpath)
//   137 linkat(olddirfd, old, newdirfd, new, flags)
//   138 faccessat(dirfd, path, mode, flags)
//   139 fchmodat(dirfd, path, mode, flags)
//   140 fchownat(dirfd, path, uid, gid, flags)     AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH
//
// `dirfd` is AT_FDCWD (-100) or a descriptor of an open *directory*; an
// absolute path ignores it.  The base is the path the directory was opened
// by (what /proc/<pid>/fd/N reports), so a directory renamed after the open
// keeps resolving through its old name -- the one deviation from Linux.
// (utimensat, 68, already took a dirfd and keeps AT_FDCWD only.)

use alloc::string::String;

use super::{
    desc_of_or_errno, dev_rdev, fs_errno, put_stat, sys_fstat, user_path, user_string, Kind,
};
use super::{copy_out, ENOENT, ENOTDIR};
use crate::user::process;
use crate::user::usermode::{EFAULT, EINVAL};

pub const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const AT_EACCESS: u64 = 0x200;
const AT_EMPTY_PATH: u64 = 0x1000;

fn is_cwd(dirfd: u64) -> bool {
    dirfd as i32 as i64 == AT_FDCWD
}

/// The absolute directory a `dirfd` stands for.
fn dir_base(dirfd: u64) -> Result<String, i64> {
    let desc = desc_of_or_errno(dirfd)?;
    let path = match (&desc.kind, &desc.path) {
        (Kind::Vfs(_), Some(p)) => p.clone(),
        _ => return Err(-ENOTDIR),
    };
    match crate::fs::fsys::vfs().stat(&path) {
        Ok(st) if st.ftype == crate::fs::vfs::FileType::Dir => Ok(path),
        Ok(_) => Err(-ENOTDIR),
        Err(e) => Err(-fs_errno(e)),
    }
}

/// `user_path`, but a relative path starts at `dirfd`.
pub(crate) fn user_path_at(dirfd: u64, ptr: u64) -> Result<String, i64> {
    if is_cwd(dirfd) {
        return user_path(ptr);
    }
    let raw = match user_string(ptr) {
        Ok(s) => s,
        Err(e) if e == -EINVAL => return Err(-ENOENT), // non-utf8, like user_path
        Err(e) => return Err(e),
    };
    if raw.is_empty() {
        return Err(-ENOENT);
    }
    if raw.starts_with('/') {
        return Ok(process::abs_current(&raw));
    }
    let base = dir_base(dirfd)?;
    Ok(crate::fs::vfs::normalize(&base, &raw))
}

/// An empty path with AT_EMPTY_PATH names the descriptor itself.
fn empty_path(path_p: u64, flags: u64) -> bool {
    flags & AT_EMPTY_PATH != 0 && path_p != 0 && matches!(user_string(path_p), Ok(s) if s.is_empty())
}

pub fn sys_fstatat(dirfd: u64, path_p: u64, buf: u64, flags: u64) -> i64 {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return -EINVAL;
    }
    if empty_path(path_p, flags) {
        return sys_fstat(dirfd, buf);
    }
    let abs = match user_path_at(dirfd, path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let r = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        crate::fs::fsys::vfs().lstat(&abs)
    } else {
        crate::fs::fsys::vfs().stat(&abs)
    };
    match r {
        Ok(st) => {
            let times = (st.atime, st.mtime, st.ctime);
            if put_stat(buf, st.ino, st.mode, (st.uid, st.gid), &st.ftype, st.size, st.nlink, dev_rdev(&st.ftype), st.blocks, times) {
                0
            } else {
                -EFAULT
            }
        }
        Err(e) => -fs_errno(e),
    }
}

pub fn sys_mkdirat(dirfd: u64, path_p: u64, mode: u64) -> i64 {
    let abs = match user_path_at(dirfd, path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let mask = process::current().map_or(0o022, |p| p.umask as u64);
    crate::fs::fsys::vfs().mkdir(&abs, ((mode & 0o7777) & !mask) as u16).map_or_else(|e| -fs_errno(e), |_| 0)
}

pub fn sys_unlinkat(dirfd: u64, path_p: u64, flags: u64) -> i64 {
    if flags & !AT_REMOVEDIR != 0 {
        return -EINVAL;
    }
    let abs = match user_path_at(dirfd, path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let vfs = crate::fs::fsys::vfs();
    if flags & AT_REMOVEDIR != 0 { vfs.rmdir(&abs) } else { vfs.unlink(&abs) }.map_or_else(|e| -fs_errno(e), |_| 0)
}

pub fn sys_readlinkat(dirfd: u64, path_p: u64, buf_p: u64, bufsz: u64) -> i64 {
    if bufsz == 0 {
        return -EINVAL;
    }
    let abs = match user_path_at(dirfd, path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match crate::fs::fsys::vfs().readlink(&abs) {
        Ok(target) => {
            let n = (target.len() as u64).min(bufsz) as usize;
            if !copy_out(buf_p, 0, &target.as_bytes()[..n]) {
                return -EFAULT;
            }
            n as i64
        }
        Err(e) => -fs_errno(e),
    }
}

pub fn sys_renameat(olddirfd: u64, old_p: u64, newdirfd: u64, new_p: u64) -> i64 {
    let old = match user_path_at(olddirfd, old_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let new = match user_path_at(newdirfd, new_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::fs::fsys::vfs().rename(&old, &new).map_or_else(|e| -fs_errno(e), |_| 0)
}

pub fn sys_symlinkat(target_p: u64, newdirfd: u64, link_p: u64) -> i64 {
    let target = match user_string(target_p) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let link = match user_path_at(newdirfd, link_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::fs::fsys::vfs().symlink(&target, &link).map_or_else(|e| -fs_errno(e), |_| 0)
}

pub fn sys_linkat(olddirfd: u64, old_p: u64, newdirfd: u64, new_p: u64, flags: u64) -> i64 {
    // AT_SYMLINK_FOLLOW (0x400) is the one flag Linux has; link() here never
    // follows, so only the default is served.
    if flags != 0 {
        return -EINVAL;
    }
    let old = match user_path_at(olddirfd, old_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let new = match user_path_at(newdirfd, new_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::fs::fsys::vfs().link(&old, &new).map_or_else(|e| -fs_errno(e), |_| 0)
}

pub fn sys_faccessat(dirfd: u64, path_p: u64, mode: u64, flags: u64) -> i64 {
    if mode & !7 != 0 || flags & !(AT_EACCESS | AT_SYMLINK_NOFOLLOW) != 0 {
        return -EINVAL;
    }
    let abs = match user_path_at(dirfd, path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::fs::fsys::vfs().access(&abs, mode as u8).map_or_else(|e| -fs_errno(e), |_| 0)
}

pub fn sys_fchmodat(dirfd: u64, path_p: u64, mode: u64, flags: u64) -> i64 {
    if flags != 0 {
        return -EINVAL; // the libc's AT_SYMLINK_NOFOLLOW emulation is not ours
    }
    let abs = match user_path_at(dirfd, path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::fs::fsys::vfs().chmod(&abs, (mode & 0o7777) as u16).map_or_else(|e| -fs_errno(e), |_| 0)
}

pub fn sys_fchownat(dirfd: u64, path_p: u64, uid: u64, gid: u64, flags: u64) -> i64 {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return -EINVAL;
    }
    if empty_path(path_p, flags) {
        return super::sys_fchown(dirfd, uid, gid);
    }
    let abs = match user_path_at(dirfd, path_p) {
        Ok(p) => p,
        Err(e) => return e,
    };
    crate::user::ident::chown_abs(&abs, uid, gid, flags & AT_SYMLINK_NOFOLLOW == 0)
}
