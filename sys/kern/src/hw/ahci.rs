// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Minimal SATA AHCI driver (Intel AHCI 1.3.1).
//
//  * Finds the first AHCI controller on the PCI bus (class 01:06:01) and
//    enables memory space + bus mastering.
//  * Every implemented port with a SATA disk attached (device detected,
//    signature 0x00000101) becomes a disk, up to MAX_DISKS.
//  * One command slot, no NCQ, no interrupts: each command is built in a
//    per-port page (command list, received-FIS area, command table with a
//    PRDT), issued through PxCI and polled to completion with a timeout.
//  * DMA goes through a per-disk bounce buffer of 16 pages (64 KiB, one PRDT
//    entry each), so callers can pass any memory; transfers are split into
//    64 KiB chunks.  All DMA memory is below 4 GiB (pmm::alloc_page_low).
//
// Commands used: IDENTIFY DEVICE, READ DMA EXT, WRITE DMA EXT, FLUSH CACHE
// EXT (LBA48 only, 512-byte logical sectors).

#![allow(dead_code)]

use core::ptr::addr_of_mut;
use core::sync::atomic::{fence, Ordering};

use crate::hw::blk::{BlkError, BlockDev, SECTOR};
use crate::hw::pci;

pub const MAX_DISKS: usize = 4;
const BOUNCE_PAGES: usize = 16;
const MAX_CHUNK_SECTORS: usize = BOUNCE_PAGES * 4096 / SECTOR; // 128

// HBA registers
const HBA_CAP: u64 = 0x00;
const HBA_GHC: u64 = 0x04;
const HBA_IS: u64 = 0x08;
const HBA_PI: u64 = 0x0C;
const HBA_VS: u64 = 0x10;
const GHC_AE: u32 = 1 << 31;

// Port registers (offset from 0x100 + 0x80 * port)
const P_CLB: u64 = 0x00;
const P_CLBU: u64 = 0x04;
const P_FB: u64 = 0x08;
const P_FBU: u64 = 0x0C;
const P_IS: u64 = 0x10;
const P_IE: u64 = 0x14;
const P_CMD: u64 = 0x18;
const P_TFD: u64 = 0x20;
const P_SIG: u64 = 0x24;
const P_SSTS: u64 = 0x28;
const P_SERR: u64 = 0x30;
const P_SACT: u64 = 0x34;
const P_CI: u64 = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const TFD_ERR: u32 = 1 << 0;
const TFD_DRQ: u32 = 1 << 3;
const TFD_BSY: u32 = 1 << 7;
const IS_TFES: u32 = 1 << 30;

const SIG_SATA: u32 = 0x0000_0101;

// ATA commands
const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_EXT: u8 = 0xEA;

// Layout of the per-port page
const OFF_CMD_LIST: usize = 0x000; // 32 headers x 32 bytes = 1 KiB, 1 KiB aligned
const OFF_FIS: usize = 0x400; // 256 bytes, 256 aligned
const OFF_CMD_TABLE: usize = 0x800; // 128 aligned: CFIS(64) ACMD(16) rsvd(48) + PRDT

#[derive(Clone, Copy)]
struct Disk {
    port: u8,
    sectors: u64,
    model: [u8; 40],
    serial: [u8; 20],
    page: u64,
    bounce: [u64; BOUNCE_PAGES],
}

struct State {
    hba: u64,
    ncs: u8,
    s64a: bool,
    version: u32,
    pci_bus: u8,
    pci_dev: u8,
    pci_func: u8,
    n_ports_impl: u32,
    disks: [Option<Disk>; MAX_DISKS],
    ready: bool,
}

static mut ST: State = State {
    hba: 0,
    ncs: 0,
    s64a: false,
    version: 0,
    pci_bus: 0,
    pci_dev: 0,
    pci_func: 0,
    n_ports_impl: 0,
    disks: [None; MAX_DISKS],
    ready: false,
};

fn st() -> &'static mut State {
    // SAFETY: single CPU; commands are synchronous.
    unsafe { &mut *addr_of_mut!(ST) }
}

// ------------------------------------------------------------- MMIO access

fn rd(addr: u64) -> u32 {
    // SAFETY: the HBA window was mapped and validated in init().
    unsafe { (addr as *const u32).read_volatile() }
}

fn wr(addr: u64, v: u32) {
    // SAFETY: as above.
    unsafe { (addr as *mut u32).write_volatile(v) }
}

fn port_base(port: u8) -> u64 {
    st().hba + 0x100 + 0x80 * port as u64
}

fn prd(port: u8, off: u64) -> u32 {
    rd(port_base(port) + off)
}

