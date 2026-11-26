// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// System V shared memory (shmget / shmat / shmdt / shmctl), the kernel half:
// syscalls 141..144, for MIT-SHM.  The rules and the segment table are in
// user/shm_core.rs (host-tested); here are the pieces that touch the kernel:
//
//   * a segment's bytes are an anonymous /dev/shm file (the memfd idiom:
//     created, unlinked, kept alive by its open VFS slot), so `shmat` maps the
//     file's own frames MAP_SHARED exactly like mmap does -- every attacher,
//     and a fork child, sees the same memory;
//   * the attachments (address-space, address, length, segment) are kept in a
//     list of their own.  `shmdt`, `munmap` of a whole attachment, the death
//     of the last handle on an address space (exit, execve) and `fork` keep it
//     and the segments' `shm_nattch` in step;
//   * a segment destroyed while still mapped (IPC_RMID is deferred until the
//     last detach, so this only happens when /dev/shm itself fails) hands its
//     frames to the last unmap, like any closed file.
//
// Differences from Linux, on purpose: no SHM_REMAP semantics beyond replacing
// what is mapped at a fixed address (the old pages go, their attachment count
// is not adjusted), a replaced MAP_FIXED attachment is only noticed when its
// address space dies, and segments are limited to what /dev/shm holds.

use alloc::vec::Vec;

use crate::fs::vfs::{FsError, Opened, O_CREATE, O_EXCL, O_READ, O_WRITE};
use crate::mem::vmm::{self, AddressSpace, PAGE};
use crate::user::process;
use crate::user::shm_core::*;
use crate::user::usermode::EFAULT;

const MMAP_LIMIT: u64 = vmm::USER_STACK_TOP - (1 << 20);

/// One mapped attachment of a segment into an address space (`cr3` names it).
struct Att {
    cr3: u64,
    addr: u64,
    len: u64,
    id: i32,
}

// The kernel is not preemptible and runs on one CPU (see mem/vmm.rs); every
// entry point below runs to completion in syscall or teardown context, the
// same rule user/fd/unix.rs follows for its registry.
static mut TABLE: ShmTable<usize> = ShmTable::new();
static mut ATTS: Vec<Att> = Vec::new();
static mut SEQ: u64 = 0;

#[allow(static_mut_refs)]
fn table() -> &'static mut ShmTable<usize> {
    // SAFETY: single CPU, no preemption, no re-entrancy (see above).
    unsafe { &mut TABLE }
}

#[allow(static_mut_refs)]
fn atts() -> &'static mut Vec<Att> {
    // SAFETY: as above.
    unsafe { &mut ATTS }
}

fn now_secs() -> i64 {
    (crate::user::sysc::realtime_ns() / 1_000_000_000) as i64
}

fn caller() -> Option<Caller> {
    let cur = process::current()?;
    let c = &cur.cred;
    Some(Caller {
        pid: process::tgid() as u32,
        euid: c.euid,
        egid: c.egid,
        groups: c.groups[..(c.ngroups as usize).min(c.groups.len())].to_vec(),
    })
}

fn errno_of(e: FsError) -> i64 {
    -crate::user::fd::fs_errno(e)
}

/// Close the backing file of a destroyed segment.
fn destroy(slot: usize) {
    let _ = crate::fs::fsys::vfs().close(slot);
}

/// The anonymous file behind a new segment: `bytes` long, mode 0600, no name.
fn make_backing(bytes: u64) -> Result<usize, i64> {
    // SAFETY: single CPU, see the statics above.
    let n = unsafe {
        SEQ += 1;
        SEQ
    };
    let path = alloc::format!("/dev/shm/.sysv-{}", n);
    let vfs = crate::fs::fsys::vfs();
    vfs.create_mode = 0o600;
    let slot = match vfs.open_desc(&path, O_READ | O_WRITE | O_CREATE | O_EXCL) {
        Ok(Opened::File(s)) => s,
        Ok(_) => return Err(-ENOMEM),
        Err(FsError::NotFound) => return Err(-ENOSPC), // no /dev/shm mounted
        Err(e) => return Err(errno_of(e)),
    };
    if let Err(e) = vfs.unlink(&path) {
        let _ = vfs.close(slot);
        return Err(errno_of(e));
    }
    if let Err(e) = vfs.ftruncate(slot, bytes) {
        let _ = vfs.close(slot);
        return Err(match e {
            FsError::NoSpace => -ENOSPC,
            e => errno_of(e),
        });
    }
    Ok(slot)
}

