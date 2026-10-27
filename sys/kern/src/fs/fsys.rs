// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Kernel side of the file system stack (phase 1.4): the global VFS instance,
// the current directory, disk / GPT partition discovery, mounting and the
// wall clock feeding the inode timestamps.  The VFS and ext4 code themselves
// (vfs.rs, ext4/) know nothing about the kernel.

use crate::hw::ahci;
use crate::hw::blk::{read_gpt, BlockDev, GptEntry, Partition};
use crate::fs::devfs::DevFs;
use crate::fs::ext4::Ext4;
use crate::fs::procfs::ProcFs;
use crate::fs::tmpfs::{TmpFs, MAX_BYTES, MAX_FILES};
use crate::fs::vfs::{self, FsError, Vfs};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;

/// Interior-mutable global for state only touched from the (single) shell
/// context: no IRQ handler and no second CPU ever looks at it.
struct Global<T>(UnsafeCell<T>);
// SAFETY: see above -- there is no concurrent access.
unsafe impl<T> Sync for Global<T> {}

static VFS: Global<Vfs> = Global(UnsafeCell::new(Vfs::new()));
static CWD: Global<String> = Global(UnsafeCell::new(String::new()));

pub fn vfs() -> &'static mut Vfs {
    // SAFETY: single-context access, see `Global`.
    unsafe { &mut *VFS.0.get() }
}

pub fn cwd() -> &'static str {
    // SAFETY: as above.
    let s = unsafe { &*CWD.0.get() };
    if s.is_empty() {
        "/"
    } else {
        s.as_str()
    }
}

pub fn set_cwd(p: &str) {
    // SAFETY: as above.
    unsafe { *CWD.0.get() = String::from(p) };
}

/// Absolute, normalized form of a user supplied path.
pub fn abs(path: &str) -> String {
    vfs::normalize(cwd(), path)
}

// ------------------------------------------------------------------- clock

/// Days since 1970-01-01 of a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of `days_from_civil`.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Seconds since the Unix epoch from the RTC (0 when it cannot be read).
pub fn unix_now() -> u32 {
    match crate::hw::rtc::read() {
        Some(t) => {
            let days = days_from_civil(t.year as i64, t.month as i64, t.day as i64);
            let secs = days * 86400 + t.hour as i64 * 3600 + t.min as i64 * 60 + t.sec as i64;
            secs.clamp(0, u32::MAX as i64) as u32
        }
        None => 0,
    }
}

/// "YYYY-MM-DD HH:MM" for a Unix timestamp.
pub struct Stamp(pub u32);

impl core::fmt::Display for Stamp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let t = self.0 as i64;
        let (y, m, d) = civil_from_days(t.div_euclid(86400));
        let s = t.rem_euclid(86400);
        write!(f, "{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, s / 3600, (s % 3600) / 60)
    }
}

// ------------------------------------------------------- device discovery

/// An ext2/3/4 volume found on an AHCI disk.
#[derive(Clone, Copy)]
pub struct Found {
    pub disk: usize,
    /// GPT partition number (1-based), `None` for the whole disk.
    pub part: Option<usize>,
    pub first: u64,
    pub sectors: u64,
}

fn gpt_of(d: &ahci::AhciDisk) -> ([GptEntry; 16], usize) {
    let mut e = [GptEntry { type_guid: [0; 16], first: 0, last: 0 }; 16];
    let n = read_gpt(d, &mut e);
    (e, n)
}

/// Every ext volume on every disk (whole-disk file systems first, then GPT
/// partitions in table order).
pub fn scan() -> Vec<Found> {
    let mut out = Vec::new();
    for i in 0..ahci::MAX_DISKS {
        let Some(d) = ahci::disk(i) else { continue };
        if Ext4::probe(&d) {
            out.push(Found { disk: i, part: None, first: 0, sectors: d.sectors() });
            continue;
        }
        let (ents, n) = gpt_of(&d);
        for (k, e) in ents[..n].iter().enumerate() {
            let len = e.last - e.first + 1;
            let p = Partition { dev: d, start: e.first, len };
            if Ext4::probe(&p) {
                out.push(Found { disk: i, part: Some(k + 1), first: e.first, sectors: len });
            }
        }
    }
    out
}

fn open_dev(f: &Found) -> Option<Box<dyn BlockDev>> {
    let d = ahci::disk(f.disk)?;
    Some(match f.part {
        None => Box::new(d),
        Some(_) => Box::new(Partition { dev: d, start: f.first, len: f.sectors }),
    })
}

