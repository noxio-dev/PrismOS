// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// ext4 driver (phase 1.4, hardened in 1.5): mounts an ext2/3/4 volume
// found on a `BlockDev` and exposes it through the VFS `FileSystem` trait.
//
// What is supported
//   * Reading: everything common mke2fs/Linux produce -- extents or classic
//     block maps, 32/64-bit group descriptors, flex_bg, uninit_bg /
//     metadata_csum volumes (checksums verified on read since phase 1.5),
//     htree directories (scanned linearly, maintained on insert since
//     phase 1.5), fast and slow symlinks, sparse files.
//   * Writing: needs EXTENTS.  Create, write, truncate, mkdir, rmdir,
//     unlink, rename, link, symlink, chmod, fallocate-style preallocation.
//     metadata_csum and uninit_bg checksums are kept up to date.
//   * Journal (phase 1.5): a dirty jbd2 journal is replayed at mount (the
//     volume then mounts read-write), and every operation's metadata
//     changes are written as one jbd2 transaction -- see journal.rs.
//     Volumes whose journal uses features this driver does not know stay
//     read-only.
//   * Refused: inline_data, bigalloc, meta_bg, encryption, casefold, MMP,
//     EA inodes.  Quotas / verity / project IDs force a read-only mount.
//
// Block cache (phase 1.5): one cache for metadata *and* file data, with
// write-back.  File data is flushed to the device at the end of every
// operation (before the journal commit block: ordered mode); metadata
// stays dirty until the journal checkpoint (`sync`, the periodic flush,
// umount, or a full journal).
//
// Layout: mod.rs (mount, caches, inodes, commit) / balloc.rs (group
// descriptors, bitmaps, allocation) / extent.rs (extent trees, file data)
// / dir.rs (directory blocks) / htree.rs (dx indexing) / journal.rs
// (jbd2) / ops.rs (namespace operations) / disk.rs, crc.rs.

#![allow(dead_code)]

mod balloc;
pub mod crc;
mod dir;
mod disk;
pub mod extent;
pub mod htree;
mod journal;
mod ops;

use crate::hw::blk::{BlockDev, SECTOR};
use crate::fs::vfs::{now, DirEntry, FileSystem, FsError, FsResult, Stat, StatFs};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;
use crc::crc32c;
use disk::*;
use extent::ExtTree;
use journal::Journal;

/// One cached block.  `kind` separates file data (flushed before every
/// journal commit, ordered mode) from metadata (protected by the journal).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlkKind {
    Meta,
    Data,
}

struct Cache {
    map: BTreeMap<u64, (Vec<u8>, BlkKind)>,
    dirty: BTreeSet<u64>,
    cap: usize,
}

pub struct Ext4 {
    dev: Box<dyn BlockDev>,
    bs: usize,
    /// Sectors per filesystem block.
    spb: u64,
    g: Geo,
    ro: bool,
    sb_raw: Vec<u8>,
    free_blocks: u64,
    free_inodes: u32,
    sb_dirty: bool,
    /// Group descriptor table, `gdt_blocks` blocks long.
    gdt: Vec<u8>,
    gdt_dirty: BTreeSet<usize>,
    cache: Cache,
    /// The extent tree of the inode touched last (reads are sequential).
    ext_cache: Option<(u32, ExtTree)>,
    gen_counter: u32,
    /// Metadata checksums verified / failed since mount (phase 1.5).
    csum_checked: Cell<u32>,
    csum_errors: Cell<u32>,
    /// Journal state (phase 1.5): None when the volume has no journal or
    /// it cannot be used (the mount is then read-only when dirty).
    jrn: Option<Journal>,
}

impl Ext4 {
    /// Mount read-write when the volume allows it, read-only otherwise.
    pub fn mount(dev: Box<dyn BlockDev>) -> FsResult<Ext4> {
        Self::do_mount(dev, false)
    }

    /// Mount read-only no matter what.
    pub fn mount_ro(dev: Box<dyn BlockDev>) -> FsResult<Ext4> {
        Self::do_mount(dev, true)
    }

    /// Cheap probe: does `dev` start with an ext2/3/4 superblock?
    pub fn probe(dev: &dyn BlockDev) -> bool {
        let mut sb = [0u8; SB_SIZE];
        dev.read(SB_OFFSET / SECTOR as u64, &mut sb).is_ok() && le16(&sb, 0x38) == MAGIC
    }

