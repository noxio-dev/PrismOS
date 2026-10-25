// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// ext4 journal (jbd2) support, phase 1.5: replay of a dirty journal at
// mount, plus a minimal writer that journals every metadata change of an
// operation into one transaction.
//
// The on-disk format is the stock jbd2 one -- which is entirely BIG-ENDIAN
// (jbd2 grew out of big-endian hardware, every multi-byte field is __be32),
// so e2fsck can replay what we write and vice versa (verified by
// tests/fs_test.py, which lets e2fsck replay our transactions):
//
//   journal superblock (first block of the journal inode, type 3/4):
//     [0x00] magic 0xC03B3998, [0x04] blocktype, [0x08] s_start (first
//     un-checkpointed transaction, 0 = clean), [0x0C] s_sequence (next
//     expected), then the v2 feature words.
//   descriptor (type 1): 12-byte header {magic, type, sequence} then a
//     packed list of tags; data block numbers in file-system order.
//   commit (type 2): the same header with the same sequence.
//   data block: the block image itself; a block starting with the jbd2
//     magic is stored zeroed in its first word and flagged ESCAPE.
//
// Writer discipline (the "minimal jbd2" the phase 1.5 checklist asks for):
// one transaction per operation, written descriptor + data + commit with a
// device flush in between, checkpoint deferred to `sync` / the periodic
// flush / a full journal.  File data is flushed before the commit block is
// written (ordered-mode guarantee), so a crash either loses the whole
// operation or keeps it whole; the journal then protects the metadata
// until it is checkpointed.

use super::disk::*;
use super::extent::{map_block, ExtTree};
use super::Ext4;
use crate::fs::vfs::{FsError, FsResult};
use alloc::vec;
use alloc::vec::Vec;

pub const JBD2_MAGIC: u32 = 0xC03B_3998;
const SB_V1: u32 = 3;
const SB_V2: u32 = 4;
const T_DESCRIPTOR: u32 = 1;
const T_COMMIT: u32 = 2;

const F_ESCAPE: u32 = 1;
const F_SAME_UUID: u32 = 2;
const F_LAST_TAG: u32 = 8;

// jbd2 feature_incompat bits we understand (in the v2 superblock).
const JBD2_INCOMPAT_REVOKE: u32 = 0x1;
const JBD2_INCOMPAT_64BIT: u32 = 0x2;
const JBD2_INCOMPAT_UNSUPPORTED: u32 = !(JBD2_INCOMPAT_REVOKE | JBD2_INCOMPAT_64BIT);

// ---- big-endian helpers: the whole jbd2 on-disk format is __be32 ----

#[inline]
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[inline]
fn put_be32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

/// The journal of a mounted volume.  `head` is the next free log position
/// (block index inside the journal), `start` the oldest transaction that
/// has not been checkpointed yet (0 = clean).
pub(super) struct Journal {
    pub ino: u32,
    tree: ExtTree,
    len: u64,
    head: u64,
    next_seq: u32,
    /// Sequence of the oldest un-checkpointed transaction (what the sb's
    /// s_sequence must show while s_start != 0: the log replays from
    /// s_start with sequence s_sequence).
    first_seq: u32,
    pub(super) start: u64,
    tag_size: usize,
    uuid: [u8; 16],
    /// The original journal superblock image: updates are read-modify-write
    /// so the uuid / features / nr_users fields survive (e2fsck checks them).
    sb_img: Vec<u8>,
}