/// `shmget(key, size, shmflg)` (syscall 141).
pub fn sys_shmget(key: u64, size: u64, flg: u64) -> i64 {
    let Some(who) = caller() else { return -EFAULT };
    match table().get(key as i32, size, flg as i32, &who, now_secs(), make_backing) {
        Ok(id) => id as i64,
        Err(e) => e,
    }
}

/// `shmat(shmid, shmaddr, shmflg)` (syscall 142): the address of the mapping.
pub fn sys_shmat(id: u64, shmaddr: u64, flg: u64) -> i64 {
    let id = id as i32;
    let flg = flg as i32;
    let Some(who) = caller() else { return -EFAULT };
    let place = match place_for(shmaddr, flg) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let rdonly = flg & SHM_RDONLY != 0;
    let (size, slot) = match table().attach(id, &who, rdonly, now_secs()) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let r = map_segment(id, slot, size, place, !rdonly, flg & SHM_REMAP != 0);
    if r < 0 {
        if let Some(b) = table().undo_attach(id) {
            destroy(b);
        }
    }
    r
}

fn map_segment(id: i32, slot: usize, size: u64, place: Place, w: bool, remap: bool) -> i64 {
    let len = (size + PAGE - 1) & !(PAGE - 1);
    let n = len / PAGE;
    let Some(cur) = process::current() else { return -EFAULT };
    let Some(aspace) = cur.aspace.as_mut() else { return -EFAULT };
    let cr3 = aspace.cr3();
    let start = match place {
        Place::At(a) => {
            if !remap && !aspace.range_free(a, len) {
                return -EINVAL; // Linux: -EINVAL unless SHM_REMAP
            }
            // SAFETY: syscall context, our CR3 is live.
            unsafe { aspace.unmap(a, len) };
            a
        }
        Place::Any => {
            let mut v = vmm::USER_MMAP_BASE;
            loop {
                if v + len > MMAP_LIMIT {
                    return -ENOMEM;
                }
                if aspace.range_free(v, len) {
                    break;
                }
                v += PAGE;
            }
            v
        }
    };
    let sig_lo = vmm::USER_SIGCODE & !(PAGE - 1);
    if start & (PAGE - 1) != 0 || start < vmm::USER_BASE || start + len > MMAP_LIMIT {
        return -EINVAL;
    }
    if start < sig_lo + PAGE && start + len > sig_lo {
        return -EINVAL;
    }
    let free = crate::mem::pmm::stats_snapshot().free_pages.load(core::sync::atomic::Ordering::Relaxed);
    if free < 64 + n / 512 {
        return -ENOMEM;
    }
    let vfs = crate::fs::fsys::vfs();
    for i in 0..n {
        let phys = match vfs.file_page(slot, i) {
            Ok(p) => p,
            Err(e) => {
                unsafe { aspace.unmap(start, len) };
                return match e {
                    FsError::NoSpace => -ENOMEM,
                    _ => -EINVAL,
                };
            }
        };
        if !crate::mem::cow::track(phys) {
            unsafe { aspace.unmap(start, len) };
            return -ENOMEM;
        }
        if !aspace.map_page_shared(start + i * PAGE, phys, w) {
            let _ = crate::mem::cow::release(phys);
            unsafe { aspace.unmap(start, len) };
            return -ENOMEM;
        }
    }
    atts().push(Att { cr3, addr: start, len, id });
    start as i64
}

/// `shmdt(shmaddr)` (syscall 143): `shmaddr` must be the start of an attachment.
pub fn sys_shmdt(addr: u64) -> i64 {
    let Some(cur) = process::current() else { return -EFAULT };
    let Some(aspace) = cur.aspace.as_mut() else { return -EFAULT };
    let cr3 = aspace.cr3();
    let Some(i) = atts().iter().position(|a| a.cr3 == cr3 && a.addr == addr) else {
        return -EINVAL;
    };
    let a = atts().remove(i);
    // SAFETY: syscall context, our CR3 is live.
    unsafe { aspace.unmap(a.addr, a.len) };
    if let Some(b) = table().detach(a.id, process::tgid() as u32, now_secs()) {
        destroy(b);
    }
    0
}