    fn do_mount(dev: Box<dyn BlockDev>, force_ro: bool) -> FsResult<Ext4> {
        let mut sb = vec![0u8; SB_SIZE];
        dev.read(SB_OFFSET / SECTOR as u64, &mut sb)?;
        if le16(&sb, 0x38) != MAGIC {
            return Err(FsError::Invalid);
        }
        let log = le32(&sb, 0x18);
        if log > 6 {
            return Err(FsError::Unsupported);
        }
        let bs = 1024usize << log;
        let rev = le32(&sb, 0x4C);
        let (inode_size, first_ino) = if rev >= 1 { (le16(&sb, 0x58) as usize, le32(&sb, 0x54)) } else { (128, 11) };
        let (compat, incompat, ro_compat) =
            if rev >= 1 { (le32(&sb, 0x5C), le32(&sb, 0x60), le32(&sb, 0x64)) } else { (0, 0, 0) };
        if incompat & !INCOMPAT_SUPPORTED != 0 || ro_compat & RO_BIGALLOC != 0 {
            return Err(FsError::Unsupported);
        }
        let is64 = incompat & INCOMPAT_64BIT != 0;
        let desc_size = if is64 {
            let d = le16(&sb, 0xFE) as usize;
            if d < 64 || d > 1024 || !d.is_power_of_two() {
                return Err(FsError::Corrupt);
            }
            d
        } else {
            32
        };
        let bpg = le32(&sb, 0x20);
        let ipg = le32(&sb, 0x28);
        let first_data = le32(&sb, 0x14);
        let blocks = le32(&sb, 4) as u64 | if is64 { (le32(&sb, 0x150) as u64) << 32 } else { 0 };
        let inodes = le32(&sb, 0);
        if bpg == 0
            || bpg as usize > bs * 8
            || ipg == 0
            || ipg as usize > bs * 8
            || !inode_size.is_power_of_two()
            || inode_size < INODE_GOOD_OLD
            || inode_size > bs
            || blocks <= first_data as u64
            || blocks > dev.sectors() / (bs / SECTOR) as u64
        {
            return Err(FsError::Corrupt);
        }
        let groups = ((blocks - first_data as u64) + bpg as u64 - 1) / bpg as u64;
        if groups == 0 || groups > u32::MAX as u64 || groups * ipg as u64 != inodes as u64 {
            return Err(FsError::Corrupt);
        }
        let groups = groups as u32;
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&sb[0x68..0x78]);
        let metadata_csum = ro_compat & RO_METADATA_CSUM != 0;
        let csum_seed = if incompat & INCOMPAT_CSUM_SEED != 0 { le32(&sb, 0x270) } else { crc32c(!0, &uuid) };
        let hash_seed = [le32(&sb, 0xEC), le32(&sb, 0xF0), le32(&sb, 0xF4), le32(&sb, 0xF8)];
        let hash_version = sb[0xFC];
        let dir_index = compat & COMPAT_DIR_INDEX != 0;
        let gdt_blocks = ((groups as usize * desc_size) + bs - 1) / bs;
        let itable_blocks = ((ipg as usize * inode_size + bs - 1) / bs) as u32;
        let geo = Geo {
            bs,
            blocks,
            inodes,
            bpg,
            ipg,
            first_data,
            first_ino,
            inode_size,
            desc_size,
            groups,
            compat,
            incompat,
            ro_compat,
            reserved_gdt: le16(&sb, 0xCE) as u32,
            uuid,
            csum_seed,
            backup_bgs: [le32(&sb, 0x24C), le32(&sb, 0x250)],
            itable_blocks,
            gdt_blocks: gdt_blocks as u32,
            metadata_csum,
            gdt_csum: ro_compat & RO_GDT_CSUM != 0 && !metadata_csum,
            filetype: incompat & INCOMPAT_FILETYPE != 0,
            hash_seed,
            hash_version,
            dir_index,
        };

        let state = le16(&sb, 0x3A);

        let spb = (bs / SECTOR) as u64;
        let gdt_blk = first_data as u64 + 1;
        let mut gdt = vec![0u8; gdt_blocks * bs];
        if gdt_blk + gdt_blocks as u64 > blocks {
            return Err(FsError::Corrupt);
        }
        dev.read(gdt_blk * spb, &mut gdt)?;

