// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// System V shared memory, the pure part: the segment table and the rules of
// shmget(2) / shmat(2) / shmdt(2) / shmctl(2) -- what MIT-SHM (an X server and
// its clients) stands on.  No kernel types in here, so tests/shm_sysv_host.rs
// can include this file as it is.  The backing store (an anonymous /dev/shm
// file), the page mapping and the user memory live in user/shm.rs; the
// per-address-space attachment list lives in mem/vmm.rs.
//
// The rules (Linux's, minus what has no meaning here):
//   shmget  key IPC_PRIVATE (0) always makes a new segment; another key finds
//           the segment that owns it, or makes one with IPC_CREAT.  IPC_EXCL
//           on an existing key is -EEXIST; asking for more bytes than the
//           segment has is -EINVAL; the requested permission bits are checked
//           against the owner / group / other triple.
//   shmat   needs read permission (and write unless SHM_RDONLY), counts one
//           more attachment, stamps `atime` / `lpid`.
//   shmdt   counts one attachment less, stamps `dtime` / `lpid`.
//   IPC_RMID  destroys a segment nobody is attached to at once; otherwise it
//           only marks it (SHM_DEST): the key is freed for a new segment and
//           the last detach destroys it.  The creator, the owner and root only.
//   IPC_SET  uid, gid and the nine permission bits; same callers.
//   IPC_STAT / SHM_STAT / SHM_STAT_ANY  the Linux x86-64 `shmid64_ds`, 112 bytes.
//
// Differences from Linux, on purpose: segment ids are a plain counter (never
// reused, never negative), SHM_LOCK / SHM_UNLOCK are accepted for root and do
// nothing (there is no swap), and a segment cannot be bigger than what
// /dev/shm can hold.

use alloc::vec::Vec;

pub const EPERM: i64 = 1;
pub const ENOENT: i64 = 2;
pub const ENOMEM: i64 = 12;
pub const EACCES: i64 = 13;
pub const EEXIST: i64 = 17;
pub const EINVAL: i64 = 22;
pub const ENOSPC: i64 = 28;
pub const EIDRM: i64 = 43;

pub const PAGE: u64 = 4096;

pub const IPC_CREAT: i32 = 0o1000;
pub const IPC_EXCL: i32 = 0o2000;

pub const IPC_RMID: i32 = 0;
pub const IPC_SET: i32 = 1;
pub const IPC_STAT: i32 = 2;
pub const IPC_INFO: i32 = 3;
pub const SHM_LOCK: i32 = 11;
pub const SHM_UNLOCK: i32 = 12;
pub const SHM_STAT: i32 = 13;
pub const SHM_INFO: i32 = 14;
pub const SHM_STAT_ANY: i32 = 15;

pub const SHM_RDONLY: i32 = 0o10000;
pub const SHM_RND: i32 = 0o20000;
pub const SHM_REMAP: i32 = 0o40000;
pub const SHM_EXEC: i32 = 0o100000;
/// Mode bit of a segment marked for destruction (what `ipcs` shows as "dest").
pub const SHM_DEST: u16 = 0o1000;

/// Limits reported by IPC_INFO and enforced by `get`.  The byte ceiling is the
/// size of /dev/shm (32 MiB, shared with shm_open and memfd): a larger segment
/// could never be backed anyway.
pub const SHMMAX: u64 = 32 << 20;
pub const SHMMIN: u64 = 1;
pub const SHMMNI: usize = 128;
pub const SHMSEG: u64 = 4096;
pub const SHMALL: u64 = SHMMAX / PAGE;

/// Size of `struct shmid64_ds` on x86-64 Linux (and of mlibc's `shmid_ds`).
pub const SHMID_DS_LEN: usize = 112;
/// Size of `struct shminfo` (IPC_INFO) and of `struct shm_info` (SHM_INFO).
pub const SHMINFO_LEN: usize = 72;
pub const SHM_INFO_LEN: usize = 48;

/// Who is asking: the effective ids and groups, and the process id that ends
/// up in `shm_cpid` / `shm_lpid`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caller {
    pub pid: u32,
    pub euid: u32,
    pub egid: u32,
    pub groups: Vec<u32>,
}

impl Caller {
    fn in_group(&self, gid: u32) -> bool {
        self.egid == gid || self.groups.contains(&gid)
    }
}

