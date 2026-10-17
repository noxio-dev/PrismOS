// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// On-disk constants, little-endian helpers and the inode wrapper.

use crate::fs::vfs::FileType;
use alloc::vec::Vec;

pub const SB_OFFSET: u64 = 1024;
pub const SB_SIZE: usize = 1024;
pub const MAGIC: u16 = 0xEF53;
pub const ROOT_INO: u32 = 2;

// s_feature_compat
pub const COMPAT_HAS_JOURNAL: u32 = 0x0004;
pub const COMPAT_DIR_INDEX: u32 = 0x0020;
pub const COMPAT_SPARSE_SUPER2: u32 = 0x0200;

// s_feature_incompat
pub const INCOMPAT_FILETYPE: u32 = 0x0002;
pub const INCOMPAT_RECOVER: u32 = 0x0004;
pub const INCOMPAT_EXTENTS: u32 = 0x0040;
pub const INCOMPAT_64BIT: u32 = 0x0080;
pub const INCOMPAT_FLEX_BG: u32 = 0x0200;
pub const INCOMPAT_LARGEDIR: u32 = 0x4000;
pub const INCOMPAT_CSUM_SEED: u32 = 0x2000;
/// Everything this driver knows how to deal with.
pub const INCOMPAT_SUPPORTED: u32 =
    INCOMPAT_FILETYPE | INCOMPAT_RECOVER | INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_LARGEDIR | INCOMPAT_CSUM_SEED;

// s_feature_ro_compat
pub const RO_SPARSE_SUPER: u32 = 0x0001;
pub const RO_LARGE_FILE: u32 = 0x0002;
pub const RO_BTREE_DIR: u32 = 0x0004;
pub const RO_HUGE_FILE: u32 = 0x0008;
pub const RO_GDT_CSUM: u32 = 0x0010;
pub const RO_DIR_NLINK: u32 = 0x0020;
pub const RO_EXTRA_ISIZE: u32 = 0x0040;
pub const RO_BIGALLOC: u32 = 0x0200;
pub const RO_METADATA_CSUM: u32 = 0x0400;
/// ro_compat bits that are safe to write with.
pub const RO_RW_SAFE: u32 =
    RO_SPARSE_SUPER | RO_LARGE_FILE | RO_BTREE_DIR | RO_HUGE_FILE | RO_GDT_CSUM | RO_DIR_NLINK | RO_EXTRA_ISIZE | RO_METADATA_CSUM;

// group descriptor flags
pub const BG_INODE_UNINIT: u16 = 1;
pub const BG_BLOCK_UNINIT: u16 = 2;
pub const BG_INODE_ZEROED: u16 = 4;

// inode flags
pub const EXT4_INDEX_FL: u32 = 0x0000_1000;
pub const EXT4_HUGE_FILE_FL: u32 = 0x0004_0000;
pub const EXT4_EXTENTS_FL: u32 = 0x0008_0000;
pub const EXT4_INLINE_DATA_FL: u32 = 0x1000_0000;
pub const EXT4_ENCRYPT_FL: u32 = 0x0000_0800;
pub const EXT4_EA_INODE_FL: u32 = 0x0020_0000;

// i_mode
pub const S_IFMT: u16 = 0xF000;
pub const S_IFREG: u16 = 0x8000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFLNK: u16 = 0xA000;

// directory entry file types
pub const FT_UNKNOWN: u8 = 0;
pub const FT_REG: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_SYMLINK: u8 = 7;
pub const FT_TAIL: u8 = 0xDE;

pub const EXT_MAGIC: u16 = 0xF30A;
pub const XATTR_MAGIC: u32 = 0xEA02_0000;

pub const INODE_GOOD_OLD: usize = 128;

#[inline]
pub fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
pub fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline]
pub fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
#[inline]
pub fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// Directory-entry type byte for an inode mode.
pub fn dirent_type(mode: u16) -> u8 {
    match mode & S_IFMT {
        S_IFREG => FT_REG,
        S_IFDIR => FT_DIR,
        S_IFLNK => FT_SYMLINK,
        0x2000 => 3,
        0x6000 => 4,
        0x1000 => 5,
        0xC000 => 6,
        _ => FT_UNKNOWN,
    }
}

pub fn file_type_of(mode: u16) -> FileType {
    match mode & S_IFMT {
        S_IFREG => FileType::Regular,
        S_IFDIR => FileType::Dir,
        S_IFLNK => FileType::Symlink,
        _ => FileType::Other,
    }
}

pub fn dirent_to_file_type(t: u8) -> FileType {
    match t {
        FT_REG => FileType::Regular,
        FT_DIR => FileType::Dir,
        FT_SYMLINK => FileType::Symlink,
        _ => FileType::Other,
    }
}

