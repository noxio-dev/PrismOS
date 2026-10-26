// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// tmpfs (phase 3.3): the in-memory file system mounted on /tmp at boot.
//
// Everything the VFS asks a file system for is RAM here: a BTreeMap of
// inode-numbered nodes carrying their data (files) or their target
// (symlinks) or their sorted children (directories), plus a byte and an
// inode quota -- a tmpfs is only bounded by what it is allowed to take,
// so this one states its bounds up front (statfs reports both, `df`
// shows them, and a write past them answers NoSpace).
//
//   * readdir order is the BTreeMap order: deterministic across calls,
//     which the getdents cursor (it re-lists and indexes every call)
//     requires.
//   * nlink bookkeeping follows ext4: a new directory is 2 and each
//     child directory adds one to its parent, a file is 1, link()
//     increments and unlink() decrements; a node at 0 leaves the map and
//     its bytes go back to the quota.
//   * rename() has ext4's semantics: same name is a no-op, a file over
//     a directory is IsDir, a directory over a file is NotDir, a
//     non-empty target is NotEmpty, and a directory never moves below
//     itself (walked through the parent links).
//   * timestamps are vfs::now() (the RTC-backed wall clock, 0 in the
//     host tests -- see set_clock).
//
// Like vfs.rs / ext4 / devfs this file knows nothing about the kernel:
// the only outside touch is the wall clock, so tests/fstest builds it
// on the host unchanged and runs the same scenarios against it.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::fs::vfs::{self, DirEntry, FileSystem, FileType, FsError, FsResult, Stat, StatFs};

/// Inode of the root directory (everything else is handed out from 2).
const ROOT_INO: u64 = 1;

/// Directory stat size: like a real tmpfs, one page (ls -l only).
const DIR_SIZE: u64 = 4096;

/// Default RAM budget of one tmpfs (bytes) and its inode budget.
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_FILES: u64 = 4096;

/// Hard links per inode (ext4's MAX_LINKS ballpark).
const LINK_MAX: u32 = 65000;

/// Symlink targets are capped like ext4's (the VFS reader stops at 255
/// anyway; the cap keeps a corrupt length from ever being stored).
const SYMLINK_MAX: usize = 4095;

enum Payload {
    File(Vec<u8>),
    Dir(BTreeMap<String, u64>),
    Link(String),
}

struct Node {
    parent: u64,
    ftype: FileType,
    /// Permission bits only, like the rest of the kernel's stat.
    mode: u16,
    nlink: u32,
    atime: u32,
    mtime: u32,
    ctime: u32,
    payload: Payload,
}

impl Node {
    fn size(&self) -> u64 {
        match &self.payload {
            Payload::File(d) => d.len() as u64,
            Payload::Link(t) => t.len() as u64,
            Payload::Dir(_) => DIR_SIZE,
        }
    }
    fn payload_bytes(&self) -> u64 {
        match &self.payload {
            Payload::File(d) => d.len() as u64,
            Payload::Link(t) => t.len() as u64,
            Payload::Dir(_) => 0,
        }
    }
}

pub struct TmpFs {
    nodes: BTreeMap<u64, Node>,
    next_ino: u64,
    max_bytes: u64,
    used_bytes: u64,
    max_files: u64,
}

fn validate_name(name: &str) -> FsResult<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(FsError::Invalid);
    }
    if name.len() > vfs::NAME_MAX {
        return Err(FsError::NameTooLong);
    }
    Ok(())
}

impl TmpFs {
    /// The tmpfs the kernel mounts: the default 16 MiB / 4096 files quota.
    pub fn new() -> TmpFs {
        TmpFs::with_limits(MAX_BYTES, MAX_FILES)
    }

