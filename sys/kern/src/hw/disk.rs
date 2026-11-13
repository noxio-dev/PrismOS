// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The disk registry: one numbering for every block device the kernel found,
// whatever bus it sits on.  SATA disks (AHCI) come first, in their historical
// order, so `disk 0` keeps meaning what it always meant; NVMe namespaces and
// USB mass storage LUNs follow.  The file system layer (fs/fsys.rs) and the
// shell only know `Disk`.
//
// Also here: `RawBlocks`, the small trait the NVMe and USB drivers implement,
// and the helpers that turn a device with 4096-byte (or larger) logical
// blocks into the 512-byte sector view `BlockDev` promises -- whole aligned
// transfers go straight through, anything else is read-modify-write.

#![allow(dead_code)]

use alloc::vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::hw::ahci::{self, AhciDisk};
use crate::hw::blk::{BlkError, BlockDev, SECTOR};
use crate::hw::hal::Global;
use crate::hw::nvme::{self, NvmeNs};
use crate::hw::usbmsc::{self, UsbDisk};

pub const MAX_DISKS: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bus {
    Sata,
    Nvme,
    Usb,
}

impl Bus {
    pub fn name(self) -> &'static str {
        match self {
            Bus::Sata => "sata",
            Bus::Nvme => "nvme",
            Bus::Usb => "usb",
        }
    }
}

/// What a registry slot holds: the driver's own handle.
#[derive(Clone, Copy)]
enum Dev {
    Sata(AhciDisk),
    Nvme(NvmeNs),
    Usb(UsbDisk),
}

static TABLE: Global<[Option<Dev>; MAX_DISKS]> = Global::new([None; MAX_DISKS]);
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// Rebuild the registry from the drivers.  Called once the storage drivers
/// have probed (and again after a USB rescan).
pub fn rescan() {
    // SAFETY: called from the boot / shell context only; `get` readers copy
    // the entry out before using it.
    let t = unsafe { TABLE.get() };
    let mut n = 0;
    let mut put = |d: Dev, n: &mut usize| {
        if *n < MAX_DISKS {
            t[*n] = Some(d);
            *n += 1;
        }
    };
    for i in 0..ahci::MAX_DISKS {
        if let Some(d) = ahci::disk(i) {
            put(Dev::Sata(d), &mut n);
        }
    }
    for i in 0..nvme::MAX_NAMESPACES {
        if let Some(d) = nvme::namespace(i) {
            put(Dev::Nvme(d), &mut n);
        }
    }
    for i in 0..usbmsc::MAX_DISKS {
        if let Some(d) = usbmsc::disk(i) {
            put(Dev::Usb(d), &mut n);
        }
    }
    for slot in t.iter_mut().skip(n) {
        *slot = None;
    }
    COUNT.store(n, Ordering::Release);
}

/// Number of disks in the registry.
pub fn count() -> usize {
    COUNT.load(Ordering::Acquire)
}

/// Disk number `i` (0-based, SATA first, then NVMe, then USB).
pub fn get(i: usize) -> Option<Disk> {
    if i < count() {
        Some(Disk(i))
    } else {
        None
    }
}

/// A handle on one registry entry; cheap to copy, implements `BlockDev`.
#[derive(Clone, Copy)]
pub struct Disk(pub usize);

impl Disk {
    fn dev(&self) -> Option<Dev> {
        // SAFETY: entries are written only by `rescan`; a copy is taken.
        unsafe { TABLE.get().get(self.0).copied().flatten() }
    }

    pub fn bus(&self) -> Bus {
        match self.dev() {
            Some(Dev::Nvme(_)) => Bus::Nvme,
            Some(Dev::Usb(_)) => Bus::Usb,
            _ => Bus::Sata,
        }
    }

    pub fn model(&self) -> &'static str {
        match self.dev() {
            Some(Dev::Sata(d)) => d.model(),
            Some(Dev::Nvme(d)) => d.model(),
            Some(Dev::Usb(d)) => d.model(),
            None => "?",
        }
    }

    pub fn serial(&self) -> &'static str {
        match self.dev() {
            Some(Dev::Sata(d)) => d.serial(),
            Some(Dev::Nvme(d)) => d.serial(),
            Some(Dev::Usb(d)) => d.serial(),
            None => "?",
        }
    }

    /// Logical block size of the device itself, in bytes (the `BlockDev`
    /// view is always 512-byte sectors).
    pub fn block_size(&self) -> usize {
        match self.dev() {
            Some(Dev::Nvme(d)) => d.block_size(),
            Some(Dev::Usb(d)) => d.block_size(),
            _ => SECTOR,
        }
    }
}

