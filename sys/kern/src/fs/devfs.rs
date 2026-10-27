// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// devfs (phase 2.6): a tiny in-memory file system mounted on /dev, holding
// the device nodes the kernel knows how to serve:
//
//   /dev/console  the kernel console: writes mirror to the text screen
//                 *and* the serial line (what kernel messages do)
//   /dev/tty      the controlling terminal of the opening session
//                 (TIOCSCTTY claims it; without one: ENXIO)
//   /dev/tty0     the console terminal: keyboard in, text screen out
//   /dev/ttyS0    the serial terminal: COM1's own queue in, COM1 out
//                 (phase 3.3.5 -- its own line discipline)
//   /dev/null     reads are an immediate EOF, writes are swallowed
//   /dev/zero     reads produce zeroes, writes are swallowed
//   /dev/full     writes answer ENOSPC
//
// It only provides the *namespace* half of a device: lookup / readdir /
// stat answer with `FileType::Dev(id)` nodes (the inode number *is* the
// device id), and the per-process descriptor layer (user/fd.rs) turns an
// opened node into the descriptor that does the actual I/O.  Read / write
// on an inode therefore never happen here (they answer Unsupported), and
// the fs is read-only: nothing can create or remove nodes in /dev.
//
// Like vfs.rs / ext4, this file knows nothing about the kernel: the only
// outside touch is the wall clock through vfs::now (see set_clock).

use alloc::string::String;
use alloc::vec::Vec;

use crate::fs::vfs::{self, DirEntry, FileSystem, FileType, FsError, FsResult, Stat, StatFs};

/// Device ids (the inode numbers devfs hands out for the nodes).
pub const DEV_CONSOLE: u32 = 1;
pub const DEV_NULL: u32 = 2;
pub const DEV_ZERO: u32 = 3;
pub const DEV_TTY: u32 = 4;
pub const DEV_TTY0: u32 = 5;
pub const DEV_FULL: u32 = 6;
pub const DEV_TTYS0: u32 = 7;

/// Name and permission bits of each device node, in readdir order.
const NODES: [(&str, u32, u16); 7] = [
    ("console", DEV_CONSOLE, 0o620), // crw--w---- like every real unix
    ("null", DEV_NULL, 0o666),
    ("zero", DEV_ZERO, 0o666),
    ("tty", DEV_TTY, 0o666),   // the controlling terminal of a session
    ("tty0", DEV_TTY0, 0o620), // the console terminal, by its other name
    ("ttyS0", DEV_TTYS0, 0o620), // COM1, a queue of its own (phase 3.3.5)
    ("full", DEV_FULL, 0o666), // writes answer ENOSPC (phase 3.3)
];

/// Inode of the /dev directory itself (devices start at 1).
const ROOT_INO: u64 = 0;

pub struct DevFs;

impl DevFs {
    pub fn new() -> DevFs {
        DevFs
    }

    fn node(id: u32) -> Option<(&'static str, u16)> {
        NODES.iter().find(|(_, n, _)| *n == id).map(|&(n, _, m)| (n, m))
    }

    fn stat_of(&mut self, ino: u64) -> Stat {
        let now = vfs::now();
        if ino == ROOT_INO {
            return Stat {
                ino: ROOT_INO,
                ftype: FileType::Dir,
                mode: 0o755,
                size: 0,
                nlink: 2,
                uid: 0,
                gid: 0,
                atime: now,
                mtime: now,
                ctime: now,
                blocks: 0,
            };
        }
        let ftype = FileType::Dev(ino as u32);
        let mode = Self::node(ino as u32).map_or(0o600, |(_, m)| m);
        Stat {
            ino,
            ftype,
            // Permission bits only, like ext4's stat (S_IFCHR shows up as
            // the 'c' type char through FileType::Dev, not through the mode).
            mode,
            size: 0,
            nlink: 1,
            uid: 0,
            gid: 0,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        }
    }
}

impl FileSystem for DevFs {
    fn fs_name(&self) -> &'static str {
        "devfs"
    }

    fn root(&self) -> u64 {
        ROOT_INO
    }

    /// Read-only as a *file system* (no creating nodes in /dev); the
    /// devices themselves still read and write through their descriptors.
    fn read_only(&self) -> bool {
        true
    }

    fn statfs(&mut self) -> FsResult<StatFs> {
        Ok(StatFs {
            block_size: 512,
            blocks: 0,
            free_blocks: 0,
            files: NODES.len() as u64,
            free_files: 0,
            name_max: 255,
        })
    }

    fn stat(&mut self, ino: u64) -> FsResult<Stat> {
        if ino == ROOT_INO || Self::node(ino as u32).is_some() {
            Ok(self.stat_of(ino))
        } else {
            Err(FsError::NotFound)
        }
    }

    fn lookup(&mut self, dir: u64, name: &str) -> FsResult<u64> {
        if dir != ROOT_INO {
            return Err(FsError::NotDir);
        }
        for (n, id, _) in NODES {
            if name == n {
                return Ok(id as u64);
            }
        }
        Err(FsError::NotFound)
    }

    fn readdir(&mut self, dir: u64) -> FsResult<Vec<DirEntry>> {
        if dir != ROOT_INO {
            return Err(FsError::NotDir);
        }
        // Like ext4, the listing includes "." and ".." pointing at the
        // root (there is nothing above /dev but the mount point).
        let mut out = Vec::with_capacity(NODES.len() + 2);
        out.push(DirEntry { ino: ROOT_INO, ftype: FileType::Dir, name: String::from(".") });
        out.push(DirEntry { ino: ROOT_INO, ftype: FileType::Dir, name: String::from("..") });
        for (n, id, _) in NODES {
            out.push(DirEntry { ino: id as u64, ftype: FileType::Dev(id), name: String::from(n) });
        }
        Ok(out)
    }

    fn read(&mut self, _ino: u64, _off: u64, _buf: &mut [u8]) -> FsResult<usize> {
        // Devices are served by the descriptor layer, never through the fs.
        Err(FsError::Unsupported)
    }

    fn readlink(&mut self, ino: u64) -> FsResult<String> {
        let _ = ino;
        Err(FsError::Invalid) // no symlinks in /dev
    }
}