    /// A tmpfs with test-sized limits (tests/fstest mounts this).
    pub fn with_limits(max_bytes: u64, max_files: u64) -> TmpFs {
        let now = vfs::now();
        let root = Node {
            parent: ROOT_INO,
            ftype: FileType::Dir,
            mode: 0o1777, // the classic /tmp: sticky, world-writable
            nlink: 2,
            atime: now,
            mtime: now,
            ctime: now,
            payload: Payload::Dir(BTreeMap::new()),
        };
        let mut nodes = BTreeMap::new();
        nodes.insert(ROOT_INO, root);
        TmpFs { nodes, next_ino: 2, max_bytes, used_bytes: 0, max_files }
    }

    // ------------------------------------------------------------- plumbing

    fn node(&self, ino: u64) -> FsResult<&Node> {
        self.nodes.get(&ino).ok_or(FsError::NotFound)
    }

    fn node_mut(&mut self, ino: u64) -> FsResult<&mut Node> {
        self.nodes.get_mut(&ino).ok_or(FsError::NotFound)
    }

    fn children(&self, dir: u64) -> FsResult<&BTreeMap<String, u64>> {
        match &self.node(dir)?.payload {
            Payload::Dir(c) => Ok(c),
            _ => Err(FsError::NotDir),
        }
    }

    fn children_mut(&mut self, dir: u64) -> FsResult<&mut BTreeMap<String, u64>> {
        match &mut self.node_mut(dir)?.payload {
            Payload::Dir(c) => Ok(c),
            _ => Err(FsError::NotDir),
        }
    }

    /// Take `delta` more bytes from the quota (the growth direction only).
    fn charge(&mut self, delta: u64) -> FsResult<()> {
        if self.used_bytes.checked_add(delta).map_or(true, |u| u > self.max_bytes) {
            return Err(FsError::NoSpace);
        }
        self.used_bytes += delta;
        Ok(())
    }

    fn refund(&mut self, delta: u64) {
        self.used_bytes = self.used_bytes.saturating_sub(delta);
    }

    /// A fresh inode with its node in place (the caller links it in).
    fn alloc_node(&mut self, parent: u64, ftype: FileType, mode: u16, payload: Payload) -> FsResult<u64> {
        if self.nodes.len() as u64 >= self.max_files {
            return Err(FsError::NoSpace);
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        let now = vfs::now();
        self.nodes.insert(
            ino,
            Node { parent, ftype, mode: mode & 0o7777, nlink: 1, atime: now, mtime: now, ctime: now, payload },
        );
        Ok(ino)
    }

    /// Remove a node whose last name is gone, giving its bytes back.
    fn drop_node(&mut self, ino: u64) {
        if let Some(n) = self.nodes.remove(&ino) {
            self.refund(n.payload_bytes());
        }
    }

    fn must_not_exist(&self, dir: u64, name: &str) -> FsResult<()> {
        if self.children(dir)?.contains_key(name) {
            return Err(FsError::Exists);
        }
        Ok(())
    }

    /// Add the entry and stamp the parent (mtime + ctime).
    fn link_into(&mut self, dir: u64, name: &str, ino: u64) -> FsResult<()> {
        let now = vfs::now();
        let parent = self.node_mut(dir)?;
        parent.mtime = now;
        parent.ctime = now;
        match &mut parent.payload {
            Payload::Dir(c) => c.insert(String::from(name), ino),
            _ => return Err(FsError::NotDir),
        };
        Ok(())
    }

    fn touch_parent(&mut self, dir: u64) -> FsResult<()> {
        let now = vfs::now();
        let parent = self.node_mut(dir)?;
        parent.mtime = now;
        parent.ctime = now;
        Ok(())
    }

    fn stat_of(&self, ino: u64) -> FsResult<Stat> {
        let n = self.node(ino)?;
        let size = n.size();
        Ok(Stat {
            ino,
            ftype: n.ftype,
            mode: n.mode,
            size,
            nlink: n.nlink,
            uid: 0,
            gid: 0,
            atime: n.atime,
            mtime: n.mtime,
            ctime: n.ctime,
            blocks: (size + 511) / 512,
        })
    }
}

impl FileSystem for TmpFs {
    fn fs_name(&self) -> &'static str {
        "tmpfs"
    }