        let cap = core::cmp::max(256, (6 << 20) / bs);
        let mut fs = Ext4 {
            dev,
            bs,
            spb,
            g: geo,
            ro: true,
            sb_raw: sb,
            free_blocks: 0,
            free_inodes: 0,
            sb_dirty: false,
            gdt,
            gdt_dirty: BTreeSet::new(),
            cache: Cache { map: BTreeMap::new(), dirty: BTreeSet::new(), cap },
            ext_cache: None,
            gen_counter: 0x9E37_79B9,
            csum_checked: Cell::new(0),
            csum_errors: Cell::new(0),
            jrn: None,
        };
        // Superblock checksum (when metadata_csum is on).
        if metadata_csum {
            let want = le32(&fs.sb_raw, 0x3FC);
            let got = crc32c(!0, &fs.sb_raw[..0x3FC]);
            fs.note_csum(want == got);
            if want != got {
                return Err(FsError::Corrupt);
            }
        }
        // Sanity check the descriptors (verifying their checksums) and
        // total the free counters (the group descriptors are the
        // reference, like the Linux driver does).
        let (mut fb, mut fi) = (0u64, 0u64);
        for g in 0..groups {
            for blk in [fs.gd_block_bitmap(g), fs.gd_inode_bitmap(g), fs.gd_inode_table(g)] {
                if blk < first_data as u64 || blk + 1 > blocks {
                    return Err(FsError::Corrupt);
                }
            }
            if fs.gd_inode_table(g) + itable_blocks as u64 > blocks {
                return Err(FsError::Corrupt);
            }
            if metadata_csum || fs.g.gdt_csum {
                let want = fs.gd16(g, 0x1E);
                fs.note_csum(want == fs.gd_csum(g));
            }
            fb += fs.gd_free_blocks(g) as u64;
            fi += fs.gd_free_inodes(g) as u64;
        }
        fs.free_blocks = fb;
        fs.free_inodes = fi as u32;
        // The root must be a directory.
        let root = fs.read_inode(ROOT_INO)?;
        if !root.is_dir() {
            return Err(FsError::Corrupt);
        }

