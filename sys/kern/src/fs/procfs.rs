// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// procfs (phase 3.3): the process / kernel state file system, mounted on
// /proc at boot.  Unlike every other file system here it is *generated*:
// nothing is stored, every read / stat / readdir answers a snapshot of the
// moment -- the seq_file idea.  The one concession to the trait: `read`
// regenerates the whole (small) content and serves the requested slice,
// so a read is O(content) -- these files are a few hundred bytes.
//
// The tree:
//
//   /proc/version   the kernel, its version, its machine
//   /proc/uptime    seconds since boot (two columns, the Linux shape)
//   /proc/meminfo   the PMM's pages, MemTotal / MemFree style
//   /proc/cpuinfo   the CPU the boot probe found
//   /proc/cmdline   the command line the loader handed the kernel
//   /proc/mounts    the mount table (what `df` and `mount` read)
//   /proc/self      -> <pid of the caller> (a symlink)
//   /proc/<pid>/    one directory per live process:
//     cmdline        argv, NUL separated, exactly as execve received it
//     stat           the classic pseudo-numeric line (ps parses it)
//     status         the human-readable one
//     exe            -> the binary path (a symlink)
//
// Pids appear and vanish with the process table; readdir snapshots it,
// and a lookup of a dead pid is NotFound.  Everything is read-only: the
// file system OF the kernel, not a file system TO it.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::fs::vfs::{self, DirEntry, FileSystem, FileType, FsError, FsResult, Stat, StatFs};
use crate::user::process;

// ------------------------------------------------------------------ layout

/// Inode of the /proc directory itself.
const ROOT: u64 = 1;
/// The static files, in readdir order (self is the magic symlink).
const VERSION: u64 = 2;
const UPTIME: u64 = 3;
const MEMINFO: u64 = 4;
const CPUINFO: u64 = 5;
const CMDLINE: u64 = 6;
const MOUNTS: u64 = 7;
const SELF: u64 = 8;

/// First inode of a pid's block: `pid_base(pid) + n`, n = 0 the directory,
/// 1 cmdline, 2 stat, 3 status, 4 exe, 5 cwd, 6 the fd directory,
/// 7 + N the fd/N symlink.  The stride leaves room for every descriptor
/// number (64 = MAX_FDS) inside the block.
fn pid_base(pid: u64) -> u64 {
    0x1000 + pid * 0x100
}

/// The per-pid files, in readdir order.
const PID_FILES: [(&str, u64, FileType); 6] = [
    ("cmdline", 1, FileType::Regular),
    ("stat", 2, FileType::Regular),
    ("status", 3, FileType::Regular),
    ("exe", 4, FileType::Symlink),
    ("cwd", 5, FileType::Symlink),
    ("fd", 6, FileType::Dir),
];

/// The largest fd number a block has symlinks for (MAX_FDS, fd.rs).
const MAX_FD_ENTRIES: u64 = 64;

// ------------------------------------------------------- the boot snapshot

use core::sync::atomic::Ordering;

/// The CPU the boot probe found (cpuinfo) and the loader's command line
/// (cmdline) -- set once from main.rs, read ever after.
struct BootInfo {
    brand: String,
    vendor: String,
    family: u32,
    model: u32,
    stepping: u32,
    cmdline: String,
}

static mut BOOT: Option<BootInfo> = None;
/// The root volume's boot-time name (the /proc/mounts source column), set
/// from fsys.rs while it mounts /.
static mut ROOT_SOURCE: Option<String> = None;

/// Record what the boot probe found (main.rs, once, before /proc is read).
pub fn set_boot_info(brand: &str, vendor: &str, family: u32, model: u32, stepping: u32, cmdline: &str) {
    // SAFETY: written once from the boot context, before any reader runs.
    unsafe {
        BOOT = Some(BootInfo {
            brand: String::from(brand),
            vendor: String::from(vendor),
            family,
            model,
            stepping,
            cmdline: String::from(cmdline),
        });
    }
}

/// Record the root volume's name (fsys.rs, while mounting /).
pub fn set_root_source(name: &str) {
    // SAFETY: written once from the boot context, before any reader runs.
    unsafe {
        ROOT_SOURCE = Some(String::from(name));
    }
}

/// The boot snapshot, if the boot recorded one.
fn boot() -> Option<&'static BootInfo> {
    // SAFETY: written once before the first reader; read-only ever after.
    unsafe { BOOT.as_ref() }
}

/// The root source, if the boot recorded one.
fn root_source() -> Option<&'static String> {
    // SAFETY: as `boot`.
    unsafe { ROOT_SOURCE.as_ref() }
}