    fn root(&self) -> u64 {
        ROOT_INO
    }

    fn read_only(&self) -> bool {
        false
    }

    fn statfs(&mut self) -> FsResult<StatFs> {
        let live = self.nodes.len() as u64;
        Ok(StatFs {
            block_size: 4096,
            blocks: self.max_bytes / 4096,
            free_blocks: (self.max_bytes - self.used_bytes) / 4096,
            files: self.max_files,
            free_files: self.max_files.saturating_sub(live),
            name_max: vfs::NAME_MAX as u32,
        })
    }

    fn stat(&mut self, ino: u64) -> FsResult<Stat> {
        self.stat_of(ino)
    }

    fn lookup(&mut self, dir: u64, name: &str) -> FsResult<u64> {
        self.children(dir)?
            .get(name)
            .copied()
            .ok_or(FsError::NotFound)
    }

    fn readdir(&mut self, dir: u64) -> FsResult<Vec<DirEntry>> {
        let parent = self.node(dir)?.parent;
        let mut out = Vec::with_capacity(self.children(dir)?.len() + 2);
        out.push(DirEntry { ino: dir, ftype: FileType::Dir, name: String::from(".") });
        out.push(DirEntry { ino: parent, ftype: FileType::Dir, name: String::from("..") });
        for (name, ino) in self.children(dir)?.iter() {
            let ftype = self.node(*ino)?.ftype;
            out.push(DirEntry { ino: *ino, ftype, name: name.clone() });
        }
        Ok(out)
    }

    fn read(&mut self, ino: u64, off: u64, buf: &mut [u8]) -> FsResult<usize> {
        let n = {
            let n = self.node(ino)?;
            match &n.payload {
                Payload::File(d) => {
                    if off >= d.len() as u64 {
                        0
                    } else {
                        let start = off as usize;
                        let want = buf.len().min(d.len() - start);
                        buf[..want].copy_from_slice(&d[start..start + want]);
                        want
                    }
                }
                Payload::Dir(_) => return Err(FsError::IsDir),
                Payload::Link(_) => return Err(FsError::Invalid),
            }
        };
        self.node_mut(ino)?.atime = vfs::now();
        Ok(n)
    }

    fn readlink(&mut self, ino: u64) -> FsResult<String> {
        match &self.node(ino)?.payload {
            Payload::Link(t) => Ok(t.clone()),
            Payload::Dir(_) => Err(FsError::Invalid),
            Payload::File(_) => Err(FsError::Invalid),
        }
    }

    fn write(&mut self, ino: u64, off: u64, data: &[u8]) -> FsResult<usize> {
        let end = off.checked_add(data.len() as u64).ok_or(FsError::Invalid)?;
        if end > self.max_bytes {
            return Err(FsError::NoSpace); // a single file cannot pass the quota
        }
        // Charge the growth before touching the node (two borrows).
        let old_len = {
            let n = self.node(ino)?;
            match &n.payload {
                Payload::File(d) => d.len() as u64,
                Payload::Dir(_) => return Err(FsError::IsDir),
                Payload::Link(_) => return Err(FsError::Invalid),
            }
        };
        if end > old_len {
            self.charge(end - old_len)?;
        }
        let now = vfs::now();
        let n = self.node_mut(ino)?;
        let d = match &mut n.payload {
            Payload::File(d) => d,
            Payload::Dir(_) => return Err(FsError::IsDir), // unchanged in between
            Payload::Link(_) => return Err(FsError::Invalid),
        };
        if end > old_len {
            d.resize(end as usize, 0); // zero-fill any gap off > len
        }
        d[off as usize..end as usize].copy_from_slice(data);
        n.mtime = now;
        n.ctime = now;
        Ok(data.len())
    }