impl BlockDev for Disk {
    fn sectors(&self) -> u64 {
        match self.dev() {
            Some(Dev::Sata(d)) => d.sectors(),
            Some(Dev::Nvme(d)) => d.sectors(),
            Some(Dev::Usb(d)) => d.sectors(),
            None => 0,
        }
    }

    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        match self.dev() {
            Some(Dev::Sata(d)) => d.read(lba, buf),
            Some(Dev::Nvme(d)) => d.read(lba, buf),
            Some(Dev::Usb(d)) => d.read(lba, buf),
            None => Err(BlkError::NoDevice),
        }
    }

    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
        match self.dev() {
            Some(Dev::Sata(d)) => d.write(lba, buf),
            Some(Dev::Nvme(d)) => d.write(lba, buf),
            Some(Dev::Usb(d)) => d.write(lba, buf),
            None => Err(BlkError::NoDevice),
        }
    }

    fn flush(&self) -> Result<(), BlkError> {
        match self.dev() {
            Some(Dev::Sata(d)) => d.flush(),
            Some(Dev::Nvme(d)) => d.flush(),
            Some(Dev::Usb(d)) => d.flush(),
            None => Err(BlkError::NoDevice),
        }
    }
}

// ----------------------------------------------------- block size adaptation

/// A device that reads and writes whole logical blocks of `block_size()`
/// bytes (512, 4096 ...).  NVMe namespaces and USB LUNs implement it; the
/// helpers below build the 512-byte-sector `BlockDev` view on top.
pub trait RawBlocks {
    /// Logical block size in bytes: a power of two, at least 512.
    fn block_size(&self) -> usize;
    /// Capacity in logical blocks.
    fn blocks(&self) -> u64;
    /// Read `buf.len() / block_size()` whole blocks starting at block `lba`.
    fn read_blocks(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError>;
    /// Write `buf.len() / block_size()` whole blocks starting at block `lba`.
    fn write_blocks(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError>;
}

/// Bounce buffer size of the slow (unaligned / large-block) path.
const BOUNCE: usize = 64 * 1024;

/// Capacity of `r` in 512-byte sectors.
pub fn sectors_of<R: RawBlocks + ?Sized>(r: &R) -> u64 {
    r.blocks() * (r.block_size() / SECTOR) as u64
}

fn check<R: RawBlocks + ?Sized>(r: &R, lba: u64, len: usize) -> Result<(), BlkError> {
    if len % SECTOR != 0 {
        return Err(BlkError::Io);
    }
    let n = (len / SECTOR) as u64;
    if lba.checked_add(n).map_or(true, |e| e > sectors_of(r)) {
        return Err(BlkError::OutOfRange);
    }
    Ok(())
}

/// Read 512-byte sectors from a device with larger logical blocks.
pub fn read_sectors<R: RawBlocks + ?Sized>(r: &R, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
    check(r, lba, buf.len())?;
    let bs = r.block_size();
    if bs == SECTOR {
        return r.read_blocks(lba, buf);
    }
    let ratio = (bs / SECTOR) as u64;
    if lba % ratio == 0 && buf.len() % bs == 0 {
        return r.read_blocks(lba / ratio, buf);
    }
    let chunk = (BOUNCE / bs).max(1);
    let mut tmp = vec![0u8; chunk * bs];
    let (mut done, mut cur) = (0usize, lba);
    while done < buf.len() {
        let blk = cur / ratio;
        let off = (cur % ratio) as usize * SECTOR;
        let want = buf.len() - done;
        let blocks = (off + want).div_ceil(bs).min(chunk);
        r.read_blocks(blk, &mut tmp[..blocks * bs])?;
        let take = (blocks * bs - off).min(want);
        buf[done..done + take].copy_from_slice(&tmp[off..off + take]);
        done += take;
        cur += (take / SECTOR) as u64;
    }
    Ok(())
}

/// Write 512-byte sectors to a device with larger logical blocks
/// (read-modify-write on the partial blocks at either end).
pub fn write_sectors<R: RawBlocks + ?Sized>(r: &R, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
    check(r, lba, buf.len())?;
    let bs = r.block_size();
    if bs == SECTOR {
        return r.write_blocks(lba, buf);
    }
    let ratio = (bs / SECTOR) as u64;
    if lba % ratio == 0 && buf.len() % bs == 0 {
        return r.write_blocks(lba / ratio, buf);
    }
    let chunk = (BOUNCE / bs).max(1);
    let mut tmp = vec![0u8; chunk * bs];
    let (mut done, mut cur) = (0usize, lba);
    while done < buf.len() {
        let blk = cur / ratio;
        let off = (cur % ratio) as usize * SECTOR;
        let want = buf.len() - done;
        let blocks = (off + want).div_ceil(bs).min(chunk);
        let span = blocks * bs;
        let take = (span - off).min(want);
        if off != 0 || take != span {
            // Partial block(s): keep what is not being overwritten.
            r.read_blocks(blk, &mut tmp[..span])?;
        }
        tmp[off..off + take].copy_from_slice(&buf[done..done + take]);
        r.write_blocks(blk, &tmp[..span])?;
        done += take;
        cur += (take / SECTOR) as u64;
    }
    Ok(())
}