/// Wall-clock seconds since boot, with two decimals (the uptime shape).
fn uptime_str() -> String {
    let ms = crate::hw::pit::uptime_ms();
    format!("{}.{:02} {}.{}\n", ms / 1000, (ms % 1000) / 10, ms / 1000, (ms % 1000) / 10)
}

/// The pmm's view of memory, MemTotal / MemFree style (kB, like Linux).
fn meminfo_str() -> String {
    let s = crate::mem::pmm::stats_snapshot();
    let total = s.total_pages * 4;
    let free = s.free_pages.load(Ordering::Relaxed) * 4;
    format!("MemTotal:       {total} kB\nMemFree:        {free} kB\nMemAvailable:   {free} kB\n")
}

fn mount_token(label: &str) -> String {
    let w: Vec<&str> = label.split_whitespace().collect();
    match w.as_slice() {
        ["disk", d, "partition", p] => format!("disk{d}p{p}"),
        ["disk", d] => format!("disk{d}"),
        [] => String::from("none"),
        _ => w.join("_"),
    }
}

fn mounts_str() -> String {
    let mut out = String::new();
    for m in vfs_mounts() {
        let src = if m.fs_name == "ext4" && m.path == "/" {
            root_source().map(|s| mount_token(s)).unwrap_or_else(|| "disk".to_string())
        } else {
            m.fs_name.to_string()
        };
        let opts = if m.read_only { "ro" } else { "rw" };
        out += &format!("{src} {} {} {} 0 0\n", m.path, m.fs_name, opts);
    }
    out
}

fn vfs_mounts() -> Vec<vfs::MountInfo> {
    crate::fs::fsys::vfs().mounts()
}

/// The content of one inode, generated on demand.
fn content(ino: u64) -> Option<String> {
    Some(match ino {
        VERSION => format!(
            "PrismOS version {} (noxiio-dev) #1 x86_64\n",
            env!("CARGO_PKG_VERSION")
        ),
        UPTIME => uptime_str(),
        MEMINFO => meminfo_str(),
        CPUINFO => {
            let b = boot()?;
            format!(
                "processor\t: 0\nvendor_id\t: {}\ncpu family\t: {}\nmodel\t\t: {}\nstepping\t: {}\nmodel name\t: {}\n",
                b.vendor, b.family, b.model, b.stepping, b.brand
            )
        }
        CMDLINE => {
            let b = boot()?;
            format!("{}\n", b.cmdline)
        }
        MOUNTS => mounts_str(),
        _ => return pid_content(ino),
    })
}

/// The per-pid files.
fn pid_content(ino: u64) -> Option<String> {
    let pid = (ino - 0x1000) / 0x100;
    let n = (ino - 0x1000) % 0x100;
    let p = process::snapshot_table().into_iter().find(|p| p.pid == pid)?;
    Some(match n {
        1 => {
            // argv, NUL separated, verbatim (an empty one stays empty).
            let mut s = String::new();
            for &b in p.argv.iter() {
                s.push(if b == 0 { '\0' } else { b as char })
            }
            if s.is_empty() {
                s.push('\0'); // like a kernel thread: one empty string
            }
            s
        }
        2 => {
            // The first fields of the classic line (what `ps` parses).
            let state = state_char(p.state);
            format!(
                "{} ({}) {} {} {} {} 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
                p.pid, p.name, state, p.ppid, p.pgid, p.sid
            )
        }
        3 => {
            let state = state_word(p.state);
            format!(
                "Name:\t{}\nState:\t{}\nPid:\t{}\nPPid:\t{}\nPGrp:\t{}\nSid:\t{}\nUid:\t0 0 0 0\nGid:\t0 0 0 0\nThreads:\t1\n",
                p.name, state, p.pid, p.ppid, p.pgid, p.sid
            )
        }
        _ => return None,
    })
}

fn state_char(s: process::State) -> char {
    state_word(s).chars().next().unwrap_or('S')
}

fn state_word(s: process::State) -> &'static str {
    match s {
        process::State::Ready => "S (sleeping)",
        process::State::Running => "R (running)",
        process::State::Sleeping => "S (sleeping)",
        process::State::Waiting => "S (sleeping)",
        process::State::Zombie => "Z (zombie)",
    }
}

