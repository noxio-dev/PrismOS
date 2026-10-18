// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// File data: extent trees (and classic block maps for reading), reading,
// writing, truncation and freeing.
//
// The writer works on a flat, sorted list of extents.  After a change the
// whole tree is regenerated from that list, reusing the existing node blocks
// (see `rebuild_tree`): it keeps the code small and the result is always a
// canonical, valid tree.

use super::crc::crc32c;
use super::disk::*;
use super::Ext4;
use crate::fs::vfs::{now, FsError, FsResult};
use alloc::vec;
use alloc::vec::Vec;

pub const INIT_MAX: u32 = 32768;
pub const UNINIT_MAX: u32 = 32767;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ext {
    pub lblk: u32,
    pub len: u32,
    pub pblk: u64,
    pub uninit: bool,
}

impl Ext {
    pub fn end(&self) -> u64 {
        self.lblk as u64 + self.len as u64
    }
}

/// The mapping of a file: its extents (sorted, non overlapping) and the
/// blocks holding the tree / indirect blocks themselves.
#[derive(Clone, Default)]
pub struct ExtTree {
    pub exts: Vec<Ext>,
    pub nodes: Vec<u64>,
}

/// Index of the extent covering `lblk`, if any.
pub fn find(exts: &[Ext], lblk: u32) -> Option<usize> {
    let idx = exts.partition_point(|e| e.lblk <= lblk);
    if idx == 0 {
        return None;
    }
    if exts[idx - 1].end() > lblk as u64 {
        Some(idx - 1)
    } else {
        None
    }
}

/// Physical block of logical block `lblk`.
pub fn map_block(exts: &[Ext], lblk: u32) -> Option<u64> {
    find(exts, lblk).map(|i| exts[i].pblk + (lblk - exts[i].lblk) as u64)
}

/// Join neighbouring extents that are contiguous on both sides.
pub fn merge_adjacent(exts: &mut Vec<Ext>) {
    let mut out: Vec<Ext> = Vec::with_capacity(exts.len());
    for &e in exts.iter() {
        if let Some(last) = out.last_mut() {
            let cap = if e.uninit { UNINIT_MAX } else { INIT_MAX };
            if last.uninit == e.uninit
                && last.end() == e.lblk as u64
                && last.pblk + last.len as u64 == e.pblk
                && last.len + e.len <= cap
            {
                last.len += e.len;
                continue;
            }
        }
        out.push(e);
    }
    *exts = out;
}

/// Turn `[lblk, lblk + len)` of the uninitialised extent covering it into an
/// initialised extent, splitting the original as needed.
fn make_init(exts: &mut Vec<Ext>, lblk: u32, len: u32) {
    let Some(i) = find(exts, lblk) else { return };
    let e = exts[i];
    let mut parts: Vec<Ext> = Vec::new();
    if lblk > e.lblk {
        parts.push(Ext { lblk: e.lblk, len: lblk - e.lblk, pblk: e.pblk, uninit: true });
    }
    parts.push(Ext { lblk, len, pblk: e.pblk + (lblk - e.lblk) as u64, uninit: false });
    let tail = lblk as u64 + len as u64;
    if tail < e.end() {
        parts.push(Ext {
            lblk: tail as u32,
            len: (e.end() - tail) as u32,
            pblk: e.pblk + (tail - e.lblk as u64),
            uninit: true,
        });
    }
    exts.splice(i..=i, parts);
}

/// Drop every mapping at or after logical block `keep`.  Returns the
/// surviving extents and the freed physical ranges.
fn trim_tail(exts: &[Ext], keep: u64) -> (Vec<Ext>, Vec<(u64, u64)>) {
    let mut out = Vec::new();
    let mut freed = Vec::new();
    for e in exts {
        if e.lblk as u64 >= keep {
            freed.push((e.pblk, e.len as u64));
        } else if e.end() > keep {
            let kept = (keep - e.lblk as u64) as u32;
            out.push(Ext { len: kept, ..*e });
            freed.push((e.pblk + kept as u64, e.len as u64 - kept as u64));
        } else {
            out.push(*e);
        }
    }
    (out, freed)
}

