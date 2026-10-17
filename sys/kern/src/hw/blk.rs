// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Block device abstraction shared by the disk drivers and the file systems.
// Deliberately free of any kernel dependency so the file system code can be
// built and tested on the host against a plain image file.

#![allow(dead_code)]

pub const SECTOR: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlkError {
    NoDevice,
    OutOfRange,
    Timeout,
    Io,
    ReadOnly,
}

/// A device addressed in 512-byte sectors.
pub trait BlockDev {
    fn sectors(&self) -> u64;
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError>;
    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError>;
    fn flush(&self) -> Result<(), BlkError>;
}

/// A window `[start, start + len)` of another device (a partition).
pub struct Partition<D: BlockDev> {
    pub dev: D,
    pub start: u64,
    pub len: u64,
}

impl<D: BlockDev> BlockDev for Partition<D> {
    fn sectors(&self) -> u64 {
        self.len
    }

    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        let n = (buf.len() / SECTOR) as u64;
        if lba.checked_add(n).map_or(true, |e| e > self.len) {
            return Err(BlkError::OutOfRange);
        }
        self.dev.read(self.start + lba, buf)
    }

    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
        let n = (buf.len() / SECTOR) as u64;
        if lba.checked_add(n).map_or(true, |e| e > self.len) {
            return Err(BlkError::OutOfRange);
        }
        self.dev.write(self.start + lba, buf)
    }

    fn flush(&self) -> Result<(), BlkError> {
        self.dev.flush()
    }
}

/// One GPT partition entry.
#[derive(Clone, Copy)]
pub struct GptEntry {
    pub type_guid: [u8; 16],
    pub first: u64,
    pub last: u64,
}

/// Read the GPT of `dev` (primary header at LBA 1).  Returns the used
/// entries, at most `out.len()` of them.
pub fn read_gpt<D: BlockDev + ?Sized>(dev: &D, out: &mut [GptEntry]) -> usize {
    let mut hdr = [0u8; SECTOR];
    if dev.read(1, &mut hdr).is_err() || &hdr[..8] != b"EFI PART" {
        return 0;
    }
    let le32 = |o: usize| u32::from_le_bytes([hdr[o], hdr[o + 1], hdr[o + 2], hdr[o + 3]]);
    let le64 = |o: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&hdr[o..o + 8]);
        u64::from_le_bytes(b)
    };
    let entries_lba = le64(72);
    let count = le32(80) as usize;
    let esize = le32(84) as usize;
    if esize < 128 || esize > SECTOR || count > 256 {
        return 0;
    }
    let mut n = 0;
    let mut sec = [0u8; SECTOR];
    let per = SECTOR / esize;
    let mut i = 0;
    while i < count && n < out.len() {
        if i % per == 0 && dev.read(entries_lba + (i / per) as u64, &mut sec).is_err() {
            break;
        }
        let e = &sec[(i % per) * esize..(i % per) * esize + esize];
        if e[..16] != [0u8; 16] {
            let mut g = [0u8; 16];
            g.copy_from_slice(&e[..16]);
            let mut b = [0u8; 8];
            b.copy_from_slice(&e[32..40]);
            let first = u64::from_le_bytes(b);
            b.copy_from_slice(&e[40..48]);
            let last = u64::from_le_bytes(b);
            if last >= first {
                out[n] = GptEntry { type_guid: g, first, last };
                n += 1;
            }
        }
        i += 1;
    }
    n
}