/// One segment.  `backing` is whatever the kernel keeps the bytes in (a VFS
/// slot); the table never looks inside it.
#[derive(Clone, Debug)]
pub struct Segment<B> {
    pub id: i32,
    /// 0 for IPC_PRIVATE, and for a segment marked for destruction.
    pub key: i32,
    /// Bytes asked for at creation (`shm_segsz`); the backing holds whole pages.
    pub size: u64,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub cuid: u32,
    pub cgid: u32,
    pub cpid: u32,
    pub lpid: u32,
    pub nattch: u64,
    pub atime: i64,
    pub dtime: i64,
    pub ctime: i64,
    pub removed: bool,
    pub backing: B,
}

impl<B> Segment<B> {
    pub fn pages(&self) -> u64 {
        (self.size + PAGE - 1) / PAGE
    }

    /// The `shmid64_ds` the kernel hands to IPC_STAT.
    pub fn to_ds(&self) -> [u8; SHMID_DS_LEN] {
        let mut b = [0u8; SHMID_DS_LEN];
        // ipc64_perm: key, uid, gid, cuid, cgid, mode (u32), seq (u16) + pad
        b[0..4].copy_from_slice(&self.key.to_le_bytes());
        b[4..8].copy_from_slice(&self.uid.to_le_bytes());
        b[8..12].copy_from_slice(&self.gid.to_le_bytes());
        b[12..16].copy_from_slice(&self.cuid.to_le_bytes());
        b[16..20].copy_from_slice(&self.cgid.to_le_bytes());
        b[20..24].copy_from_slice(&(self.mode as u32).to_le_bytes());
        b[24..26].copy_from_slice(&((self.id as u32 & 0xFFFF) as u16).to_le_bytes());
        // then: segsz, atime, dtime, ctime, cpid, lpid, nattch (offsets 48..96)
        b[48..56].copy_from_slice(&self.size.to_le_bytes());
        b[56..64].copy_from_slice(&self.atime.to_le_bytes());
        b[64..72].copy_from_slice(&self.dtime.to_le_bytes());
        b[72..80].copy_from_slice(&self.ctime.to_le_bytes());
        b[80..84].copy_from_slice(&self.cpid.to_le_bytes());
        b[84..88].copy_from_slice(&self.lpid.to_le_bytes());
        b[88..96].copy_from_slice(&self.nattch.to_le_bytes());
        b
    }
}

/// What IPC_SET reads from a user's `shmid_ds`: the three fields it may change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DsSet {
    pub uid: u32,
    pub gid: u32,
    pub mode: u16,
}

impl DsSet {
    pub fn from_ds(b: &[u8; SHMID_DS_LEN]) -> DsSet {
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        DsSet { uid: u32_at(4), gid: u32_at(8), mode: (u32_at(20) & 0o777) as u16 }
    }
}

/// Where `shmat` puts a segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// `shmaddr == NULL`: the kernel picks (first fit).
    Any,
    /// A page-aligned address the caller named.
    At(u64),
}

/// The address rules of shmat(2): a NULL address lets the kernel choose; an
/// address must be page-aligned unless SHM_RND rounds it down (SHMLBA is one
/// page here).
pub fn place_for(shmaddr: u64, shmflg: i32) -> Result<Place, i64> {
    if shmaddr == 0 {
        return Ok(Place::Any);
    }
    if shmflg & SHM_RND != 0 {
        return Ok(Place::At(shmaddr & !(PAGE - 1)));
    }
    if shmaddr & (PAGE - 1) != 0 {
        return Err(-EINVAL);
    }
    Ok(Place::At(shmaddr))
}

/// Permission test of ipcperms(): `want` is the requested bits in any of the
/// three triples (a `shmflg & 0o777`, or 0o444 / 0o666 for an attach);
/// whichever triple applies to the caller must grant them.  Root passes.
fn permitted<B>(seg: &Segment<B>, who: &Caller, want: u16) -> bool {
    if who.euid == 0 {
        return true;
    }
    let requested = ((want >> 6) | (want >> 3) | want) & 0o7;
    let mut granted = seg.mode;
    if who.euid == seg.uid || who.euid == seg.cuid {
        granted >>= 6;
    } else if who.in_group(seg.gid) || who.in_group(seg.cgid) {
        granted >>= 3;
    }
    requested & !(granted & 0o7) == 0
}