/// `shmctl(shmid, cmd, buf)` (syscall 144).
pub fn sys_shmctl(id: u64, cmd: u64, buf: u64) -> i64 {
    let id = id as i32;
    // IPC_64 may be or-ed in by a libc that asks for the 64-bit layout; there
    // is only that one here.
    let cmd = (cmd as i32) & !0x100;
    let Some(who) = caller() else { return -EFAULT };
    let now = now_secs();
    let t = table();
    match cmd {
        IPC_RMID => match t.rmid(id, &who, now) {
            Ok(Some(b)) => {
                destroy(b);
                0
            }
            Ok(None) => 0,
            Err(e) => e,
        },
        IPC_SET => {
            let Some(cur) = process::current() else { return -EFAULT };
            let Some(aspace) = cur.aspace.as_ref() else { return -EFAULT };
            let mut raw = [0u8; SHMID_DS_LEN];
            if !aspace.read(buf, &mut raw) {
                return -EFAULT;
            }
            match t.set(id, &who, DsSet::from_ds(&raw), now) {
                Ok(()) => 0,
                Err(e) => e,
            }
        }
        IPC_STAT => match t.stat(id, &who) {
            Ok(b) => put_user(buf, &b, 0),
            Err(e) => e,
        },
        SHM_STAT | SHM_STAT_ANY => match t.stat_index(id as usize, &who, cmd == SHM_STAT_ANY) {
            Ok((sid, b)) => put_user(buf, &b, sid as i64),
            Err(e) => e,
        },
        IPC_INFO => put_user(buf, &shminfo_bytes(), t.max_index()),
        SHM_INFO => put_user(buf, &shm_info_bytes(t), t.max_index()),
        SHM_LOCK | SHM_UNLOCK => match t.lock(id, &who) {
            Ok(()) => 0,
            Err(e) => e,
        },
        _ => -EINVAL,
    }
}

/// Copy `bytes` to the user address `buf`; `ok` is the call's result.
fn put_user(buf: u64, bytes: &[u8], ok: i64) -> i64 {
    let Some(cur) = process::current() else { return -EFAULT };
    let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return -EFAULT };
    if !aspace.ensure_writable(buf) || !aspace.write(buf, bytes, false) {
        return -EFAULT;
    }
    ok
}

// ------------------------------------------------------------------ hooks

/// `fork` copied an address space: the child attaches everything the parent
/// had.  (The frames are shared by the COW clone; only the counts move.)
pub fn on_fork(parent_cr3: u64, child_cr3: u64) {
    let dup: Vec<Att> = atts()
        .iter()
        .filter(|a| a.cr3 == parent_cr3)
        .map(|a| Att { cr3: child_cr3, addr: a.addr, len: a.len, id: a.id })
        .collect();
    for a in dup {
        table().note_fork(a.id);
        atts().push(a);
    }
}

/// `munmap(addr, len)` took away `[addr, addr + len)`: every attachment that
/// lay wholly inside it is detached.
pub fn on_unmap(cr3: u64, addr: u64, len: u64) {
    let end = addr.saturating_add(len);
    let mut i = 0;
    while i < atts().len() {
        let a = &atts()[i];
        if a.cr3 == cr3 && a.addr >= addr && a.addr + a.len <= end {
            let a = atts().remove(i);
            if let Some(b) = table().detach(a.id, 0, now_secs()) {
                destroy(b);
            }
        } else {
            i += 1;
        }
    }
}

/// The last handle on the address space `cr3` is going away (exit, execve):
/// all its attachments end.  The pages themselves are freed by the tables.
pub fn on_aspace_gone(cr3: u64) {
    if !atts().iter().any(|a| a.cr3 == cr3) {
        return;
    }
    let mut i = 0;
    while i < atts().len() {
        if atts()[i].cr3 == cr3 {
            let a = atts().remove(i);
            if let Some(b) = table().detach(a.id, 0, now_secs()) {
                destroy(b);
            }
        } else {
            i += 1;
        }
    }
}