/// The stat of one inode, synthesized: regular files 0444, directories
/// 0555, the magic symlinks 0777.
fn stat_of(ino: u64, ftype: FileType) -> Stat {
    let mode = match ftype {
        FileType::Dir => 0o555,
        FileType::Symlink => 0o777,
        _ => 0o444,
    };
    let size = if ftype == FileType::Dir { 0 } else { content_len(ino) };
    Stat {
        ino,
        ftype,
        mode,
        size,
        nlink: 1,
        uid: 0,
        gid: 0,
        atime: vfs::now(),
        mtime: vfs::now(),
        ctime: vfs::now(),
        blocks: size.div_ceil(512),
    }
}

/// The content's length, generated (an unreadable file is still listable).
fn content_len(ino: u64) -> u64 {
    content(ino).map_or(0, |s| s.len() as u64)
}

// ------------------------------------------------------------------- the fs

pub struct ProcFs;

impl ProcFs {
    pub fn new() -> ProcFs {
        ProcFs
    }

    /// Whether `ino` is a live pid directory (or inside its block).
    fn pid_of(ino: u64) -> Option<u64> {
        if ino < pid_base(0) {
            return None;
        }
        let pid = (ino - 0x1000) / 0x100;
        process::snapshot_table().iter().any(|p| p.pid == pid).then_some(pid)
    }
}