/// Superblock-derived geometry and feature set.
#[derive(Clone)]
pub struct Geo {
    pub bs: usize,
    pub blocks: u64,
    pub inodes: u32,
    pub bpg: u32,
    pub ipg: u32,
    pub first_data: u32,
    pub first_ino: u32,
    pub inode_size: usize,
    pub desc_size: usize,
    pub groups: u32,
    pub compat: u32,
    pub incompat: u32,
    pub ro_compat: u32,
    pub reserved_gdt: u32,
    pub uuid: [u8; 16],
    pub csum_seed: u32,
    pub backup_bgs: [u32; 2],
    pub itable_blocks: u32,
    pub gdt_blocks: u32,
    pub metadata_csum: bool,
    pub gdt_csum: bool,
    pub filetype: bool,
}

/// An inode as read from disk.  The raw bytes are kept so that fields this
/// driver does not know about (extended attributes, nanosecond times, ...)
/// survive a read-modify-write.
#[derive(Clone)]
pub struct Inode {
    pub ino: u32,
    pub raw: Vec<u8>,
}

impl Inode {
    pub fn mode(&self) -> u16 {
        le16(&self.raw, 0)
    }
    pub fn set_mode(&mut self, m: u16) {
        put16(&mut self.raw, 0, m)
    }
    pub fn is_dir(&self) -> bool {
        self.mode() & S_IFMT == S_IFDIR
    }
    pub fn is_reg(&self) -> bool {
        self.mode() & S_IFMT == S_IFREG
    }
    pub fn is_symlink(&self) -> bool {
        self.mode() & S_IFMT == S_IFLNK
    }
    pub fn uid(&self) -> u32 {
        le16(&self.raw, 2) as u32 | (le16(&self.raw, 0x78) as u32) << 16
    }
    pub fn gid(&self) -> u32 {
        le16(&self.raw, 0x18) as u32 | (le16(&self.raw, 0x7A) as u32) << 16
    }
    pub fn size(&self) -> u64 {
        le32(&self.raw, 4) as u64 | (le32(&self.raw, 0x6C) as u64) << 32
    }
    pub fn set_size(&mut self, s: u64) {
        put32(&mut self.raw, 4, s as u32);
        put32(&mut self.raw, 0x6C, (s >> 32) as u32);
    }
    pub fn atime(&self) -> u32 {
        le32(&self.raw, 8)
    }
    pub fn ctime(&self) -> u32 {
        le32(&self.raw, 0xC)
    }
    pub fn mtime(&self) -> u32 {
        le32(&self.raw, 0x10)
    }
    pub fn set_atime(&mut self, t: u32) {
        put32(&mut self.raw, 8, t)
    }
    pub fn set_ctime(&mut self, t: u32) {
        put32(&mut self.raw, 0xC, t)
    }
    pub fn set_mtime(&mut self, t: u32) {
        put32(&mut self.raw, 0x10, t)
    }
    pub fn set_dtime(&mut self, t: u32) {
        put32(&mut self.raw, 0x14, t)
    }
    pub fn links(&self) -> u16 {
        le16(&self.raw, 0x1A)
    }
    pub fn set_links(&mut self, n: u16) {
        put16(&mut self.raw, 0x1A, n)
    }
    /// i_blocks in 512-byte units.
    pub fn blocks512(&self) -> u64 {
        le32(&self.raw, 0x1C) as u64 | (le16(&self.raw, 0x74) as u64) << 32
    }
    pub fn set_blocks512(&mut self, n: u64) {
        put32(&mut self.raw, 0x1C, n as u32);
        put16(&mut self.raw, 0x74, (n >> 32) as u16);
    }
    pub fn flags(&self) -> u32 {
        le32(&self.raw, 0x20)
    }
    pub fn set_flags(&mut self, f: u32) {
        put32(&mut self.raw, 0x20, f)
    }
    pub fn block_area(&self) -> &[u8] {
        &self.raw[0x28..0x64]
    }
    pub fn block_area_mut(&mut self) -> &mut [u8] {
        &mut self.raw[0x28..0x64]
    }
    pub fn generation(&self) -> u32 {
        le32(&self.raw, 0x64)
    }
    pub fn file_acl(&self) -> u64 {
        le32(&self.raw, 0x68) as u64 | (le16(&self.raw, 0x76) as u64) << 32
    }
    pub fn extra_isize(&self) -> usize {
        if self.raw.len() > INODE_GOOD_OLD {
            le16(&self.raw, 0x80) as usize
        } else {
            0
        }
    }
    pub fn uses_extents(&self) -> bool {
        self.flags() & EXT4_EXTENTS_FL != 0
    }
}
