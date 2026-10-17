// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Directory blocks.  Every directory is handled as a plain linear list of
// entries, which is also what an htree directory is underneath (its index
// blocks look like unused entries).  Lookups therefore scan; insertion into
// an indexed directory drops the index flag, like the Linux driver does for
// operations it cannot index.

use super::crc::crc32c;
use super::disk::*;
use super::extent::{map_block, merge_adjacent, Ext, ExtTree};
use super::Ext4;
use crate::vfs::{DirEntry, FileType, FsError, FsResult};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// One parsed directory entry.
#[derive(Clone, Copy)]
pub(super) struct RawEnt {
    pub off: usize,
    pub ino: u32,
    pub rec_len: usize,
    pub name_len: usize,
    pub ftype: u8,
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

fn is_tail(blk: &[u8]) -> bool {
    let o = blk.len() - 12;
    le32(blk, o) == 0 && le16(blk, o + 4) == 12 && blk[o + 6] == 0 && blk[o + 7] == FT_TAIL
}

impl Ext4 {
    /// End of the entry area of a directory block (before the checksum tail).
    fn dir_block_end(&self, blk: &[u8]) -> FsResult<usize> {
        let tail = is_tail(blk);
        if self.g.metadata_csum && !tail {
            return Err(FsError::Unsupported);
        }
        Ok(if tail { blk.len() - 12 } else { blk.len() })
    }

    pub(super) fn parse_dir_block(&self, blk: &[u8]) -> FsResult<Vec<RawEnt>> {
        let end = if is_tail(blk) { blk.len() - 12 } else { blk.len() };
        let mut v = Vec::new();
        let mut off = 0;
        while off < end {
            if off + 8 > end {
                return Err(FsError::Corrupt);
            }
            let mut rec_len = le16(blk, off + 4) as usize;
            if rec_len == 0 && blk.len() == 65536 {
                rec_len = 65536;
            }
            let name_len = blk[off + 6] as usize;
            if rec_len < 8 || rec_len % 4 != 0 || off + rec_len > end || 8 + name_len > rec_len {
                return Err(FsError::Corrupt);
            }
            v.push(RawEnt { off, ino: le32(blk, off), rec_len, name_len, ftype: blk[off + 7] });
            off += rec_len;
        }
        Ok(v)
    }

    fn set_dir_csum(&self, dir: &Inode, blk: &mut [u8]) {
        if self.g.metadata_csum && is_tail(blk) {
            let n = blk.len();
            let c = crc32c(self.inode_seed(dir.ino, dir.generation()), &blk[..n - 12]);
            put32(blk, n - 4, c);
        }
    }

    fn write_dirent(blk: &mut [u8], off: usize, ino: u32, rec_len: usize, name: &[u8], ft: u8) {
        put32(blk, off, ino);
        put16(blk, off + 4, rec_len as u16);
        blk[off + 6] = name.len() as u8;
        blk[off + 7] = ft;
        blk[off + 8..off + 8 + name.len()].copy_from_slice(name);
    }

    /// A fresh directory block holding "." and "..".
    pub(super) fn new_dir_block(&self, dir: &Inode, parent: u32) -> Vec<u8> {
        let bs = self.bs;
        let ft = if self.g.filetype { FT_DIR } else { 0 };
        let usable = if self.g.metadata_csum { bs - 12 } else { bs };
        let mut b = vec![0u8; bs];
        Self::write_dirent(&mut b, 0, dir.ino, 12, b".", ft);
        Self::write_dirent(&mut b, 12, parent, usable - 12, b"..", ft);
        if self.g.metadata_csum {
            put16(&mut b, bs - 12 + 4, 12);
            b[bs - 12 + 7] = FT_TAIL;
            self.set_dir_csum(dir, &mut b);
        }
        b
    }

    fn dir_tree(&mut self, dir: &Inode) -> FsResult<(ExtTree, u32)> {
        if !dir.is_dir() {
            return Err(FsError::NotDir);
        }
        let tree = self.tree_for(dir)?;
        let nblocks = ((dir.size() + self.bs as u64 - 1) / self.bs as u64) as u32;
        Ok((tree, nblocks))
    }

