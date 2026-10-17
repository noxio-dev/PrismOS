// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Group descriptors, bitmaps and the block / inode allocators.

use super::crc::{crc16, crc32c};
use super::disk::*;
use super::Ext4;
use crate::fs::vfs::{FsError, FsResult};
use alloc::vec;
use alloc::vec::Vec;

#[inline]
fn bit(bm: &[u8], i: usize) -> bool {
    bm[i >> 3] & (1 << (i & 7)) != 0
}
#[inline]
fn set_bit(bm: &mut [u8], i: usize) {
    bm[i >> 3] |= 1 << (i & 7);
}
#[inline]
fn clear_bit(bm: &mut [u8], i: usize) {
    bm[i >> 3] &= !(1 << (i & 7));
}

impl Ext4 {
    // ------------------------------------------------ descriptor accessors

    fn gd_off(&self, g: u32) -> usize {
        g as usize * self.g.desc_size
    }
    fn gd16(&self, g: u32, o: usize) -> u16 {
        le16(&self.gdt, self.gd_off(g) + o)
    }
    fn gd32(&self, g: u32, o: usize) -> u32 {
        le32(&self.gdt, self.gd_off(g) + o)
    }
    fn gd_set16(&mut self, g: u32, o: usize, v: u16) {
        let p = self.gd_off(g) + o;
        put16(&mut self.gdt, p, v);
    }
    fn gd_set32(&mut self, g: u32, o: usize, v: u32) {
        let p = self.gd_off(g) + o;
        put32(&mut self.gdt, p, v);
    }
    fn wide(&self) -> bool {
        self.g.desc_size >= 64
    }

    pub(super) fn gd_block_bitmap(&self, g: u32) -> u64 {
        self.gd32(g, 0) as u64 | if self.wide() { (self.gd32(g, 0x20) as u64) << 32 } else { 0 }
    }
    pub(super) fn gd_inode_bitmap(&self, g: u32) -> u64 {
        self.gd32(g, 4) as u64 | if self.wide() { (self.gd32(g, 0x24) as u64) << 32 } else { 0 }
    }
    pub(super) fn gd_inode_table(&self, g: u32) -> u64 {
        self.gd32(g, 8) as u64 | if self.wide() { (self.gd32(g, 0x28) as u64) << 32 } else { 0 }
    }
    pub(super) fn gd_free_blocks(&self, g: u32) -> u32 {
        self.gd16(g, 0xC) as u32 | if self.wide() { (self.gd16(g, 0x2C) as u32) << 16 } else { 0 }
    }
    fn gd_set_free_blocks(&mut self, g: u32, v: u32) {
        self.gd_set16(g, 0xC, v as u16);
        if self.wide() {
            self.gd_set16(g, 0x2C, (v >> 16) as u16);
        }
    }
    pub(super) fn gd_free_inodes(&self, g: u32) -> u32 {
        self.gd16(g, 0xE) as u32 | if self.wide() { (self.gd16(g, 0x2E) as u32) << 16 } else { 0 }
    }
    fn gd_set_free_inodes(&mut self, g: u32, v: u32) {
        self.gd_set16(g, 0xE, v as u16);
        if self.wide() {
            self.gd_set16(g, 0x2E, (v >> 16) as u16);
        }
    }
    fn gd_used_dirs(&self, g: u32) -> u32 {
        self.gd16(g, 0x10) as u32 | if self.wide() { (self.gd16(g, 0x30) as u32) << 16 } else { 0 }
    }
    fn gd_set_used_dirs(&mut self, g: u32, v: u32) {
        self.gd_set16(g, 0x10, v as u16);
        if self.wide() {
            self.gd_set16(g, 0x30, (v >> 16) as u16);
        }
    }
    fn gd_flags(&self, g: u32) -> u16 {
        self.gd16(g, 0x12)
    }
    fn gd_itable_unused(&self, g: u32) -> u32 {
        self.gd16(g, 0x1C) as u32 | if self.wide() { (self.gd16(g, 0x32) as u32) << 16 } else { 0 }
    }
    fn gd_set_itable_unused(&mut self, g: u32, v: u32) {
        self.gd_set16(g, 0x1C, v as u16);
        if self.wide() {
            self.gd_set16(g, 0x32, (v >> 16) as u16);
        }
    }

