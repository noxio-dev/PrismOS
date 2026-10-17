// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// ext4 driver (phase 1.4): mounts an ext2/3/4 volume found on a `BlockDev`
// and exposes it through the VFS `FileSystem` trait.
//
// What is supported
//   * Reading: everything common mke2fs/Linux produce -- extents or classic
//     block maps, 32/64-bit group descriptors, flex_bg, uninit_bg /
//     metadata_csum volumes (checksums are not verified on read), htree
//     directories (scanned linearly), fast and slow symlinks, sparse files.
//   * Writing: needs EXTENTS and a cleanly unmounted journal (the journal is
//     never replayed, so `needs_recovery` volumes mount read-only).  Create,
//     write, truncate, mkdir, rmdir, unlink, rename, link, symlink, chmod.
//     metadata_csum and uninit_bg checksums are kept up to date.
//   * Refused: inline_data, bigalloc, meta_bg, encryption, casefold, MMP,
//     EA inodes.  Quotas / verity / project IDs force a read-only mount.
//
// Writes go straight to the device at the end of every operation (there is
// no journal): a crash in the middle of an operation can leave the volume
// needing an fsck, but nothing is cached across operations.
//
// Layout: mod.rs (mount, caches, inodes) / balloc.rs (group descriptors,
// bitmaps, allocation) / extent.rs (extent trees, file data) / dir.rs
// (directory blocks) / ops.rs (namespace operations) / disk.rs, crc.rs.

#![allow(dead_code)]

mod balloc;
mod crc;
mod dir;
mod disk;
mod extent;
mod ops;

use crate::blk::{BlockDev, SECTOR};
use crate::vfs::{now, DirEntry, FileSystem, FsError, FsResult, Stat, StatFs};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use crc::crc32c;
use disk::*;
use extent::ExtTree;

/// Metadata block cache.  Entries are clean copies of what is on disk or
/// dirty blocks waiting for the end of the current operation.
struct Cache {
    map: BTreeMap<u64, Vec<u8>>,
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
        };

        let state = le16(&sb, 0x3A);
        let ro = force_ro
            || incompat & INCOMPAT_RECOVER != 0
            || incompat & INCOMPAT_EXTENTS == 0
            || ro_compat & !RO_RW_SAFE != 0
            || state & 1 == 0
            || state & 2 != 0;

        let spb = (bs / SECTOR) as u64;
        let gdt_blk = first_data as u64 + 1;
        let mut gdt = vec![0u8; gdt_blocks * bs];
        if gdt_blk + gdt_blocks as u64 > blocks {
            return Err(FsError::Corrupt);
        }
        dev.read(gdt_blk * spb, &mut gdt)?;

        let cap = core::cmp::max(16, (1 << 20) / bs);
        let mut fs = Ext4 {
            dev,
            bs,
            spb,
            g: geo,
            ro,
            sb_raw: sb,
            free_blocks: 0,
            free_inodes: 0,
            sb_dirty: false,
            gdt,
            gdt_dirty: BTreeSet::new(),
            cache: Cache { map: BTreeMap::new(), dirty: BTreeSet::new(), cap },
            ext_cache: None,
            gen_counter: 0x9E37_79B9,
        };
        // Sanity check the descriptors and total the free counters (the
        // group descriptors are the reference, like the Linux driver does).
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
        Ok(fs)
    }

    /// Hand the device back (after `sync`).
    pub fn into_dev(self) -> Box<dyn BlockDev> {
        self.dev
    }

    pub fn block_size(&self) -> usize {
        self.bs
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

    fn cache_insert(&mut self, blk: u64, data: Vec<u8>, dirty: bool) {
        if !self.cache.map.contains_key(&blk) && self.cache.map.len() >= self.cache.cap {
            let victim = self.cache.map.keys().find(|k| !self.cache.dirty.contains(k)).copied();
            if let Some(v) = victim {
                self.cache.map.remove(&v);
            }
        }
        self.cache.map.insert(blk, data);
        if dirty {
            self.cache.dirty.insert(blk);
        }
    }

    /// Read one metadata block through the cache.
    fn bread(&mut self, blk: u64) -> FsResult<Vec<u8>> {
        if blk >= self.g.blocks {
            return Err(FsError::Corrupt);
        }
        if let Some(b) = self.cache.map.get(&blk) {
            return Ok(b.clone());
        }
        let mut buf = vec![0u8; self.bs];
        self.dev.read(blk * self.spb, &mut buf)?;
        self.cache_insert(blk, buf.clone(), false);
        Ok(buf)
    }

    /// Replace a metadata block; it reaches the disk at the next commit.
    fn bwrite(&mut self, blk: u64, data: Vec<u8>) -> FsResult<()> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        if blk >= self.g.blocks || data.len() != self.bs {
            return Err(FsError::Corrupt);
        }
        self.cache_insert(blk, data, true);
        Ok(())
    }

    /// Read `buf.len() / bs` consecutive blocks, bypassing the cache (file
    /// data).  Dirty cached blocks are never file data, so this is coherent.
    fn read_raw(&mut self, blk: u64, buf: &mut [u8]) -> FsResult<()> {
        let n = (buf.len() / self.bs) as u64;
        if blk.checked_add(n).map_or(true, |e| e > self.g.blocks) {
            return Err(FsError::Corrupt);
        }
        self.dev.read(blk * self.spb, buf)?;
        Ok(())
    }

    /// Write consecutive blocks straight to the device (file data).
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

    /// Write every pending change to the device: group descriptor blocks,
    /// dirty metadata blocks, then the superblock.
    fn commit(&mut self) -> FsResult<()> {
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
        for b in dirty {
            if let Some(data) = self.cache.map.get(&b) {
                self.dev.write(b * self.spb, data)?;
            }
        }
        self.cache.dirty.clear();
        if self.sb_dirty {
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
            self.dev.write(SB_OFFSET / SECTOR as u64, &self.sb_raw)?;
            self.sb_dirty = false;
        }
        Ok(())
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
        Ok(Inode { ino, raw: b[off..off + self.g.inode_size].to_vec() })
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

    fn sync(&mut self) -> FsResult<()> {
        self.commit()?;
        if !self.ro {
            self.dev.flush()?;
        }
        Ok(())
    }
}