    fn truncate(&mut self, ino: u64, size: u64) -> FsResult<()> {
        if size > self.max_bytes {
            return Err(FsError::NoSpace);
        }
        let old_len = {
            let n = self.node(ino)?;
            match &n.payload {
                Payload::File(d) => d.len() as u64,
                Payload::Dir(_) => return Err(FsError::IsDir),
                Payload::Link(_) => return Err(FsError::Invalid),
            }
        };
        if size > old_len {
            self.charge(size - old_len)?;
        } else {
            self.refund(old_len - size);
        }
        let now = vfs::now();
        let n = self.node_mut(ino)?;
        match &mut n.payload {
            Payload::File(d) => d.resize(size as usize, 0),
            Payload::Dir(_) => return Err(FsError::IsDir),
            Payload::Link(_) => return Err(FsError::Invalid),
        }
        n.mtime = now;
        n.ctime = now;
        Ok(())
    }

    fn create(&mut self, dir: u64, name: &str, mode: u16) -> FsResult<u64> {
        validate_name(name)?;
        self.must_not_exist(dir, name)?;
        let ino = self.alloc_node(dir, FileType::Regular, mode, Payload::File(Vec::new()))?;
        self.link_into(dir, name, ino)?;
        Ok(ino)
    }

    fn mkdir(&mut self, dir: u64, name: &str, mode: u16) -> FsResult<u64> {
        validate_name(name)?;
        self.must_not_exist(dir, name)?;
        let ino = self.alloc_node(dir, FileType::Dir, mode, Payload::Dir(BTreeMap::new()))?;
        let n = self.node_mut(ino)?;
        n.nlink = 2; // "." and the name
        self.link_into(dir, name, ino)?;
        // A subdirectory adds one to its parent's link count.
        self.node_mut(dir)?.nlink += 1;
        Ok(ino)
    }

    fn symlink(&mut self, dir: u64, name: &str, target: &str) -> FsResult<u64> {
        validate_name(name)?;
        let t = target.as_bytes();
        if t.is_empty() || t.len() > SYMLINK_MAX {
            return Err(FsError::Invalid);
        }
        self.charge(t.len() as u64)?;
        self.must_not_exist(dir, name)?;
        let payload = Payload::Link(String::from(target));
        let ino = self.alloc_node(dir, FileType::Symlink, 0o777, payload)?;
        self.link_into(dir, name, ino)?;
        Ok(ino)
    }

    fn link(&mut self, dir: u64, name: &str, ino: u64) -> FsResult<()> {
        validate_name(name)?;
        self.must_not_exist(dir, name)?;
        let now = vfs::now();
        {
            let t = self.node_mut(ino)?;
            if t.ftype == FileType::Dir {
                return Err(FsError::IsDir);
            }
            if t.nlink >= LINK_MAX {
                return Err(FsError::TooManyLinks);
            }
            t.nlink += 1;
            t.ctime = now;
        }
        self.link_into(dir, name, ino)
    }

    fn unlink(&mut self, dir: u64, name: &str) -> FsResult<()> {
        validate_name(name)?;
        let ino = {
            let c = self.children(dir)?;
            c.get(name).copied().ok_or(FsError::NotFound)?
        };
        if self.node(ino)?.ftype == FileType::Dir {
            return Err(FsError::IsDir); // that is rmdir's job
        }
        self.children_mut(dir)?.remove(name);
        self.touch_parent(dir)?;
        let gone = {
            let n = self.node_mut(ino)?;
            n.nlink -= 1;
            n.ctime = vfs::now();
            n.nlink == 0
        };
        if gone {
            self.drop_node(ino);
        }
        Ok(())
    }