/// The creator, the owner and root may remove or reconfigure a segment.
fn is_master<B>(seg: &Segment<B>, who: &Caller) -> bool {
    who.euid == 0 || who.euid == seg.uid || who.euid == seg.cuid
}

pub struct ShmTable<B> {
    segs: Vec<Segment<B>>,
    next_id: i32,
}

impl<B: Clone> ShmTable<B> {
    pub const fn new() -> ShmTable<B> {
        ShmTable { segs: Vec::new(), next_id: 0 }
    }

    pub fn len(&self) -> usize {
        self.segs.len()
    }

    pub fn get_seg(&self, id: i32) -> Option<&Segment<B>> {
        self.segs.iter().find(|s| s.id == id)
    }

    fn idx(&self, id: i32) -> Result<usize, i64> {
        self.segs.iter().position(|s| s.id == id).ok_or(-EINVAL)
    }

    /// Pages the live segments account for (what SHMALL bounds).
    pub fn total_pages(&self) -> u64 {
        self.segs.iter().map(|s| s.pages()).sum()
    }

    /// shmget(2).  `make` builds the backing store for a new segment from its
    /// size rounded up to whole pages; its error is returned as is.
    pub fn get(
        &mut self,
        key: i32,
        size: u64,
        flg: i32,
        who: &Caller,
        now: i64,
        make: impl FnOnce(u64) -> Result<B, i64>,
    ) -> Result<i32, i64> {
        if key != 0 {
            if let Some(s) = self.segs.iter().find(|s| s.key == key && !s.removed) {
                if flg & IPC_CREAT != 0 && flg & IPC_EXCL != 0 {
                    return Err(-EEXIST);
                }
                if size > s.size {
                    return Err(-EINVAL);
                }
                if !permitted(s, who, (flg & 0o777) as u16) {
                    return Err(-EACCES);
                }
                return Ok(s.id);
            }
            if flg & IPC_CREAT == 0 {
                return Err(-ENOENT);
            }
        }
        if size < SHMMIN || size > SHMMAX {
            return Err(-EINVAL);
        }
        let pages = (size + PAGE - 1) / PAGE;
        if self.segs.len() >= SHMMNI || self.total_pages() + pages > SHMALL {
            return Err(-ENOSPC);
        }
        let backing = make(pages * PAGE)?;
        let id = self.next_id;
        self.next_id = if id == i32::MAX { 0 } else { id + 1 };
        self.segs.push(Segment {
            id,
            key,
            size,
            mode: (flg & 0o777) as u16,
            uid: who.euid,
            gid: who.egid,
            cuid: who.euid,
            cgid: who.egid,
            cpid: who.pid,
            lpid: 0,
            nattch: 0,
            atime: 0,
            dtime: 0,
            ctime: now,
            removed: false,
            backing,
        });
        Ok(id)
    }

    /// shmat(2), the table half: check the permissions, count the attachment
    /// and give back what the mapping needs (the segment size and its backing).
    pub fn attach(&mut self, id: i32, who: &Caller, rdonly: bool, now: i64) -> Result<(u64, B), i64> {
        let i = self.idx(id)?;
        let seg = &mut self.segs[i];
        let want = if rdonly { 0o444 } else { 0o666 };
        if !permitted(seg, who, want) {
            return Err(-EACCES);
        }
        seg.nattch += 1;
        seg.lpid = who.pid;
        seg.atime = now;
        Ok((seg.size, seg.backing.clone()))
    }

    /// Take back an attachment `attach` counted, when the mapping failed: no
    /// timestamps, no `lpid`.  Returns the backing if that destroyed the segment.
    pub fn undo_attach(&mut self, id: i32) -> Option<B> {
        self.drop_attachment(id, None)
    }

    /// shmdt(2), the table half; also what unmapping an attachment, a process
    /// exit and a replaced address space come to.  Returns the backing when
    /// this was the last attachment of a segment marked for destruction.
    pub fn detach(&mut self, id: i32, pid: u32, now: i64) -> Option<B> {
        self.drop_attachment(id, Some((pid, now)))
    }

    /// A fork duplicated an attachment: one more, nothing else changes.
    pub fn note_fork(&mut self, id: i32) {
        if let Some(s) = self.segs.iter_mut().find(|s| s.id == id) {
            s.nattch += 1;
        }
    }