impl FileSystem for ProcFs {
    fn fs_name(&self) -> &'static str {
        "procfs"
    }

    fn root(&self) -> u64 {
        ROOT
    }

    /// Read-only as a file system (nothing may write to the kernel's state
    /// through here).
    fn read_only(&self) -> bool {
        true
    }

    fn statfs(&mut self) -> FsResult<StatFs> {
        Ok(StatFs {
            block_size: 512,
            blocks: 0,
            free_blocks: 0,
            files: 7 + 16 * process::snapshot_table().len() as u64,
            free_files: 0,
            name_max: 255,
        })
    }

    fn stat(&mut self, ino: u64) -> FsResult<Stat> {
        match ino {
            ROOT => Ok(stat_of(ROOT, FileType::Dir)),
            VERSION | UPTIME | MEMINFO | CPUINFO | CMDLINE | MOUNTS => {
                Ok(stat_of(ino, FileType::Regular))
            }
            SELF => Ok(stat_of(SELF, FileType::Symlink)),
            _ => {
                let pid = Self::pid_of(ino).ok_or(FsError::NotFound)?;
                match (ino - pid_base(pid)) % 0x100 {
                    0 | 6 => Ok(stat_of(ino, FileType::Dir)),
                    1 | 2 | 3 => Ok(stat_of(ino, FileType::Regular)),
                    4 | 5 => Ok(stat_of(ino, FileType::Symlink)),
                    n if (7..7 + MAX_FD_ENTRIES).contains(&n) => {
                        Ok(stat_of(ino, FileType::Symlink))
                    }
                    _ => Err(FsError::NotFound),
                }
            }
        }
    }

    fn lookup(&mut self, dir: u64, name: &str) -> FsResult<u64> {
        if dir == ROOT {
            return match name {
                "version" => Ok(VERSION),
                "uptime" => Ok(UPTIME),
                "meminfo" => Ok(MEMINFO),
                "cpuinfo" => Ok(CPUINFO),
                "cmdline" => Ok(CMDLINE),
                "mounts" => Ok(MOUNTS),
                "self" => Ok(SELF),
                _ => {
                    let pid: u64 = match name.parse() {
                        Ok(p) if p > 0 => p,
                        _ => return Err(FsError::NotFound),
                    };
                    // Only live pids have a directory.
                    if process::snapshot_table().iter().any(|p| p.pid == pid) {
                        Ok(pid_base(pid))
                    } else {
                        Err(FsError::NotFound)
                    }
                }
            };
        }
        // A pid directory.
        let pid = Self::pid_of(dir).ok_or(FsError::NotDir)?;
        if dir == pid_base(pid) {
            for (n, off, _) in PID_FILES {
                if name == n {
                    return Ok(pid_base(pid) + off);
                }
            }
            return Err(FsError::NotFound);
        }
        // Its fd directory: one symlink per open number.
        if dir == pid_base(pid) + 6 {
            let fd: u64 = match name.parse() {
                Ok(f) if f < MAX_FD_ENTRIES => f,
                _ => return Err(FsError::NotFound),
            };
            if crate::user::fd::fd_names(pid).iter().any(|(n, _)| *n as u64 == fd) {
                return Ok(pid_base(pid) + 7 + fd);
            }
            return Err(FsError::NotFound);
        }
        Err(FsError::NotDir)
    }

    fn readdir(&mut self, dir: u64) -> FsResult<Vec<DirEntry>> {
        if dir == ROOT {
            let mut out = Vec::new();
            out.push(DirEntry { ino: ROOT, ftype: FileType::Dir, name: String::from(".") });
            out.push(DirEntry { ino: ROOT, ftype: FileType::Dir, name: String::from("..") });
            for (name, ino, ftype) in [
                ("version", VERSION, FileType::Regular),
                ("uptime", UPTIME, FileType::Regular),
                ("meminfo", MEMINFO, FileType::Regular),
                ("cpuinfo", CPUINFO, FileType::Regular),
                ("cmdline", CMDLINE, FileType::Regular),
                ("mounts", MOUNTS, FileType::Regular),
                ("self", SELF, FileType::Symlink),
            ] {
                out.push(DirEntry { ino, ftype, name: String::from(name) });
            }
            for p in process::snapshot_table() {
                out.push(DirEntry {
                    ino: pid_base(p.pid),
                    ftype: FileType::Dir,
                    name: p.pid.to_string(),
                });
            }
            return Ok(out);
        }
        let pid = Self::pid_of(dir).ok_or(FsError::NotDir)?;
        if dir == pid_base(pid) {
            let mut out = Vec::new();
            out.push(DirEntry { ino: dir, ftype: FileType::Dir, name: String::from(".") });
            out.push(DirEntry { ino: ROOT, ftype: FileType::Dir, name: String::from("..") });
            for (name, off, ftype) in PID_FILES {
                out.push(DirEntry { ino: pid_base(pid) + off, ftype, name: String::from(name) });
            }
            return Ok(out);
        }
        // Its fd directory: the open numbers, in order.
        if dir == pid_base(pid) + 6 {
            let mut out = Vec::new();
            out.push(DirEntry { ino: dir, ftype: FileType::Dir, name: String::from(".") });
            out.push(DirEntry { ino: pid_base(pid), ftype: FileType::Dir, name: String::from("..") });
            for (n, _) in crate::user::fd::fd_names(pid) {
                out.push(DirEntry {
                    ino: pid_base(pid) + 7 + n as u64,
                    ftype: FileType::Symlink,
                    name: n.to_string(),
                });
            }
            return Ok(out);
        }
        Err(FsError::NotDir)
    }

    /// Generate the whole content, serve the slice at `off` (the seq_file
    /// shape -- these files are a few hundred bytes).
    fn read(&mut self, ino: u64, off: u64, buf: &mut [u8]) -> FsResult<usize> {
        match self.stat(ino) {
            Ok(s) if s.ftype == FileType::Dir => return Err(FsError::IsDir),
            Ok(_) => {}
            Err(e) => return Err(e),
        }
        let Some(s) = content(ino) else {
            return Err(FsError::NotFound);
        };
        let bytes = s.as_bytes();
        if off >= bytes.len() as u64 {
            return Ok(0); // EOF
        }
        let start = off as usize;
        let n = buf.len().min(bytes.len() - start);
        buf[..n].copy_from_slice(&bytes[start..start + n]);
        Ok(n)
    }

    /// The magic symlinks: /proc/self -> the caller's pid,
    /// /proc/<pid>/exe -> the binary it executed, /proc/<pid>/cwd -> the
    /// working directory it resolves its paths against, and
    /// /proc/<pid>/fd/N -> the description behind that number (phase
    /// 3.3.6: the path a file was opened from, the device's node, "pipe").
    fn readlink(&mut self, ino: u64) -> FsResult<String> {
        match ino {
            SELF => Ok(process::getpid().to_string()),
            _ => {
                let pid = Self::pid_of(ino).ok_or(FsError::Invalid)?;
                let n = (ino - pid_base(pid)) % 0x100;
                if n == 4 {
                    let p = process::snapshot_table().into_iter().find(|p| p.pid == pid);
                    match p {
                        Some(p) if !p.exe.is_empty() => Ok(p.exe),
                        _ => Ok(String::from("/")),
                    }
                } else if n == 5 {
                    let p = process::snapshot_table().into_iter().find(|p| p.pid == pid);
                    match p {
                        Some(p) if !p.cwd.is_empty() => Ok(p.cwd),
                        _ => Ok(String::from("/")),
                    }
                } else if (7..7 + MAX_FD_ENTRIES).contains(&n) {
                    let fd = n - 7;
                    match crate::user::fd::fd_names(pid).into_iter().find(|(k, _)| *k as u64 == fd) {
                        Some((_, name)) => Ok(name),
                        None => Err(FsError::NotFound), // closed between the look and the read
                    }
                } else {
                    Err(FsError::Invalid) // not a symlink
                }
            }
        }
    }
}