        // --- mount decision + journal replay (phase 1.5) -----------------
        // With a usable journal, a dirty volume (INCOMPAT_RECOVER or a
        // non-zero s_start) is replayed and then mounts read-write; the
        // clean bit alone no longer forces read-only.  Without a journal
        // the old rules apply.
        let base_ro = force_ro
            || incompat & INCOMPAT_EXTENTS == 0
            || ro_compat & !RO_RW_SAFE != 0
            || state & 2 != 0;
        let dirty = incompat & INCOMPAT_RECOVER != 0 || state & 1 == 0;
        let sb_copy = fs.sb_raw.clone();
        let jrn_result = Journal::load(&mut fs, &sb_copy);
        match jrn_result {
            Some(Ok(mut j)) => {
                let j_dirty = j.start != 0;
                if base_ro {
                    // Even read-only mounts with a *clean* journal are fine;
                    // a dirty one is not replayed (like the Linux driver).
                    fs.jrn = Some(j);
                    fs.ro = true;
                } else if dirty || j_dirty {
                    // Replay (a clean journal with RECOVER set just clears
                    // the flag).  Afterwards the log is consumed: reset it
                    // so the next mount does not replay again (idempotent
                    // anyway, but this keeps s_start honest).
                    match j.replay(&mut fs) {
                        Ok(n) => {
                            let _ = n;
                            let _ = j.checkpoint_done(&mut fs);
                            fs.jrn = Some(j);
                            fs.ro = false;
                            mark_fs_in_use(&mut fs, incompat);
                        }
                        Err(_) => {
                            fs.jrn = Some(j);
                            fs.ro = true;
                        }
                    }
                } else {
                    fs.jrn = Some(j);
                    fs.ro = false;
                    mark_fs_in_use(&mut fs, incompat);
                }
            }
            Some(Err(_)) => {
                // Journal with unsupported features: keep it, stay safe.
                fs.ro = true;
            }
            None => {
                fs.ro = base_ro || dirty;
            }
        }
        Ok(fs)
    }

    /// Hand the device back (after `sync`).
    pub fn into_dev(self) -> Box<dyn BlockDev> {
        self.dev
    }

    pub fn block_size(&self) -> usize {
        self.bs
    }

    /// Checksum statistics for `mount -v` / `df`: (verified, failed).
    pub fn csum_stats(&self) -> (u32, u32) {
        (self.csum_checked.get(), self.csum_errors.get())
    }

    fn note_csum(&self, ok: bool) {
        self.csum_checked.set(self.csum_checked.get() + 1);
        if !ok {
            self.csum_errors.set(self.csum_errors.get() + 1);
        }
    }

    pub fn volume_info(&self) -> VolumeInfo {
        VolumeInfo {
            block_size: self.bs as u32,
            blocks: self.g.blocks,
            free_blocks: self.free_blocks,
            inodes: self.g.inodes,
            free_inodes: self.free_inodes,
            groups: self.g.groups,
            journal: self.g.compat & COMPAT_HAS_JOURNAL != 0,
            needs_recovery: self.g.incompat & INCOMPAT_RECOVER != 0,
            metadata_csum: self.g.metadata_csum,
            extents: self.g.incompat & INCOMPAT_EXTENTS != 0,
            is64: self.g.incompat & INCOMPAT_64BIT != 0,
            read_only: self.ro,
            label: self.label(),
        }
    }

    fn label(&self) -> String {
        let l = &self.sb_raw[0x78..0x88];
        let end = l.iter().position(|&b| b == 0).unwrap_or(l.len());
        String::from_utf8_lossy(&l[..end]).into_owned()
    }

    // ----------------------------------------------------------- block I/O

    fn cache_insert(&mut self, blk: u64, data: Vec<u8>, dirty: bool, data_kind: bool) {
        if !self.cache.map.contains_key(&blk) && self.cache.map.len() >= self.cache.cap {
            let victim = self.cache.map.keys().find(|k| !self.cache.dirty.contains(k)).copied();
            if let Some(v) = victim {
                self.cache.map.remove(&v);
            }
        }
        let kind = if data_kind { BlkKind::Data } else { BlkKind::Meta };
        self.cache.map.insert(blk, (data, kind));
        if dirty {
            self.cache.dirty.insert(blk);
        }
    }

    /// Read one block through the cache.
    fn bread(&mut self, blk: u64) -> FsResult<Vec<u8>> {
        if blk >= self.g.blocks {
            return Err(FsError::Corrupt);
        }
        if let Some((b, _)) = self.cache.map.get(&blk) {
            return Ok(b.clone());
        }
        let mut buf = vec![0u8; self.bs];
        self.dev.read(blk * self.spb, &mut buf)?;
        self.cache_insert(blk, buf.clone(), false, false);
        Ok(buf)
    }

    /// Read a block that is known to be a directory leaf of `dir`,
    /// verifying its checksum first (phase 1.5).
    pub(super) fn bread_dir(&mut self, dir: &Inode, blk: u64) -> FsResult<Vec<u8>> {
        let b = self.bread(blk)?;
        if self.g.metadata_csum && dir_uses_tail(&b) {
            let n = b.len();
            let want = le32(&b, n - 4);
            let got = crc32c(self.inode_seed(dir.ino, dir.generation()), &b[..n - 12]);
            self.note_csum(want == got);
            if want != got {
                return Err(FsError::Corrupt);
            }
        }
        Ok(b)
    }

    /// Replace a metadata block; it reaches the disk at the next commit
    /// (or, with a journal, at the next checkpoint).
    fn bwrite(&mut self, blk: u64, data: Vec<u8>) -> FsResult<()> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        if blk >= self.g.blocks || data.len() != self.bs {
            return Err(FsError::Corrupt);
        }
        self.cache_insert(blk, data, true, false);
        Ok(())
    }

    /// Replace a file-data block (write-back; flushed at the commit, or
    /// earlier when the cache is full).
    fn bwrite_data(&mut self, blk: u64, data: Vec<u8>) -> FsResult<()> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        if blk >= self.g.blocks || data.len() != self.bs {
            return Err(FsError::Corrupt);
        }
        if self.cache.map.len() >= self.cache.cap && !self.cache.map.contains_key(&blk) {
            self.flush_data()?;
        }
        self.cache_insert(blk, data, true, true);
        Ok(())
    }

    /// Read `buf.len() / bs` consecutive blocks, bypassing the cache.
    /// Used by the journal and for uncached bulk reads.
    fn read_raw(&mut self, blk: u64, buf: &mut [u8]) -> FsResult<()> {
        let n = (buf.len() / self.bs) as u64;
        if blk.checked_add(n).map_or(true, |e| e > self.g.blocks) {
            return Err(FsError::Corrupt);
        }
        self.dev.read(blk * self.spb, buf)?;
        Ok(())
    }

    /// Write consecutive blocks straight to the device, dropping them from
    /// the cache (used by the journal only).
    fn write_raw(&mut self, blk: u64, buf: &[u8]) -> FsResult<()> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        let n = (buf.len() / self.bs) as u64;
        if blk.checked_add(n).map_or(true, |e| e > self.g.blocks) {
            return Err(FsError::Corrupt);
        }
        for b in blk..blk + n {
            self.cache.map.remove(&b);
            self.cache.dirty.remove(&b);
        }
        self.dev.write(blk * self.spb, buf)?;
        Ok(())
    }

    /// Push every dirty file-data block to the device (ordered mode: this
    /// happens before a journal commit block is written).  Consecutive
    /// blocks are batched into single device writes.
    fn flush_data(&mut self) -> FsResult<()> {
        if self.ro {
            return Ok(());
        }
        let data_blks: Vec<u64> = self
            .cache
            .dirty
            .iter()
            .copied()
            .filter(|b| self.cache.map.get(b).map_or(false, |(_, k)| *k == BlkKind::Data))
            .collect();
        self.write_runs(&data_blks)?;
        for b in &data_blks {
            self.cache.dirty.remove(b);
        }
        if !data_blks.is_empty() {
            self.dev.flush()?;
        }
        Ok(())
    }

    /// Write a sorted list of blocks to the device, merging consecutive
    /// ones into a single write.
    fn write_runs(&mut self, blks: &[u64]) -> FsResult<()> {
        let mut i = 0usize;
        while i < blks.len() {
            let mut j = i + 1;
            while j < blks.len() && blks[j] == blks[j - 1] + 1 {
                j += 1;
            }
            let n = j - i;
            if n == 1 {
                if let Some((data, _)) = self.cache.map.get(&blks[i]) {
                    let d = data.clone();
                    self.dev.write(blks[i] * self.spb, &d)?;
                }
            } else {
                let mut buf = Vec::with_capacity(n * self.bs);
                for b in &blks[i..j] {
                    if let Some((data, _)) = self.cache.map.get(b) {
                        buf.extend_from_slice(data);
                    } else {
                        return Err(FsError::Corrupt);
                    }
                }
                self.dev.write(blks[i] * self.spb, &buf)?;
            }
            i = j;
        }
        Ok(())
    }

    /// The metadata image of every dirty structure, in a stable order:
    /// [(block, bytes)] for the journal transaction.
    fn meta_dirty_blocks(&mut self) -> Vec<(u64, Vec<u8>)> {
        let gdt_blk = self.g.first_data as u64 + 1;
        let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
        let idxs: Vec<usize> = self.gdt_dirty.iter().copied().collect();
        for i in idxs {
            out.push((gdt_blk + i as u64, self.gdt[i * self.bs..(i + 1) * self.bs].to_vec()));
        }
        let dirty: Vec<u64> = self.cache.dirty.iter().copied().collect();
        for b in dirty {
            if let Some((data, BlkKind::Meta)) = self.cache.map.get(&b) {
                out.push((b, data.clone()));
            }
        }
        if self.sb_dirty {
            // The journal stores whole *disk blocks*: the superblock lives
            // at byte 1024, so the image is the block with the sb copied
            // in at its real offset (the whole block for bs = 1024).
            let blk = SB_OFFSET as u64 / self.bs as u64;
            let sbimg = self.sb_image();
            let mut img = vec![0u8; self.bs];
            let off = (SB_OFFSET % self.bs as u64) as usize;
            let n = SB_SIZE.min(self.bs - off);
            img[off..off + n].copy_from_slice(&sbimg[..n]);
            out.push((blk, img));
        }
        out
    }

    /// Write every pending metadata change straight to the device and
    /// flush: the checkpoint of the journal (or the whole commit when the
    /// volume has none).
    fn checkpoint(&mut self) -> FsResult<()> {
        if self.ro {
            return Ok(());
        }
        let gdt_blk = self.g.first_data as u64 + 1;
        let idxs: Vec<usize> = self.gdt_dirty.iter().copied().collect();
        for i in idxs {
            let slice = &self.gdt[i * self.bs..(i + 1) * self.bs];
            self.dev.write((gdt_blk + i as u64) * self.spb, slice)?;
        }
        self.gdt_dirty.clear();
        let dirty: Vec<u64> = self.cache.dirty.iter().copied().collect();
        self.write_runs(&dirty)?;
        for b in dirty {
            self.cache.dirty.remove(&b);
        }
        if self.sb_dirty {
            let img = self.sb_image();
            self.dev.write(SB_OFFSET / SECTOR as u64, &img)?;
            self.sb_dirty = false;
        }
        if self.jrn.is_some() {
            let mut j = self.jrn.take().unwrap();
            let r = j.checkpoint_done(self);
            let _ = self.jrn.insert(j);
            r?;
            // The journal is empty and everything is written: the volume
            // is "cleanly unmounted" again (the mount marker is undone).
            put16(&mut self.sb_raw, 0x3A, 1);
            let incompat = self.g.incompat & !INCOMPAT_RECOVER;
            put32(&mut self.sb_raw, 0x60, incompat);
            self.g.incompat = incompat;
            if self.g.metadata_csum {
                let c = crc32c(!0, &self.sb_raw[..0x3FC]);
                put32(&mut self.sb_raw, 0x3FC, c);
            }
            self.dev.write(SB_OFFSET / SECTOR as u64, &self.sb_raw)?;
            self.sb_dirty = false;
        }
        self.dev.flush()?;
        Ok(())
    }

    /// Current superblock image with the live counters, time and checksum.
    fn sb_image(&mut self) -> Vec<u8> {
        let t = now();
        let fb = self.free_blocks;
        let fi = self.free_inodes;
        let is64 = self.g.incompat & INCOMPAT_64BIT != 0;
        let csum = self.g.metadata_csum;
        let sb = &mut self.sb_raw;
        put32(sb, 0x0C, fb as u32);
        if is64 {
            put32(sb, 0x158, (fb >> 32) as u32);
        }
        put32(sb, 0x10, fi);
        if t != 0 {
            put32(sb, 0x30, t);
        }
        if csum {
            let c = crc32c(!0, &sb[..0x3FC]);
            put32(sb, 0x3FC, c);
        }
        sb.clone()
    }

    fn finish_sb(&mut self) {
        if !self.sb_dirty {
            return;
        }
        let img = self.sb_image();
        self.sb_raw = img;
    }

    /// End of an operation: file data reaches the device first (ordered
    /// mode), then the metadata goes into one journal transaction -- or,
    /// without a journal, straight to the disk.
    fn commit(&mut self) -> FsResult<()> {
        if self.ro {
            return Ok(());
        }
        if self.jrn.is_some() {
            // Ordered mode: the data of this operation hits the disk
            // before the commit block can make it live.
            self.flush_data()?;
            let blocks = self.meta_dirty_blocks();
            if blocks.is_empty() {
                return Ok(());
            }
            let mut j = self.jrn.take().unwrap();
            let r = j.commit_txn(self, &blocks);
            let _ = self.jrn.insert(j);
            r
        } else {
            self.checkpoint()
        }
    }

    /// Commit, keeping the first error.
    fn finish<T>(&mut self, r: FsResult<T>) -> FsResult<T> {
        let c = self.commit();
        match (r, c) {
            (Ok(v), Ok(())) => Ok(v),
            (Err(e), _) => Err(e),
            (Ok(_), Err(e)) => Err(e),
        }
    }

    // -------------------------------------------------------------- inodes

    fn inode_loc(&self, ino: u32) -> FsResult<(u64, usize)> {
        if ino == 0 || ino > self.g.inodes {
            return Err(FsError::Corrupt);
        }
        let g = (ino - 1) / self.g.ipg;
        let idx = ((ino - 1) % self.g.ipg) as u64;
        let byte = idx * self.g.inode_size as u64;
        Ok((self.gd_inode_table(g) + byte / self.bs as u64, (byte % self.bs as u64) as usize))
    }

    fn read_inode(&mut self, ino: u32) -> FsResult<Inode> {
        let (blk, off) = self.inode_loc(ino)?;
        let b = self.bread(blk)?;
        let raw = b[off..off + self.g.inode_size].to_vec();
        // Verify the inode checksum (phase 1.5).
        if self.g.metadata_csum {
            let want = (le16(&raw, 0x7C) as u32) | if raw.len() > INODE_GOOD_OLD && le16(&raw, 0x80) >= 4 {
                (le16(&raw, 0x82) as u32) << 16
            } else {
                0
            };
            let got = self.inode_csum(&raw, ino);
            self.note_csum(want == got);
            if want != got {
                return Err(FsError::Corrupt);
            }
        }
        Ok(Inode { ino, raw })
    }

    /// Seed for the per-inode checksums (inode, extent blocks, dir blocks).
    fn inode_seed(&self, ino: u32, generation: u32) -> u32 {
        crc32c(crc32c(self.g.csum_seed, &ino.to_le_bytes()), &generation.to_le_bytes())
    }

    fn inode_csum(&self, raw: &[u8], ino: u32) -> u32 {
        let isz = raw.len();
        let mut c = self.inode_seed(ino, le32(raw, 0x64));
        c = crc32c(c, &raw[..0x7C]);
        c = crc32c(c, &[0, 0]);
        c = crc32c(c, &raw[0x7E..INODE_GOOD_OLD]);
        if isz > INODE_GOOD_OLD {
            c = crc32c(c, &raw[INODE_GOOD_OLD..0x82]);
            if le16(raw, 0x80) >= 4 {
                c = crc32c(c, &[0, 0]);
                c = crc32c(c, &raw[0x84..isz]);
            } else {
                c = crc32c(c, &raw[0x82..isz]);
            }
        }
        c
    }

    fn write_inode(&mut self, inode: &mut Inode) -> FsResult<()> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        if self.g.metadata_csum {
            let c = self.inode_csum(&inode.raw, inode.ino);
            put16(&mut inode.raw, 0x7C, c as u16);
            if inode.raw.len() > INODE_GOOD_OLD && le16(&inode.raw, 0x80) >= 4 {
                put16(&mut inode.raw, 0x82, (c >> 16) as u16);
            }
        }
        let (blk, off) = self.inode_loc(inode.ino)?;
        let mut b = self.bread(blk)?;
        b[off..off + self.g.inode_size].copy_from_slice(&inode.raw);
        self.bwrite(blk, b)
    }

    fn next_generation(&mut self) -> u32 {
        self.gen_counter = self.gen_counter.wrapping_mul(1664525).wrapping_add(1013904223) ^ now();
        self.gen_counter
    }

    fn stat_of(&self, i: &Inode) -> Stat {
        Stat {
            ino: i.ino as u64,
            ftype: file_type_of(i.mode()),
            mode: i.mode() & 0o7777,
            size: i.size(),
            nlink: i.links() as u32,
            uid: i.uid(),
            gid: i.gid(),
            atime: i.atime(),
            mtime: i.mtime(),
            ctime: i.ctime(),
            blocks: i.blocks512(),
        }
    }
}