fn pwr(port: u8, off: u64, v: u32) {
    wr(port_base(port) + off, v)
}

/// Poll `cond` for up to `ms` milliseconds (tick based) and a hard spin cap,
/// so a machine with interrupts off cannot hang here.
fn wait(ms: u64, mut cond: impl FnMut() -> bool) -> bool {
    let start = crate::hw::pit::uptime_ms();
    let mut spins = 0u64;
    loop {
        if cond() {
            return true;
        }
        spins += 1;
        if crate::hw::pit::uptime_ms().wrapping_sub(start) > ms || spins > 50_000_000 {
            return false;
        }
        core::hint::spin_loop();
    }
}

// ------------------------------------------------------------ port control

fn stop_port(port: u8) -> bool {
    let cmd = prd(port, P_CMD);
    pwr(port, P_CMD, cmd & !CMD_ST);
    if !wait(500, || prd(port, P_CMD) & CMD_CR == 0) {
        return false;
    }
    let cmd = prd(port, P_CMD);
    if cmd & (CMD_FRE | CMD_FR) != 0 {
        pwr(port, P_CMD, cmd & !CMD_FRE);
        if !wait(500, || prd(port, P_CMD) & CMD_FR == 0) {
            return false;
        }
    }
    true
}

fn start_port(port: u8) -> bool {
    if !wait(1000, || prd(port, P_CMD) & CMD_CR == 0) {
        return false;
    }
    let cmd = prd(port, P_CMD);
    pwr(port, P_CMD, cmd | CMD_FRE);
    pwr(port, P_CMD, prd(port, P_CMD) | CMD_ST);
    true
}

/// Build a command in slot 0 and run it to completion.
/// `bytes` is the data length (0 for no data), `write` the direction.
fn exec(d: &Disk, cmd: u8, lba: u64, count: u32, write: bool, bytes: usize) -> Result<(), BlkError> {
    let port = d.port;
    let prdtl = (bytes + 4095) / 4096;
    debug_assert!(prdtl <= BOUNCE_PAGES);
    let table = d.page + OFF_CMD_TABLE as u64;

    // SAFETY: the page is ours, low memory (identity mapped), zeroed at setup.
    unsafe {
        // Command header, slot 0.
        let hdr = (d.page + OFF_CMD_LIST as u64) as *mut u32;
        hdr.write_volatile(5 | ((write as u32) << 6) | ((prdtl as u32) << 16));
        hdr.add(1).write_volatile(0); // PRDBC
        hdr.add(2).write_volatile(table as u32);
        hdr.add(3).write_volatile(0);

        // Command table: H2D register FIS + PRDT.
        let t = table as *mut u8;
        core::ptr::write_bytes(t, 0, 0x80 + 16 * BOUNCE_PAGES);
        t.write(0x27); // FIS type: register host to device
        t.add(1).write(0x80); // C bit: command
        t.add(2).write(cmd);
        t.add(4).write(lba as u8);
        t.add(5).write((lba >> 8) as u8);
        t.add(6).write((lba >> 16) as u8);
        t.add(7).write(if cmd == ATA_IDENTIFY { 0 } else { 0x40 }); // LBA mode
        t.add(8).write((lba >> 24) as u8);
        t.add(9).write((lba >> 32) as u8);
        t.add(10).write((lba >> 40) as u8);
        t.add(12).write(count as u8);
        t.add(13).write((count >> 8) as u8);
        let mut left = bytes;
        for i in 0..prdtl {
            let e = t.add(0x80 + 16 * i) as *mut u32;
            let len = left.min(4096);
            e.write(d.bounce[i] as u32);
            e.add(1).write(0);
            e.add(2).write(0);
            e.add(3).write((len - 1) as u32);
            left -= len;
        }
    }
    fence(Ordering::SeqCst);

    if !wait(1000, || prd(port, P_TFD) & (TFD_BSY | TFD_DRQ) == 0) {
        recover(port);
        return Err(BlkError::Timeout);
    }
    pwr(port, P_IS, 0xFFFF_FFFF);
    pwr(port, P_SERR, 0xFFFF_FFFF);
    pwr(port, P_CI, 1);
    let mut failed = false;
    let done = wait(10_000, || {
        if prd(port, P_IS) & IS_TFES != 0 {
            failed = true;
            return true;
        }
        prd(port, P_CI) & 1 == 0
    });
    if !done {
        recover(port);
        return Err(BlkError::Timeout);
    }
    if failed || prd(port, P_TFD) & TFD_ERR != 0 {
        recover(port);
        return Err(BlkError::Io);
    }
    Ok(())
}