    fn gd_csum(&self, g: u32) -> u16 {
        let ds = self.g.desc_size;
        let off = self.gd_off(g);
        let d = &self.gdt[off..off + ds];
        if self.g.metadata_csum {
            let mut c = crc32c(self.g.csum_seed, &g.to_le_bytes());
            c = crc32c(c, &d[..0x1E]);
            c = crc32c(c, &[0, 0]);
            if ds > 0x20 {
                c = crc32c(c, &d[0x20..ds]);
            }
            c as u16
        } else if self.g.gdt_csum {
            let mut c = crc16(0xFFFF, &self.g.uuid);
            c = crc16(c, &g.to_le_bytes());
            c = crc16(c, &d[..0x1E]);
            c = crc16(c, &[0, 0]);
            if ds > 0x20 {
                c = crc16(c, &d[0x20..ds]);
            }
            c
        } else {
            0
        }
    }

    /// Recompute the checksum of descriptor `g` and queue its block.
    fn gd_finish(&mut self, g: u32) {
        if self.g.metadata_csum || self.g.gdt_csum {
            let c = self.gd_csum(g);
            self.gd_set16(g, 0x1E, c);
        }
        let i = self.gd_off(g) / self.bs;
        self.gdt_dirty.insert(i);
    }

    // ------------------------------------------------------------- geometry

    /// First block of group `g`.
    pub(super) fn group_start(&self, g: u32) -> u64 {
        self.g.first_data as u64 + g as u64 * self.g.bpg as u64
    }

    /// Number of valid blocks in group `g` (the last one may be short).
    fn group_blocks(&self, g: u32) -> u32 {
        let start = self.group_start(g);
        core::cmp::min(self.g.bpg as u64, self.g.blocks - start) as u32
    }

    fn has_super(&self, g: u32) -> bool {
        if g == 0 {
            return true;
        }
        if self.g.compat & COMPAT_SPARSE_SUPER2 != 0 {
            return g == self.g.backup_bgs[0] || g == self.g.backup_bgs[1];
        }
        if self.g.ro_compat & RO_SPARSE_SUPER == 0 {
            return true;
        }
        if g == 1 {
            return true;
        }
        for base in [3u32, 5, 7] {
            let mut p = base as u64;
            while p < g as u64 {
                p *= base as u64;
            }
            if p == g as u64 {
                return true;
            }
        }
        false
    }

    /// (group, bit) of an absolute block number.
    fn blk_to_group(&self, blk: u64) -> (u32, usize) {
        let rel = blk - self.g.first_data as u64;
        ((rel / self.g.bpg as u64) as u32, (rel % self.g.bpg as u64) as usize)
    }

    // -------------------------------------------------------------- bitmaps

    /// Block bitmap of group `g`; for a BLOCK_UNINIT group it is rebuilt from
    /// the group layout (the on-disk block holds garbage).
    fn load_bbitmap(&mut self, g: u32) -> FsResult<Vec<u8>> {
        let uninit_ok = self.g.metadata_csum || self.g.gdt_csum;
        if uninit_ok && self.gd_flags(g) & BG_BLOCK_UNINIT != 0 {
            return Ok(self.uninit_bbitmap(g));
        }
        self.bread(self.gd_block_bitmap(g))
    }

    fn uninit_bbitmap(&self, g: u32) -> Vec<u8> {
        let mut bm = vec![0u8; self.bs];
        let start = self.group_start(g);
        let end = start + self.group_blocks(g) as u64;
        if self.has_super(g) {
            let n = 1 + self.g.gdt_blocks + self.g.reserved_gdt;
            for i in 0..n as usize {
                if (i as u64) < self.group_blocks(g) as u64 {
                    set_bit(&mut bm, i);
                }
            }
        }
        let mark = |blk: u64, bm: &mut Vec<u8>| {
            if blk >= start && blk < end {
                set_bit(bm, (blk - start) as usize);
            }
        };
        mark(self.gd_block_bitmap(g), &mut bm);
        mark(self.gd_inode_bitmap(g), &mut bm);
        let t = self.gd_inode_table(g);
        for k in 0..self.g.itable_blocks as u64 {
            mark(t + k, &mut bm);
        }
        // Padding past the end of the group is always set.
        for i in self.group_blocks(g) as usize..self.bs * 8 {
            set_bit(&mut bm, i);
        }
        bm
    }