/// Mark the volume "in use" on the device at RW mount with a journal, the
/// way the Linux driver does: state = not-clean and INCOMPAT_RECOVER set,
/// so a crash forces the next mount (or e2fsck) to replay the journal.
/// `incompat` is the on-disk feature set read at mount.
fn mark_fs_in_use(fs: &mut Ext4, incompat: u32) {
    let inc = incompat | INCOMPAT_RECOVER;
    put16(&mut fs.sb_raw, 0x3A, 0); // not cleanly unmounted
    put32(&mut fs.sb_raw, 0x60, inc);
    fs.g.incompat = inc;
    if fs.g.metadata_csum {
        let c = crc32c(!0, &fs.sb_raw[..0x3FC]);
        put32(&mut fs.sb_raw, 0x3FC, c);
    }
    let _ = fs.dev.write(SB_OFFSET / SECTOR as u64, &fs.sb_raw);
    fs.sb_dirty = false; // the on-disk copy is current
}

/// Does this dirent block carry the metadata_csum tail?
fn dir_uses_tail(b: &[u8]) -> bool {
    b.len() >= 12 && {
        let o = b.len() - 12;
        le32(b, o) == 0 && le16(b, o + 4) == 12 && b[o + 6] == 0 && b[o + 7] == FT_TAIL
    }
}