/// After an error: stop and restart the port so the next command can run.
fn recover(port: u8) {
    pwr(port, P_IS, 0xFFFF_FFFF);
    pwr(port, P_SERR, 0xFFFF_FFFF);
    if stop_port(port) {
        start_port(port);
    }
}

// -------------------------------------------------------------------- init

fn le16(b: &[u8], word: usize) -> u16 {
    u16::from_le_bytes([b[word * 2], b[word * 2 + 1]])
}

/// Copy an ATA string (each 16-bit word holds two characters, high byte
/// first) into `out`.
fn ata_string(b: &[u8], first_word: usize, out: &mut [u8]) {
    for i in 0..out.len() / 2 {
        out[2 * i] = b[(first_word + i) * 2 + 1];
        out[2 * i + 1] = b[(first_word + i) * 2];
    }
}

fn setup_disk(port: u8) -> Option<Disk> {
    if !stop_port(port) {
        return None;
    }
    let page = crate::mem::pmm::alloc_page_low()?;
    let mut d = Disk { port, sectors: 0, model: [b' '; 40], serial: [b' '; 20], page, bounce: [0; BOUNCE_PAGES] };
    for i in 0..BOUNCE_PAGES {
        match crate::mem::pmm::alloc_page_low() {
            Some(p) => d.bounce[i] = p,
            None => {
                free_disk_pages(&d, i);
                return None;
            }
        }
    }
    pwr(port, P_CLB, (page + OFF_CMD_LIST as u64) as u32);
    pwr(port, P_CLBU, 0);
    pwr(port, P_FB, (page + OFF_FIS as u64) as u32);
    pwr(port, P_FBU, 0);
    pwr(port, P_SERR, 0xFFFF_FFFF);
    pwr(port, P_IS, 0xFFFF_FFFF);
    pwr(port, P_IE, 0); // polling only
    if !start_port(port) {
        free_disk_pages(&d, BOUNCE_PAGES);
        return None;
    }

    // IDENTIFY DEVICE into the first bounce page.
    if exec(&d, ATA_IDENTIFY, 0, 0, false, 512).is_err() {
        free_disk_pages(&d, BOUNCE_PAGES);
        return None;
    }
    // SAFETY: bounce[0] is our low page holding the 512-byte identify block.
    let id = unsafe { core::slice::from_raw_parts(d.bounce[0] as *const u8, 512) };
    let lba48 = le16(id, 83) & (1 << 10) != 0;
    let sectors = if lba48 {
        (le16(id, 100) as u64)
            | (le16(id, 101) as u64) << 16
            | (le16(id, 102) as u64) << 32
            | (le16(id, 103) as u64) << 48
    } else {
        (le16(id, 60) as u64) | (le16(id, 61) as u64) << 16
    };
    // Logical sector size other than 512 bytes: not supported.
    let w106 = le16(id, 106);
    if w106 & 0xC000 == 0x4000 && w106 & (1 << 12) != 0 {
        let words = (le16(id, 117) as u32) | (le16(id, 118) as u32) << 16;
        if words != 256 {
            free_disk_pages(&d, BOUNCE_PAGES);
            return None;
        }
    }
    if !lba48 || sectors == 0 {
        free_disk_pages(&d, BOUNCE_PAGES);
        return None;
    }
    d.sectors = sectors;
    ata_string(id, 27, &mut d.model);
    ata_string(id, 10, &mut d.serial);
    Some(d)
}

fn free_disk_pages(d: &Disk, bounce_used: usize) {
    crate::mem::pmm::free_page(d.page);
    for i in 0..bounce_used.min(BOUNCE_PAGES) {
        crate::mem::pmm::free_page(d.bounce[i]);
    }
}

