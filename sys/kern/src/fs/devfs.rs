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
//   /dev/tty1..6  the virtual terminals (VT_* / KD ioctls, user/vt.rs)
//   /dev/input/event0,1  evdev keyboard / mouse (input/mod.rs)
//   /dev/dri/card0  the DRM / KMS device (only when a framebuffer exists)
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
/// /dev/random and /dev/urandom (random.rs).
pub const DEV_RANDOM: u32 = 9;
pub const DEV_URANDOM: u32 = 10;
/// /dev/dri/card0: the DRM device (drm/mod.rs).  Lives in the /dev/dri
/// subdirectory, whose own inode is DRI_DIR_INO.
pub const DEV_DRI_CARD0: u32 = 8;
const DRI_DIR_INO: u64 = 0x100;
/// /dev/tty1../dev/tty6: the virtual terminals (user/vt.rs); the id is
/// DEV_TTY1 + (n - 1).
pub const DEV_TTY1: u32 = 16;
pub const N_VT_NODES: u32 = 6;
/// /dev/input/event0, event1: evdev (input/mod.rs); the id is DEV_EVENT0 + n.
pub const DEV_EVENT0: u32 = 24;
pub const N_EVENT_NODES: u32 = 2;
const INPUT_DIR_INO: u64 = 0x101;
const INPUT_NODES: [(&str, u32, u16); 2] = [("event0", DEV_EVENT0, 0o660), ("event1", DEV_EVENT0 + 1, 0o660)];
const VT_NODES: [(&str, u32, u16); 6] = [
    ("tty1", DEV_TTY1, 0o620),
    ("tty2", DEV_TTY1 + 1, 0o620),
    ("tty3", DEV_TTY1 + 2, 0o620),
    ("tty4", DEV_TTY1 + 3, 0o620),
    ("tty5", DEV_TTY1 + 4, 0o620),
    ("tty6", DEV_TTY1 + 5, 0o620),
];
const DRI_NODES: [(&str, u32, u16); 1] = [("card0", DEV_DRI_CARD0, 0o660)];

/// Name and permission bits of each device node, in readdir order.
const NODES: [(&str, u32, u16); 9] = [
    ("console", DEV_CONSOLE, 0o620), // crw--w---- like every real unix
    ("null", DEV_NULL, 0o666),
    ("zero", DEV_ZERO, 0o666),
    ("tty", DEV_TTY, 0o666),   // the controlling terminal of a session
    ("tty0", DEV_TTY0, 0o620), // the console terminal, by its other name
    ("ttyS0", DEV_TTYS0, 0o620), // COM1, a queue of its own (phase 3.3.5)
    ("full", DEV_FULL, 0o666), // writes answer ENOSPC (phase 3.3)
    ("random", DEV_RANDOM, 0o666),  // blocks until the CSPRNG is seeded (phase 3.5.2)
    ("urandom", DEV_URANDOM, 0o666), // never blocks
];

/// Inode of the /dev directory itself (devices start at 1).
const ROOT_INO: u64 = 0;

pub struct DevFs;

impl DevFs {
    pub fn new() -> DevFs {
        DevFs
    }

    fn node(id: u32) -> Option<(&'static str, u16)> {
        NODES
            .iter()
            .chain(DRI_NODES.iter())
            .chain(VT_NODES.iter())
            .chain(INPUT_NODES.iter())
            .find(|(_, n, _)| *n == id)
            .map(|&(n, _, m)| (n, m))
    }

    fn stat_of(&mut self, ino: u64) -> Stat {
        let now = vfs::now();
        if ino == ROOT_INO || ino == DRI_DIR_INO || ino == INPUT_DIR_INO {
            return Stat {
                ino,
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
        if ino == ROOT_INO || ino == DRI_DIR_INO || ino == INPUT_DIR_INO || (ino < 0x100 && Self::node(ino as u32).is_some()) {
            Ok(self.stat_of(ino))
        } else {
            Err(FsError::NotFound)
        }
    }

    fn lookup(&mut self, dir: u64, name: &str) -> FsResult<u64> {
        if dir == DRI_DIR_INO {
            for (n, id, _) in DRI_NODES {
                if name == n {
                    return Ok(id as u64);
                }
            }
            return Err(FsError::NotFound);
        }
        if dir == INPUT_DIR_INO {
            for (n, id, _) in INPUT_NODES {
                if name == n {
                    return Ok(id as u64);
                }
            }
            return Err(FsError::NotFound);
        }
        if dir != ROOT_INO {
            return Err(FsError::NotDir);
        }
        if name == "dri" && crate::drm::present() {
            return Ok(DRI_DIR_INO);
        }
        if name == "input" {
            return Ok(INPUT_DIR_INO);
        }
        for (n, id, _) in VT_NODES {
            if name == n {
                return Ok(id as u64);
            }
        }
        for (n, id, _) in NODES {
            if name == n {
                return Ok(id as u64);
            }
        }
        Err(FsError::NotFound)
    }

    fn readdir(&mut self, dir: u64) -> FsResult<Vec<DirEntry>> {
        if dir == DRI_DIR_INO {
            let mut out = Vec::new();
            out.push(DirEntry { ino: DRI_DIR_INO, ftype: FileType::Dir, name: String::from(".") });
            out.push(DirEntry { ino: ROOT_INO, ftype: FileType::Dir, name: String::from("..") });
            for (n, id, _) in DRI_NODES {
                out.push(DirEntry { ino: id as u64, ftype: FileType::Dev(id), name: String::from(n) });
            }
            return Ok(out);
        }
        if dir == INPUT_DIR_INO {
            let mut out = Vec::new();
            out.push(DirEntry { ino: INPUT_DIR_INO, ftype: FileType::Dir, name: String::from(".") });
            out.push(DirEntry { ino: ROOT_INO, ftype: FileType::Dir, name: String::from("..") });
            for (n, id, _) in INPUT_NODES {
                out.push(DirEntry { ino: id as u64, ftype: FileType::Dev(id), name: String::from(n) });
            }
            return Ok(out);
        }
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
        for (n, id, _) in VT_NODES {
            out.push(DirEntry { ino: id as u64, ftype: FileType::Dev(id), name: String::from(n) });
        }
        out.push(DirEntry { ino: INPUT_DIR_INO, ftype: FileType::Dir, name: String::from("input") });
        if crate::drm::present() {
            out.push(DirEntry { ino: DRI_DIR_INO, ftype: FileType::Dir, name: String::from("dri") });
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
