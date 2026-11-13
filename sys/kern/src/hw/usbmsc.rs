// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// USB mass storage, Bulk-Only Transport with the SCSI transparent command
// set (the protocol of every USB stick and most external disks).  LUN 0 of
// each device becomes a disk in the registry (hw/disk.rs).
//
// A command is CBW (OUT) -> optional data stage -> CSW (IN).  Stalls are
// cleared and the CSW read again; a bad CSW triggers the BOT reset recovery.
// Transfers go through one 64 KiB DMA bounce buffer, serialised by a lock.

#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, Ordering};

use crate::hw::blk::{BlkError, BlockDev};
use crate::hw::disk::{self, RawBlocks};
use crate::hw::hal::{self, Global};
use crate::hw::xhci::{self, Found, Handle, XErr};
use crate::kprintln;
use crate::lib::spin::SpinLock;

pub const MAX_DISKS: usize = 4;
const BOUNCE_PAGES: usize = 16; // 64 KiB
const BOUNCE: usize = BOUNCE_PAGES * hal::PAGE;
const CBW_SIG: u32 = 0x4342_5355;
const CSW_SIG: u32 = 0x5342_5355;

struct Lun {
    used: bool,
    h: Handle,
    iface: u8,
    in_dci: u8,
    out_dci: u8,
    bs: u32,
    blocks: u64,
    model: [u8; 40],
    model_len: usize,
    tag: u32,
}

const EMPTY: Lun = Lun {
    used: false,
    h: Handle { ctrl: 0, slot: 0 },
    iface: 0,
    in_dci: 0,
    out_dci: 0,
    bs: 512,
    blocks: 0,
    model: [0; 40],
    model_len: 0,
    tag: 0,
};

static LUNS: Global<[Lun; MAX_DISKS]> = Global::new([EMPTY; MAX_DISKS]);
static GONE: [AtomicBool; MAX_DISKS] = [const { AtomicBool::new(false) }; MAX_DISKS];
/// Serialises every command (they share the bounce buffer and CBW/CSW page).
static IO: SpinLock<Bufs> = SpinLock::new(Bufs { bounce: 0, cbw: 0 });

struct Bufs {
    bounce: u64,
    cbw: u64, // CBW at +0, CSW at +64
}

fn luns() -> &'static mut [Lun; MAX_DISKS] {
    // SAFETY: slots are filled by `attach` (shell / boot context) and only
    // read afterwards; `used`/`GONE` gate the readers.
    unsafe { LUNS.get() }
}

fn map_err(e: XErr) -> BlkError {
    match e {
        XErr::Timeout => BlkError::Timeout,
        XErr::NoDev => BlkError::NoDevice,
        _ => BlkError::Io,
    }
}

// --------------------------------------------------------------------- BOT

fn bytes(addr: u64, len: usize) -> &'static mut [u8] {
    // SAFETY: addr/len inside a DMA page we own (identity mapped).
    unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len) }
}

fn reset_recovery(l: &Lun) {
    let _ = xhci::control(l.h, 0x21, 0xFF, 0, l.iface as u16, &mut []);
    let _ = xhci::clear_halt(l.h, l.in_dci);
    let _ = xhci::clear_halt(l.h, l.out_dci);
}

/// Run one SCSI command.  Data goes through the bounce buffer; returns the
/// number of bytes actually transferred.
fn bot(l: &mut Lun, b: &Bufs, cdb: &[u8], len: usize, dir_in: bool) -> Result<usize, BlkError> {
    if GONE[l_index(l)].load(Ordering::Acquire) {
        return Err(BlkError::NoDevice);
    }
    l.tag = l.tag.wrapping_add(1);
    let tag = l.tag;
    let cbw = bytes(b.cbw, 31);
    cbw.fill(0);
    cbw[0..4].copy_from_slice(&CBW_SIG.to_le_bytes());
    cbw[4..8].copy_from_slice(&tag.to_le_bytes());
    cbw[8..12].copy_from_slice(&(len as u32).to_le_bytes());
    cbw[12] = if dir_in { 0x80 } else { 0 };
    cbw[14] = cdb.len() as u8;
    cbw[15..15 + cdb.len()].copy_from_slice(cdb);
    hal::mb();
    xhci::bulk(l.h, l.out_dci, b.cbw, 31, false).map_err(|e| {
        reset_recovery(l);
        map_err(e)
    })?;

    let mut moved = 0;
    if len > 0 {
        let dci = if dir_in { l.in_dci } else { l.out_dci };
        match xhci::bulk(l.h, dci, b.bounce, len, dir_in) {
            Ok(n) => moved = n,
            Err(XErr::Stall) => {
                let _ = xhci::clear_halt(l.h, dci);
            }
            Err(e) => {
                reset_recovery(l);
                return Err(map_err(e));
            }
        }
    }

    let csw_addr = b.cbw + 64;
    let mut got = xhci::bulk(l.h, l.in_dci, csw_addr, 13, true);
    if let Err(XErr::Stall) = got {
        let _ = xhci::clear_halt(l.h, l.in_dci);
        got = xhci::bulk(l.h, l.in_dci, csw_addr, 13, true);
    }
    match got {
        Ok(13) => {}
        Ok(_) | Err(_) => {
            reset_recovery(l);
            return Err(match got {
                Err(e) => map_err(e),
                _ => BlkError::Io,
            });
        }
    }
    let csw = bytes(csw_addr, 13);
    let sig = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
    let rtag = u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]);
    if sig != CSW_SIG || rtag != tag {
        reset_recovery(l);
        return Err(BlkError::Io);
    }
    match csw[12] {
        0 => Ok(moved),
        2 => {
            reset_recovery(l);
            Err(BlkError::Io)
        }
        _ => Err(BlkError::Io),
    }
}