/// Probe the AHCI controller and its disks.  Returns the number of disks.
/// Needs PCI enumerated and the system tick running (command time-outs).
pub fn init() -> usize {
    let Some(dev) = pci::find_class(0x01, 0x06, Some(0x01)) else { return 0 };
    let Some(bar) = dev.bar(5) else { return 0 };
    if bar.io || bar.size < 0x180 {
        return 0;
    }
    let Some(hba) = crate::mem::vmm::phys_access(bar.addr) else { return 0 };
    dev.enable(pci::CMD_MEM | pci::CMD_BUS_MASTER);

    let s = st();
    s.hba = hba;
    s.pci_bus = dev.bus;
    s.pci_dev = dev.dev;
    s.pci_func = dev.func;
    wr(hba + HBA_GHC, rd(hba + HBA_GHC) | GHC_AE);
    let cap = rd(hba + HBA_CAP);
    s.ncs = ((cap >> 8) & 0x1F) as u8 + 1;
    s.s64a = cap & (1 << 31) != 0;
    s.version = rd(hba + HBA_VS);
    let pi = rd(hba + HBA_PI);
    s.n_ports_impl = pi.count_ones();
    wr(hba + HBA_IS, 0xFFFF_FFFF);
    s.ready = true;

    let mut n = 0;
    for port in 0..32u8 {
        // Only ports whose register block fits inside the BAR (ICH9 has 4 KiB).
        if pi & (1 << port) == 0 || n >= MAX_DISKS || 0x100 + 0x80 * (port as u64 + 1) > bar.size {
            continue;
        }
        let ssts = prd(port, P_SSTS);
        if ssts & 0xF != 3 || (ssts >> 8) & 0xF != 1 || prd(port, P_SIG) != SIG_SATA {
            continue;
        }
        if let Some(d) = setup_disk(port) {
            s.disks[n] = Some(d);
            n += 1;
        }
    }
    n
}

// --------------------------------------------------------------------- API

pub struct Info {
    pub version: u32,
    pub ports: u32,
    pub slots: u8,
    pub s64a: bool,
    pub pci: (u8, u8, u8),
}

pub fn controller() -> Option<Info> {
    let s = st();
    if !s.ready {
        return None;
    }
    Some(Info {
        version: s.version,
        ports: s.n_ports_impl,
        slots: s.ncs,
        s64a: s.s64a,
        pci: (s.pci_bus, s.pci_dev, s.pci_func),
    })
}

pub fn disk_count() -> usize {
    st().disks.iter().filter(|d| d.is_some()).count()
}

/// A SATA disk, addressed by index (0..disk_count()).
#[derive(Clone, Copy)]
pub struct AhciDisk(pub usize);

pub fn disk(i: usize) -> Option<AhciDisk> {
    st().disks.get(i)?.as_ref().map(|_| AhciDisk(i))
}

impl AhciDisk {
    fn d(&self) -> &'static Disk {
        st().disks[self.0].as_ref().expect("disk vanished")
    }

    pub fn port(&self) -> u8 {
        self.d().port
    }

    pub fn model(&self) -> &'static str {
        core::str::from_utf8(&self.d().model).unwrap_or("?").trim()
    }

    pub fn serial(&self) -> &'static str {
        core::str::from_utf8(&self.d().serial).unwrap_or("?").trim()
    }
}

impl BlockDev for AhciDisk {
    fn sectors(&self) -> u64 {
        self.d().sectors
    }

    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        let d = self.d();
        if buf.len() % SECTOR != 0 {
            return Err(BlkError::Io);
        }
        let total = (buf.len() / SECTOR) as u64;
        if lba.checked_add(total).map_or(true, |e| e > d.sectors) {
            return Err(BlkError::OutOfRange);
        }
        let (mut lba, mut done) = (lba, 0usize);
        while done < buf.len() {
            let n = ((buf.len() - done) / SECTOR).min(MAX_CHUNK_SECTORS);
            let bytes = n * SECTOR;
            exec(d, ATA_READ_DMA_EXT, lba, n as u32, false, bytes)?;
            for i in 0..(bytes + 4095) / 4096 {
                let len = (bytes - i * 4096).min(4096);
                // SAFETY: bounce pages are ours; the destination slice is in range.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        d.bounce[i] as *const u8,
                        buf.as_mut_ptr().add(done + i * 4096),
                        len,
                    );
                }
            }
            done += bytes;
            lba += n as u64;
        }
        Ok(())
    }

    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
        let d = self.d();
        if buf.len() % SECTOR != 0 {
            return Err(BlkError::Io);
        }
        let total = (buf.len() / SECTOR) as u64;
        if lba.checked_add(total).map_or(true, |e| e > d.sectors) {
            return Err(BlkError::OutOfRange);
        }
        let (mut lba, mut done) = (lba, 0usize);
        while done < buf.len() {
            let n = ((buf.len() - done) / SECTOR).min(MAX_CHUNK_SECTORS);
            let bytes = n * SECTOR;
            for i in 0..(bytes + 4095) / 4096 {
                let len = (bytes - i * 4096).min(4096);
                // SAFETY: as in read().
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buf.as_ptr().add(done + i * 4096),
                        d.bounce[i] as *mut u8,
                        len,
                    );
                }
            }
            exec(d, ATA_WRITE_DMA_EXT, lba, n as u32, true, bytes)?;
            done += bytes;
            lba += n as u64;
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), BlkError> {
        exec(self.d(), ATA_FLUSH_EXT, 0, 0, false, 0)
    }
}