/// Summary for the `mount`/`df` style shell commands.
#[derive(Clone, Debug)]
pub struct VolumeInfo {
    pub block_size: u32,
    pub blocks: u64,
    pub free_blocks: u64,
    pub inodes: u32,
    pub free_inodes: u32,
    pub groups: u32,
    pub journal: bool,
    pub needs_recovery: bool,
    pub metadata_csum: bool,
    pub extents: bool,
    pub is64: bool,
    pub read_only: bool,
    pub label: String,
}

fn ino32(ino: u64) -> FsResult<u32> {
    u32::try_from(ino).map_err(|_| FsError::Invalid)
}

impl FileSystem for Ext4 {
    fn fs_name(&self) -> &'static str {
        if self.g.incompat & INCOMPAT_EXTENTS != 0 {
            "ext4"
        } else if self.g.compat & COMPAT_HAS_JOURNAL != 0 {
            "ext3"
        } else {
            "ext2"
        }
    }

    fn root(&self) -> u64 {
        ROOT_INO as u64
    }

    fn read_only(&self) -> bool {
        self.ro
    }

    fn statfs(&mut self) -> FsResult<StatFs> {
        Ok(StatFs {
            block_size: self.bs as u32,
            blocks: self.g.blocks,
            free_blocks: self.free_blocks,
            files: self.g.inodes as u64,
            free_files: self.free_inodes as u64,
            name_max: 255,
        })
    }

    fn stat(&mut self, ino: u64) -> FsResult<Stat> {
        let i = self.read_inode(ino32(ino)?)?;
        Ok(self.stat_of(&i))
    }

    fn lookup(&mut self, dir: u64, name: &str) -> FsResult<u64> {
        self.op_lookup(ino32(dir)?, name).map(|i| i as u64)
    }

    fn readdir(&mut self, dir: u64) -> FsResult<Vec<DirEntry>> {
        self.op_readdir(ino32(dir)?)
    }

    fn read(&mut self, ino: u64, off: u64, buf: &mut [u8]) -> FsResult<usize> {
        let inode = self.read_inode(ino32(ino)?)?;
        if inode.is_dir() {
            return Err(FsError::IsDir);
        }
        self.file_read(&inode, off, buf)
    }

    fn readlink(&mut self, ino: u64) -> FsResult<String> {
        self.op_readlink(ino32(ino)?)
    }

    fn write(&mut self, ino: u64, off: u64, data: &[u8]) -> FsResult<usize> {
        let r = self.file_write(ino32(ino)?, off, data);
        self.finish(r)
    }

    fn truncate(&mut self, ino: u64, size: u64) -> FsResult<()> {
        let r = self.file_truncate(ino32(ino)?, size);
        self.finish(r)
    }

    fn create(&mut self, dir: u64, name: &str, mode: u16) -> FsResult<u64> {
        let r = self.op_create(ino32(dir)?, name, mode);
        self.finish(r).map(|i| i as u64)
    }

    fn mkdir(&mut self, dir: u64, name: &str, mode: u16) -> FsResult<u64> {
        let r = self.op_mkdir(ino32(dir)?, name, mode);
        self.finish(r).map(|i| i as u64)
    }

    fn symlink(&mut self, dir: u64, name: &str, target: &str) -> FsResult<u64> {
        let r = self.op_symlink(ino32(dir)?, name, target);
        self.finish(r).map(|i| i as u64)
    }

    fn link(&mut self, dir: u64, name: &str, ino: u64) -> FsResult<()> {
        let r = self.op_link(ino32(dir)?, name, ino32(ino)?);
        self.finish(r)
    }

    fn unlink(&mut self, dir: u64, name: &str) -> FsResult<()> {
        let r = self.op_unlink(ino32(dir)?, name);
        self.finish(r)
    }

    fn rmdir(&mut self, dir: u64, name: &str) -> FsResult<()> {
        let r = self.op_rmdir(ino32(dir)?, name);
        self.finish(r)
    }

    fn rename(&mut self, odir: u64, oname: &str, ndir: u64, nname: &str) -> FsResult<()> {
        let r = self.op_rename(ino32(odir)?, oname, ino32(ndir)?, nname);
        self.finish(r)
    }

    fn chmod(&mut self, ino: u64, mode: u16) -> FsResult<()> {
        let r = self.op_chmod(ino32(ino)?, mode);
        self.finish(r)
    }

    /// fallocate-style preallocation: unwritten extents covering
    /// [start, start+len); the size grows past EOF unless keep_size.
    fn fallocate(&mut self, ino: u64, start: u64, len: u64, keep_size: bool) -> FsResult<()> {
        let r = self.file_fallocate(ino32(ino)?, start, len, keep_size);
        self.finish(r)
    }

    fn sync(&mut self) -> FsResult<()> {
        self.commit()?;
        self.checkpoint()?;
        if !self.ro {
            self.dev.flush()?;
        }
        Ok(())
    }
}