fn in_ranges(r: &[(u32, u32)], l: u32) -> bool {
    let i = r.partition_point(|x| x.0 <= l);
    i > 0 && (l as u64) < r[i - 1].0 as u64 + r[i - 1].1 as u64
}

impl Ext4 {
    // ------------------------------------------------------------ loading

    /// Read the mapping of `inode` (extent tree or block map), verifying
    /// the checksums of the index blocks on the way (phase 1.5).
    pub(super) fn load_tree(&mut self, inode: &Inode) -> FsResult<ExtTree> {
        if inode.uses_extents() {
            let mut t = ExtTree::default();
            let area = inode.block_area().to_vec();
            let ctx = (inode.ino, inode.generation());
            self.walk_node(&area, &mut t, 5, ctx)?;
            Ok(t)
        } else {
            self.load_blockmap(inode)
        }
    }

    /// Like `load_tree`, remembering the result for the next call.
    pub(super) fn tree_for(&mut self, inode: &Inode) -> FsResult<ExtTree> {
        if let Some((ino, t)) = &self.ext_cache {
            if *ino == inode.ino {
                return Ok(t.clone());
            }
        }
        let t = self.load_tree(inode)?;
        self.ext_cache = Some((inode.ino, t.clone()));
        Ok(t)
    }

    /// Verify the checksum of one on-disk extent/index block (the tail
    /// sits right after the entry area at 12 + 12*eh_max).
    fn verify_extent_csum(&self, ctx: (u32, u32), blk: &[u8]) -> FsResult<()> {
        if !self.g.metadata_csum || blk.len() < 24 || le16(blk, 0) != EXT_MAGIC {
            return Ok(());
        }
        let max = le16(blk, 4) as usize;
        let tail = 12 + 12 * max;
        if tail + 4 > blk.len() {
            return Ok(());
        }
        let want = le32(blk, tail);
        let seed = self.inode_seed(ctx.0, ctx.1);
        let got = crc32c(seed, &blk[..tail]);
        self.note_csum(want == got);
        if want != got {
            return Err(FsError::Corrupt);
        }
        Ok(())
    }

    fn walk_node(&mut self, node: &[u8], t: &mut ExtTree, depth_left: u32, ctx: (u32, u32)) -> FsResult<()> {
        if node.len() < 12 || le16(node, 0) != EXT_MAGIC {
            return Err(FsError::Corrupt);
        }
        let entries = le16(node, 2) as usize;
        let max = le16(node, 4) as usize;
        let depth = le16(node, 6) as u32;
        if entries > max || 12 + 12 * entries > node.len() || depth > depth_left {
            return Err(FsError::Corrupt);
        }
        for i in 0..entries {
            let o = 12 + 12 * i;
            if depth == 0 {
                let lblk = le32(node, o);
                let raw = le16(node, o + 4) as u32;
                let pblk = le32(node, o + 8) as u64 | (le16(node, o + 6) as u64) << 32;
                let (len, uninit) = if raw > INIT_MAX { (raw - INIT_MAX, true) } else { (raw, false) };
                if len == 0 || pblk + len as u64 > self.g.blocks {
                    return Err(FsError::Corrupt);
                }
                if let Some(last) = t.exts.last() {
                    if (lblk as u64) < last.end() {
                        return Err(FsError::Corrupt);
                    }
                }
                t.exts.push(Ext { lblk, len, pblk, uninit });
            } else {
                let child = le32(node, o + 4) as u64 | (le16(node, o + 8) as u64) << 32;
                if child >= self.g.blocks {
                    return Err(FsError::Corrupt);
                }
                t.nodes.push(child);
                let buf = self.bread(child)?;
                self.verify_extent_csum(ctx, &buf)?;
                self.walk_node(&buf, t, depth - 1, ctx)?;
                if t.nodes.len() > self.g.blocks as usize {
                    return Err(FsError::Corrupt);
                }
            }
        }
        Ok(())
    }

