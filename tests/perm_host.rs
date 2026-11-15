// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of phase 3.5.3 / 3.5.4: the VFS permission model on a real
// tmpfs (owner/group/other bits, root bypass, sticky, chown rules, access
// with real ids, setuid clearing) and the pure set*id rules of user/ident.rs.
//
//   rustc --edition 2021 --test tests/perm_host.rs -o /tmp/perm_host \
//     && /tmp/perm_host --test-threads=1

#![allow(dead_code, unused_imports)]
extern crate alloc;

mod hw {
    pub mod blk {
        #[derive(Debug)]
        pub enum BlkError {
            ReadOnly,
            Other,
        }
    }
}
#[path = "../sys/kern/src/fs/vfs.rs"]
pub mod vfs_impl;
#[path = "../sys/kern/src/fs/tmpfs.rs"]
pub mod tmpfs_impl;
#[path = "../sys/kern/src/user/ident_rules.rs"]
mod r;
mod fs {
    pub use super::tmpfs_impl as tmpfs;
    pub use super::vfs_impl as vfs;
}

use fs::vfs::*;
use std::cell::Cell;

thread_local! { static WHO: Cell<Cred> = Cell::new(Cred::ROOT); }
fn hook() -> Cred {
    WHO.with(|w| w.get())
}
fn as_user(uid: u32, gid: u32, groups: &[u32]) {
    let mut c = Cred { uid, euid: uid, gid, egid: gid, groups: [0; MAX_GROUPS], ngroups: groups.len() as u8 };
    c.groups[..groups.len()].copy_from_slice(groups);
    WHO.with(|w| w.set(c));
}
fn as_root() {
    WHO.with(|w| w.set(Cred::ROOT));
}

fn fresh() -> Vfs {
    set_cred_hook(hook);
    as_root();
    let mut v = Vfs::new();
    v.mount("/", Box::new(fs::tmpfs::TmpFs::new())).unwrap();
    v
}

/// open(2) the way a process does: the permission check lives there.
fn try_open(v: &mut Vfs, path: &str, flags: u32) -> FsResult<()> {
    let fd = v.open(path, flags)?;
    v.close(fd).unwrap();
    Ok(())
}
fn rd(v: &mut Vfs, p: &str) -> FsResult<()> {
    try_open(v, p, O_READ)
}
fn wr(v: &mut Vfs, p: &str) -> FsResult<()> {
    try_open(v, p, O_WRITE)
}

/// Create `path` as root with the given owner and mode.
fn file(v: &mut Vfs, path: &str, uid: u32, gid: u32, mode: u16) {
    as_root();
    v.write_file(path, b"x").unwrap();
    v.chown(path, true, Some(uid), Some(gid)).unwrap();
    v.chmod(path, mode).unwrap();
}

#[test]
fn owner_group_other_bits() {
    let mut v = fresh();
    file(&mut v, "/f", 1000, 100, 0o640);
    as_user(1000, 1000, &[]);
    assert!(rd(&mut v, "/f").is_ok());
    assert!(wr(&mut v, "/f").is_ok());
    as_user(2000, 100, &[]);
    assert!(rd(&mut v, "/f").is_ok(), "group read");
    assert_eq!(wr(&mut v, "/f").unwrap_err(), FsError::Access, "group has no write");
    as_user(2000, 2000, &[100]);
    assert!(rd(&mut v, "/f").is_ok(), "supplementary group");
    as_user(3000, 3000, &[]);
    assert_eq!(rd(&mut v, "/f").unwrap_err(), FsError::Access, "other has nothing");
    as_root();
    assert!(rd(&mut v, "/f").is_ok(), "root bypass");
}

#[test]
fn owner_bits_beat_group_bits() {
    let mut v = fresh();
    // The classic: owner has no access, group does -- the owner is still denied.
    file(&mut v, "/f", 1000, 100, 0o070);
    as_user(1000, 100, &[]);
    assert_eq!(rd(&mut v, "/f").unwrap_err(), FsError::Access);
}

#[test]
fn directory_search_and_write() {
    let mut v = fresh();
    as_root();
    v.mkdir("/d", 0o700).unwrap();
    v.chown("/d", true, Some(1000), Some(1000)).unwrap();
    file(&mut v, "/d/f", 1000, 1000, 0o644);
    as_user(2000, 2000, &[]);
    assert_eq!(rd(&mut v, "/d/f").unwrap_err(), FsError::Access, "no x on /d");
    assert_eq!(try_open(&mut v, "/d/g", O_WRITE | O_CREATE).unwrap_err(), FsError::Access);
    as_user(1000, 1000, &[]);
    assert!(rd(&mut v, "/d/f").is_ok());
    assert!(try_open(&mut v, "/d/g", O_WRITE | O_CREATE).is_ok());
}

#[test]
fn created_files_belong_to_the_creator() {
    let mut v = fresh();
    as_root();
    v.mkdir("/t", 0o777).unwrap();
    as_user(1234, 55, &[]);
    v.write_file("/t/mine", b"a").unwrap();
    let st = v.stat("/t/mine").unwrap();
    assert_eq!((st.uid, st.gid), (1234, 55));
}