    fn load_ibitmap(&mut self, g: u32) -> FsResult<Vec<u8>> {
        let uninit_ok = self.g.metadata_csum || self.g.gdt_csum;
        if uninit_ok && self.gd_flags(g) & BG_INODE_UNINIT != 0 {
            return Ok(vec![0u8; self.bs]);
        }
        self.bread(self.gd_inode_bitmap(g))
    }

    fn store_bbitmap(&mut self, g: u32, bm: Vec<u8>) -> FsResult<()> {
        if self.g.metadata_csum {
            let c = crc32c(self.g.csum_seed, &bm[..self.g.bpg as usize / 8]);
            self.gd_set16(g, 0x18, c as u16);
            if self.wide() {
                self.gd_set16(g, 0x38, (c >> 16) as u16);
            }
        }
        let f = self.gd_flags(g) & !BG_BLOCK_UNINIT;
        self.gd_set16(g, 0x12, f);
        let blk = self.gd_block_bitmap(g);
        self.bwrite(blk, bm)
    }

    fn store_ibitmap(&mut self, g: u32, bm: Vec<u8>) -> FsResult<()> {
        if self.g.metadata_csum {
            let c = crc32c(self.g.csum_seed, &bm[..self.g.ipg as usize / 8]);
            self.gd_set16(g, 0x1A, c as u16);
            if self.wide() {
                self.gd_set16(g, 0x3A, (c >> 16) as u16);
            }
        }
        let f = self.gd_flags(g) & !BG_INODE_UNINIT;
        self.gd_set16(g, 0x12, f);
        let blk = self.gd_inode_bitmap(g);
        self.bwrite(blk, bm)
    }

    // ---------------------------------------------------------- block alloc