    // -------------------------------------------------------------- queries

    pub(super) fn op_lookup(&mut self, dir: u32, name: &str) -> FsResult<u32> {
        let d = self.read_inode(dir)?;
        let (tree, nblocks) = self.dir_tree(&d)?;
        let nb = name.as_bytes();
        for lb in 0..nblocks {
            let Some(p) = map_block(&tree.exts, lb) else { continue };
            let b = self.bread(p)?;
            for e in self.parse_dir_block(&b)? {
                if e.ino != 0 && e.name_len == nb.len() && &b[e.off + 8..e.off + 8 + e.name_len] == nb {
                    return Ok(e.ino);
                }
            }
        }
        Err(FsError::NotFound)
    }

    pub(super) fn op_readdir(&mut self, dir: u32) -> FsResult<Vec<DirEntry>> {
        let d = self.read_inode(dir)?;
        let (tree, nblocks) = self.dir_tree(&d)?;
        let mut out = Vec::new();
        for lb in 0..nblocks {
            let Some(p) = map_block(&tree.exts, lb) else { continue };
            let b = self.bread(p)?;
            for e in self.parse_dir_block(&b)? {
                if e.ino == 0 {
                    continue;
                }
                let name = String::from_utf8_lossy(&b[e.off + 8..e.off + 8 + e.name_len]).into_owned();
                let ftype = if self.g.filetype && e.ftype != FT_UNKNOWN {
                    dirent_to_file_type(e.ftype)
                } else {
                    match self.read_inode(e.ino) {
                        Ok(i) => file_type_of(i.mode()),
                        Err(_) => FileType::Other,
                    }
                };
                out.push(DirEntry { ino: e.ino as u64, ftype, name });
            }
        }
        Ok(out)
    }