fn l_index(l: &Lun) -> usize {
    let base = luns().as_ptr() as usize;
    (l as *const Lun as usize - base) / core::mem::size_of::<Lun>()
}

fn request_sense(l: &mut Lun, b: &Bufs) {
    let _ = bot(l, b, &[0x03, 0, 0, 0, 18, 0], 18, true);
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

// ------------------------------------------------------------------ attach

/// Bring up the SCSI side of a freshly enumerated mass storage interface.
pub fn attach(f: &Found) -> bool {
    let Some(i) = luns().iter().position(|l| !l.used) else {
        kprintln!("usb: too many mass storage devices");
        return false;
    };
    let mut io = IO.lock();
    if io.bounce == 0 {
        match (hal::dma_alloc(BOUNCE_PAGES), hal::dma_alloc(1)) {
            (Some(a), Some(c)) => {
                io.bounce = a;
                io.cbw = c;
            }
            _ => return false,
        }
    }
    let l = Lun {
        used: false,
        h: f.h,
        iface: f.iface,
        in_dci: f.in_dci,
        out_dci: f.out_dci,
        bs: 512,
        blocks: 0,
        model: [0; 40],
        model_len: 0,
        tag: 0,
    };
    // Park the LUN in its slot first: `bot` finds its index from the address.
    GONE[i].store(false, Ordering::Release);
    luns()[i] = l;
    let l = &mut luns()[i];
    let b = &*io;

    // INQUIRY.
    if bot(l, b, &[0x12, 0, 0, 0, 36, 0], 36, true).is_err() {
        return false;
    }
    let inq = bytes(b.bounce, 36);
    if inq[0] & 0x1F != 0 && inq[0] & 0x1F != 0x0E {
        kprintln!("usb: not a disk (SCSI type {:#x})", inq[0] & 0x1F);
        return false;
    }
    let mut n = 0;
    let mut last_space = true;
    for &c in inq[8..32].iter() {
        let c = if (0x20..0x7F).contains(&c) { c } else { b' ' };
        if c == b' ' && last_space {
            continue;
        }
        last_space = c == b' ';
        l.model[n] = c;
        n += 1;
    }
    while n > 0 && l.model[n - 1] == b' ' {
        n -= 1;
    }
    l.model_len = n;

    // TEST UNIT READY until the medium is up (clears unit attention).
    let mut ready = false;
    for _ in 0..20 {
        if bot(l, b, &[0, 0, 0, 0, 0, 0], 0, false).is_ok() {
            ready = true;
            break;
        }
        request_sense(l, b);
        hal::delay_ms(100);
    }
    if !ready {
        kprintln!("usb: device not ready");
        return false;
    }

    // READ CAPACITY (10), (16) when the disk is bigger than 2 TiB.
    if bot(l, b, &[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8, true).is_err() {
        return false;
    }
    let cap = bytes(b.bounce, 8);
    let (last, bs) = (be32(&cap[0..4]), be32(&cap[4..8]));
    let (blocks, bs) = if last == u32::MAX {
        let mut cdb = [0u8; 16];
        cdb[0] = 0x9E;
        cdb[1] = 0x10;
        cdb[10..14].copy_from_slice(&32u32.to_be_bytes());
        if bot(l, b, &cdb, 32, true).is_err() {
            return false;
        }
        let c = bytes(b.bounce, 12);
        let last = u64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
        (last.saturating_add(1), be32(&c[8..12]))
    } else {
        (last as u64 + 1, bs)
    };
    if !(512..=4096).contains(&bs) || !bs.is_power_of_two() || blocks == 0 {
        kprintln!("usb: unsupported block size {} / capacity", bs);
        return false;
    }
    l.bs = bs;
    l.blocks = blocks;
    l.used = true;
    true
}

/// The device is gone: fail its I/O at once (lock-free, safe to call while a
/// transfer holds the controller).
pub fn detach(h: Handle) {
    for (i, l) in luns().iter_mut().enumerate() {
        if l.used && l.h == h {
            GONE[i].store(true, Ordering::Release);
            l.used = false;
        }
    }
}

pub fn disk(i: usize) -> Option<UsbDisk> {
    // Registry order: by slot index among live disks.
    luns().iter().enumerate().filter(|(_, l)| l.used).nth(i).map(|(k, _)| UsbDisk(k))
}

// -------------------------------------------------------------------- disk

#[derive(Clone, Copy)]
pub struct UsbDisk(usize);

impl UsbDisk {
    fn lun(&self) -> &'static mut Lun {
        &mut luns()[self.0]
    }

    pub fn model(&self) -> &'static str {
        let l = self.lun();
        let s = core::str::from_utf8(&l.model[..l.model_len]).unwrap_or("?");
        if s.is_empty() {
            "USB storage"
        } else {
            s
        }
    }

    pub fn serial(&self) -> &'static str {
        "-"
    }

    pub fn block_size(&self) -> usize {
        self.lun().bs as usize
    }

    pub fn sectors(&self) -> u64 {
        disk::sectors_of(self)
    }

    pub fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        disk::read_sectors(self, lba, buf)
    }

    pub fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
        disk::write_sectors(self, lba, buf)
    }

    pub fn flush(&self) -> Result<(), BlkError> {
        let io = IO.lock();
        let l = self.lun();
        if !l.used {
            return Err(BlkError::NoDevice);
        }
        // SYNCHRONIZE CACHE(10): sticks that do not implement it just fail.
        match bot(l, &io, &[0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0], 0, false) {
            Err(BlkError::NoDevice) => Err(BlkError::NoDevice),
            _ => Ok(()),
        }
    }
}

