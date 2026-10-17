// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Namespace operations: create, mkdir, symlink, link, unlink, rmdir, rename,
// chmod, readlink.

use super::disk::*;
use super::extent::{Ext, ExtTree};
use super::Ext4;
use crate::fs::vfs::{now, FsError, FsResult, NAME_MAX};
use alloc::string::String;
use alloc::vec;

const MAX_LINKS: u16 = 65000;

fn validate_name(name: &str) -> FsResult<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(FsError::Invalid);
    }
    if name.len() > NAME_MAX {
        return Err(FsError::NameTooLong);
    }
    Ok(())
}

impl Ext4 {
    fn check_rw(&self) -> FsResult<()> {
        if self.ro {
            Err(FsError::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// A zeroed in-memory inode with sane defaults.
    fn new_inode(&mut self, ino: u32, mode: u16, links: u16, extents: bool) -> Inode {
        let isz = self.g.inode_size;
        let mut i = Inode { ino, raw: vec![0u8; isz] };
        i.set_mode(mode);
        i.set_links(links);
        let t = now();
        i.set_atime(t);
        i.set_ctime(t);
        i.set_mtime(t);
        let g = self.next_generation();
        put32(&mut i.raw, 0x64, g);
        if isz > INODE_GOOD_OLD {
            let want = le16(&self.sb_raw, 0x15E) as usize;
            let extra = (if want >= 4 { want } else { 32 }).min(isz - INODE_GOOD_OLD) & !3;
            put16(&mut i.raw, 0x80, extra as u16);
            if extra >= 0x94 + 4 - INODE_GOOD_OLD {
                put32(&mut i.raw, 0x90, t);
            }
        }
        if extents {
            i.set_flags(EXT4_EXTENTS_FL);
            let a = i.block_area_mut();
            put16(a, 0, EXT_MAGIC);
            put16(a, 2, 0);
            put16(a, 4, 4);
            put16(a, 6, 0);
        }
        i
    }

    /// Turn an inode into a free one and release everything it owned.
    fn kill_inode(&mut self, inode: &mut Inode) -> FsResult<()> {
        let was_dir = inode.is_dir();
        self.free_inode_blocks(inode)?;
        inode.set_links(0);
        inode.set_dtime(now().max(1));
        inode.set_size(0);
        inode.set_blocks512(0);
        inode.set_flags(0);
        inode.block_area_mut().fill(0);
        self.write_inode(inode)?;
        self.free_inode_bit(inode.ino, was_dir)
    }

    fn touch_dir(&self, d: &mut Inode) {
        let t = now();
        d.set_mtime(t);
        d.set_ctime(t);
    }

    fn require_dir(&mut self, dir: u32) -> FsResult<Inode> {
        let d = self.read_inode(dir)?;
        if !d.is_dir() {
            return Err(FsError::NotDir);
        }
        Ok(d)
    }

    fn must_not_exist(&mut self, dir: u32, name: &str) -> FsResult<()> {
        match self.op_lookup(dir, name) {
            Ok(_) => Err(FsError::Exists),
            Err(FsError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub(super) fn op_create(&mut self, dir: u32, name: &str, mode: u16) -> FsResult<u32> {
        self.check_rw()?;
        validate_name(name)?;
        let mut d = self.require_dir(dir)?;
        self.must_not_exist(dir, name)?;
        let ino = self.alloc_inode(self.inode_group(dir), false)?;
        self.ext_cache = None;
        let mut i = self.new_inode(ino, S_IFREG | (mode & 0o7777), 1, true);
        self.write_inode(&mut i)?;
        if let Err(e) = self.dir_add(&mut d, name.as_bytes(), ino, i.mode()) {
            let _ = self.kill_inode(&mut i);
            return Err(e);
        }
        self.touch_dir(&mut d);
        self.write_inode(&mut d)?;
        Ok(ino)
    }

    pub(super) fn op_mkdir(&mut self, dir: u32, name: &str, mode: u16) -> FsResult<u32> {
        self.check_rw()?;
        validate_name(name)?;
        let mut d = self.require_dir(dir)?;
        if d.links() >= MAX_LINKS {
            return Err(FsError::TooManyLinks);
        }
        self.must_not_exist(dir, name)?;
        let grp = self.inode_group(dir);
        let ino = self.alloc_inode(grp, true)?;
        self.ext_cache = None;
        let (p, _) = match self.alloc_blocks(self.group_start(grp), 1) {
            Ok(x) => x,
            Err(e) => {
                let _ = self.free_inode_bit(ino, true);
                return Err(e);
            }
        };
        let bs = self.bs as u64;
        let mut i = self.new_inode(ino, S_IFDIR | (mode & 0o7777), 2, true);
        i.set_size(bs);
        i.set_blocks512(bs / 512);
        let mut tree = ExtTree::default();
        if let Err(e) = self.rebuild_tree(&mut i, &mut tree, vec![Ext { lblk: 0, len: 1, pblk: p, uninit: false }]) {
            let _ = self.free_blocks(p, 1);
            let _ = self.free_inode_bit(ino, true);
            return Err(e);
        }
        let blk = self.new_dir_block(&i, dir);
        self.bwrite(p, blk)?;
        self.write_inode(&mut i)?;
        if let Err(e) = self.dir_add(&mut d, name.as_bytes(), ino, i.mode()) {
            let _ = self.kill_inode(&mut i);
            return Err(e);
        }
        d.set_links(d.links() + 1);
        self.touch_dir(&mut d);
        self.write_inode(&mut d)?;
        Ok(ino)
    }

    pub(super) fn op_symlink(&mut self, dir: u32, name: &str, target: &str) -> FsResult<u32> {
        self.check_rw()?;
        validate_name(name)?;
        let t = target.as_bytes();
        if t.is_empty() || t.len() > 4095 {
            return Err(FsError::Invalid);
        }
        let mut d = self.require_dir(dir)?;
        self.must_not_exist(dir, name)?;
        let bs = self.bs;
        let grp = self.inode_group(dir);
        let ino = self.alloc_inode(grp, false)?;
        self.ext_cache = None;
        let fast = t.len() < 60;
        let mut i = self.new_inode(ino, S_IFLNK | 0o777, 1, !fast);
        i.set_size(t.len() as u64);
        if fast {
            i.block_area_mut()[..t.len()].copy_from_slice(t);
        } else {
            let nblk = (t.len() + bs - 1) / bs;
            let (p, n) = match self.alloc_blocks(self.group_start(grp), nblk as u32) {
                Ok(x) => x,
                Err(e) => {
                    let _ = self.free_inode_bit(ino, false);
                    return Err(e);
                }
            };
            if (n as usize) < nblk {
                let _ = self.free_blocks(p, n as u64);
                let _ = self.free_inode_bit(ino, false);
                return Err(FsError::NoSpace);
            }
            for k in 0..nblk {
                let mut b = vec![0u8; bs];
                let part = &t[k * bs..core::cmp::min(t.len(), (k + 1) * bs)];
                b[..part.len()].copy_from_slice(part);
                self.write_raw(p + k as u64, &b)?;
            }
            i.set_blocks512((nblk * bs / 512) as u64);
            let mut tree = ExtTree::default();
            if let Err(e) = self.rebuild_tree(&mut i, &mut tree, vec![Ext { lblk: 0, len: nblk as u32, pblk: p, uninit: false }]) {
                let _ = self.free_blocks(p, nblk as u64);
                let _ = self.free_inode_bit(ino, false);
                return Err(e);
            }
        }
        self.write_inode(&mut i)?;
        if let Err(e) = self.dir_add(&mut d, name.as_bytes(), ino, i.mode()) {
            let _ = self.kill_inode(&mut i);
            return Err(e);
        }
        self.touch_dir(&mut d);
        self.write_inode(&mut d)?;
        Ok(ino)
    }

    pub(super) fn op_readlink(&mut self, ino: u32) -> FsResult<String> {
        let i = self.read_inode(ino)?;
        if !i.is_symlink() {
            return Err(FsError::Invalid);
        }
        let size = i.size() as usize;
        if size > 4095 {
            return Err(FsError::Corrupt);
        }
        let mut buf = vec![0u8; size];
        if self.symlink_is_fast(&i) {
            buf.copy_from_slice(&i.block_area()[..size]);
        } else {
            let n = self.file_read(&i, 0, &mut buf)?;
            buf.truncate(n);
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    pub(super) fn op_link(&mut self, dir: u32, name: &str, ino: u32) -> FsResult<()> {
        self.check_rw()?;
        validate_name(name)?;
        let mut d = self.require_dir(dir)?;
        let mut t = self.read_inode(ino)?;
        if t.is_dir() {
            return Err(FsError::IsDir);
        }
        if t.links() >= MAX_LINKS {
            return Err(FsError::TooManyLinks);
        }
        if t.links() == 0 {
            return Err(FsError::NotFound);
        }
        self.must_not_exist(dir, name)?;
        self.dir_add(&mut d, name.as_bytes(), ino, t.mode())?;
        self.touch_dir(&mut d);
        self.write_inode(&mut d)?;
        t.set_links(t.links() + 1);
        t.set_ctime(now());
        self.write_inode(&mut t)
    }

    pub(super) fn op_unlink(&mut self, dir: u32, name: &str) -> FsResult<()> {
        self.check_rw()?;
        if name == "." || name == ".." || name.is_empty() {
            return Err(FsError::Invalid);
        }
        let mut d = self.require_dir(dir)?;
        let ino = self.op_lookup(dir, name)?;
        let mut t = self.read_inode(ino)?;
        if t.is_dir() {
            return Err(FsError::IsDir);
        }
        self.dir_remove(&d, name.as_bytes())?;
        self.touch_dir(&mut d);
        self.write_inode(&mut d)?;
        if t.links() <= 1 {
            self.kill_inode(&mut t)
        } else {
            t.set_links(t.links() - 1);
            t.set_ctime(now());
            self.write_inode(&mut t)
        }
    }

    pub(super) fn op_rmdir(&mut self, dir: u32, name: &str) -> FsResult<()> {
        self.check_rw()?;
        if name == "." || name == ".." || name.is_empty() {
            return Err(FsError::Invalid);
        }
        let mut d = self.require_dir(dir)?;
        let ino = self.op_lookup(dir, name)?;
        let mut t = self.read_inode(ino)?;
        if !t.is_dir() {
            return Err(FsError::NotDir);
        }
        if !self.dir_is_empty(&t)? {
            return Err(FsError::NotEmpty);
        }
        self.dir_remove(&d, name.as_bytes())?;
        d.set_links(d.links().saturating_sub(1).max(1));
        self.touch_dir(&mut d);
        self.write_inode(&mut d)?;
        self.kill_inode(&mut t)
    }

    pub(super) fn op_rename(&mut self, odir: u32, oname: &str, ndir: u32, nname: &str) -> FsResult<()> {
        self.check_rw()?;
        validate_name(oname)?;
        validate_name(nname)?;
        if odir == ndir && oname == nname {
            return Ok(());
        }
        let o_ino = self.op_lookup(odir, oname)?;
        let mut oi = self.read_inode(o_ino)?;
        let o_is_dir = oi.is_dir();
        let n_exist = match self.op_lookup(ndir, nname) {
            Ok(i) => Some(i),
            Err(FsError::NotFound) => None,
            Err(e) => return Err(e),
        };
        if n_exist == Some(o_ino) {
            return Ok(());
        }
        // A directory must not be moved into its own subtree.
        if o_is_dir && odir != ndir {
            let mut cur = ndir;
            for _ in 0..4096 {
                if cur == o_ino {
                    return Err(FsError::Invalid);
                }
                if cur == ROOT_INO {
                    break;
                }
                cur = self.op_lookup(cur, "..")?;
            }
        }
        let mut target: Option<Inode> = None;
        if let Some(n) = n_exist {
            let t = self.read_inode(n)?;
            if o_is_dir {
                if !t.is_dir() {
                    return Err(FsError::NotDir);
                }
                if !self.dir_is_empty(&t)? {
                    return Err(FsError::NotEmpty);
                }
            } else if t.is_dir() {
                return Err(FsError::IsDir);
            }
            target = Some(t);
        }
        let mut nd = self.require_dir(ndir)?;
        if o_is_dir && odir != ndir && target.is_none() && nd.links() >= MAX_LINKS {
            return Err(FsError::TooManyLinks);
        }

        // 1. The new name.
        if target.is_some() {
            self.dir_replace(&nd, nname.as_bytes(), o_ino, oi.mode())?;
        } else {
            self.dir_add(&mut nd, nname.as_bytes(), o_ino, oi.mode())?;
        }
        // 2. Drop the old one.
        let mut od: Option<Inode> = None;
        if odir == ndir {
            self.dir_remove(&nd, oname.as_bytes())?;
        } else {
            let o = self.require_dir(odir)?;
            self.dir_remove(&o, oname.as_bytes())?;
            od = Some(o);
        }
        // 3. Link counts and "..".
        let t_is_dir = target.as_ref().map_or(false, |t| t.is_dir());
        if o_is_dir {
            if odir != ndir {
                self.dir_set_dotdot(&oi, ndir)?;
                if let Some(o) = od.as_mut() {
                    o.set_links(o.links().saturating_sub(1).max(1));
                }
                if !t_is_dir {
                    nd.set_links(nd.links() + 1);
                }
            } else if t_is_dir {
                nd.set_links(nd.links().saturating_sub(1).max(1));
            }
        }
        self.touch_dir(&mut nd);
        self.write_inode(&mut nd)?;
        if let Some(mut o) = od {
            self.touch_dir(&mut o);
            self.write_inode(&mut o)?;
        }
        oi.set_ctime(now());
        self.write_inode(&mut oi)?;
        // 4. The replaced file goes away.
        if let Some(mut t) = target {
            if t.is_dir() || t.links() <= 1 {
                self.kill_inode(&mut t)?;
            } else {
                t.set_links(t.links() - 1);
                t.set_ctime(now());
                self.write_inode(&mut t)?;
            }
        }
        Ok(())
    }

    pub(super) fn op_chmod(&mut self, ino: u32, mode: u16) -> FsResult<()> {
        self.check_rw()?;
        let mut i = self.read_inode(ino)?;
        i.set_mode((i.mode() & S_IFMT) | (mode & 0o7777));
        i.set_ctime(now());
        self.write_inode(&mut i)
    }
}