    fn push_mapped(&self, t: &mut ExtTree, lblk: u64, pblk: u64) -> FsResult<()> {
        if lblk > u32::MAX as u64 || pblk >= self.g.blocks {
            return Err(FsError::Corrupt);
        }
        if let Some(last) = t.exts.last_mut() {
            if last.end() == lblk && last.pblk + last.len as u64 == pblk && last.len < INIT_MAX {
                last.len += 1;
                return Ok(());
            }
        }
        t.exts.push(Ext { lblk: lblk as u32, len: 1, pblk, uninit: false });
        Ok(())
    }

    /// Classic ext2/3 block map, converted to extents.
    fn load_blockmap(&mut self, inode: &Inode) -> FsResult<ExtTree> {
        let mut t = ExtTree::default();
        let bs = self.bs as u64;
        let ptrs = bs / 4;
        let nblk = (inode.size() + bs - 1) / bs;
        let area = inode.block_area().to_vec();
        let mut lblk = 0u64;
        for i in 0..12 {
            if lblk >= nblk {
                break;
            }
            let p = le32(&area, i * 4) as u64;
            if p != 0 {
                self.push_mapped(&mut t, lblk, p)?;
            }
            lblk += 1;
        }
        let spans = [ptrs, ptrs * ptrs, ptrs * ptrs * ptrs];
        for level in 1..=3u32 {
            if lblk >= nblk {
                break;
            }
            let p = le32(&area, 48 + (level as usize - 1) * 4) as u64;
            if p != 0 {
                self.walk_indirect(p, level, lblk, nblk, &mut t)?;
            }
            lblk += spans[level as usize - 1];
        }
        Ok(t)
    }

