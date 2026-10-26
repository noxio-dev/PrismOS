// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Virtual file system layer (phase 1.4).
//
//  * `FileSystem` is the trait a concrete file system (ext4.rs) implements.
//    It works on inode numbers: no paths, no mount points.
//  * `Vfs` owns the mount table and the open-file table, resolves paths
//    (mount point crossing, symlinks, `.`/`..`) and exposes a path-based API
//    plus a small descriptor API (open/read/write/seek/close) that the
//    syscall layer sits on (phase 2.6: user/fd.rs puts the per-process
//    descriptor tables in front of it).
//
//  * device nodes (phase 2.6): a `FileType::Dev(id)` entry is what devfs
//    (fs/devfs.rs) answers `stat` with.  `open_desc` maps one to
//    `Opened::Dev(id)` -- the caller (the descriptor layer) owns its I/O,
//    the VFS never reads or writes a device.
//
// Like blk.rs this file has no dependency on the rest of the kernel (only on
// `alloc` and `core`), so it is built and tested on the host as well.  The
// one hook towards the kernel is the wall clock, see `set_clock`.

#![allow(dead_code)]

use crate::hw::blk::BlkError;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

// ------------------------------------------------------------------ errors

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Exists,
    NotDir,
    IsDir,
    NotEmpty,
    NoSpace,
    Io,
    ReadOnly,
    Invalid,
    NameTooLong,
    Unsupported,
    Corrupt,
    Busy,
    TooManyLinks,
    Loop,
    BadFd,
    CrossDevice,
    TooBig,
}

impl FsError {
    pub fn as_str(self) -> &'static str {
        match self {
            FsError::NotFound => "no such file or directory",
            FsError::Exists => "file exists",
            FsError::NotDir => "not a directory",
            FsError::IsDir => "is a directory",
            FsError::NotEmpty => "directory not empty",
            FsError::NoSpace => "no space left on device",
            FsError::Io => "input/output error",
            FsError::ReadOnly => "read-only file system",
            FsError::Invalid => "invalid argument",
            FsError::NameTooLong => "file name too long",
            FsError::Unsupported => "operation not supported",
            FsError::Corrupt => "file system is corrupted",
            FsError::Busy => "device or resource busy",
            FsError::TooManyLinks => "too many links",
            FsError::Loop => "too many levels of symbolic links",
            FsError::BadFd => "bad file descriptor",
            FsError::CrossDevice => "cross-device link",
            FsError::TooBig => "file too large",
        }
    }
}

impl From<BlkError> for FsError {
    fn from(e: BlkError) -> FsError {
        match e {
            BlkError::ReadOnly => FsError::ReadOnly,
            _ => FsError::Io,
        }
    }
}

pub type FsResult<T> = Result<T, FsError>;

// ------------------------------------------------------------------- clock

static CLOCK: AtomicUsize = AtomicUsize::new(0);

/// Install the wall-clock source (seconds since the Unix epoch).  Used for
/// the timestamps written to inodes; without it they are all 0.
pub fn set_clock(f: fn() -> u32) {
    CLOCK.store(f as usize, Ordering::Relaxed);
}

pub fn now() -> u32 {
    let p = CLOCK.load(Ordering::Relaxed);
    if p == 0 {
        return 0;
    }
    // SAFETY: only `set_clock` writes this, always with a valid `fn() -> u32`.
    let f: fn() -> u32 = unsafe { core::mem::transmute::<usize, fn() -> u32>(p) };
    f()
}

// ------------------------------------------------------------------- types

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Dir,
    Symlink,
    /// A device node (phase 2.6): the number is the device id (1 = console,
    /// 2 = null, 3 = zero, see fs/devfs.rs).  Only devfs produces these.
    Dev(u32),
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub ino: u64,
    pub ftype: FileType,
    /// Permission bits (the low 12 bits of the mode).
    pub mode: u16,
    pub size: u64,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    /// Size on disk in 512-byte units.
    pub blocks: u64,
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub ino: u64,
    pub ftype: FileType,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StatFs {
    pub block_size: u32,
    pub blocks: u64,
    pub free_blocks: u64,
    pub files: u64,
    pub free_files: u64,
    pub name_max: u32,
}