impl Journal {
    /// Load the journal of `fs` (superblock + journal inode + jbd2
    /// superblock).  Returns None when there is none, or Some(Err) when it
    /// is unusable (unknown features, corrupt header): the caller then
    /// mounts read-only.
    pub(super) fn load(fs: &mut Ext4, sb: &[u8]) -> Option<FsResult<Journal>> {
        if le32(sb, 0x5C) & COMPAT_HAS_JOURNAL == 0 {
            return None;
        }
        let ino = le32(sb, 0xE0);
        if ino == 0 || ino > fs.g.inodes {
            return None;
        }
        let jino = match fs.read_inode(ino) {
            Ok(i) => i,
            Err(_) => return None,
        };
        if !jino.is_reg() || jino.size() == 0 {
            return None;
        }
        let tree = match fs.load_tree(&jino) {
            Ok(t) => t,
            Err(_) => return None,
        };
        let len = jino.size() / fs.bs as u64;
        if len < 16 || len > (1 << 22) {
            return None;
        }
        let Some(p0) = map_block(&tree.exts, 0) else { return None };
        let mut jsb = vec![0u8; fs.bs];
        if fs.dev.read(p0 * fs.spb, &mut jsb).is_err() {
            return None;
        }
        if be32(&jsb, 0) != JBD2_MAGIC {
            return None;
        }
        let btype = be32(&jsb, 4);
        if btype != SB_V1 && btype != SB_V2 {
            return None;
        }
        // journal_superblock_t: h_sequence 0x08, s_blocksize 0x0C, s_maxlen
        // 0x10, s_first 0x14, s_sequence 0x18, s_start 0x1C, errno 0x20,
        // compat 0x24, incompat 0x28, ro 0x2C, uuid 0x30.
        if be32(&jsb, 0xC) != fs.bs as u32 || be32(&jsb, 0x10) != len as u32 {
            return Some(Err(FsError::Unsupported));
        }
        let mut tag_size = 8usize; // blocknr, checksum16 (unused), flags16
        if btype == SB_V2 {
            let incompat = be32(&jsb, 0x28);
            if incompat & JBD2_INCOMPAT_UNSUPPORTED != 0 {
                return Some(Err(FsError::Unsupported));
            }
            if incompat & JBD2_INCOMPAT_64BIT != 0 {
                tag_size += 4;
            }
        }
        let start = be32(&jsb, 0x1C) as u64;
        let seq = be32(&jsb, 0x18);
        if start >= len {
            return Some(Err(FsError::Corrupt));
        }
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&sb[0x68..0x78]);
        let sb_img = jsb.clone();
        // Clean journal: s_sequence is the next sequence to write; dirty:
        // the first one to replay (replay() advances next_seq itself).
        let next_seq = if start == 0 { seq.max(1) } else { seq.max(1) };
        Some(Ok(Journal { ino, tree, len, head: start.max(1), next_seq, first_seq: seq.max(1), start, tag_size, uuid, sb_img }))
    }

    fn pblk_of(&self, _fs: &Ext4, lblk: u64) -> FsResult<u64> {
        map_block(&self.tree.exts, lblk as u32).ok_or(FsError::Corrupt)
    }

    fn read_log(&self, fs: &Ext4, lblk: u64, buf: &mut [u8]) -> FsResult<()> {
        let p = self.pblk_of(fs, lblk)?;
        fs.dev.read(p * fs.spb, buf).map_err(FsError::from)
    }

    fn write_log(&self, fs: &Ext4, lblk: u64, buf: &[u8]) -> FsResult<()> {
        let p = self.pblk_of(fs, lblk)?;
        fs.dev.write(p * fs.spb, buf).map_err(FsError::from)
    }

    /// Number of free log blocks between `head` and `start` (circular).
    fn free_blocks(&self) -> u64 {
        if self.start == 0 {
            self.len - 1 // everything but the superblock block
        } else if self.head >= self.start {
            self.len - 1 - (self.head - self.start)
        } else {
            self.start - self.head - 1
        }
    }

    /// Write the journal superblock: a read-modify-write of the original
    /// image (the uuid, features and nr_users fields must survive, e2fsck
    /// validates them).  While transactions are pending, s_sequence shows
    /// the sequence of the FIRST pending one (that is where a replay
    /// starts); when clean, the next sequence to be written.
    fn write_sb(&self, fs: &Ext4) -> FsResult<()> {
        let mut jsb = self.sb_img.clone();
        put_be32(&mut jsb, 0, JBD2_MAGIC);
        put_be32(&mut jsb, 4, SB_V2);
        let seq = if self.start != 0 { self.first_seq } else { self.next_seq };
        put_be32(&mut jsb, 0x18, seq); // s_sequence
        put_be32(&mut jsb, 0x1C, self.start as u32); // s_start
        put_be32(&mut jsb, 0x20, 0); // s_errno
        self.write_log(fs, 0, &jsb)
    }

    // ------------------------------------------------------------ replay

    /// Replay every committed transaction from `s_start`.  Returns the
    /// number of transactions applied.  Uncommitted tails are skipped.
    pub(super) fn replay(&mut self, fs: &mut Ext4) -> FsResult<u32> {
        let bs = fs.bs;
        if self.start == 0 {
            return Ok(0);
        }
        let mut pos = self.start;
        let mut seq = self.next_seq;
        let mut done = 0u32;
        let mut hdr = vec![0u8; bs];
        for _ in 0..65536 {
            // Descriptor?
            self.read_log(fs, pos, &mut hdr)?;
            if be32(&hdr, 0) != JBD2_MAGIC || be32(&hdr, 4) != T_DESCRIPTOR || be32(&hdr, 8) != seq {
                break;
            }
            // Parse the tag list.  Tag = blocknr BE32, reserved csum BE16,
            // flags BE16 (+ blocknr_high BE32 with the 64bit feature); the
            // file-system uuid (16 bytes) follows every tag without
            // SAME_UUID.
            let mut tags: Vec<(u64, bool)> = Vec::new(); // (fs block, escape)
            let mut off = 12usize;
            loop {
                if off + self.tag_size + 16 > bs {
                    break; // no room for another tag
                }
                let blocknr = be32(&hdr, off) as u64 | if self.tag_size > 8 {
                    (be32(&hdr, off + 8) as u64) << 32
                } else {
                    0
                };
                let flags = u16::from_be_bytes([hdr[off + 6], hdr[off + 7]]) as u32;
                off += self.tag_size;
                if flags & F_SAME_UUID == 0 {
                    off += 16; // the uuid follows
                }
                tags.push((blocknr, flags & F_ESCAPE != 0));
                if flags & F_LAST_TAG != 0 {
                    break;
                }
            }
            if tags.is_empty() {
                break;
            }
            // Read the data blocks, then look for the commit that follows.
            let mut data: Vec<Vec<u8>> = Vec::new();
            let mut cur = step(pos, self.len);
            for _ in 0..tags.len() {
                let mut b = vec![0u8; bs];
                self.read_log(fs, cur, &mut b)?;
                data.push(b);
                cur = step(cur, self.len);
            }
            // Commit block with the same sequence?
            self.read_log(fs, cur, &mut hdr)?;
            let commit_ok = be32(&hdr, 0) == JBD2_MAGIC && be32(&hdr, 4) == T_COMMIT && be32(&hdr, 8) == seq;
            if !commit_ok {
                break; // uncommitted tail: stop before applying anything
            }
            // Apply: write every tagged block back to the file system.
            for (i, (blocknr, escape)) in tags.iter().enumerate() {
                let mut b = data[i].clone();
                if *escape {
                    put32(&mut b, 0, JBD2_MAGIC); // restore the escaped word
                }
                if *blocknr >= fs.g.blocks {
                    return Err(FsError::Corrupt);
                }
                fs.cache_insert(*blocknr, b, true, false);
            }
            done += 1;
            seq += 1;
            pos = step(cur, self.len);
        }
        if done > 0 {
            self.head = pos;
            self.next_seq = seq;
        }
        Ok(done)
    }

    /// Mark everything checkpointed: s_start back to 0.
    pub(super) fn checkpoint_done(&mut self, fs: &Ext4) -> FsResult<()> {
        self.start = 0;
        self.head = 1;
        self.write_sb(fs)?;
        fs.dev.flush().map_err(FsError::from)
    }

    // ------------------------------------------------------------ writer

    /// Append one transaction holding `blocks` (fs block -> content) and
    /// write the commit.  File data must already be on the device (ordered
    /// mode).  Checkpoints the journal first when it is too full.
    pub(super) fn commit_txn(&mut self, fs: &mut Ext4, blocks: &[(u64, Vec<u8>)]) -> FsResult<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        let bs = fs.bs;
        let per_tag = self.tag_size;
        let tags_per_desc = (bs - 12 - 16).saturating_sub(per_tag) / per_tag + 1;
        let need = 1 + blocks.len().div_ceil(tags_per_desc) + blocks.len() + 1;
        if self.free_blocks() < need as u64 + 1 {
            // Journal full: flush the pending blocks to their final place
            // (the caller's cache write-back) and restart the log.
            fs.checkpoint()?;
            // `fs.jrn` is taken (we are `self`), so fs.checkpoint() could not
            // touch the on-disk journal superblock: release the log here,
            // persistently, before any new transaction overwrites it.
            self.checkpoint_done(fs)?;
        }
        let saved = (self.start, self.first_seq, self.next_seq, self.head);
        let txn_start = if self.head == 0 { 1 } else { self.head };
        let seq = self.next_seq;
        let mut pos = txn_start;

        // Descriptor blocks (several when the tag list overflows).
        let mut idx = 0usize;
        while idx < blocks.len() {
            let mut desc = vec![0u8; bs];
            put_be32(&mut desc, 0, JBD2_MAGIC);
            put_be32(&mut desc, 4, T_DESCRIPTOR);
            put_be32(&mut desc, 8, seq);
            let mut off = 12usize;
            let n = (blocks.len() - idx).min(tags_per_desc);
            for k in 0..n {
                let (blocknr, data) = &blocks[idx + k];
                let escaped = le32(data, 0) == JBD2_MAGIC;
                // Exactly the kernel's scheme (jbd2/commit.c): the first
                // tag of a descriptor carries no SAME_UUID and the
                // file-system uuid follows it; later tags are SAME_UUID;
                // the final tag also carries LAST_TAG.
                let mut flags = 0u32;
                if escaped {
                    flags |= F_ESCAPE;
                }
                if k > 0 {
                    flags |= F_SAME_UUID;
                }
                if k + 1 == n && idx + n == blocks.len() {
                    flags |= F_LAST_TAG;
                }
                put_be32(&mut desc, off, *blocknr as u32);
                desc[off + 4] = 0; // reserved truncated csum
                desc[off + 5] = 0;
                desc[off + 6] = (flags >> 8) as u8; // flags: __be16
                desc[off + 7] = flags as u8;
                if per_tag > 8 {
                    put_be32(&mut desc, off + 8, (*blocknr >> 32) as u32);
                }
                off += per_tag;
                if k == 0 {
                    // The uuid follows the first tag.
                    desc[off..off + 16].copy_from_slice(&self.uuid);
                    off += 16;
                }
            }
            self.write_log(fs, pos, &desc)?;
            pos = step(pos, self.len);
            // The data blocks of this descriptor.
            for k in 0..n {
                let mut data = blocks[idx + k].1.clone();
                if le32(&data, 0) == JBD2_MAGIC {
                    // Escaped: the stored image is zeroed in its first word.
                    put32(&mut data, 0, 0);
                }
                self.write_log(fs, pos, &data)?;
                pos = step(pos, self.len);
            }
            idx += n;
        }

        // Commit block, then publish: flush the device before the journal
        // superblock claims the transaction.
        let mut commit = vec![0u8; bs];
        put_be32(&mut commit, 0, JBD2_MAGIC);
        put_be32(&mut commit, 4, T_COMMIT);
        put_be32(&mut commit, 8, seq);
        for b in commit[12..].iter_mut() {
            *b = 0x5a; // jbd2 fills the commit payload with a pattern
        }
        self.write_log(fs, pos, &commit)?;
        fs.dev.flush().map_err(FsError::from)?;
        if self.start == 0 {
            self.start = txn_start;
            self.first_seq = seq;
        }
        self.next_seq = seq + 1;
        self.head = step(pos, self.len);
        // The in-memory state only advances when the superblock write
        // really happened; otherwise it must keep matching the disk.
        let published = self.write_sb(fs).and_then(|_| fs.dev.flush().map_err(FsError::from));
        if published.is_err() {
            (self.start, self.first_seq, self.next_seq, self.head) = saved;
        }
        published
    }
}

#[inline]
fn step(pos: u64, len: u64) -> u64 {
    let p = pos + 1;
    if p >= len {
        1
    } else {
        p
    }
}