/// Find a volume by "disk", "diskN", "disk:part" or "diskN:part"
/// (partition numbers start at 1).
pub fn find(spec: &str) -> Option<Found> {
    // The optional "disk" prefix matches the names shown by `mount -l` and
    // the boot log ("disk 0 partition 3").
    let spec = spec.strip_prefix("disk").unwrap_or(spec);
    let (d, p) = match spec.split_once(':') {
        Some((d, p)) => (d.parse::<usize>().ok()?, Some(p.parse::<usize>().ok()?)),
        None => (spec.parse::<usize>().ok()?, None),
    };
    let all = scan();
    all.iter().copied().find(|f| f.disk == d && (p.is_none() || f.part == p))
}

// ---------------------------------------------------------------- mounting

pub fn mount(f: &Found, path: &str, read_only: bool) -> Result<(), FsError> {
    let dev = open_dev(f).ok_or(FsError::Io)?;
    let fs = if read_only { Ext4::mount_ro(dev)? } else { Ext4::mount(dev)? };
    vfs().mount(path, Box::new(fs))
}

/// Boot-time setup: install the clock, mount the first ext volume on "/"
/// and the device directory on /dev (phase 2.6).
pub fn init() {
    vfs::set_clock(unix_now);
    let found = scan();
    let Some(f) = found.first() else {
        crate::kprintln!("vfs      : no ext2/3/4 volume found, nothing mounted");
        return;
    };
    let name = |f: &Found| -> String {
        match f.part {
            None => alloc::format!("disk {}", f.disk),
            Some(p) => alloc::format!("disk {} partition {}", f.disk, p),
        }
    };
    match mount(f, "/", false) {
        Ok(()) => {
            let m = vfs().mounts();
            let st = vfs().statfs("/").unwrap_or_default();
            crate::kprintln!(
                "vfs      : {} on / ({}), {} MiB, {} MiB free, {}",
                name(f),
                m.first().map_or("?", |m| m.fs_name),
                (st.blocks * st.block_size as u64) >> 20,
                (st.free_blocks * st.block_size as u64) >> 20,
                if m.first().map_or(true, |m| m.read_only) { "read-only" } else { "read-write" }
            );
            // What /proc/mounts calls the root (the volume's boot-time name).
            crate::fs::procfs::set_root_source(&name(f));
        }
        Err(e) => crate::kprintln!("vfs      : {} is not mountable: {}", name(f), e.as_str()),
    }
    // /dev holds the device nodes (console, tty, null, zero, full) served
    // by the descriptor layer (user/fd.rs).  It needs an existing /dev
    // directory in the root file system (the build creates one); without a
    // root mount there is nothing to hang it on either.
    if vfs().is_mounted() {
        match vfs().mount("/dev", Box::new(DevFs::new())) {
            Ok(()) => crate::kprintln!("vfs      : devfs on /dev (console, tty, tty0, ttyS0, null, zero, full)"),
            Err(e) => crate::kprintln!("vfs      : cannot mount devfs on /dev: {}", e.as_str()),
        }
    }
    // What the kernel does NOT mount anymore (phase 3.3.7): /tmp and
    // /proc are pid 1's to set up -- init mounts them through
    // `mount(2)` like on a real unix (the kernel keeps / and /dev, the
    // devtmpfs split).  The rescue shell asks for the same two through
    // `mount_proc_and_tmp` below, because with init gone there is nobody
    // else to do it.
}