fn cdb_rw(read: bool, lba: u64, n: u32) -> ([u8; 16], usize) {
    let mut c = [0u8; 16];
    if lba < (1 << 32) && n <= 0xFFFF {
        c[0] = if read { 0x28 } else { 0x2A };
        c[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
        c[7..9].copy_from_slice(&(n as u16).to_be_bytes());
        (c, 10)
    } else {
        c[0] = if read { 0x88 } else { 0x8A };
        c[2..10].copy_from_slice(&lba.to_be_bytes());
        c[10..14].copy_from_slice(&n.to_be_bytes());
        (c, 16)
    }
}

impl RawBlocks for UsbDisk {
    fn block_size(&self) -> usize {
        self.lun().bs as usize
    }

    fn blocks(&self) -> u64 {
        let l = self.lun();
        if l.used {
            l.blocks
        } else {
            0
        }
    }

    fn read_blocks(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        let bs = self.block_size();
        let io = IO.lock();
        let l = self.lun();
        if !l.used {
            return Err(BlkError::NoDevice);
        }
        let per = BOUNCE / bs;
        let (mut done, mut cur) = (0usize, lba);
        while done < buf.len() {
            let n = ((buf.len() - done) / bs).min(per);
            let (cdb, cl) = cdb_rw(true, cur, n as u32);
            let mut r = bot(l, &io, &cdb[..cl], n * bs, true);
            if r.is_err() && !GONE[self.0].load(Ordering::Acquire) {
                request_sense(l, &io);
                r = bot(l, &io, &cdb[..cl], n * bs, true);
            }
            if r? != n * bs {
                return Err(BlkError::Io);
            }
            buf[done..done + n * bs].copy_from_slice(bytes(io.bounce, n * bs));
            done += n * bs;
            cur += n as u64;
        }
        Ok(())
    }

    fn write_blocks(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
        let bs = self.block_size();
        let io = IO.lock();
        let l = self.lun();
        if !l.used {
            return Err(BlkError::NoDevice);
        }
        let per = BOUNCE / bs;
        let (mut done, mut cur) = (0usize, lba);
        while done < buf.len() {
            let n = ((buf.len() - done) / bs).min(per);
            bytes(io.bounce, n * bs).copy_from_slice(&buf[done..done + n * bs]);
            hal::mb();
            let (cdb, cl) = cdb_rw(false, cur, n as u32);
            let mut r = bot(l, &io, &cdb[..cl], n * bs, false);
            if r.is_err() && !GONE[self.0].load(Ordering::Acquire) {
                request_sense(l, &io);
                bytes(io.bounce, n * bs).copy_from_slice(&buf[done..done + n * bs]);
                r = bot(l, &io, &cdb[..cl], n * bs, false);
            }
            r?;
            done += n * bs;
            cur += n as u64;
        }
        Ok(())
    }
}

impl BlockDev for UsbDisk {
    fn sectors(&self) -> u64 {
        UsbDisk::sectors(self)
    }

    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        UsbDisk::read(self, lba, buf)
    }

    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
        UsbDisk::write(self, lba, buf)
    }

    fn flush(&self) -> Result<(), BlkError> {
        UsbDisk::flush(self)
    }
}
