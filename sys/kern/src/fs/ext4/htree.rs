// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Hash-tree directory indexing (phase 1.5).  Up to now every directory was
// handled as a plain linear list and inserting into an indexed directory
// dropped the EXT4_INDEX_FL; this module keeps the index instead.
//
//   * dx_hash: the ext4 name hash (legacy / TEA / half-MD4, signed and
//     unsigned flavours) exactly as e2fsprogs computes it (verified against
//     `debugfs hash`), seeded from the superblock.
//   * dx_probe: walk the root (+ index nodes) to the leaf that owns a hash.
//   * dx_add: insert into the probed leaf; split leaves and index nodes
//     (entries redistributed by hash); grow the tree one level when the
//     root fills up.
//   * dx_convert: turn a multi-block linear directory into an indexed one
//     (what `e2fsck -D` does), triggered when a directory outgrows one
//     block, like the Linux driver.
//
// On-disk layout (verified against a real e2fsck-indexed image):
//   root: "." (rec_len 12), ".." (rec_len = bs-12), dx_root_info at 24
//         { reserved=0, hash_version, info_length=8, indirect_levels },
//         entries at 24+info_length.  Entry slot 0 overlaps the countlimit
//         {limit, count}, but its *block* field is the live entry for
//         hashes below entry[1].hash.
//   node: 8-byte fake dirent (ino=0, rec_len = bs), entries at 8, slot 0
//         again overlaid by the countlimit.
//   both: dx_tail { reserved, checksum } at ent_off + limit*8.  The
//         checksum covers [0, ent_off + count*8), then the tail's reserved
//         word, then 4 zero bytes (ext4_dx_csum in Linux).
//   leaf: plain dirent block with the 12-byte metadata_csum tail.

use super::crc::crc32c;
use super::dir::align4;
use super::disk::*;
use super::extent::{map_block, ExtTree};
use super::Ext4;
use crate::fs::vfs::{FsError, FsResult};
use alloc::vec;
use alloc::vec::Vec;

pub const HASH_LEGACY: u8 = 0;
pub const HASH_HALF_MD4: u8 = 1;
pub const HASH_TEA: u8 = 2;
pub const HASH_LEGACY_UNSIGNED: u8 = 3;
pub const HASH_HALF_MD4_UNSIGNED: u8 = 4;
pub const HASH_TEA_UNSIGNED: u8 = 5;

const DX_TAIL: usize = 8;
const DX_ENTRY: usize = 8;
const MAX_LEVELS: u8 = 3;

// ------------------------------------------------------------------- hash

/// The "half MD4" transform: 8 words in, 4 words of state.
fn half_md4_transform(buf: &mut [u32; 4], ins: &[u32; 8]) {
    const K1: u32 = 0;
    const K2: u32 = 0o13240474631;
    const K3: u32 = 0o15666365641;
    let mut v = *buf;
    // Target cycle is always a, d, c, b (ext4's ROUND macro chain).
    let tgt = [0usize, 3, 2, 1, 0, 3, 2, 1];
    macro_rules! fgd {
        (f) => {
            |x: u32, y: u32, z: u32| z ^ (x & (y ^ z))
        };
        (g) => {
            |x: u32, y: u32, z: u32| (x & y).wrapping_add((x ^ y) & z)
        };
        (h) => {
            |x: u32, y: u32, z: u32| x ^ y ^ z
        };
    }
    macro_rules! rnd {
        ($fun:ident, $t:expr, $x:expr, $s:expr) => {{
            let f = fgd!($fun);
            let (a, b, c, d) = (v[0], v[1], v[2], v[3]);
            let r = match $t {
                0 => a.wrapping_add(f(b, c, d)).wrapping_add($x).rotate_left($s),
                1 => b.wrapping_add(f(c, d, a)).wrapping_add($x).rotate_left($s),
                2 => c.wrapping_add(f(d, a, b)).wrapping_add($x).rotate_left($s),
                _ => d.wrapping_add(f(a, b, c)).wrapping_add($x).rotate_left($s),
            };
            v[$t] = r;
        }};
    }
    for i in 0..8 {
        rnd!(f, tgt[i], ins[i].wrapping_add(K1), [3, 7, 11, 19][i % 4]);
    }
    for (j, i) in [1usize, 3, 5, 7, 0, 2, 4, 6].iter().enumerate() {
        rnd!(g, tgt[j], ins[*i].wrapping_add(K2), [3, 5, 9, 13][j % 4]);
    }
    for (j, i) in [3usize, 7, 2, 6, 1, 5, 0, 4].iter().enumerate() {
        rnd!(h, tgt[j], ins[*i].wrapping_add(K3), [3, 9, 11, 15][j % 4]);
    }
    for k in 0..4 {
        buf[k] = buf[k].wrapping_add(v[k]);
    }
}