    fn rmdir(&mut self, dir: u64, name: &str) -> FsResult<()> {
        validate_name(name)?;
        let ino = {
            let c = self.children(dir)?;
            c.get(name).copied().ok_or(FsError::NotFound)?
        };
        {
            let n = self.node(ino)?;
            if n.ftype != FileType::Dir {
                return Err(FsError::NotDir);
            }
            match &n.payload {
                Payload::Dir(c) if c.is_empty() => {}
                Payload::Dir(_) => return Err(FsError::NotEmpty),
                _ => return Err(FsError::NotDir),
            }
        }
        self.children_mut(dir)?.remove(name);
        self.node_mut(dir)?.nlink -= 1; // the subdirectory is gone
        self.touch_parent(dir)?;
        self.drop_node(ino);
        Ok(())
    }

    fn rename(&mut self, odir: u64, oname: &str, ndir: u64, nname: &str) -> FsResult<()> {
        validate_name(oname)?;
        validate_name(nname)?;
        if odir == ndir && oname == nname {
            return Ok(());
        }
        let o_ino = {
            let c = self.children(odir)?;
            c.get(oname).copied().ok_or(FsError::NotFound)?
        };
        let o_is_dir = self.node(o_ino)?.ftype == FileType::Dir;

        // A directory must not move below itself (walk the parent chain).
        if o_is_dir {
            let mut cur = ndir;
            for _ in 0..=self.max_files {
                if cur == o_ino {
                    return Err(FsError::Invalid);
                }
                if cur == ROOT_INO {
                    break;
                }
                cur = self.node(cur)?.parent;
            }
        }

        let n_exist = match self.children(ndir)?.get(nname) {
            Some(&i) => Some(i),
            None => None,
        };
        if n_exist == Some(o_ino) {
            return Ok(()); // the same inode under the same name
        }

        // What the target may be, exactly ext4's rules.
        if let Some(t_ino) = n_exist {
            let t = self.node(t_ino)?;
            if o_is_dir {
                if t.ftype != FileType::Dir {
                    return Err(FsError::NotDir);
                }
                match &t.payload {
                    Payload::Dir(c) if c.is_empty() => {}
                    Payload::Dir(_) => return Err(FsError::NotEmpty),
                    _ => return Err(FsError::NotDir),
                }
            } else if t.ftype == FileType::Dir {
                return Err(FsError::IsDir);
            }
        }

        let now = vfs::now();
        // Take the target's name away first (link counts, quota).
        if let Some(t_ino) = n_exist {
            let gone = {
                let t = self.node_mut(t_ino)?;
                t.nlink = t.nlink.saturating_sub(1);
                t.ctime = now;
                t.nlink == 0
            };
            if gone {
                self.drop_node(t_ino);
            }
            if o_is_dir {
                // The replaced directory was a child of the new parent.
                self.node_mut(ndir)?.nlink -= 1;
            }
        }

        // The new name, then drop the old one.
        self.link_into(ndir, nname, o_ino)?;
        self.children_mut(odir)?.remove(oname);

        // ".." and the parents' link counts follow the move.
        if o_is_dir && odir != ndir {
            let n = self.node_mut(o_ino)?;
            n.parent = ndir;
            n.ctime = now;
            self.node_mut(odir)?.nlink -= 1;
            self.node_mut(ndir)?.nlink += 1;
        } else {
            self.node_mut(o_ino)?.ctime = now;
        }
        self.touch_parent(odir)?;
        if odir != ndir {
            self.touch_parent(ndir)?;
        }
        Ok(())
    }

    fn chmod(&mut self, ino: u64, mode: u16) -> FsResult<()> {
        let n = self.node_mut(ino)?;
        n.mode = mode & 0o7777;
        n.ctime = vfs::now();
        Ok(())
    }

    /// RAM has no preallocation to do: without keep_size the file simply
    /// grows to cover [start, start + len), zero-filled like everything
    /// else; with keep_size it is a size check and nothing more.
    fn fallocate(&mut self, ino: u64, start: u64, len: u64, keep_size: bool) -> FsResult<()> {
        let end = start.checked_add(len).ok_or(FsError::Invalid)?;
        if !keep_size && end > self.node(ino)?.size() {
            self.truncate(ino, end)?;
        }
        Ok(())
    }
}