/// What a concrete file system provides.  All operations address inodes;
/// the mutating ones default to `ReadOnly` so a read-only driver only has to
/// implement the first half.
pub trait FileSystem {
    fn fs_name(&self) -> &'static str;
    fn root(&self) -> u64;
    fn read_only(&self) -> bool;
    fn statfs(&mut self) -> FsResult<StatFs>;

    fn stat(&mut self, ino: u64) -> FsResult<Stat>;
    fn lookup(&mut self, dir: u64, name: &str) -> FsResult<u64>;
    fn readdir(&mut self, dir: u64) -> FsResult<Vec<DirEntry>>;
    fn read(&mut self, ino: u64, off: u64, buf: &mut [u8]) -> FsResult<usize>;
    fn readlink(&mut self, ino: u64) -> FsResult<String>;

    fn write(&mut self, _ino: u64, _off: u64, _data: &[u8]) -> FsResult<usize> {
        Err(FsError::ReadOnly)
    }
    fn truncate(&mut self, _ino: u64, _size: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }
    fn create(&mut self, _dir: u64, _name: &str, _mode: u16) -> FsResult<u64> {
        Err(FsError::ReadOnly)
    }
    fn mkdir(&mut self, _dir: u64, _name: &str, _mode: u16) -> FsResult<u64> {
        Err(FsError::ReadOnly)
    }
    fn symlink(&mut self, _dir: u64, _name: &str, _target: &str) -> FsResult<u64> {
        Err(FsError::ReadOnly)
    }
    fn link(&mut self, _dir: u64, _name: &str, _ino: u64) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }
    fn unlink(&mut self, _dir: u64, _name: &str) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }
    fn rmdir(&mut self, _dir: u64, _name: &str) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }
    fn rename(&mut self, _odir: u64, _oname: &str, _ndir: u64, _nname: &str) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }
    fn chmod(&mut self, _ino: u64, _mode: u16) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }
    /// fallocate-style preallocation: allocate unwritten blocks for
    /// [start, start+len) without writing data; the file size grows past
    /// EOF unless `keep_size` (phase 1.5).
    fn fallocate(&mut self, _ino: u64, _start: u64, _len: u64, _keep_size: bool) -> FsResult<()> {
        Err(FsError::ReadOnly)
    }
    /// Push every pending change to the device.
    fn sync(&mut self) -> FsResult<()> {
        Ok(())
    }
}

// -------------------------------------------------------------------- paths

pub const MAX_SYMLINKS: u32 = 8;
pub const NAME_MAX: usize = 255;
pub const PATH_MAX: usize = 4096;