#[test]
fn sticky_directory() {
    let mut v = fresh();
    as_root();
    v.mkdir("/tmp", 0o1777).unwrap();
    as_user(1000, 1000, &[]);
    v.write_file("/tmp/a", b"a").unwrap();
    as_user(2000, 2000, &[]);
    assert_eq!(v.unlink("/tmp/a").unwrap_err(), FsError::NotPermitted, "not yours, sticky dir");
    as_user(1000, 1000, &[]);
    assert!(v.unlink("/tmp/a").is_ok(), "the owner may");
}

#[test]
fn chown_rules() {
    let mut v = fresh();
    file(&mut v, "/f", 1000, 100, 0o644);
    as_user(2000, 2000, &[]);
    assert_eq!(v.chown("/f", true, Some(2000), None).unwrap_err(), FsError::NotPermitted, "not the owner");
    as_user(1000, 1000, &[100]);
    assert_eq!(v.chown("/f", true, Some(2000), None).unwrap_err(), FsError::NotPermitted, "no giving away");
    assert!(v.chown("/f", true, None, Some(100)).is_ok(), "own group is fine");
    assert_eq!(v.chown("/f", true, None, Some(777)).unwrap_err(), FsError::NotPermitted, "foreign group");
    as_root();
    assert!(v.chown("/f", true, Some(5), Some(6)).is_ok());
    let st = v.stat("/f").unwrap();
    assert_eq!((st.uid, st.gid), (5, 6));
}

#[test]
fn chown_by_owner_clears_setuid() {
    let mut v = fresh();
    file(&mut v, "/f", 1000, 100, 0o6755);
    as_user(1000, 100, &[]);
    v.chown("/f", true, None, Some(100)).unwrap();
    assert_eq!(v.stat("/f").unwrap().mode & 0o6000, 0);
}

#[test]
fn chmod_needs_ownership() {
    let mut v = fresh();
    file(&mut v, "/f", 1000, 100, 0o644);
    as_user(2000, 100, &[]);
    assert!(v.chmod("/f", 0o777).is_err());
    as_user(1000, 100, &[]);
    assert!(v.chmod("/f", 0o600).is_ok());
}

#[test]
fn access_uses_real_ids() {
    let mut v = fresh();
    file(&mut v, "/f", 1000, 100, 0o600);
    // Effective 1000 (setuid), real 2000: open works, access(R_OK) does not.
    let mut c = Cred { uid: 2000, euid: 1000, gid: 2000, egid: 2000, groups: [0; MAX_GROUPS], ngroups: 0 };
    c.groups[0] = 0;
    WHO.with(|w| w.set(c));
    assert!(rd(&mut v, "/f").is_ok());
    assert_eq!(v.access("/f", R_OK).unwrap_err(), FsError::Access);
    assert!(v.access("/f", 0).is_ok(), "F_OK only needs existence");
}

#[test]
fn exec_needs_x_bit() {
    let mut v = fresh();
    file(&mut v, "/prog", 0, 0, 0o750);
    as_user(1000, 1000, &[]);
    assert_eq!(v.exec_check("/prog").unwrap_err(), FsError::Access);
    file(&mut v, "/prog2", 0, 0, 0o4755);
    as_user(1000, 1000, &[]);
    let st = v.exec_check("/prog2").unwrap();
    assert_eq!((st.uid, st.mode & 0o4000), (0, 0o4000), "caller applies the setuid bit");
    as_root();
    file(&mut v, "/data", 0, 0, 0o644);
    assert!(v.exec_check("/data").is_err(), "root still needs some x bit");
}

#[test]
fn rules_of_setuid_family() {
    let ids = (1000, 1000, 1000);
    assert_eq!(r::rule_set(true, ids, 5), Some((5, 5, 5)), "root sets all three");
    assert_eq!(r::rule_set(false, (1000, 0, 0), 1000), Some((1000, 1000, 0)));
    assert_eq!(r::rule_set(false, (1000, 1000, 1000), 0), None);
    assert_eq!(r::rule_set(false, (1000, 1000, 0), 0), Some((1000, 0, 0)), "back to saved root");
    assert_eq!(r::rule_setre(false, ids, 1000, u32::MAX), Some((1000, 1000, 1000)));
    assert_eq!(r::rule_setre(false, ids, 0, u32::MAX), None);
    assert_eq!(r::rule_setre(true, (1000, 1000, 1000), 0, 0), Some((0, 0, 0)));
    assert_eq!(r::rule_setres(false, (1, 2, 3), 3, 1, 2), Some((3, 1, 2)), "permutation allowed");
    assert_eq!(r::rule_setres(false, (1, 2, 3), 4, u32::MAX, u32::MAX), None);
    assert_eq!(r::rule_setres(true, (1, 2, 3), 4, 5, 6), Some((4, 5, 6)));
}