    /// True when the directory holds nothing but "." and "..".
    pub(super) fn dir_is_empty(&mut self, dir: &Inode) -> FsResult<bool> {
        let (tree, nblocks) = self.dir_tree(dir)?;
        for lb in 0..nblocks {
            let Some(p) = map_block(&tree.exts, lb) else { continue };
            let b = self.bread(p)?;
            for e in self.parse_dir_block(&b)? {
                if e.ino == 0 {
                    continue;
                }
                let n = &b[e.off + 8..e.off + 8 + e.name_len];
                if n != b"." && n != b".." {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    // ------------------------------------------------------------ mutation

    /// Try to place an entry in `b`.  Returns whether it fit.
    fn try_insert(&self, b: &mut [u8], name: &[u8], ino: u32, ft: u8) -> FsResult<bool> {
        let need = 8 + align4(name.len());
        self.dir_block_end(b)?;
        for e in self.parse_dir_block(b)? {
            if e.ino == 0 {
                if e.rec_len >= need {
                    Self::write_dirent(b, e.off, ino, e.rec_len, name, ft);
                    return Ok(true);
                }
            } else {
                let used = 8 + align4(e.name_len);
                if e.rec_len >= used + need {
                    put16(b, e.off + 4, used as u16);
                    Self::write_dirent(b, e.off + used, ino, e.rec_len - used, name, ft);
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Add `name -> ino` to `dir`.  The caller writes `dir` afterwards (its
    /// size, block count or flags may change).
    pub(super) fn dir_add(&mut self, dir: &mut Inode, name: &[u8], ino: u32, mode: u16) -> FsResult<()> {
        if !dir.uses_extents() {
            return Err(FsError::Unsupported);
        }
        if dir.flags() & EXT4_INDEX_FL != 0 {
            if self.g.metadata_csum {
                return Err(FsError::Unsupported);
            }
            dir.set_flags(dir.flags() & !EXT4_INDEX_FL);
        }
        let ft = if self.g.filetype { dirent_type(mode) } else { 0 };
        let (tree, nblocks) = self.dir_tree(dir)?;
        for lb in 0..nblocks {
            let Some(p) = map_block(&tree.exts, lb) else { continue };
            let mut b = self.bread(p)?;
            if self.try_insert(&mut b, name, ino, ft)? {
                self.set_dir_csum(dir, &mut b);
                return self.bwrite(p, b);
            }
        }
        // No room: grow the directory by one block.
        let p = self.dir_append_block(dir)?;
        let bs = self.bs;
        let usable = if self.g.metadata_csum { bs - 12 } else { bs };
        let mut b = vec![0u8; bs];
        put16(&mut b, 4, usable as u16);
        if self.g.metadata_csum {
            put16(&mut b, bs - 12 + 4, 12);
            b[bs - 12 + 7] = FT_TAIL;
        }
        if !self.try_insert(&mut b, name, ino, ft)? {
            return Err(FsError::NameTooLong);
        }
        self.set_dir_csum(dir, &mut b);
        self.bwrite(p, b)
    }

    /// Append one (empty, unmapped-content) block to a directory's mapping.
    fn dir_append_block(&mut self, dir: &mut Inode) -> FsResult<u64> {
        let bs = self.bs as u64;
        let mut tree = self.tree_for(dir)?;
        let lblk = ((dir.size() + bs - 1) / bs) as u32;
        let goal = match tree.exts.last() {
            Some(e) => e.pblk + e.len as u64,
            None => self.group_start(self.inode_group(dir.ino)),
        };
        let (p, _) = self.alloc_blocks(goal, 1)?;
        let mut exts = tree.exts.clone();
        exts.push(Ext { lblk, len: 1, pblk: p, uninit: false });
        merge_adjacent(&mut exts);
        if let Err(e) = self.rebuild_tree(dir, &mut tree, exts) {
            let _ = self.free_blocks(p, 1);
            return Err(e);
        }
        dir.set_blocks512(dir.blocks512() + bs / 512);
        dir.set_size((lblk as u64 + 1) * bs);
        Ok(p)
    }

    /// Run `f` on the entry called `name`; `f` gets the block and the entry
    /// (plus the previous entry of the same block, if any) and returns
    /// whether it changed the block.
    fn dir_edit<F>(&mut self, dir: &Inode, name: &[u8], mut f: F) -> FsResult<(u32, u8)>
    where
        F: FnMut(&mut [u8], RawEnt, Option<RawEnt>),
    {
        let (tree, nblocks) = self.dir_tree(dir)?;
        for lb in 0..nblocks {
            let Some(p) = map_block(&tree.exts, lb) else { continue };
            let mut b = self.bread(p)?;
            let ents = self.parse_dir_block(&b)?;
            let mut prev: Option<RawEnt> = None;
            for e in ents {
                if e.ino != 0 && e.name_len == name.len() && &b[e.off + 8..e.off + 8 + e.name_len] == name {
                    let (ino, ft) = (e.ino, e.ftype);
                    self.dir_block_end(&b)?;
                    f(&mut b, e, prev);
                    self.set_dir_csum(dir, &mut b);
                    self.bwrite(p, b)?;
                    return Ok((ino, ft));
                }
                prev = Some(e);
            }
        }
        Err(FsError::NotFound)
    }

    /// Remove the entry `name`; returns its inode number.
    pub(super) fn dir_remove(&mut self, dir: &Inode, name: &[u8]) -> FsResult<u32> {
        let (ino, _) = self.dir_edit(dir, name, |b, e, prev| match prev {
            Some(p) => put16(b, p.off + 4, (p.rec_len + e.rec_len) as u16),
            None => put32(b, e.off, 0),
        })?;
        Ok(ino)
    }

    /// Point the existing entry `name` at another inode.
    pub(super) fn dir_replace(&mut self, dir: &Inode, name: &[u8], ino: u32, mode: u16) -> FsResult<()> {
        let ft = if self.g.filetype { dirent_type(mode) } else { 0 };
        self.dir_edit(dir, name, |b, e, _| {
            put32(b, e.off, ino);
            b[e.off + 7] = ft;
        })?;
        Ok(())
    }

    /// Rewrite the ".." entry of directory `dir`.
    pub(super) fn dir_set_dotdot(&mut self, dir: &Inode, parent: u32) -> FsResult<()> {
        self.dir_edit(dir, b"..", |b, e, _| put32(b, e.off, parent))?;
        Ok(())
    }
}