    /// Allocate up to `want` contiguous blocks, preferring the neighbourhood
    /// of `goal`.  Returns (first block, count) with 1 <= count <= want.
    pub(super) fn alloc_blocks(&mut self, goal: u64, want: u32) -> FsResult<(u64, u32)> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        if want == 0 {
            return Err(FsError::Invalid);
        }
        if self.free_blocks == 0 {
            return Err(FsError::NoSpace);
        }
        let goal = if goal < self.g.first_data as u64 || goal >= self.g.blocks { self.g.first_data as u64 } else { goal };
        let (g0, bit0) = self.blk_to_group(goal);
        let ng = self.g.groups;
        for k in 0..=ng {
            let g = (g0 + k) % ng;
            if self.gd_free_blocks(g) == 0 {
                continue;
            }
            let nbits = self.group_blocks(g) as usize;
            let start = if k == 0 { bit0.min(nbits) } else { 0 };
            let end = if k == ng { bit0.min(nbits) } else { nbits };
            if start >= end {
                continue;
            }
            let mut bm = self.load_bbitmap(g)?;
            let mut i = start;
            while i < end && bit(&bm, i) {
                i += 1;
            }
            if i >= end {
                continue;
            }
            let mut j = i;
            while j < end && !bit(&bm, j) && ((j - i) as u32) < want {
                j += 1;
            }
            for b in i..j {
                set_bit(&mut bm, b);
            }
            let n = (j - i) as u32;
            self.store_bbitmap(g, bm)?;
            let f = self.gd_free_blocks(g);
            self.gd_set_free_blocks(g, f - n);
            self.gd_finish(g);
            self.free_blocks -= n as u64;
            self.sb_dirty = true;
            return Ok((self.group_start(g) + i as u64, n));
        }
        Err(FsError::NoSpace)
    }

    /// Give `len` blocks starting at `start` back.
    pub(super) fn free_blocks(&mut self, start: u64, len: u64) -> FsResult<()> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        if len == 0 {
            return Ok(());
        }
        if start < self.g.first_data as u64 || start + len > self.g.blocks {
            return Err(FsError::Corrupt);
        }
        let mut blk = start;
        let mut left = len;
        while left > 0 {
            let (g, b0) = self.blk_to_group(blk);
            let room = self.group_blocks(g) as usize - b0;
            let n = core::cmp::min(room as u64, left) as usize;
            let mut bm = self.load_bbitmap(g)?;
            let mut freed = 0u32;
            for b in b0..b0 + n {
                if bit(&bm, b) {
                    clear_bit(&mut bm, b);
                    freed += 1;
                }
            }
            self.store_bbitmap(g, bm)?;
            let f = self.gd_free_blocks(g);
            self.gd_set_free_blocks(g, f + freed);
            self.gd_finish(g);
            self.free_blocks += freed as u64;
            self.sb_dirty = true;
            blk += n as u64;
            left -= n as u64;
        }
        Ok(())
    }

    // ---------------------------------------------------------- inode alloc

    /// Allocate an inode, preferring group `pgroup`.
    pub(super) fn alloc_inode(&mut self, pgroup: u32, is_dir: bool) -> FsResult<u32> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        let ng = self.g.groups;
        let has_unused = self.g.metadata_csum || self.g.gdt_csum;
        for k in 0..ng {
            let g = (pgroup + k) % ng;
            if self.gd_free_inodes(g) == 0 {
                continue;
            }
            let mut bm = self.load_ibitmap(g)?;
            let ipg = self.g.ipg as usize;
            let mut i = 0;
            while i < ipg && bit(&bm, i) {
                i += 1;
            }
            if i >= ipg {
                continue;
            }
            // Never hand out a reserved inode (only group 0 has them).
            if g == 0 && (i as u32) + 1 < self.g.first_ino {
                continue;
            }
            set_bit(&mut bm, i);
            if has_unused {
                // Inodes past the "unused" boundary were never initialised:
                // clear the slots between the old boundary and this one.
                let boundary = ipg as u32 - self.gd_itable_unused(g).min(ipg as u32);
                if i as u32 >= boundary {
                    self.zero_inode_slots(g, boundary as usize, i)?;
                    self.gd_set_itable_unused(g, ipg as u32 - (i as u32 + 1));
                }
            }
            self.store_ibitmap(g, bm)?;
            let f = self.gd_free_inodes(g);
            self.gd_set_free_inodes(g, f - 1);
            if is_dir {
                let d = self.gd_used_dirs(g);
                self.gd_set_used_dirs(g, d + 1);
            }
            self.gd_finish(g);
            self.free_inodes -= 1;
            self.sb_dirty = true;
            return Ok(g * self.g.ipg + i as u32 + 1);
        }
        Err(FsError::NoSpace)
    }

    /// Zero inode slots `[from, to)` of group `g`'s table.
    fn zero_inode_slots(&mut self, g: u32, from: usize, to: usize) -> FsResult<()> {
        let table = self.gd_inode_table(g);
        let isz = self.g.inode_size;
        let mut idx = from;
        while idx < to {
            let byte = idx * isz;
            let blk = table + (byte / self.bs) as u64;
            let mut b = self.bread(blk)?;
            let first = byte % self.bs;
            let in_blk = (self.bs - first) / isz;
            let n = core::cmp::min(in_blk, to - idx);
            for x in &mut b[first..first + n * isz] {
                *x = 0;
            }
            self.bwrite(blk, b)?;
            idx += n;
        }
        Ok(())
    }

    /// Release the bitmap bit (and counters) of an inode.
    pub(super) fn free_inode_bit(&mut self, ino: u32, was_dir: bool) -> FsResult<()> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        if ino == 0 || ino > self.g.inodes {
            return Err(FsError::Corrupt);
        }
        let g = (ino - 1) / self.g.ipg;
        let i = ((ino - 1) % self.g.ipg) as usize;
        let mut bm = self.load_ibitmap(g)?;
        if bit(&bm, i) {
            clear_bit(&mut bm, i);
            self.store_ibitmap(g, bm)?;
            let f = self.gd_free_inodes(g);
            self.gd_set_free_inodes(g, f + 1);
            if was_dir {
                let d = self.gd_used_dirs(g);
                self.gd_set_used_dirs(g, d.saturating_sub(1));
            }
            self.gd_finish(g);
            self.free_inodes += 1;
            self.sb_dirty = true;
        }
        Ok(())
    }

    /// Group an inode lives in (allocation hints).
    pub(super) fn inode_group(&self, ino: u32) -> u32 {
        (ino - 1) / self.g.ipg
    }
}
