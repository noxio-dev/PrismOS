// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of System V shared memory (sys/kern/src/user/shm_core.rs):
// shmget key / flag rules, permissions, attach counting, deferred IPC_RMID,
// IPC_SET / IPC_STAT layout, the limits.
//
//   rustc --edition 2021 --test tests/shm_sysv_host.rs -o /tmp/shm_sysv_host \
//     && /tmp/shm_sysv_host --test-threads=1

#![allow(dead_code, unused_imports)]
extern crate alloc;

#[path = "../sys/kern/src/user/shm_core.rs"]
mod shm_core;

use shm_core::*;

type T = ShmTable<usize>;

fn root() -> Caller {
    Caller { pid: 1, euid: 0, egid: 0, groups: vec![] }
}
fn user(uid: u32, gid: u32) -> Caller {
    Caller { pid: 100 + uid, euid: uid, egid: gid, groups: vec![] }
}
fn mk(_bytes: u64) -> Result<usize, i64> {
    Ok(7)
}

#[test]
fn private_always_makes_a_new_segment() {
    let mut t = T::new();
    let a = t.get(0, 100, 0o600, &user(1, 1), 5, mk).unwrap();
    let b = t.get(0, 100, 0o600, &user(1, 1), 5, mk).unwrap();
    assert_ne!(a, b);
    assert_eq!(t.len(), 2);
}

#[test]
fn keyed_lookup_create_and_excl() {
    let mut t = T::new();
    let who = user(1, 1);
    assert_eq!(t.get(42, 100, 0o600, &who, 0, mk), Err(-ENOENT));
    let a = t.get(42, 100, IPC_CREAT | 0o600, &who, 0, mk).unwrap();
    assert_eq!(t.get(42, 50, 0o600, &who, 0, mk), Ok(a));
    assert_eq!(t.get(42, 200, 0o600, &who, 0, mk), Err(-EINVAL), "bigger than the segment");
    assert_eq!(t.get(42, 100, IPC_CREAT | IPC_EXCL | 0o600, &who, 0, mk), Err(-EEXIST));
}

#[test]
fn size_limits() {
    let mut t = T::new();
    assert_eq!(t.get(0, 0, 0o600, &root(), 0, mk), Err(-EINVAL));
    assert_eq!(t.get(0, SHMMAX + 1, 0o600, &root(), 0, mk), Err(-EINVAL));
    assert!(t.get(0, SHMMAX, 0o600, &root(), 0, mk).is_ok());
    // /dev/shm is full now (SHMALL): one more page is refused.
    assert_eq!(t.get(0, 1, 0o600, &root(), 0, mk), Err(-ENOSPC));
}

#[test]
fn backing_error_is_returned_and_nothing_is_created() {
    let mut t = T::new();
    assert_eq!(t.get(0, 10, 0o600, &root(), 0, |_| Err(-ENOSPC)), Err(-ENOSPC));
    assert_eq!(t.len(), 0);
}

#[test]
fn backing_is_whole_pages() {
    let mut t = T::new();
    let mut asked = 0;
    t.get(0, 5000, 0o600, &root(), 0, |b| {
        asked = b;
        Ok(1)
    })
    .unwrap();
    assert_eq!(asked, 8192);
}

#[test]
fn permissions_owner_group_other() {
    let mut t = T::new();
    let owner = user(10, 20);
    let id = t.get(0, 100, 0o640, &owner, 0, mk).unwrap();
    assert!(t.attach(id, &owner, false, 0).is_ok());
    let grp = Caller { pid: 2, euid: 11, egid: 20, groups: vec![] };
    assert!(t.attach(id, &grp, true, 0).is_ok(), "group may read");
    assert_eq!(t.attach(id, &grp, false, 0), Err(-EACCES), "group may not write");
    let other = user(12, 99);
    assert_eq!(t.attach(id, &other, true, 0), Err(-EACCES));
    assert!(t.attach(id, &root(), false, 0).is_ok(), "root passes");
    // a supplementary group counts
    let sup = Caller { pid: 3, euid: 13, egid: 99, groups: vec![20] };
    assert!(t.attach(id, &sup, true, 0).is_ok());
}

#[test]
fn attach_detach_count_and_stamps() {
    let mut t = T::new();
    let who = user(1, 1);
    let id = t.get(0, 4096, 0o600, &who, 3, mk).unwrap();
    let (size, b) = t.attach(id, &who, false, 10).unwrap();
    assert_eq!((size, b), (4096, 7));
    t.attach(id, &who, false, 11).unwrap();
    let s = t.get_seg(id).unwrap();
    assert_eq!((s.nattch, s.atime, s.lpid, s.ctime), (2, 11, who.pid, 3));
    assert_eq!(t.detach(id, 55, 20), None);
    let s = t.get_seg(id).unwrap();
    assert_eq!((s.nattch, s.dtime, s.lpid), (1, 20, 55));
}

#[test]
fn rmid_without_attachments_destroys_at_once() {
    let mut t = T::new();
    let who = user(1, 1);
    let id = t.get(5, 100, IPC_CREAT | 0o600, &who, 0, mk).unwrap();
    assert_eq!(t.rmid(id, &who, 0), Ok(Some(7)));
    assert_eq!(t.len(), 0);
    assert_eq!(t.attach(id, &who, false, 0), Err(-EINVAL));
}