    fn drop_attachment(&mut self, id: i32, stamp: Option<(u32, i64)>) -> Option<B> {
        let i = self.segs.iter().position(|s| s.id == id)?;
        let seg = &mut self.segs[i];
        seg.nattch = seg.nattch.saturating_sub(1);
        if let Some((pid, now)) = stamp {
            seg.lpid = pid;
            seg.dtime = now;
        }
        if seg.nattch == 0 && seg.removed {
            return Some(self.segs.remove(i).backing);
        }
        None
    }

    /// IPC_RMID.  Returns the backing when the segment was destroyed on the
    /// spot; a segment still attached is only marked.
    pub fn rmid(&mut self, id: i32, who: &Caller, now: i64) -> Result<Option<B>, i64> {
        let i = self.idx(id)?;
        if !is_master(&self.segs[i], who) {
            return Err(-EPERM);
        }
        if self.segs[i].nattch == 0 {
            return Ok(Some(self.segs.remove(i).backing));
        }
        let seg = &mut self.segs[i];
        seg.removed = true;
        seg.key = 0;
        seg.mode |= SHM_DEST;
        seg.ctime = now;
        Ok(None)
    }

    /// IPC_SET.
    pub fn set(&mut self, id: i32, who: &Caller, ds: DsSet, now: i64) -> Result<(), i64> {
        let i = self.idx(id)?;
        let seg = &mut self.segs[i];
        if !is_master(seg, who) {
            return Err(-EPERM);
        }
        seg.uid = ds.uid;
        seg.gid = ds.gid;
        seg.mode = (seg.mode & !0o777) | (ds.mode & 0o777);
        seg.ctime = now;
        Ok(())
    }

    /// IPC_STAT: the segment's `shmid64_ds`; needs read permission.
    pub fn stat(&self, id: i32, who: &Caller) -> Result<[u8; SHMID_DS_LEN], i64> {
        let s = self.get_seg(id).ok_or(-EINVAL)?;
        if !permitted(s, who, 0o444) {
            return Err(-EACCES);
        }
        Ok(s.to_ds())
    }

    /// SHM_STAT / SHM_STAT_ANY: the segment at table index `idx` (`ipcs`
    /// walks them).  Returns its id and its `shmid64_ds`; SHM_STAT_ANY skips
    /// the read-permission test.
    pub fn stat_index(&self, idx: usize, who: &Caller, any: bool) -> Result<(i32, [u8; SHMID_DS_LEN]), i64> {
        let s = self.segs.get(idx).ok_or(-EINVAL)?;
        if !any && !permitted(s, who, 0o444) {
            return Err(-EACCES);
        }
        Ok((s.id, s.to_ds()))
    }

    /// Highest table index in use (what IPC_INFO and SHM_INFO return).
    pub fn max_index(&self) -> i64 {
        self.segs.len().saturating_sub(1) as i64
    }

    /// SHM_LOCK / SHM_UNLOCK: root only, and there is nothing to lock.
    pub fn lock(&self, id: i32, who: &Caller) -> Result<(), i64> {
        self.idx(id)?;
        if who.euid != 0 {
            return Err(-EPERM);
        }
        Ok(())
    }
}

/// `struct shminfo`, the IPC_INFO answer.
pub fn shminfo_bytes() -> [u8; SHMINFO_LEN] {
    let mut b = [0u8; SHMINFO_LEN];
    b[0..8].copy_from_slice(&SHMMAX.to_le_bytes());
    b[8..16].copy_from_slice(&SHMMIN.to_le_bytes());
    b[16..24].copy_from_slice(&(SHMMNI as u64).to_le_bytes());
    b[24..32].copy_from_slice(&SHMSEG.to_le_bytes());
    b[32..40].copy_from_slice(&SHMALL.to_le_bytes());
    b
}

/// `struct shm_info`, the SHM_INFO answer: ids in use, pages held, pages
/// resident (the same: there is no swap), swapped (none).
pub fn shm_info_bytes<B: Clone>(t: &ShmTable<B>) -> [u8; SHM_INFO_LEN] {
    let mut b = [0u8; SHM_INFO_LEN];
    b[0..4].copy_from_slice(&(t.len() as i32).to_le_bytes());
    let pages = t.total_pages();
    b[8..16].copy_from_slice(&pages.to_le_bytes());
    b[16..24].copy_from_slice(&pages.to_le_bytes());
    b
}