    fn walk_indirect(&mut self, blk: u64, level: u32, base: u64, nblk: u64, t: &mut ExtTree) -> FsResult<()> {
        t.nodes.push(blk);
        let b = self.bread(blk)?;
        let ptrs = (self.bs / 4) as u64;
        let child_span = match level {
            1 => 1,
            2 => ptrs,
            _ => ptrs * ptrs,
        };
        for i in 0..ptrs {
            let lb = base + i * child_span;
            if lb >= nblk {
                break;
            }
            let p = le32(&b, i as usize * 4) as u64;
            if p == 0 {
                continue;
            }
            if level == 1 {
                self.push_mapped(t, lb, p)?;
            } else {
                self.walk_indirect(p, level - 1, lb, nblk, t)?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------- reading

    /// Read `out.len()` bytes starting at byte `in_blk` of block `pblk`
    /// (running over the following contiguous blocks).  Block-aligned
    /// ranges that are entirely uncached take one raw device read; the
    /// rest goes through the cache (phase 1.5: file data is cached).
    fn read_run(&mut self, pblk: u64, in_blk: usize, out: &mut [u8]) -> FsResult<()> {
        let bs = self.bs;
        if in_blk == 0 && out.len() >= bs && out.len() % bs == 0 {
            let n = (out.len() / bs) as u64;
            let any_cached = (0..n).any(|k| self.cache.map.contains_key(&(pblk + k)));
            if !any_cached {
                return self.read_raw(pblk, out);
            }
        }
        let (mut src, mut skip, mut done) = (pblk, in_blk, 0usize);
        while done < out.len() {
            let b = self.bread(src)?;
            let take = core::cmp::min(bs - skip, out.len() - done);
            out[done..done + take].copy_from_slice(&b[skip..skip + take]);
            done += take;
            skip = 0;
            src += 1;
        }
        Ok(())
    }

    /// Read file data; holes and unwritten extents read as zeros.
    pub(super) fn file_read(&mut self, inode: &Inode, off: u64, buf: &mut [u8]) -> FsResult<usize> {
        let size = inode.size();
        if off >= size || buf.is_empty() {
            return Ok(0);
        }
        let n = core::cmp::min(buf.len() as u64, size - off) as usize;
        let tree = self.tree_for(inode)?;
        let bs = self.bs as u64;
        let (mut done, mut pos) = (0usize, off);
        while done < n {
            let lb = pos / bs;
            if lb > u32::MAX as u64 {
                return Err(FsError::Corrupt);
            }
            let lblk = lb as u32;
            let in_blk = (pos % bs) as usize;
            let want = n - done;
            match find(&tree.exts, lblk) {
                Some(i) => {
                    let e = tree.exts[i];
                    let avail = (e.end() - lb) * bs - in_blk as u64;
                    let take = core::cmp::min(want as u64, avail) as usize;
                    if e.uninit {
                        buf[done..done + take].fill(0);
                    } else {
                        let p = e.pblk + (lblk - e.lblk) as u64;
                        self.read_run(p, in_blk, &mut buf[done..done + take])?;
                    }
                    done += take;
                    pos += take as u64;
                }
                None => {
                    let next = tree.exts.partition_point(|e| e.lblk <= lblk);
                    let hole = if next < tree.exts.len() { tree.exts[next].lblk as u64 * bs - pos } else { u64::MAX };
                    let take = core::cmp::min(want as u64, hole) as usize;
                    buf[done..done + take].fill(0);
                    done += take;
                    pos += take as u64;
                }
            }
        }
        Ok(n)
    }

    // -------------------------------------------------------------- writing

    fn node_csum(&self, ino: u32, generation: u32, blk: &[u8], max: usize) -> u32 {
        crc32c(self.inode_seed(ino, generation), &blk[..12 + 12 * max])
    }

    /// Regenerate the extent tree of `inode` from `new_exts`.  Existing node
    /// blocks are reused, missing ones allocated, surplus ones freed.  On
    /// error nothing visible has changed (apart from allocations rolled back).
    pub(super) fn rebuild_tree(&mut self, inode: &mut Inode, tree: &mut ExtTree, new_exts: Vec<Ext>) -> FsResult<()> {
        let bs = self.bs;
        let cap = (bs - 12) / 12;
        let n = new_exts.len();
        // Number of nodes per level, leaves first; the root lives in the inode.
        let mut counts: Vec<usize> = Vec::new();
        if n > 4 {
            let mut c = (n + cap - 1) / cap;
            counts.push(c);
            while c > 4 {
                c = (c + cap - 1) / cap;
                counts.push(c);
            }
        }
        let need: usize = counts.iter().sum();
        let old_total = tree.nodes.len();
        let mut nodes = tree.nodes.clone();
        let mut fresh: Vec<u64> = Vec::new();
        while nodes.len() < need {
            let goal = nodes
                .last()
                .copied()
                .or(new_exts.last().map(|e| e.pblk))
                .unwrap_or_else(|| self.group_start(self.inode_group(inode.ino)));
            match self.alloc_blocks(goal, 1) {
                Ok((b, _)) => {
                    nodes.push(b);
                    fresh.push(b);
                }
                Err(e) => {
                    for b in fresh {
                        let _ = self.free_blocks(b, 1);
                    }
                    return Err(e);
                }
            }
        }
        let surplus = nodes.split_off(need);
        let generation = inode.generation();
        let ino = inode.ino;

        let encode_hdr = |buf: &mut [u8], entries: usize, max: usize, depth: usize| {
            put16(buf, 0, EXT_MAGIC);
            put16(buf, 2, entries as u16);
            put16(buf, 4, max as u16);
            put16(buf, 6, depth as u16);
            put32(buf, 8, 0);
        };
        let encode_ext = |buf: &mut [u8], o: usize, e: &Ext| {
            put32(buf, o, e.lblk);
            put16(buf, o + 4, if e.uninit { e.len as u16 + INIT_MAX as u16 } else { e.len as u16 });
            put16(buf, o + 6, (e.pblk >> 32) as u16);
            put32(buf, o + 8, e.pblk as u32);
        };
        let encode_idx = |buf: &mut [u8], o: usize, first: u32, child: u64| {
            put32(buf, o, first);
            put32(buf, o + 4, child as u32);
            put16(buf, o + 8, (child >> 32) as u16);
            put16(buf, o + 10, 0);
        };

        let mut root = [0u8; 60];
        if counts.is_empty() {
            encode_hdr(&mut root, n, 4, 0);
            for (i, e) in new_exts.iter().enumerate() {
                encode_ext(&mut root, 12 + 12 * i, e);
            }
        } else {
            let mut idx = 0usize;
            let mut cur: Vec<(u32, u64)> = Vec::new();
            for chunk in new_exts.chunks(cap) {
                let blk = nodes[idx];
                idx += 1;
                let mut buf = vec![0u8; bs];
                encode_hdr(&mut buf, chunk.len(), cap, 0);
                for (i, e) in chunk.iter().enumerate() {
                    encode_ext(&mut buf, 12 + 12 * i, e);
                }
                if self.g.metadata_csum {
                    let c = self.node_csum(ino, generation, &buf, cap);
                    put32(&mut buf, 12 + 12 * cap, c);
                }
                self.bwrite(blk, buf)?;
                cur.push((chunk[0].lblk, blk));
            }
            for lvl in 1..counts.len() {
                let mut next: Vec<(u32, u64)> = Vec::new();
                for chunk in cur.chunks(cap) {
                    let blk = nodes[idx];
                    idx += 1;
                    let mut buf = vec![0u8; bs];
                    encode_hdr(&mut buf, chunk.len(), cap, lvl);
                    for (i, c) in chunk.iter().enumerate() {
                        encode_idx(&mut buf, 12 + 12 * i, c.0, c.1);
                    }
                    if self.g.metadata_csum {
                        let c = self.node_csum(ino, generation, &buf, cap);
                        put32(&mut buf, 12 + 12 * cap, c);
                    }
                    self.bwrite(blk, buf)?;
                    next.push((chunk[0].0, blk));
                }
                cur = next;
            }
            encode_hdr(&mut root, cur.len(), 4, counts.len());
            for (i, c) in cur.iter().enumerate() {
                encode_idx(&mut root, 12 + 12 * i, c.0, c.1);
            }
        }
        inode.block_area_mut().copy_from_slice(&root);
        inode.set_flags(inode.flags() | EXT4_EXTENTS_FL);
        let per = (bs / 512) as i64;
        let blocks = inode.blocks512() as i64 + (need as i64 - old_total as i64) * per;
        inode.set_blocks512(blocks.max(0) as u64);
        for b in surplus {
            self.free_blocks(b, 1)?;
        }
        tree.exts = new_exts;
        tree.nodes = nodes;
        self.ext_cache = Some((inode.ino, tree.clone()));
        Ok(())
    }

    /// Where to put new data for logical block `lblk`: right after the
    /// previous extent when there is one, else in the inode's group.
    fn write_goal(&self, exts: &[Ext], lblk: u32, inode: &Inode) -> u64 {
        let i = exts.partition_point(|e| e.lblk < lblk);
        if i > 0 {
            let p = exts[i - 1];
            return p.pblk + p.len as u64;
        }
        if let Some(n) = exts.first() {
            return n.pblk.saturating_sub(1);
        }
        self.group_start(self.inode_group(inode.ino))
    }

    pub(super) fn file_write(&mut self, ino: u32, off: u64, data: &[u8]) -> FsResult<usize> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        let mut inode = self.read_inode(ino)?;
        if !inode.is_reg() {
            return Err(if inode.is_dir() { FsError::IsDir } else { FsError::Invalid });
        }
        if !inode.uses_extents() || inode.flags() & (EXT4_HUGE_FILE_FL | EXT4_ENCRYPT_FL | EXT4_INLINE_DATA_FL) != 0 {
            return Err(FsError::Unsupported);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let bs = self.bs as u64;
        let end = off.checked_add(data.len() as u64).ok_or(FsError::TooBig)?;
        if end > (u32::MAX as u64) * bs {
            return Err(FsError::TooBig);
        }
        let mut tree = self.tree_for(&inode)?;
        let first = (off / bs) as u32;
        let last = ((end - 1) / bs) as u32;

        // 1. Make sure every block of [first, last] is mapped and initialised.
        let mut exts = tree.exts.clone();
        let mut fresh: Vec<(u32, u32)> = Vec::new();
        let mut allocated: Vec<(u64, u32)> = Vec::new();
        let mut stop: u64 = last as u64 + 1;
        let mut lb = first as u64;
        let mut failure: Option<FsError> = None;
        while lb <= last as u64 {
            let l32 = lb as u32;
            if let Some(i) = find(&exts, l32) {
                let e = exts[i];
                let seg_end = core::cmp::min(e.end(), last as u64 + 1);
                if e.uninit {
                    make_init(&mut exts, l32, (seg_end - lb) as u32);
                    fresh.push((l32, (seg_end - lb) as u32));
                }
                lb = seg_end;
            } else {
                let nx = exts.partition_point(|e| e.lblk <= l32);
                let next_start = if nx < exts.len() { exts[nx].lblk as u64 } else { u64::MAX };
                let hole_end = core::cmp::min(next_start, last as u64 + 1);
                let mut cur = lb;
                while cur < hole_end {
                    let want = core::cmp::min(hole_end - cur, INIT_MAX as u64) as u32;
                    let goal = self.write_goal(&exts, cur as u32, &inode);
                    match self.alloc_blocks(goal, want) {
                        Ok((p, n)) => {
                            let pos = exts.partition_point(|x| x.lblk < cur as u32);
                            exts.insert(pos, Ext { lblk: cur as u32, len: n, pblk: p, uninit: false });
                            allocated.push((p, n));
                            fresh.push((cur as u32, n));
                            cur += n as u64;
                        }
                        Err(e) => {
                            failure = Some(e);
                            break;
                        }
                    }
                }
                if failure.is_some() {
                    stop = cur;
                    break;
                }
                lb = hole_end;
            }
        }
        if let Some(e) = failure {
            if stop <= first as u64 {
                // Nothing could be allocated for the very first block.
                for (p, n) in allocated {
                    let _ = self.free_blocks(p, n as u64);
                }
                return Err(e);
            }
        }

        // 2. Publish the new mapping.
        merge_adjacent(&mut exts);
        if exts != tree.exts {
            if let Err(e) = self.rebuild_tree(&mut inode, &mut tree, exts) {
                for (p, n) in allocated {
                    let _ = self.free_blocks(p, n as u64);
                }
                return Err(e);
            }
        }
        let added: u64 = allocated.iter().map(|a| a.1 as u64).sum();
        inode.set_blocks512(inode.blocks512() + added * (bs / 512));

        // 3. Write the data through the block cache (write-back: it
        //    reaches the device at the commit, batched into runs).
        let eff_end = core::cmp::min(end, stop * bs);
        let bsz = self.bs;
        let mut pos = off;
        let mut src = 0usize;
        while pos < eff_end {
            let lblk = (pos / bs) as u32;
            let in_blk = (pos % bs) as usize;
            let take = core::cmp::min(bsz - in_blk, (eff_end - pos) as usize);
            let i = find(&tree.exts, lblk).ok_or(FsError::Corrupt)?;
            let e = tree.exts[i];
            let p = e.pblk + (lblk - e.lblk) as u64;
            if take == bsz {
                self.bwrite_data(p, data[src..src + bsz].to_vec())?;
            } else {
                let mut blk = if in_ranges(&fresh, lblk) {
                    vec![0u8; bsz]
                } else {
                    self.bread(p)?
                };
                blk[in_blk..in_blk + take].copy_from_slice(&data[src..src + take]);
                self.bwrite_data(p, blk)?;
            }
            src += take;
            pos += take as u64;
        }

        if eff_end > inode.size() {
            inode.set_size(eff_end);
        }
        let t = now();
        inode.set_mtime(t);
        inode.set_ctime(t);
        self.write_inode(&mut inode)?;
        Ok((eff_end - off) as usize)
    }

    /// fallocate-style preallocation (phase 1.5): every hole in
    /// [start, start+len) becomes an *unwritten* extent -- the blocks are
    /// allocated and will read as zeros, but no data is written.  The
    /// size grows past EOF unless `keep_size`.
    pub(super) fn file_fallocate(&mut self, ino: u32, start: u64, len: u64, keep_size: bool) -> FsResult<()> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        let mut inode = self.read_inode(ino)?;
        if !inode.is_reg() {
            return Err(if inode.is_dir() { FsError::IsDir } else { FsError::Invalid });
        }
        if !inode.uses_extents() || inode.flags() & (EXT4_HUGE_FILE_FL | EXT4_ENCRYPT_FL | EXT4_INLINE_DATA_FL) != 0 {
            return Err(FsError::Unsupported);
        }
        if len == 0 {
            return Ok(());
        }
        let bs = self.bs as u64;
        let end = start.checked_add(len).ok_or(FsError::TooBig)?;
        if end > (u32::MAX as u64) * bs {
            return Err(FsError::TooBig);
        }
        let first = (start / bs) as u32;
        let last = ((end - 1) / bs) as u32;
        let mut tree = self.tree_for(&inode)?;
        let mut exts = tree.exts.clone();
        let mut allocated: Vec<(u64, u32)> = Vec::new();
        let mut lb = first as u64;
        while lb <= last as u64 {
            if let Some(i) = find(&exts, lb as u32) {
                // Already mapped (written or unwritten): nothing to do.
                lb = exts[i].end();
                continue;
            }
            let nx = exts.partition_point(|e| e.lblk <= lb as u32);
            let next_start = if nx < exts.len() { exts[nx].lblk as u64 } else { u64::MAX };
            let hole_end = core::cmp::min(next_start, last as u64 + 1);
            let mut cur = lb;
            while cur < hole_end {
                let want = core::cmp::min(hole_end - cur, UNINIT_MAX as u64) as u32;
                let goal = self.write_goal(&exts, cur as u32, &inode);
                match self.alloc_blocks(goal, want) {
                    Ok((p, n)) => {
                        let pos = exts.partition_point(|x| x.lblk < cur as u32);
                        exts.insert(pos, Ext { lblk: cur as u32, len: n, pblk: p, uninit: true });
                        allocated.push((p, n));
                        cur += n as u64;
                    }
                    Err(e) => {
                        for (p, n) in allocated {
                            let _ = self.free_blocks(p, n as u64);
                        }
                        return Err(e);
                    }
                }
            }
            lb = hole_end;
        }
        merge_adjacent(&mut exts);
        if exts != tree.exts {
            if let Err(e) = self.rebuild_tree(&mut inode, &mut tree, exts) {
                for (p, n) in allocated {
                    let _ = self.free_blocks(p, n as u64);
                }
                return Err(e);
            }
        }
        let added: u64 = allocated.iter().map(|a| a.1 as u64).sum();
        inode.set_blocks512(inode.blocks512() + added * (bs / 512));
        if !keep_size && end > inode.size() {
            inode.set_size(end);
        }
        let t = now();
        inode.set_mtime(t);
        inode.set_ctime(t);
        self.write_inode(&mut inode)
    }

    pub(super) fn file_truncate(&mut self, ino: u32, size: u64) -> FsResult<()> {
        if self.ro {
            return Err(FsError::ReadOnly);
        }
        let mut inode = self.read_inode(ino)?;
        if !inode.is_reg() {
            return Err(if inode.is_dir() { FsError::IsDir } else { FsError::Invalid });
        }
        let bs = self.bs as u64;
        if size > (u32::MAX as u64) * bs {
            return Err(FsError::TooBig);
        }
        let cur = inode.size();
        if size >= cur {
            if size > cur {
                inode.set_size(size);
                let t = now();
                inode.set_mtime(t);
                inode.set_ctime(t);
                self.write_inode(&mut inode)?;
            }
            return Ok(());
        }
        if !inode.uses_extents() || inode.flags() & (EXT4_HUGE_FILE_FL | EXT4_ENCRYPT_FL | EXT4_INLINE_DATA_FL) != 0 {
            return Err(FsError::Unsupported);
        }
        let mut tree = self.tree_for(&inode)?;
        let keep = (size + bs - 1) / bs;
        let (new_exts, freed) = trim_tail(&tree.exts, keep);
        if new_exts != tree.exts {
            self.rebuild_tree(&mut inode, &mut tree, new_exts)?;
        }
        let mut nfreed = 0u64;
        for (p, n) in freed {
            self.free_blocks(p, n)?;
            nfreed += n;
        }
        let b = inode.blocks512().saturating_sub(nfreed * (bs / 512));
        inode.set_blocks512(b);
        // Zero the tail of the last kept block so a later extension reads
        // zeros there (through the cache).
        if size % bs != 0 {
            let lblk = (size / bs) as u32;
            if let Some(i) = find(&tree.exts, lblk) {
                let e = tree.exts[i];
                if !e.uninit {
                    let p = e.pblk + (lblk - e.lblk) as u64;
                    let mut blk = self.bread(p)?;
                    blk[(size % bs) as usize..].fill(0);
                    self.bwrite_data(p, blk)?;
                }
            }
        }
        inode.set_size(size);
        let t = now();
        inode.set_mtime(t);
        inode.set_ctime(t);
        self.write_inode(&mut inode)
    }

    // -------------------------------------------------------------- freeing

    fn is_fast_symlink(&self, inode: &Inode) -> bool {
        if !inode.is_symlink() || inode.uses_extents() || inode.size() >= 60 {
            return false;
        }
        let acl = if inode.file_acl() != 0 { (self.bs / 512) as u64 } else { 0 };
        inode.blocks512() == acl
    }

    pub(super) fn symlink_is_fast(&self, inode: &Inode) -> bool {
        self.is_fast_symlink(inode)
    }

    /// Free an extended-attribute block (shared blocks just lose a reference).
    fn free_xattr_block(&mut self, blk: u64) -> FsResult<()> {
        if blk < self.g.first_data as u64 || blk >= self.g.blocks {
            return Ok(());
        }
        let mut b = self.bread(blk)?;
        if le32(&b, 0) == XATTR_MAGIC {
            let refs = le32(&b, 4);
            if refs > 1 {
                put32(&mut b, 4, refs - 1);
                if self.g.metadata_csum {
                    put32(&mut b, 0x10, 0);
                    let mut c = crc32c(self.g.csum_seed, &blk.to_le_bytes());
                    c = crc32c(c, &b);
                    put32(&mut b, 0x10, c);
                }
                return self.bwrite(blk, b);
            }
        }
        self.free_blocks(blk, 1)
    }

    /// Release every block of an inode that is going away: data, tree nodes
    /// and the extended-attribute block.
    pub(super) fn free_inode_blocks(&mut self, inode: &Inode) -> FsResult<()> {
        self.ext_cache = None;
        let t = inode.mode() & S_IFMT;
        let has_data = t == S_IFREG || t == S_IFDIR || (t == S_IFLNK && !self.is_fast_symlink(inode));
        if has_data {
            let tree = self.load_tree(inode)?;
            for e in &tree.exts {
                self.free_blocks(e.pblk, e.len as u64)?;
            }
            for &n in &tree.nodes {
                self.free_blocks(n, 1)?;
            }
        }
        let acl = inode.file_acl();
        if acl != 0 {
            self.free_xattr_block(acl)?;
        }
        Ok(())
    }
}