#[test]
fn rmid_with_attachments_is_deferred() {
    let mut t = T::new();
    let who = user(1, 1);
    let id = t.get(5, 100, IPC_CREAT | 0o600, &who, 0, mk).unwrap();
    t.attach(id, &who, false, 0).unwrap();
    t.attach(id, &who, false, 0).unwrap();
    assert_eq!(t.rmid(id, &who, 9), Ok(None));
    let s = t.get_seg(id).unwrap();
    assert!(s.removed && s.key == 0 && s.mode & SHM_DEST != 0);
    // the key is free again: a new segment under it, not the dying one
    let id2 = t.get(5, 100, IPC_CREAT | 0o600, &who, 0, mk).unwrap();
    assert_ne!(id, id2);
    assert_eq!(t.detach(id, 1, 1), None);
    assert_eq!(t.detach(id, 1, 2), Some(7), "the last detach destroys it");
    assert!(t.get_seg(id).is_none());
    assert!(t.get_seg(id2).is_some());
}

#[test]
fn undo_attach_leaves_no_trace_and_may_destroy() {
    let mut t = T::new();
    let who = user(1, 1);
    let id = t.get(0, 100, 0o600, &who, 0, mk).unwrap();
    t.attach(id, &who, false, 50).unwrap();
    t.rmid(id, &who, 0).unwrap();
    assert_eq!(t.undo_attach(id), Some(7));
    assert!(t.get_seg(id).is_none());
}

#[test]
fn fork_adds_an_attachment() {
    let mut t = T::new();
    let who = user(1, 1);
    let id = t.get(0, 100, 0o600, &who, 0, mk).unwrap();
    t.attach(id, &who, false, 0).unwrap();
    t.note_fork(id);
    assert_eq!(t.get_seg(id).unwrap().nattch, 2);
}

#[test]
fn only_owner_creator_or_root_may_rmid_or_set() {
    let mut t = T::new();
    let owner = user(1, 1);
    let id = t.get(0, 100, 0o666, &owner, 0, mk).unwrap();
    let other = user(2, 1);
    assert_eq!(t.rmid(id, &other, 0), Err(-EPERM));
    let ds = DsSet { uid: 2, gid: 1, mode: 0o600 };
    assert_eq!(t.set(id, &other, ds, 0), Err(-EPERM));
    t.set(id, &owner, ds, 4).unwrap();
    let s = t.get_seg(id).unwrap();
    assert_eq!((s.uid, s.cuid, s.mode, s.ctime), (2, 1, 0o600, 4));
    // creator keeps the right after the owner changed
    assert!(t.rmid(id, &owner, 0).is_ok());
}

#[test]
fn stat_layout_is_shmid64_ds() {
    let mut t = T::new();
    let who = user(5, 6);
    let id = t.get(77, 5000, IPC_CREAT | 0o640, &who, 123, mk).unwrap();
    t.attach(id, &who, false, 200).unwrap();
    let b = t.stat(id, &who).unwrap();
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    assert_eq!(b.len(), 112);
    assert_eq!(u32_at(0), 77);
    assert_eq!((u32_at(4), u32_at(8), u32_at(12), u32_at(16)), (5, 6, 5, 6));
    assert_eq!(u32_at(20), 0o640);
    assert_eq!(u64_at(48), 5000);
    assert_eq!((u64_at(56), u64_at(64), u64_at(72)), (200, 0, 123));
    assert_eq!((u32_at(80), u32_at(84)), (who.pid, who.pid));
    assert_eq!(u64_at(88), 1);
    // IPC_SET round trip through the same layout
    assert_eq!(DsSet::from_ds(&b), DsSet { uid: 5, gid: 6, mode: 0o640 });
    // stat needs read permission
    assert_eq!(t.stat(id, &user(9, 9)), Err(-EACCES));
}

#[test]
fn stat_index_walks_the_table() {
    let mut t = T::new();
    let a = t.get(0, 10, 0o600, &user(1, 1), 0, mk).unwrap();
    let b = t.get(0, 10, 0o600, &user(1, 1), 0, mk).unwrap();
    assert_eq!(t.stat_index(1, &user(1, 1), false).unwrap().0, b);
    assert_eq!(t.stat_index(0, &user(2, 2), false), Err(-EACCES));
    assert_eq!(t.stat_index(0, &user(2, 2), true).unwrap().0, a);
    assert_eq!(t.stat_index(2, &root(), true), Err(-EINVAL));
    assert_eq!(t.max_index(), 1);
}

#[test]
fn shmat_address_rules() {
    assert_eq!(place_for(0, 0), Ok(Place::Any));
    assert_eq!(place_for(0x10000, 0), Ok(Place::At(0x10000)));
    assert_eq!(place_for(0x10123, 0), Err(-EINVAL));
    assert_eq!(place_for(0x10123, SHM_RND), Ok(Place::At(0x10000)));
}

#[test]
fn lock_is_root_only() {
    let mut t = T::new();
    let id = t.get(0, 10, 0o600, &user(1, 1), 0, mk).unwrap();
    assert_eq!(t.lock(id, &user(1, 1)), Err(-EPERM));
    assert_eq!(t.lock(id, &root()), Ok(()));
    assert_eq!(t.lock(999, &root()), Err(-EINVAL));
}

#[test]
fn info_structs() {
    let b = shminfo_bytes();
    assert_eq!(u64::from_le_bytes(b[0..8].try_into().unwrap()), SHMMAX);
    let mut t = T::new();
    t.get(0, 5000, 0o600, &root(), 0, mk).unwrap();
    let i = shm_info_bytes(&t);
    assert_eq!(i32::from_le_bytes(i[0..4].try_into().unwrap()), 1);
    assert_eq!(u64::from_le_bytes(i[8..16].try_into().unwrap()), 2);
}

#[test]
fn ids_are_never_reused() {
    let mut t = T::new();
    let a = t.get(0, 10, 0o600, &root(), 0, mk).unwrap();
    t.rmid(a, &root(), 0).unwrap();
    let b = t.get(0, 10, 0o600, &root(), 0, mk).unwrap();
    assert_ne!(a, b);
}
