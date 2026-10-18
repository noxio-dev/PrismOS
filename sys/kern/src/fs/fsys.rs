// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Kernel side of the file system stack (phase 1.4): the global VFS instance,
// the current directory, disk / GPT partition discovery, mounting and the
// wall clock feeding the inode timestamps.  The VFS and ext4 code themselves
// (vfs.rs, ext4/) know nothing about the kernel.

use crate::hw::ahci;
use crate::hw::blk::{read_gpt, BlockDev, GptEntry, Partition};
use crate::fs::ext4::Ext4;
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

/// Boot-time setup: install the clock and mount the first ext volume on "/".
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
        }
        Err(e) => crate::kprintln!("vfs      : {} is not mountable: {}", name(f), e.as_str()),
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