/// Make `path` absolute (relative to `cwd`) and remove `.`, `..` and
/// repeated slashes.  `..` at the root stays at the root.
pub fn normalize(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let start: &str = if path.starts_with('/') { "" } else { cwd };
    for src in [start, path] {
        for c in src.split('/') {
            match c {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                _ => parts.push(c),
            }
        }
    }
    let mut out = String::new();
    for c in &parts {
        out.push('/');
        out.push_str(c);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Split a normalized absolute path into (parent, leaf).  The root has no
/// leaf.
pub fn split_parent(path: &str) -> Option<(&str, &str)> {
    if path == "/" {
        return None;
    }
    let i = path.rfind('/')?;
    let parent = if i == 0 { "/" } else { &path[..i] };
    Some((parent, &path[i + 1..]))
}

fn join(base: &str, name: &str) -> String {
    let mut s = String::from(base);
    if !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(name);
    s
}

// ---------------------------------------------------------------- open files

pub const O_READ: u32 = 1;
pub const O_WRITE: u32 = 2;
pub const O_CREATE: u32 = 4;
pub const O_TRUNC: u32 = 8;
pub const O_APPEND: u32 = 16;
pub const O_EXCL: u32 = 32;
/// Non-blocking I/O on the description (phase 3.1): a pipe / console read
/// with nothing pending answers -EAGAIN at once instead of parking, a full
/// pipe write ditto.  Description-level (shared by dup / fork).
pub const O_NONBLOCK: u32 = 64;
/// Close-on-exec (phase 3.1): an fd-*number* property (kept by the
/// descriptor layer's table, never here) -- `execve` closes the marked
/// numbers, dup / dup2 clear it on their new number.
pub const O_CLOEXEC: u32 = 128;

#[derive(Clone, Copy, Debug)]
pub enum Whence {
    Set(u64),
    Cur(i64),
    End(i64),
}

/// What `open_desc` hands back (phase 2.6): a slot in the open-file table,
/// or a device node the caller serves itself (its I/O never goes through
/// this table -- see fs/devfs.rs and user/fd.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opened {
    File(usize),
    Dev(u32),
}

struct OpenFile {
    mount: u32,
    ino: u64,
    off: u64,
    flags: u32,
    is_dir: bool,
}

struct Mount {
    id: u32,
    path: String,
    fs: Box<dyn FileSystem>,
}

#[derive(Clone, Debug)]
pub struct MountInfo {
    pub path: String,
    pub fs_name: &'static str,
    pub read_only: bool,
}

pub struct Vfs {
    mounts: Vec<Mount>,
    files: Vec<Option<OpenFile>>,
    next_id: u32,
}

impl Vfs {
    pub const fn new() -> Vfs {
        Vfs { mounts: Vec::new(), files: Vec::new(), next_id: 1 }
    }

    // ------------------------------------------------------------- mounts

    pub fn mounts(&self) -> Vec<MountInfo> {
        self.mounts
            .iter()
            .map(|m| MountInfo { path: m.path.clone(), fs_name: m.fs.fs_name(), read_only: m.fs.read_only() })
            .collect()
    }

    /// The mount `path` sits on (its name / read-only state) -- what a
    /// `statfs` of the path reports (phase 3.3).
    pub fn mount_of(&self, path: &str) -> Option<MountInfo> {
        self.mount_for(path).map(|i| MountInfo {
            path: self.mounts[i].path.clone(),
            fs_name: self.mounts[i].fs.fs_name(),
            read_only: self.mounts[i].fs.read_only(),
        })
    }

    pub fn is_mounted(&self) -> bool {
        !self.mounts.is_empty()
    }

    fn mount_for(&self, path: &str) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, m) in self.mounts.iter().enumerate() {
            let p = m.path.as_str();
            let hit = p == "/" || path == p || (path.starts_with(p) && path.as_bytes().get(p.len()) == Some(&b'/'));
            if hit && best.map_or(true, |b| self.mounts[b].path.len() < p.len()) {
                best = Some(i);
            }
        }
        best
    }

    fn is_mountpoint(&self, abs: &str) -> bool {
        self.mounts.iter().any(|m| m.path == abs)
    }

    /// Mount `fs` at `path`.  The first mount must be "/"; later ones need
    /// an existing directory that is not already a mount point.
    pub fn mount(&mut self, path: &str, fs: Box<dyn FileSystem>) -> FsResult<()> {
        let path = normalize("/", path);
        if self.mounts.is_empty() {
            if path != "/" {
                return Err(FsError::NotFound);
            }
        } else {
            if self.is_mountpoint(&path) {
                return Err(FsError::Busy);
            }
            let st = self.stat(&path)?;
            if st.ftype != FileType::Dir {
                return Err(FsError::NotDir);
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.mounts.push(Mount { id, path, fs });
        Ok(())
    }

    /// Flush and detach the file system mounted at `path`.
    pub fn umount(&mut self, path: &str) -> FsResult<()> {
        let path = normalize("/", path);
        let Some(i) = self.mounts.iter().position(|m| m.path == path) else {
            return Err(FsError::Invalid);
        };
        let id = self.mounts[i].id;
        if self.files.iter().flatten().any(|f| f.mount == id) {
            return Err(FsError::Busy);
        }
        // Nothing else may be mounted below it.
        let below = |p: &str| p != path && (path == "/" || (p.starts_with(path.as_str()) && p.as_bytes().get(path.len()) == Some(&b'/')));
        if self.mounts.iter().any(|m| below(&m.path)) {
            return Err(FsError::Busy);
        }
        self.mounts[i].fs.sync()?;
        self.mounts.remove(i);
        Ok(())
    }

    pub fn sync_all(&mut self) -> FsResult<()> {
        let mut res = Ok(());
        for m in self.mounts.iter_mut() {
            if let Err(e) = m.fs.sync() {
                res = Err(e);
            }
        }
        res
    }

    // ------------------------------------------------------- resolution

    /// Resolve an absolute normalized path to (mount index, inode).
    fn walk(&mut self, path: &str, follow_last: bool) -> FsResult<(usize, u64)> {
        let mut path = String::from(path);
        let mut depth = 0u32;
        'restart: loop {
            let mi = self.mount_for(&path).ok_or(FsError::NotFound)?;
            let mp = self.mounts[mi].path.clone();
            let rest = if mp == "/" { &path[1..] } else { &path[mp.len()..] };
            let comps: Vec<String> = rest.split('/').filter(|s| !s.is_empty()).map(String::from).collect();
            let mut ino = self.mounts[mi].fs.root();
            let mut consumed = mp;
            for (i, c) in comps.iter().enumerate() {
                if c.len() > NAME_MAX {
                    return Err(FsError::NameTooLong);
                }
                let last = i + 1 == comps.len();
                let fs = &mut self.mounts[mi].fs;
                let child = fs.lookup(ino, c)?;
                let st = fs.stat(child)?;
                if st.ftype == FileType::Symlink && (!last || follow_last) {
                    depth += 1;
                    if depth > MAX_SYMLINKS {
                        return Err(FsError::Loop);
                    }
                    let target = fs.readlink(child)?;
                    let mut np = if target.starts_with('/') { target } else { join(&consumed, &target) };
                    for r in &comps[i + 1..] {
                        np = join(&np, r);
                    }
                    path = normalize("/", &np);
                    continue 'restart;
                }
                consumed = join(&consumed, c);
                ino = child;
            }
            return Ok((mi, ino));
        }
    }

    /// Resolve the parent directory of `abs` (following symlinks) and return
    /// (mount index, directory inode, leaf name).
    fn walk_parent(&mut self, abs: &str) -> FsResult<(usize, u64, String)> {
        let (parent, leaf) = split_parent(abs).ok_or(FsError::Invalid)?;
        if leaf.len() > NAME_MAX {
            return Err(FsError::NameTooLong);
        }
        let leaf = String::from(leaf);
        let (mi, dir) = self.walk(parent, true)?;
        let st = self.mounts[mi].fs.stat(dir)?;
        if st.ftype != FileType::Dir {
            return Err(FsError::NotDir);
        }
        Ok((mi, dir, leaf))
    }

    fn check_len(path: &str) -> FsResult<()> {
        if path.len() > PATH_MAX {
            Err(FsError::NameTooLong)
        } else {
            Ok(())
        }
    }

    // ------------------------------------------------------ path-based API

    pub fn stat(&mut self, path: &str) -> FsResult<Stat> {
        Self::check_len(path)?;
        let p = normalize("/", path);
        let (mi, ino) = self.walk(&p, true)?;
        self.mounts[mi].fs.stat(ino)
    }

    /// Like `stat` but does not follow a final symlink.
    pub fn lstat(&mut self, path: &str) -> FsResult<Stat> {
        Self::check_len(path)?;
        let p = normalize("/", path);
        let (mi, ino) = self.walk(&p, false)?;
        self.mounts[mi].fs.stat(ino)
    }

    pub fn exists(&mut self, path: &str) -> bool {
        self.lstat(path).is_ok()
    }

    pub fn statfs(&mut self, path: &str) -> FsResult<StatFs> {
        let p = normalize("/", path);
        let (mi, _) = self.walk(&p, true)?;
        self.mounts[mi].fs.statfs()
    }

    pub fn readdir(&mut self, path: &str) -> FsResult<Vec<DirEntry>> {
        Self::check_len(path)?;
        let p = normalize("/", path);
        let (mi, ino) = self.walk(&p, true)?;
        let mut v = self.mounts[mi].fs.readdir(ino)?;
        // A mount point always lists as a directory (what is mounted on it).
        for e in v.iter_mut() {
            let child = join(&p, &e.name);
            if e.name != "." && e.name != ".." && self.is_mountpoint(&child) {
                e.ftype = FileType::Dir;
            }
        }
        Ok(v)
    }

    pub fn readlink(&mut self, path: &str) -> FsResult<String> {
        let p = normalize("/", path);
        let (mi, ino) = self.walk(&p, false)?;
        self.mounts[mi].fs.readlink(ino)
    }

    /// Read at most `max` bytes of a whole file.
    pub fn read_file(&mut self, path: &str, max: usize) -> FsResult<Vec<u8>> {
        let p = normalize("/", path);
        let (mi, ino) = self.walk(&p, true)?;
        let fs = &mut self.mounts[mi].fs;
        let st = fs.stat(ino)?;
        match st.ftype {
            FileType::Dir => return Err(FsError::IsDir),
            FileType::Regular => {}
            // A device node has no file data; whole-file helpers refuse it.
            FileType::Dev(_) => return Err(FsError::Unsupported),
            _ => return Err(FsError::Invalid),
        }
        if st.size as usize > max {
            return Err(FsError::TooBig);
        }
        let mut buf = Vec::new();
        buf.resize(st.size as usize, 0u8);
        let mut done = 0usize;
        while done < buf.len() {
            let n = fs.read(ino, done as u64, &mut buf[done..])?;
            if n == 0 {
                break;
            }
            done += n;
        }
        buf.truncate(done);
        Ok(buf)
    }

    /// Create or truncate `path` and write `data`.
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> FsResult<()> {
        self.put_file(path, data, false)
    }

    pub fn append_file(&mut self, path: &str, data: &[u8]) -> FsResult<()> {
        self.put_file(path, data, true)
    }

    fn put_file(&mut self, path: &str, data: &[u8], append: bool) -> FsResult<()> {
        let flags = O_WRITE | O_CREATE | if append { O_APPEND } else { O_TRUNC };
        let fd = self.open(path, flags)?;
        let mut res = Ok(());
        let mut done = 0;
        while done < data.len() {
            match self.write(fd, &data[done..]) {
                Ok(0) => {
                    res = Err(FsError::NoSpace);
                    break;
                }
                Ok(n) => done += n,
                Err(e) => {
                    res = Err(e);
                    break;
                }
            }
        }
        let _ = self.close(fd);
        res
    }

    pub fn create(&mut self, path: &str, mode: u16) -> FsResult<()> {
        let p = normalize("/", path);
        if self.is_mountpoint(&p) {
            return Err(FsError::Exists);
        }
        let (mi, dir, leaf) = self.walk_parent(&p)?;
        self.mounts[mi].fs.create(dir, &leaf, mode).map(|_| ())
    }

    pub fn mkdir(&mut self, path: &str, mode: u16) -> FsResult<()> {
        let p = normalize("/", path);
        if self.is_mountpoint(&p) {
            return Err(FsError::Exists);
        }
        let (mi, dir, leaf) = self.walk_parent(&p)?;
        self.mounts[mi].fs.mkdir(dir, &leaf, mode).map(|_| ())
    }

    /// `mkdir -p`: create every missing component.
    pub fn mkdir_p(&mut self, path: &str, mode: u16) -> FsResult<()> {
        let p = normalize("/", path);
        let mut cur = String::new();
        for c in p.split('/').filter(|s| !s.is_empty()) {
            cur.push('/');
            cur.push_str(c);
            match self.stat(&cur) {
                Ok(st) if st.ftype == FileType::Dir => {}
                Ok(_) => return Err(FsError::NotDir),
                Err(FsError::NotFound) => self.mkdir(&cur, mode)?,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn symlink(&mut self, target: &str, path: &str) -> FsResult<()> {
        let p = normalize("/", path);
        if self.is_mountpoint(&p) {
            return Err(FsError::Exists);
        }
        let (mi, dir, leaf) = self.walk_parent(&p)?;
        self.mounts[mi].fs.symlink(dir, &leaf, target).map(|_| ())
    }

    /// Hard link `new` to the existing file `old`.
    pub fn link(&mut self, old: &str, new: &str) -> FsResult<()> {
        let o = normalize("/", old);
        let n = normalize("/", new);
        let (omi, ino) = self.walk(&o, false)?;
        if self.mounts[omi].fs.stat(ino)?.ftype == FileType::Dir {
            return Err(FsError::IsDir);
        }
        if self.is_mountpoint(&n) {
            return Err(FsError::Exists);
        }
        let (nmi, dir, leaf) = self.walk_parent(&n)?;
        if nmi != omi {
            return Err(FsError::CrossDevice);
        }
        self.mounts[omi].fs.link(dir, &leaf, ino)
    }

    pub fn unlink(&mut self, path: &str) -> FsResult<()> {
        let p = normalize("/", path);
        if self.is_mountpoint(&p) {
            return Err(FsError::Busy);
        }
        let (mi, dir, leaf) = self.walk_parent(&p)?;
        self.mounts[mi].fs.unlink(dir, &leaf)
    }

    pub fn rmdir(&mut self, path: &str) -> FsResult<()> {
        let p = normalize("/", path);
        if self.is_mountpoint(&p) {
            return Err(FsError::Busy);
        }
        let (mi, dir, leaf) = self.walk_parent(&p)?;
        self.mounts[mi].fs.rmdir(dir, &leaf)
    }

    pub fn rename(&mut self, old: &str, new: &str) -> FsResult<()> {
        let o = normalize("/", old);
        let n = normalize("/", new);
        if o == n {
            return Ok(());
        }
        if self.is_mountpoint(&o) || self.is_mountpoint(&n) {
            return Err(FsError::Busy);
        }
        // A directory cannot move below itself.
        if n.starts_with(o.as_str()) && n.as_bytes().get(o.len()) == Some(&b'/') {
            return Err(FsError::Invalid);
        }
        let (omi, odir, oleaf) = self.walk_parent(&o)?;
        let (nmi, ndir, nleaf) = self.walk_parent(&n)?;
        if omi != nmi {
            return Err(FsError::CrossDevice);
        }
        self.mounts[omi].fs.rename(odir, &oleaf, ndir, &nleaf)
    }

    pub fn truncate(&mut self, path: &str, size: u64) -> FsResult<()> {
        let p = normalize("/", path);
        let (mi, ino) = self.walk(&p, true)?;
        self.mounts[mi].fs.truncate(ino, size)
    }

    pub fn chmod(&mut self, path: &str, mode: u16) -> FsResult<()> {
        let p = normalize("/", path);
        let (mi, ino) = self.walk(&p, true)?;
        self.mounts[mi].fs.chmod(ino, mode)
    }

    /// Preallocate blocks for [start, start + len) of `path`.
    pub fn fallocate(&mut self, path: &str, start: u64, len: u64, keep_size: bool) -> FsResult<()> {
        let p = normalize("/", path);
        let (mi, ino) = self.walk(&p, true)?;
        self.mounts[mi].fs.fallocate(ino, start, len, keep_size)
    }

    // ------------------------------------------------------ descriptor API

    fn file(&self, fd: usize) -> FsResult<&OpenFile> {
        self.files.get(fd).and_then(|f| f.as_ref()).ok_or(FsError::BadFd)
    }

    fn mount_index(&self, id: u32) -> FsResult<usize> {
        self.mounts.iter().position(|m| m.id == id).ok_or(FsError::BadFd)
    }

    pub fn open(&mut self, path: &str, flags: u32) -> FsResult<usize> {
        match self.open_desc(path, flags)? {
            // A regular file / directory: the table slot is the description.
            Opened::File(fd) => Ok(fd),
            // Devices go through the descriptor layer, never through the
            // plain table API (nothing here could serve their I/O).
            Opened::Dev(_) => Err(FsError::Unsupported),
        }
    }

    /// `open`, telling devices apart (phase 2.6).  Same rules as `open` for
    /// everything else (create on ENOENT, truncate, directory and read-only
    /// checks); a `Dev(id)` node resolves to its device id instead of a table
    /// slot.  `O_TRUNC` / `O_APPEND` on a device are ignored (a device has no
    /// offset) and its permissions are whatever was asked for.
    pub fn open_desc(&mut self, path: &str, flags: u32) -> FsResult<Opened> {
        Self::check_len(path)?;
        if flags & (O_READ | O_WRITE) == 0 {
            return Err(FsError::Invalid);
        }
        let p = normalize("/", path);
        let (mi, ino) = match self.walk(&p, true) {
            Ok(r) => {
                if flags & O_CREATE != 0 && flags & O_EXCL != 0 {
                    return Err(FsError::Exists);
                }
                r
            }
            Err(FsError::NotFound) if flags & O_CREATE != 0 => {
                let (mi, dir, leaf) = self.walk_parent(&p)?;
                let ino = self.mounts[mi].fs.create(dir, &leaf, 0o644)?;
                (mi, ino)
            }
            Err(e) => return Err(e),
        };
        let st = self.mounts[mi].fs.stat(ino)?;
        // A device node: no table slot, no I/O here -- the descriptor layer
        // turns the id into the device it serves.
        if let FileType::Dev(id) = st.ftype {
            return Ok(Opened::Dev(id));
        }
        let is_dir = st.ftype == FileType::Dir;
        if is_dir && flags & O_WRITE != 0 {
            return Err(FsError::IsDir);
        }
        if flags & O_WRITE != 0 && self.mounts[mi].fs.read_only() {
            return Err(FsError::ReadOnly);
        }
        if !is_dir && st.ftype != FileType::Regular {
            return Err(FsError::Invalid);
        }
        if flags & O_TRUNC != 0 && flags & O_WRITE != 0 && !is_dir {
            self.mounts[mi].fs.truncate(ino, 0)?;
        }
        let of = OpenFile { mount: self.mounts[mi].id, ino, off: 0, flags, is_dir };
        if let Some(i) = self.files.iter().position(|f| f.is_none()) {
            self.files[i] = Some(of);
            Ok(Opened::File(i))
        } else {
            self.files.push(Some(of));
            Ok(Opened::File(self.files.len() - 1))
        }
    }

    pub fn close(&mut self, fd: usize) -> FsResult<()> {
        match self.files.get_mut(fd) {
            Some(slot @ Some(_)) => {
                *slot = None;
                Ok(())
            }
            _ => Err(FsError::BadFd),
        }
    }

    pub fn read(&mut self, fd: usize, buf: &mut [u8]) -> FsResult<usize> {
        let f = self.file(fd)?;
        if f.is_dir {
            return Err(FsError::IsDir);
        }
        if f.flags & O_READ == 0 {
            return Err(FsError::BadFd);
        }
        let (mid, ino, off) = (f.mount, f.ino, f.off);
        let mi = self.mount_index(mid)?;
        let n = self.mounts[mi].fs.read(ino, off, buf)?;
        if let Some(Some(f)) = self.files.get_mut(fd) {
            f.off += n as u64;
        }
        Ok(n)
    }

    pub fn write(&mut self, fd: usize, data: &[u8]) -> FsResult<usize> {
        let f = self.file(fd)?;
        if f.flags & O_WRITE == 0 {
            return Err(FsError::BadFd);
        }
        let (mid, ino, mut off, append) = (f.mount, f.ino, f.off, f.flags & O_APPEND != 0);
        let mi = self.mount_index(mid)?;
        if append {
            off = self.mounts[mi].fs.stat(ino)?.size;
        }
        let n = self.mounts[mi].fs.write(ino, off, data)?;
        if let Some(Some(f)) = self.files.get_mut(fd) {
            f.off = off + n as u64;
        }
        Ok(n)
    }

    pub fn seek(&mut self, fd: usize, whence: Whence) -> FsResult<u64> {
        let f = self.file(fd)?;
        let (mid, ino, cur) = (f.mount, f.ino, f.off);
        let mi = self.mount_index(mid)?;
        let base = match whence {
            Whence::Set(o) => o as i128,
            Whence::Cur(d) => cur as i128 + d as i128,
            Whence::End(d) => self.mounts[mi].fs.stat(ino)?.size as i128 + d as i128,
        };
        if base < 0 {
            return Err(FsError::Invalid);
        }
        let new = base as u64;
        if let Some(Some(f)) = self.files.get_mut(fd) {
            f.off = new;
        }
        Ok(new)
    }

    pub fn fstat(&mut self, fd: usize) -> FsResult<Stat> {
        let f = self.file(fd)?;
        let (mid, ino) = (f.mount, f.ino);
        let mi = self.mount_index(mid)?;
        self.mounts[mi].fs.stat(ino)
    }

    /// Positional read that does not touch the description's offset (phase
    /// 3.1: `mmap` fills a private file mapping through it, and a shared
    /// offset would surprise whoever holds the fd).
    pub fn pread(&mut self, fd: usize, off: u64, buf: &mut [u8]) -> FsResult<usize> {
        let f = self.file(fd)?;
        if f.is_dir {
            return Err(FsError::IsDir);
        }
        if f.flags & O_READ == 0 {
            return Err(FsError::BadFd);
        }
        let (mid, ino) = (f.mount, f.ino);
        let mi = self.mount_index(mid)?;
        self.mounts[mi].fs.read(ino, off, buf)
    }

    /// Read the directory open as `fd`, one entry at a time, from its
    /// current offset (which doubles as the entry index -- `lseek(fd, 0,
    /// SET)` rewinds, getdents-style).  `fill` returns false when its
    /// buffer is full; the walk stops there and the returned offset is what
    /// the next call resumes from.  Returns the new offset, or the number
    /// of entries handed out so far when `fill` refused one.  A non-directory
    /// fd is NotDir (phase 3.1: the getdents half of opendir/readdir).
    pub fn getdents_fd(&mut self, fd: usize, fill: &mut dyn FnMut(&DirEntry) -> bool) -> FsResult<u64> {
        let f = self.file(fd)?;
        if !f.is_dir {
            return Err(FsError::NotDir);
        }
        let (mid, ino, off) = (f.mount, f.ino, f.off);
        let mi = self.mount_index(mid)?;
        let entries = self.mounts[mi].fs.readdir(ino)?;
        let mut i = (off as usize).min(entries.len());
        while i < entries.len() {
            if !fill(&entries[i]) {
                break;
            }
            i += 1;
        }
        if let Some(Some(f)) = self.files.get_mut(fd) {
            f.off = i as u64;
        }
        Ok(i as u64)
    }

    pub fn ftruncate(&mut self, fd: usize, size: u64) -> FsResult<()> {
        let f = self.file(fd)?;
        if f.flags & O_WRITE == 0 {
            return Err(FsError::BadFd);
        }
        let (mid, ino) = (f.mount, f.ino);
        let mi = self.mount_index(mid)?;
        self.mounts[mi].fs.truncate(ino, size)
    }

    /// Number of descriptors currently open.
    pub fn open_count(&self) -> usize {
        self.files.iter().flatten().count()
    }
}