/// TEA transform: 4 words in, 2 words of state churned.
fn tea_transform(buf: &mut [u32; 4], ins: &[u32; 4]) {
    const DELTA: u32 = 0x9E37_79B9;
    let mut sum = 0u32;
    let (mut b0, mut b1) = (buf[0], buf[1]);
    let (a, b, c, d) = (ins[0], ins[1], ins[2], ins[3]);
    for _ in 0..16 {
        sum = sum.wrapping_add(DELTA);
        b0 = b0.wrapping_add(((b1 << 4).wrapping_add(a)) ^ b1.wrapping_add(sum) ^ ((b1 >> 5).wrapping_add(b)));
        b1 = b1.wrapping_add(((b0 << 4).wrapping_add(c)) ^ b0.wrapping_add(sum) ^ ((b0 >> 5).wrapping_add(d)));
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

/// The old (pre-htree) ext2 hash.
fn legacy_hash(name: &[u8], signed: bool) -> u32 {
    let (mut h0, mut h1) = (0x12a3fe2du32, 0x37abe8f9u32);
    for &b in name {
        let v: i32 = if signed { b as i8 as i32 } else { b as i32 };
        let mut h = h1.wrapping_add(h0 ^ v.wrapping_mul(7152373) as u32);
        if h & 0x8000_0000 != 0 {
            h = h.wrapping_sub(0x7fff_ffff);
        }
        h1 = h0;
        h0 = h;
    }
    h0 << 1
}

/// Pack `num` words from `msg` the way the kernel's str2hashbuf does:
/// big-endian words, the tail padded with the (remaining) length pattern.
fn str2hashbuf(msg: &[u8], num: usize, signed: bool) -> Vec<u32> {
    let len = msg.len();
    let mut pad = (len as u32) | ((len as u32) << 8);
    pad |= pad << 16;
    let mut take = len.min(num * 4);
    let mut out = Vec::with_capacity(num);
    let mut i = 0usize;
    while take >= 4 {
        let w = if signed {
            ((msg[i] as i8 as i32 as u32) << 24)
                | ((msg[i + 1] as i8 as i32 as u32 & 0xFF) << 16)
                | ((msg[i + 2] as i8 as i32 as u32 & 0xFF) << 8)
                | (msg[i + 3] as i8 as i32 as u32 & 0xFF)
        } else {
            ((msg[i] as u32) << 24) | ((msg[i + 1] as u32) << 16) | ((msg[i + 2] as u32) << 8) | msg[i + 3] as u32
        };
        out.push(w);
        i += 4;
        take -= 4;
    }
    let mut val = pad;
    for k in 0..take {
        let b: u32 = if signed {
            msg[i + k] as i8 as i32 as u32 & 0xFF
        } else {
            msg[i + k] as u32
        };
        val = b.wrapping_add(val << 8);
    }
    out.push(val);
    while out.len() < num {
        out.push(pad);
    }
    out.truncate(num);
    out
}

/// The ext4 directory hash of `name` -> (hash, minor_hash), exactly the
/// kernel's ext4fs_dirhash.  `seed` = the superblock hash seed words.
pub fn dx_hash_raw(name: &[u8], seed: &[u32; 4], version: u8) -> (u32, u32) {
    let mut buf = *seed;
    if seed == &[0, 0, 0, 0] {
        buf = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    }
    let signed = matches!(version, HASH_LEGACY | HASH_HALF_MD4 | HASH_TEA);
    let mut minor = 0u32;
    let mut hash;
    match version {
        HASH_LEGACY | HASH_LEGACY_UNSIGNED => {
            hash = legacy_hash(name, signed);
            minor = 0;
        }
        HASH_HALF_MD4 | HASH_HALF_MD4_UNSIGNED => {
            let mut p = 0usize;
            while p < name.len() {
                let words = str2hashbuf(&name[p..], 8, signed);
                let mut ins = [0u32; 8];
                ins.copy_from_slice(&words);
                half_md4_transform(&mut buf, &ins);
                p += 32;
            }
            minor = buf[2];
            hash = buf[1];
        }
        HASH_TEA | HASH_TEA_UNSIGNED => {
            let mut p = 0usize;
            while p < name.len() {
                let words = str2hashbuf(&name[p..], 4, signed);
                let mut ins = [0u32; 4];
                ins.copy_from_slice(&words);
                tea_transform(&mut buf, &ins);
                p += 16;
            }
            hash = buf[0];
            minor = buf[1];
        }
        _ => return (0, 0),
    }
    hash &= !1;
    // The EOF pseudo-hash must stay reserved.
    if hash == 0x7FFF_FFFE {
        hash = 0x7FFF_FFFC;
    }
    (hash, minor)
}

// ---------------------------------------------------------- block helpers

/// Checksum of an index block: covers [0, count_off + count*8), then the
/// tail's reserved word, then four zero bytes (the csum field zeroed).
pub(super) fn dx_csum(seed: u32, blk: &[u8], count_off: usize, count: usize, tail_off: usize) -> u32 {
    let mut c = crc32c(seed, &blk[..count_off + count * DX_ENTRY]);
    c = crc32c(c, &blk[tail_off..tail_off + 4]);
    c = crc32c(c, &[0, 0, 0, 0]);
    c
}

/// One level of the index on the way down to a leaf.
#[derive(Clone, Copy)]
pub(super) struct DxFrame {
    /// Physical block of this index block.
    pub pblk: u64,
    /// Offset of the entries area (32 for the root, 8 for a node).
    pub ent_off: usize,
    /// Slot we descended through (0 = the countlimit slot).
    pub at: usize,
}

/// Parsed index block header.
pub(super) struct DxHdr {
    pub ent_off: usize,
    pub limit: usize,
    pub count: usize,
    pub levels: u8,
    pub hash_version: u8,
}

impl Ext4 {
    // ---------------------------------------------------------- geometry

    fn dx_root_limit(&self) -> usize {
        let mut space = self.bs.saturating_sub(12 + 12 + 8);
        if self.g.metadata_csum {
            space = space.saturating_sub(DX_TAIL);
        }
        space / DX_ENTRY
    }

    fn dx_node_limit(&self) -> usize {
        let mut space = self.bs - 8;
        if self.g.metadata_csum {
            space -= DX_TAIL;
        }
        space / DX_ENTRY
    }

    /// Parse and sanity-check the dx header of a root block.
    pub(super) fn dx_parse_root(&self, blk: &[u8]) -> FsResult<DxHdr> {
        if blk.len() < self.bs || le16(blk, 4) != 12 {
            return Err(FsError::Corrupt);
        }
        // dotdot rec_len = bs - 12 (stored 0 for 64 KiB blocks).
        if le16(blk, 16) as usize != self.bs - 12 && !(self.bs == 65536 && le16(blk, 16) == 0) {
            return Err(FsError::Corrupt);
        }
        if le32(blk, 24) != 0 || blk[29] != 8 {
            return Err(FsError::Corrupt);
        }
        let hash_version = blk[28];
        let levels = blk[30];
        if hash_version >= 6 || levels > MAX_LEVELS {
            return Err(FsError::Corrupt);
        }
        let ent_off = 24 + blk[29] as usize;
        let limit = le16(blk, ent_off) as usize;
        let count = le16(blk, ent_off + 2) as usize;
        if limit != self.dx_root_limit() || count == 0 || count > limit {
            return Err(FsError::Corrupt);
        }
        Ok(DxHdr { ent_off, limit, count, levels, hash_version })
    }

    /// Parse and sanity-check the dx header of an index node block.
    fn dx_parse_node(&self, blk: &[u8]) -> FsResult<DxHdr> {
        if blk.len() < self.bs || le32(blk, 0) != 0 {
            return Err(FsError::Corrupt);
        }
        if le16(blk, 4) as usize != self.bs && !(self.bs == 65536 && le16(blk, 4) == 0) {
            return Err(FsError::Corrupt);
        }
        let ent_off = 8usize;
        let limit = le16(blk, ent_off) as usize;
        let count = le16(blk, ent_off + 2) as usize;
        if limit != self.dx_node_limit() || count == 0 || count > limit {
            return Err(FsError::Corrupt);
        }
        Ok(DxHdr { ent_off, limit, count, levels: 0, hash_version: 0 })
    }

    /// Verify the metadata_csum of an index block (root or node).
    pub(super) fn dx_verify_csum(&self, dir: &Inode, blk: &[u8], hdr: &DxHdr) -> FsResult<()> {
        if !self.g.metadata_csum {
            return Ok(());
        }
        let tail = hdr.ent_off + hdr.limit * DX_ENTRY;
        if tail + DX_TAIL > self.bs {
            return Err(FsError::Corrupt);
        }
        let want = le32(blk, tail + 4);
        let seed = self.inode_seed(dir.ino, dir.generation());
        let got = dx_csum(seed, blk, hdr.ent_off, hdr.count, tail);
        self.note_csum(want == got);
        if want != got {
            return Err(FsError::Corrupt);
        }
        Ok(())
    }

    /// Rewrite the metadata_csum of a modified index block.
    pub(super) fn dx_set_csum(&self, dir: &Inode, blk: &mut [u8], hdr: &DxHdr) {
        if !self.g.metadata_csum {
            return;
        }
        let tail = hdr.ent_off + hdr.limit * DX_ENTRY;
        let seed = self.inode_seed(dir.ino, dir.generation());
        let c = dx_csum(seed, blk, hdr.ent_off, hdr.count, tail);
        put32(blk, tail + 4, c);
    }

    fn dx_hash_of(&self, name: &[u8], version: u8) -> u32 {
        dx_hash_raw(name, &self.g.hash_seed, version).0
    }

    // -------------------------------------------------------------- probe

    /// Walk the index for `hash`; returns the frames (root first, deepest
    /// last) and the leaf's logical block number.
    pub(super) fn dx_probe(
        &mut self,
        dir: &Inode,
        tree: &ExtTree,
        hash: u32,
    ) -> FsResult<(Vec<DxFrame>, u32)> {
        let root_p = map_block(&tree.exts, 0).ok_or(FsError::Corrupt)?;
        let mut blk = self.bread(root_p)?;
        let hdr = self.dx_parse_root(&blk)?;
        self.dx_verify_csum(dir, &blk, &hdr)?;
        let mut levels = hdr.levels;
        let mut frames: Vec<DxFrame> = Vec::new();
        let mut cur_p = root_p;
        let mut ent_off = hdr.ent_off;
        loop {
            let count = le16(&blk, ent_off + 2) as usize;
            // Binary search: last slot with hash <= hash.  Slot 0 overlaps
            // the countlimit and counts as hash 0.
            let (mut lo, mut hi) = (1usize, count - 1);
            while lo <= hi {
                let m = lo + (hi - lo) / 2;
                if le32(&blk, ent_off + m * DX_ENTRY) > hash {
                    if m == 0 {
                        break;
                    }
                    hi = m - 1;
                } else {
                    lo = m + 1;
                }
            }
            let at = lo - 1;
            let child = le32(&blk, ent_off + at * DX_ENTRY + 4);
            frames.push(DxFrame { pblk: cur_p, ent_off, at });
            if levels == 0 {
                return Ok((frames, child));
            }
            levels -= 1;
            cur_p = map_block(&tree.exts, child).ok_or(FsError::Corrupt)?;
            blk = self.bread(cur_p)?;
            let nhdr = self.dx_parse_node(&blk)?;
            self.dx_verify_csum(dir, &blk, &nhdr)?;
            ent_off = nhdr.ent_off;
        }
    }

    // ------------------------------------------------------------- insert

    /// Insert `(hash, lblk)` after slot `at` of the index block at
    /// `frames[depth]`.  Splits the block (and its parents) when full;
    /// grows the tree when the root is full.
    fn dx_insert_index(
        &mut self,
        dir: &mut Inode,
        frames: &[DxFrame],
        depth: usize,
        hash: u32,
        lblk: u32,
    ) -> FsResult<()> {
        let fr = frames[depth];
        let mut blk = self.bread(fr.pblk)?;
        let hdr = if depth == 0 { self.dx_parse_root(&blk)? } else { self.dx_parse_node(&blk)? };
        if hdr.count < hdr.limit {
            return self.dx_write_entry(dir, fr.pblk, &mut blk, &hdr, fr.at + 1, hash, lblk);
        }
        // Full.  The root grows a level; a node splits.
        if depth == 0 {
            self.dx_grow_root(dir, frames, hash, lblk)
        } else {
            self.dx_split_node(dir, frames, depth, hash, lblk)
        }
    }

    /// Drop `(hash, lblk)` into index-block slot `at` (which must be free
    /// space), bump the count and rewrite the checksum.
    fn dx_write_entry(
        &mut self,
        dir: &Inode,
        pblk: u64,
        blk: &mut [u8],
        hdr: &DxHdr,
        at: usize,
        hash: u32,
        lblk: u32,
    ) -> FsResult<()> {
        let base = hdr.ent_off + at * DX_ENTRY;
        let tail = hdr.ent_off + hdr.count * DX_ENTRY;
        if at > hdr.count || tail + DX_ENTRY > hdr.ent_off + hdr.limit * DX_ENTRY + 1 {
            return Err(FsError::Corrupt);
        }
        blk.copy_within(base..tail, base + DX_ENTRY);
        put32(blk, base, hash);
        put32(blk, base + 4, lblk);
        put16(blk, hdr.ent_off + 2, (hdr.count + 1) as u16);
        let hdr = DxHdr { count: hdr.count + 1, ..*hdr };
        self.dx_set_csum(dir, blk, &hdr);
        self.bwrite(pblk, blk.to_vec())
    }

    /// The root is full: move its entries into a fresh index node, raise
    /// the root's indirect_levels, then insert the new entry into the node
    /// (which now has room).
    fn dx_grow_root(&mut self, dir: &mut Inode, frames: &[DxFrame], hash: u32, lblk: u32) -> FsResult<()> {
        let root_p = frames[0].pblk;
        let root = self.bread(root_p)?;
        let hdr = self.dx_parse_root(&root)?;
        let parent = le32(&root, 12); // ".." keeps pointing at the parent
        let node_p = self.dir_append_block(dir)?;
        let node_lblk = ((dir.size() / self.bs as u64) - 1) as u32;
        // Build the new node: fake dirent + a copy of the root's entries.
        let mut node = vec![0u8; self.bs];
        put16(&mut node, 4, self.bs as u16);
        let n_limit = self.dx_node_limit();
        let len = hdr.count * DX_ENTRY;
        node[8..8 + len].copy_from_slice(&root[hdr.ent_off..hdr.ent_off + len]);
        put16(&mut node, 8, n_limit as u16);
        put16(&mut node, 10, hdr.count as u16);
        let nhdr = DxHdr { ent_off: 8, limit: n_limit, count: hdr.count, levels: 0, hash_version: 0 };
        self.dx_set_csum(dir, &mut node, &nhdr);
        self.bwrite(node_p, node)?;

        // Rebuild the root: one entry, pointing at the node.
        let r_limit = self.dx_root_limit();
        let mut new_root = vec![0u8; self.bs];
        build_root_skeleton(&mut new_root, hdr.hash_version, hdr.levels + 1, r_limit, dir.ino, parent, self.g.filetype);
        put16(&mut new_root, hdr.ent_off + 2, 1);
        put32(&mut new_root, hdr.ent_off + 4, node_lblk);
        let rhdr = DxHdr { ent_off: hdr.ent_off, limit: r_limit, count: 1, levels: hdr.levels + 1, hash_version: hdr.hash_version };
        self.dx_set_csum(dir, &mut new_root, &rhdr);
        self.bwrite(root_p, new_root)?;

        // Insert the new entry into the (now roomy) node, via a fresh probe.
        self.dx_insert_by_probe(dir, hash, lblk)
    }

    /// Split a full index node at `depth`: entries from the median on move
    /// to a new node, the median boundary goes to the parent (splitting it
    /// in turn when it is full too), and the new entry is re-probed into
    /// whichever half owns its hash.
    fn dx_split_node(&mut self, dir: &mut Inode, frames: &[DxFrame], depth: usize, hash: u32, lblk: u32) -> FsResult<()> {
        let fr = frames[depth];
        let mut blk = self.bread(fr.pblk)?;
        let hdr = self.dx_parse_node(&blk)?;
        let mid = hdr.count / 2;
        let med_hash = le32(&blk, hdr.ent_off + mid * DX_ENTRY);
        let _ = med_hash;

        // New node takes entries [mid, count).
        let new_p = self.dir_append_block(dir)?;
        let new_lblk = ((dir.size() / self.bs as u64) - 1) as u32;
        let mut node = vec![0u8; self.bs];
        put16(&mut node, 4, self.bs as u16);
        let n_limit = self.dx_node_limit();
        let move_len = (hdr.count - mid) * DX_ENTRY;
        node[8..8 + move_len].copy_from_slice(&blk[hdr.ent_off + mid * DX_ENTRY..hdr.ent_off + hdr.count * DX_ENTRY]);
        put16(&mut node, 8, n_limit as u16);
        put16(&mut node, 10, (hdr.count - mid) as u16);
        let nhdr = DxHdr { ent_off: 8, limit: n_limit, count: hdr.count - mid, levels: 0, hash_version: 0 };
        self.dx_set_csum(dir, &mut node, &nhdr);
        self.bwrite(new_p, node)?;

        // Old node keeps [0, mid).
        put16(&mut blk, hdr.ent_off + 2, mid as u16);
        let ohdr = DxHdr { count: mid, ..hdr };
        self.dx_set_csum(dir, &mut blk, &ohdr);
        self.bwrite(fr.pblk, blk)?;

        // Median boundary -> parent (may recurse); then the original entry
        // lands on whichever half owns its hash, via a fresh probe.
        self.dx_insert_index(dir, frames, depth - 1, med_hash, new_lblk)?;
        self.dx_insert_by_probe(dir, hash, lblk)
    }

    /// Re-probe from the root and insert `(hash, lblk)` into the deepest
    /// index block (used after structural changes invalidate the frames).
    fn dx_insert_by_probe(&mut self, dir: &mut Inode, hash: u32, lblk: u32) -> FsResult<()> {
        for _ in 0..8 {
            let tree = self.tree_for(dir)?;
            let (frames, _) = self.dx_probe(dir, &tree, hash)?;
            let depth = frames.len() - 1;
            let fr = frames[depth];
            let mut blk = self.bread(fr.pblk)?;
            let hdr = if depth == 0 { self.dx_parse_root(&blk)? } else { self.dx_parse_node(&blk)? };
            if hdr.count < hdr.limit {
                return self.dx_write_entry(dir, fr.pblk, &mut blk, &hdr, fr.at + 1, hash, lblk);
            }
            // Full again: split / grow through the generic path.
            self.dx_insert_index(dir, &frames, depth, hash, lblk)?;
            return Ok(());
        }
        Err(FsError::Corrupt)
    }

    // ------------------------------------------------------ dir_add (dx)

    /// Add `name -> ino` to an indexed directory, keeping the index.
    pub(super) fn dx_add(&mut self, dir: &mut Inode, name: &[u8], ino: u32, mode: u16) -> FsResult<()> {
        let ft = if self.g.filetype { dirent_type(mode) } else { 0 };
        // The hash version is authoritative in the root block.
        let tree = self.tree_for(dir)?;
        let root_p = map_block(&tree.exts, 0).ok_or(FsError::Corrupt)?;
        let root_blk = self.bread(root_p)?;
        let hdr = self.dx_parse_root(&root_blk)?;
        let h = self.dx_hash_of(name, hdr.hash_version);
        let (frames, leaf_lblk) = self.dx_probe(dir, &tree, h)?;
        let leaf_p = map_block(&tree.exts, leaf_lblk).ok_or(FsError::Corrupt)?;
        let mut leaf = self.bread(leaf_p)?;
        if !self.try_insert(&mut leaf, name, ino, ft)? {
            // Fragmented leaf: pack the live entries before splitting.
            self.compact_leaf(&mut leaf)?;
            if !self.try_insert(&mut leaf, name, ino, ft)? {
                return self.dx_split_leaf(dir, &frames, leaf_p, &leaf, h, name, ino, ft);
            }
        }
        self.set_dir_csum(dir, &mut leaf);
        self.bwrite(leaf_p, leaf)
    }

    /// Split a full leaf: live entries are redistributed by hash into the
    /// old and a new leaf (balanced by bytes, not by count), the new entry
    /// joins its side, and `(split_hash, new_leaf)` is added to the index.
    #[allow(clippy::too_many_arguments)]
    fn dx_split_leaf(
        &mut self,
        dir: &mut Inode,
        frames: &[DxFrame],
        leaf_p: u64,
        leaf: &[u8],
        h: u32,
        name: &[u8],
        ino: u32,
        ft: u8,
    ) -> FsResult<()> {
        let version = {
            let rb = self.bread(frames[0].pblk)?;
            self.dx_parse_root(&rb)?.hash_version
        };
        // Live entries (skip "." / ".."), with their hashes.
        let mut map: Vec<(u32, usize, usize, u32, u8)> = Vec::new(); // (hash, off, name_len, ino, ftype)
        for e in self.parse_dir_block(leaf)? {
            if e.ino == 0 {
                continue;
            }
            let nm = &leaf[e.off + 8..e.off + 8 + e.name_len];
            if nm == b"." || nm == b".." {
                continue;
            }
            map.push((self.dx_hash_of(nm, version), e.off, e.name_len, e.ino, e.ftype));
        }
        map.sort_by_key(|m| m.0);
        if map.len() < 2 {
            return Err(FsError::Unsupported); // cannot split a 1-entry leaf
        }
        let usable = self.usable_dirent_bytes();
        // Balance by bytes: the left side takes entries until it holds at
        // least half the block.
        let mut acc = 0usize;
        let mut mid = 1usize;
        for (i, (_, _, nl, _, _)) in map.iter().enumerate() {
            acc += 8 + align4(*nl);
            if acc >= usable / 2 {
                mid = i + 1;
                break;
            }
        }
        mid = mid.min(map.len() - 1).max(1);
        let split_hash = map[mid].0;
        if split_hash == map[0].0 {
            // Degenerate: every name in the leaf hashes the same; an index
            // split cannot separate them (Linux keeps walking continuation
            // blocks instead).
            return Err(FsError::Unsupported);
        }

        // New leaf block at the end of the directory.
        let new_p = self.dir_append_block(dir)?;
        let new_lblk = ((dir.size() / self.bs as u64) - 1) as u32;
        let mut old_blk = self.empty_leaf_block();
        let mut new_blk = self.empty_leaf_block();
        let push = |blk: &mut [u8], cursor: &mut usize, ino_: u32, ft_: u8, nm: &[u8]| {
            let used = 8 + align4(nm.len());
            Self::write_dirent(blk, *cursor, ino_, used, nm, ft_);
            *cursor += used;
        };
        let (mut co, mut cn) = (0usize, 0usize);
        for (i, (_, off, nl, eino, eft)) in map.iter().enumerate() {
            let nm = &leaf[off + 8..off + 8 + nl];
            if i < mid {
                push(&mut old_blk, &mut co, *eino, *eft, nm);
            } else {
                push(&mut new_blk, &mut cn, *eino, *eft, nm);
            }
        }
        if h >= split_hash {
            push(&mut new_blk, &mut cn, ino, ft, name);
        } else {
            push(&mut old_blk, &mut co, ino, ft, name);
        }
        self.finish_leaf(dir, &mut old_blk, co)?;
        self.finish_leaf(dir, &mut new_blk, cn)?;
        self.bwrite(leaf_p, old_blk)?;
        self.bwrite(new_p, new_blk)?;
        // Boundary entry for the new leaf goes into the deepest index block.
        let r = self.dx_insert_index(dir, frames, frames.len() - 1, split_hash, new_lblk);
        r
    }

    // ------------------------------------------------------------- helpers

    /// Bytes of a leaf block usable for dirents (before the checksum tail).
    pub(super) fn usable_dirent_bytes(&self) -> usize {
        self.bs - if self.g.metadata_csum { 12 } else { 0 }
    }

    /// A fresh empty leaf block: one free dirent covering the usable space,
    /// plus the checksum tail skeleton when metadata_csum is on.
    pub(super) fn empty_leaf_block(&self) -> Vec<u8> {
        let bs = self.bs;
        let usable = self.usable_dirent_bytes();
        let mut b = vec![0u8; bs];
        put16(&mut b, 4, usable as u16);
        if self.g.metadata_csum {
            put16(&mut b, bs - 12 + 4, 12);
            b[bs - 12 + 7] = FT_TAIL;
        }
        b
    }

    /// Pack the live entries of `blk` at its start and let the last entry
    /// absorb the remaining space (its slack is what try_insert splits;
    /// a leftover of less than 8 bytes cannot hold a free dirent).
    pub(super) fn compact_leaf(&mut self, blk: &mut [u8]) -> FsResult<()> {
        let ents = self.parse_dir_block(blk)?;
        let names: Vec<(u32, u8, Vec<u8>)> = ents
            .iter()
            .filter(|e| e.ino != 0)
            .map(|e| (e.ino, e.ftype, blk[e.off + 8..e.off + 8 + e.name_len].to_vec()))
            .collect();
        if names.is_empty() {
            return Ok(());
        }
        let usable = self.usable_dirent_bytes();
        let (mut cur, mut last_off) = (0usize, 0usize);
        for (ino, ft, nm) in &names {
            let r = 8 + align4(nm.len());
            Self::write_dirent(blk, cur, *ino, r, nm, *ft);
            last_off = cur;
            cur += r;
        }
        if cur > usable {
            return Err(FsError::Corrupt);
        }
        put16(blk, last_off + 4, (usable - last_off) as u16);
        Ok(())
    }

    /// Extend the last dirent of a freshly packed leaf to the end of the
    /// usable area and stamp the checksum tail.  `cursor` is the end offset
    /// of the packed entries (the gap after it is still zeroes, so the
    /// block is not parseable yet -- walk by rec_len instead).
    fn finish_leaf(&mut self, dir: &Inode, blk: &mut [u8], cursor: usize) -> FsResult<()> {
        let usable = self.usable_dirent_bytes();
        if cursor == 0 {
            // Nothing live: one free dirent spans the block.
            put16(blk, 0, 0);
            put16(blk, 4, usable as u16);
        } else {
            let mut off = 0usize;
            while off < cursor {
                let rec = le16(blk, off + 4) as usize;
                if rec < 8 || off + rec > cursor {
                    return Err(FsError::Corrupt);
                }
                if off + rec == cursor {
                    // The last entry: its slack runs to the usable end.
                    put16(blk, off + 4, (usable - off) as u16);
                    break;
                }
                off += rec;
            }
        }
        self.set_dir_csum(dir, blk);
        Ok(())
    }

    // ----------------------------------------------------------- convert

    /// Turn a multi-block linear directory into an indexed one (what
    /// `e2fsck -D` does): every entry is rehashed and redistributed by
    /// hash over the existing blocks (plus new ones when needed), block 0
    /// becomes the dx root.  Called from dir_add when the directory has
    /// outgrown a single block and the volume supports indexing.
    pub(super) fn dx_convert(&mut self, dir: &mut Inode) -> FsResult<()> {
        if !self.g.dir_index || self.g.hash_version >= 6 {
            return Err(FsError::Unsupported);
        }
        let tree = self.tree_for(dir)?;
        let nblocks = ((dir.size() + self.bs as u64 - 1) / self.bs as u64) as u32;
        if nblocks < 2 {
            return Err(FsError::Unsupported);
        }
        // 1. Collect the live entries and the parent inode.
        let mut parent = 0u32;
        let mut ents: Vec<(u32, u32, u8, Vec<u8>)> = Vec::new(); // (hash, ino, ftype, name)
        for lb in 0..nblocks {
            let Some(p) = map_block(&tree.exts, lb) else { continue };
            let b = self.bread(p)?;
            for e in self.parse_dir_block(&b)? {
                if e.ino == 0 {
                    continue;
                }
                let nm = &b[e.off + 8..e.off + 8 + e.name_len];
                if nm == b"." || nm == b".." {
                    if nm == b".." {
                        parent = e.ino;
                    }
                    continue;
                }
                let h = self.dx_hash_of(nm, self.g.hash_version);
                ents.push((h, e.ino, e.ftype, nm.to_vec()));
            }
        }
        if parent == 0 {
            return Err(FsError::Corrupt);
        }
        ents.sort_by(|a, b| a.0.cmp(&b.0).then(a.3.cmp(&b.3)));

        // 2. Distribute over the leaves.  Every existing block (1..nblocks)
        //    stays reachable; append more when the data does not fit.
        let usable = self.usable_dirent_bytes();
        let total: usize = ents.iter().map(|(_, _, _, n)| 8 + align4(n.len())).sum();
        let want = ((total + usable - 1) / usable).max(1); // leaves needed by size
        let mut nleaves = want.max((nblocks - 1) as usize); // keep old blocks reachable
        let root_limit = self.dx_root_limit();
        if nleaves > root_limit - 1 {
            // Would need another tree level; stay linear.
            return Err(FsError::Unsupported);
        }
        let mut leaf_lblks: Vec<u32> = (1..nblocks).collect();
        while leaf_lblks.len() < nleaves {
            self.dir_append_block(dir)?;
            leaf_lblks.push(((dir.size() / self.bs as u64) - 1) as u32);
        }
        if nleaves > leaf_lblks.len() {
            nleaves = leaf_lblks.len();
        }

        // 3. Fill the leaves in hash order, roughly half full each so an
        //    insert does not immediately split.
        let target = (total / nleaves).max(usable / 2);
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        let mut cursors: Vec<usize> = Vec::new();
        let mut cur_blk = self.empty_leaf_block();
        let mut cur_used = 0usize;
        for (_, ino, ft, nm) in &ents {
            let need = 8 + align4(nm.len());
            if cur_used + need > target && cur_used > 0 && blocks.len() + 1 < nleaves {
                blocks.push(cur_blk);
                cursors.push(cur_used);
                cur_blk = self.empty_leaf_block();
                cur_used = 0;
            }
            if cur_used + need > usable {
                return Err(FsError::Unsupported); // one giant entry: no leaf fits
            }
            Self::write_dirent(&mut cur_blk, cur_used, *ino, need, nm, *ft);
            cur_used += need;
        }
        blocks.push(cur_blk);
        cursors.push(cur_used);

        // 4. Write the leaves and build the root index.
        let mut root = vec![0u8; self.bs];
        build_root_skeleton(&mut root, self.g.hash_version, 0, root_limit, dir.ino, parent, self.g.filetype);
        let mut count = 1usize;
        let mut prev_hash = 0u32;
        for (i, blk) in blocks.iter_mut().enumerate() {
            let p = map_block(&self.tree_for(dir)?.exts, leaf_lblks[i]).ok_or(FsError::Corrupt)?;
            self.finish_leaf(dir, blk, cursors[i])?;
            self.bwrite(p, blk.clone())?;
            let first = le32(blk, 0);
            let _ = first;
            // Index entry i: slot 0 (the countlimit slot) covers the first
            // leaf, real slots the rest.
            if i > 0 {
                // The boundary hash: hash of the first name in this leaf.
                let e = self.parse_dir_block(blk)?.iter().find(|e| e.ino != 0).copied();
                let Some(e) = e else { continue }; // empty leaf: reachable, no entry needed
                let nm = &blk[e.off + 8..e.off + 8 + e.name_len];
                let bh = self.dx_hash_of(nm, self.g.hash_version);
                put32(&mut root, 32 + count * DX_ENTRY, bh);
                put32(&mut root, 32 + count * DX_ENTRY + 4, leaf_lblks[i]);
                count += 1;
                prev_hash = bh;
            } else {
                put32(&mut root, 32 + 4, leaf_lblks[0]);
            }
        }
        let _ = prev_hash;
        put16(&mut root, 32 + 2, count as u16);
        let rhdr = self.dx_parse_root(&root)?;
        self.dx_set_csum(dir, &mut root, &rhdr);
        let root_p = map_block(&self.tree_for(dir)?.exts, 0).ok_or(FsError::Corrupt)?;
        self.bwrite(root_p, root)?;
        dir.set_flags(dir.flags() | EXT4_INDEX_FL);
        Ok(())
    }
}

/// Write the fixed parts of a fresh dx root block (dirents + info + countlimit).
pub(super) fn build_root_skeleton(
    blk: &mut [u8],
    hash_version: u8,
    levels: u8,
    limit: usize,
    ino: u32,
    parent: u32,
    filetype: bool,
) {
    let bs = blk.len();
    let ft = if filetype { FT_DIR } else { 0 };
    blk.fill(0);
    put32(blk, 0, ino);
    put16(blk, 4, 12);
    blk[6] = 1;
    blk[7] = ft;
    blk[8] = b'.';
    put32(blk, 12, parent);
    put16(blk, 16, (bs - 12) as u16);
    blk[18] = 2;
    blk[19] = ft;
    blk[20] = b'.';
    blk[21] = b'.';
    put32(blk, 24, 0);
    blk[28] = hash_version;
    blk[29] = 8;
    blk[30] = levels;
    put16(blk, 32, limit as u16);
    put16(blk, 34, 0);
}