/// The rescue mode's mounts (phase 3.3.7): the kernel shell has no init
/// above it, so the kernel hands it the /tmp and /proc a session would
/// have mounted -- same file systems, same log lines as init's boot.
pub fn mount_proc_and_tmp() {
    if !vfs().is_mounted() {
        return;
    }
    // Idempotent: a second call (a rescue hand-over after a session, a
    // restart) leaves the working mounts alone.
    let mounted = |p: &str| vfs().mounts().iter().any(|m| m.path == p);
    // /tmp is a tmpfs (phase 3.3): RAM-backed, everything the VFS can do
    // to a file system, bounded by a 16 MiB / 4096-file quota (fs/tmpfs.rs).
    // The mount needs an existing /tmp directory in the root -- the build
    // creates one (1777); mkdir_p keeps older images working.
    if !mounted("/tmp") {
        match vfs().mkdir_p("/tmp", 0o1777) {
            Ok(()) => match vfs().mount("/tmp", Box::new(TmpFs::new())) {
                Ok(()) => crate::kprintln!(
                    "vfs      : tmpfs on /tmp (RAM, {} MiB / {} files quota)",
                    MAX_BYTES >> 20,
                    MAX_FILES
                ),
                Err(e) => crate::kprintln!("vfs      : cannot mount tmpfs on /tmp: {}", e.as_str()),
            },
            Err(e) => crate::kprintln!("vfs      : cannot create /tmp: {}", e.as_str()),
        }
    }
    // /proc is the generated kernel / process state (phase 3.3,
    // fs/procfs.rs): version, uptime, meminfo, cpuinfo, cmdline, mounts,
    // self and one directory per live process.  Read-only by construction.
    if !mounted("/proc") {
        match vfs().mkdir_p("/proc", 0o555) {
            Ok(()) => match vfs().mount("/proc", Box::new(ProcFs::new())) {
                Ok(()) => crate::kprintln!("vfs      : procfs on /proc (generated, read-only)"),
                Err(e) => crate::kprintln!("vfs      : cannot mount procfs on /proc: {}", e.as_str()),
            },
            Err(e) => crate::kprintln!("vfs      : cannot create /proc: {}", e.as_str()),
        }
    }
}

// ------------------------------------------------ mount / umount (phase 3.3)

/// `mount(source, target, fstype, flags)` (syscall 55): mount a discovered
/// ext volume ("disk1", "disk0:2" -- the names the boot log and `mount -l`
/// print) or a fresh tmpfs at an existing directory.  Read-only is the
/// only flag; `fstype` is checked against what the source turns out to be
/// (accepting ext2/3/4 for the ext family).  The kernel's own mounts --
/// /, /dev, /tmp, /proc -- are made the same way at boot.
pub fn sys_mount(src_p: u64, tgt_p: u64, type_p: u64, flags: u64) -> i64 {
    let (src, tgt, fstype) = match (user_str(src_p), user_str(tgt_p), user_str(type_p)) {
        (Ok(s), Ok(t), Ok(y)) => (s, t, y),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return e,
    };
    let read_only = flags & 1 != 0; // MS_RDONLY
    if read_only && src.is_empty() {
        return -22; // EINVAL
    }
    if fstype == "tmpfs" {
        return vfs().mkdir_p(&tgt, 0o1777).and_then(|_| vfs().mount(&tgt, Box::new(TmpFs::new()))).map_or_else(
            |e| -crate::user::fd::fs_errno(e),
            |_| 0,
        );
    }
    // The process state file system, read-only and generated on read --
    // what init mounts on /proc (phase 3.3.7; the fstype name is the one
    // the mount table's fs_name column shows).
    if fstype == "procfs" {
        return vfs().mkdir_p(&tgt, 0o555).and_then(|_| vfs().mount(&tgt, Box::new(ProcFs::new()))).map_or_else(
            |e| -crate::user::fd::fs_errno(e),
            |_| 0,
        );
    }
    // A real volume: resolve the source spec through the discovery scan.
    let Some(f) = find(&src) else {
        return -2; // ENOENT
    };
    if !matches!(fstype.as_str(), "ext2" | "ext3" | "ext4" | "") {
        return -22; // EINVAL
    }
    mount(&f, &tgt, read_only).map_or_else(|e| -crate::user::fd::fs_errno(e), |_| 0)
}

/// `umount(target)` (syscall 56): flush and detach the file system at
/// `target` -- the VFS refuses a busy one (open files, mounts below).
pub fn sys_umount(tgt_p: u64) -> i64 {
    let tgt = match user_str(tgt_p) {
        Ok(t) => t,
        Err(e) => return e,
    };
    vfs().umount(&tgt).map_or_else(|e| -crate::user::fd::fs_errno(e), |_| 0)
}

/// Read a NUL-terminated string from the caller (the mount pair's paths).
fn user_str(ptr: u64) -> Result<String, i64> {
    let Some(cur) = crate::user::process::current() else { return Err(-14) };
    let Some(aspace) = cur.aspace.as_ref() else { return Err(-14) };
    match crate::user::syscall::read_user_cstr(aspace, ptr, 255) {
        Ok(v) => match String::from_utf8(v) {
            Ok(s) => Ok(s),
            Err(_) => Err(-22), // a non-utf8 string is not a path
        },
        Err(e) => Err(e),
    }
}

/// Flush every mounted file system (used before reboot / halt).
pub fn sync_all() {
    if vfs().is_mounted() {
        if let Err(e) = vfs().sync_all() {
            crate::kprintln!("vfs: sync failed: {}", e.as_str());
        }
    }
}
