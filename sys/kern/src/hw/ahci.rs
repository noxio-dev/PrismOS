// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// SATA AHCI driver (Intel AHCI 1.3.1), phase 1.5: interrupt-driven
// completion through MSI and Native Command Queuing instead of polling.
//
//  * Finds the first AHCI controller on the PCI bus (class 01:06:01),
//    enables memory space + bus mastering, and wires its MSI capability to
//    vector 0x31 -- no INTx/IOAPIC routing needed.
//  * Every implemented port with a SATA disk attached (signature
//    0x00000101) becomes a disk, up to MAX_DISKS.
//  * Commands: IDENTIFY DEVICE and FLUSH CACHE EXT run in slot 0; READ and
//    WRITE use FPDMA QUEUED (NCQ) in up to NCQ_DEPTH slots at once, each
//    covering up to 64 KiB through its own bounce pages, issued through
//    PxSACT|PxCI.  Completion is signalled by the device (D2H / Set
//    Device Bits FIS with I=1) as an MSI, the handler clears PxSACT bits
//    into a per-disk completion bitmap; waiters sleep with `sti; hlt` when
//    interrupts are on and fall back to polling otherwise (and on time-out).
//  * All DMA memory is below 4 GiB (pmm::alloc_page_low).
//
// AHCI requires the per-port command list (1 KiB), the received-FIS area
// (256 B) and each command table (CFIS + ACMD + PRDT) to live in device
// memory; one 4 KiB page holds the list+FIS, and slots 0..NCQ_DEPTH-1 get
// a page each for their table.

#![allow(dead_code)]

use core::ptr::addr_of_mut;
use core::sync::atomic::{fence, AtomicBool, AtomicU32, Ordering};

use crate::hw::blk::{BlkError, BlockDev, SECTOR};
use crate::hw::pci;

pub const MAX_DISKS: usize = 4;
const BOUNCE_PAGES: usize = 16; // per NCQ command
const MAX_CHUNK_SECTORS: usize = BOUNCE_PAGES * 4096 / SECTOR; // 128
/// How many NCQ commands to keep in flight (bounded by the controller too).
const NCQ_DEPTH: usize = 8;

// HBA registers
const HBA_CAP: u64 = 0x00;
const HBA_GHC: u64 = 0x04;
const HBA_IS: u64 = 0x08;
const HBA_PI: u64 = 0x0C;
const HBA_VS: u64 = 0x10;
const GHC_AE: u32 = 1 << 31;
const GHC_IE: u32 = 1 << 1;

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
const IS_DHRS: u32 = 1 << 0;
const IS_SDBS: u32 = 1 << 5;
const IS_PSS: u32 = 1 << 26;
const IS_DPS: u32 = 1 << 5;

const SIG_SATA: u32 = 0x0000_0101;

// ATA commands
const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_FPDMA: u8 = 0x60; // NCQ read
const ATA_WRITE_FPDMA: u8 = 0x61; // NCQ write
const ATA_FLUSH_EXT: u8 = 0xEA;

// Layout of the per-port page
const OFF_CMD_LIST: usize = 0x000; // 32 headers x 32 bytes = 1 KiB
const OFF_FIS: usize = 0x400; // 256 bytes
// Command table of slot i: its own page at (port page + 1 + i)
const TABLE_HDR: usize = 0x80; // CFIS(64) + ACMD(16) + rsvd(48)

#[derive(Clone, Copy)]
struct Disk {
    port: u8,
    sectors: u64,
    model: [u8; 40],
    serial: [u8; 20],
    page: u64,
    /// Bounce pages, NCQ_DEPTH chunks of BOUNCE_PAGES.
    bounce: u64,
    /// NCQ support from IDENTIFY (word 77 queue depth / word 76 bit 8).
    ncq: bool,
    /// Command-table pages allocated after the port page (contiguous run).
    table_pages: usize,
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
    /// Per-disk bitmask of completed commands, set from the MSI handler.
    done: [AtomicU32; MAX_DISKS],
    /// Per-disk bitmask of commands in flight (mirrors PxSACT|PxCI).
    inflight: [AtomicU32; MAX_DISKS],
    /// Set when a command completed with a task file error.
    error: [AtomicBool; MAX_DISKS],
    /// Interrupts delivered (for the `ahci` command).
    pub_irqs: AtomicU32,
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
    done: [const { AtomicU32::new(0) }; MAX_DISKS],
    inflight: [const { AtomicU32::new(0) }; MAX_DISKS],
    error: [const { AtomicBool::new(false) }; MAX_DISKS],
    pub_irqs: AtomicU32::new(0),
};

fn st() -> &'static mut State {
    // SAFETY: the disk path is serialized by the kernel's single shell
    // context; the MSI handler only touches the atomic fields.
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

/// Are interrupts enabled in RFLAGS right now?
fn interrupts_on() -> bool {
    let flags: u64;
    // SAFETY: reading RFLAGS is side-effect free.
    unsafe { core::arch::asm!("pushfq; pop {}", out(reg) flags, options(nomem)) };
    flags & 0x200 != 0
}

/// Wait for slot `mask` of disk `d` to complete.  With interrupts on this
/// is truly interrupt-driven (`sti; hlt` sleeps until the MSI lands);
/// otherwise (IF=0 context, or after a time-out) it falls back to polling.
fn wait_slots(d: usize, mask: u32, ms: u64) -> Result<(), BlkError> {
    let Some(disk) = st().disks[d].as_ref() else { return Err(BlkError::Io) };
    let port = disk.port;
    let deadline = crate::hw::pit::uptime_ms() + ms;
    loop {
        // Harvest from the hardware directly: a finished command has both
        // its PxSACT (NCQ) and PxCI bits cleared.  This doubles as the
        // polling fallback when the MSI never arrives (or is not wired).
        let pending = prd(port, P_SACT) | prd(port, P_CI);
        let hw_done = st().inflight[d].load(Ordering::Acquire) & !pending;
        if hw_done != 0 {
            st().done[d].fetch_or(hw_done, Ordering::AcqRel);
            st().inflight[d].fetch_and(!hw_done, Ordering::AcqRel);
        }
        if prd(port, P_IS) & IS_TFES != 0 {
            return Err(BlkError::Io);
        }
        let done = st().done[d].load(Ordering::Acquire);
        if done & mask == mask {
            return Ok(());
        }
        if crate::hw::pit::uptime_ms() >= deadline {
            return Err(BlkError::Timeout);
        }
        if interrupts_on() {
            // Truly interrupt-driven: sleep until the MSI (or the tick)
            // lands.  SAFETY: deliberate sti;hlt -- no trailing cli: the
            // interrupt handler's iretq brings IF back set, so we return
            // to the loop (and eventually to the caller) with interrupts
            // in the state we found them.  Leaking IF=0 here froze the
            // shell after the first disk access (hlt with interrupts off).
            unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
        } else {
            core::hint::spin_loop();
        }
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

/// After an error: stop and restart the port so the next command can run.
fn recover(port: u8) {
    pwr(port, P_IS, 0xFFFF_FFFF);
    pwr(port, P_SERR, 0xFFFF_FFFF);
    if stop_port(port) {
        start_port(port);
    }
}

/// Table page of slot `slot` on the port page `page`.
fn table_of(page: u64, slot: u8) -> u64 {
    page + 4096 * (1 + slot as u64)
}

/// Build one command header + table for `slot`.  `bytes` is the data length
/// (0 for no data), `write` the direction, `ncq` FPDMA vs legacy slot-0 use.
/// FPDMA carries the sector count in the auxiliary word of the register FIS.
unsafe fn build_cmd(
    d: &Disk,
    slot: u8,
    cmd: u8,
    lba: u64,
    count: u32,
    write: bool,
    bytes: usize,
    ncq: bool,
) {
    let prdtl = (bytes + 4095) / 4096;
    let hdr = (d.page + OFF_CMD_LIST as u64 + 32 * slot as u64) as *mut u32;
    // Command header: 5 dwords (CFL=5, A=0, W, P=ncq, C=0 for NCQ), PRDTL.
    let mut flags = 5u32 | ((write as u32) << 6) | ((prdtl as u32) << 16);
    if ncq {
        flags |= 1 << 7; // P: prefetchable, harmless and lets hosts queue
    }
    hdr.write_volatile(flags);
    hdr.add(1).write_volatile(0); // PRDBC (byte count, informational)
    hdr.add(2).write_volatile(table_of(d.page, slot) as u32);
    hdr.add(3).write_volatile(0);

    let t = table_of(d.page, slot) as *mut u8;
    core::ptr::write_bytes(t, 0, 0x80 + 16 * BOUNCE_PAGES);
    t.write(0x27); // FIS type: register host to device
    t.add(1).write(0x80); // C bit: command
    t.add(2).write(cmd);
    t.add(4).write(lba as u8);
    t.add(5).write((lba >> 8) as u8);
    t.add(6).write((lba >> 16) as u8);
    t.add(7).write(if cmd == ATA_IDENTIFY { 0 } else { 0x40 }); // device: LBA
    t.add(8).write((lba >> 24) as u8);
    t.add(9).write((lba >> 32) as u8);
    t.add(10).write((lba >> 40) as u8);
    if ncq {
        // FPDMA: the sector count travels in the FEATURES field (SATA
        // queued commands) and the queue tag in the auxiliary dword.
        t.add(3).write(count as u8);
        t.add(0x0B).write((count >> 8) as u8);
        t.add(0x10).write(slot); // aux: tag (bits 0-5)
    } else {
        t.add(12).write(count as u8);
        t.add(13).write((count >> 8) as u8);
    }
    let mut left = bytes;
    for i in 0..prdtl {
        let e = t.add(0x80 + 16 * i) as *mut u32;
        let len = left.min(4096);
        // Each NCQ command gets its own bounce chunk: slot's chunk.
        let base = if ncq { d.bounce + 4096 * (BOUNCE_PAGES as u64 * slot as u64 + i as u64) } else { d.bounce + 4096 * i as u64 };
        e.write(base as u32);
        e.add(1).write(0);
        e.add(2).write(0);
        e.add(3).write((len - 1) as u32);
        left -= len;
    }
}

/// Run one command (slot 0, non-NCQ) to completion: IDENTIFY / FLUSH.
/// `idx` is the disk's slot in the table (the caller registers the disk
/// first so the completion path can find it).
fn exec_simple(idx: usize, cmd: u8, bytes: usize, write: bool) -> Result<(), BlkError> {
    let d = st().disks[idx].as_ref().ok_or(BlkError::Io)?;
    let port = d.port;
    // SAFETY: the port page and bounce chunk are ours.
    unsafe { build_cmd(d, 0, cmd, 0, 0, write, bytes, false) };
    fence(Ordering::SeqCst);
    if !wait(1000, || prd(port, P_TFD) & (TFD_BSY | TFD_DRQ) == 0) {
        recover(port);
        return Err(BlkError::Timeout);
    }
    let mask = 1u32;
    st().done[idx].store(0, Ordering::Release);
    st().inflight[idx].store(mask, Ordering::Release);
    pwr(port, P_IS, 0xFFFF_FFFF);
    pwr(port, P_SERR, 0xFFFF_FFFF);
    pwr(port, P_CI, 1);
    // Interrupt-driven wait with the polling fallback inside.
    wait_slots(idx, mask, 10_000)?;
    st().inflight[idx].store(0, Ordering::Release);
    if prd(port, P_TFD) & TFD_ERR != 0 {
        recover(port);
        return Err(BlkError::Io);
    }
    Ok(())
}

fn disk_index(d: &Disk) -> Option<usize> {
    let addr = d as *const Disk as usize;
    st().disks.iter().position(|x| x.as_ref().map_or(false, |e| e as *const Disk as usize == addr))
}

/// Issue one NCQ command in `slot` (the caller manages the flight set).
unsafe fn issue_ncq(d: &Disk, slot: u8, cmd: u8, lba: u64, count: u32, write: bool, bytes: usize) {
    build_cmd(d, slot, cmd, lba, count, write, bytes, true);
    let idx = disk_index(d).unwrap();
    st().done[idx].fetch_and(!(1 << slot), Ordering::AcqRel);
    st().inflight[idx].fetch_or(1 << slot, Ordering::AcqRel);
}

/// The MSI handler: harvest finished commands into `done`.
pub fn on_irq() {
    let s = st();
    if !s.ready {
        return;
    }
    s.pub_irqs.fetch_add(1, Ordering::Relaxed);
    wr(s.hba + HBA_IS, 0xFFFF_FFFF);
    for d in s.disks.iter().flatten() {
        let port = d.port;
        let is = prd(port, P_IS);
        if is == 0 {
            continue;
        }
        pwr(port, P_IS, is); // clear
        if is & IS_TFES != 0 {
            if let Some(i) = disk_index(d) {
                s.error[i].store(true, Ordering::Release);
            }
            pwr(port, P_SERR, 0xFFFF_FFFF);
            continue;
        }
        if is & (IS_DHRS | IS_SDBS | IS_PSS | IS_DPS) == 0 {
            continue;
        }
        // For NCQ, the device clears PxSACT bits of finished commands; for
        // slot-0 legacy commands, PxCI clears.
        let sact = prd(port, P_SACT);
        let ci = prd(port, P_CI);
        if let Some(i) = disk_index(d) {
            let inflight = s.inflight[i].load(Ordering::Acquire);
            let finished = inflight & !sact & !ci;
            if finished != 0 {
                s.done[i].fetch_or(finished, Ordering::AcqRel);
                s.inflight[i].fetch_and(!finished, Ordering::AcqRel);
            }
        }
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

/// Walk the PCI capability list and set up a single-message MSI at
/// vector 0x31 (edge, fixed, physical destination = this CPU's LAPIC).
fn setup_msi(dev: &pci::PciDev) -> bool {
    let mut cap = pci::cfg_read8(dev.bus, dev.dev, dev.func, 0x34) as u32 & !0x3;
    if cap == 0 {
        return false;
    }
    for _ in 0..48 {
        let id = pci::cfg_read8(dev.bus, dev.dev, dev.func, cap as u8);
        if id == 0x05 {
            // MSI capability: control @2, address @4, (upper @8 if 64-bit),
            // data @8 or @0xC.
            let ctrl = pci::cfg_read16(dev.bus, dev.dev, dev.func, (cap + 2) as u8);
            let is64 = ctrl & (1 << 7) != 0;
            let addr: u32 = 0xFEE0_0000 | ((crate::hw::apic::lapic_id() as u32) << 12);
            let data: u32 = 0x31; // vector, edge, fixed delivery
            pci::cfg_write32(dev.bus, dev.dev, dev.func, cap as u8, addr);
            if is64 {
                pci::cfg_write32(dev.bus, dev.dev, dev.func, (cap + 8) as u8, 0);
            }
            let data_off = if is64 { cap + 12 } else { cap + 8 };
            pci::cfg_write16(dev.bus, dev.dev, dev.func, data_off as u8, data as u16);
            // Enable, 1 message (MMEN=0 means one).
            pci::cfg_write16(dev.bus, dev.dev, dev.func, (cap + 2) as u8, ctrl | 1);
            return true;
        }
        let next = pci::cfg_read8(dev.bus, dev.dev, dev.func, (cap + 1) as u8) & 0xFC;
        if next == 0 {
            break;
        }
        cap = next as u32;
    }
    false
}

fn free_disk_pages(d: &Disk, bounce_used: usize) {
    // The port page heads one contiguous run of (1 + tables) pages; free
    // exactly what alloc_port_pages() took.
    for i in 0..d.table_pages + 1 {
        crate::mem::pmm::free_page(d.page + 4096 * i as u64);
    }
    for i in 0..bounce_used.min(NCQ_DEPTH * BOUNCE_PAGES) {
        crate::mem::pmm::free_page(d.bounce + 4096 * i as u64);
    }
}

/// Allocate the port page plus one command-table page per NCQ slot as ONE
/// physically contiguous run: the command headers address the tables as
/// (port page + 4096 * (1 + slot)), which only works when the pages are
/// adjacent.  (They were not before -- the "tables" landed on random
/// pages, often the bounce buffers, and the DMA wrote command-table bytes
/// to disk.)  Returns the base physical address, or None.
fn alloc_port_pages(tables: usize) -> Option<u64> {
    crate::mem::pmm::alloc_pages_low_contig(1 + tables)
}

fn setup_disk(port: u8, idx: usize) -> Option<Disk> {
    if !stop_port(port) {
        return None;
    }
    // Port page + one table page per NCQ slot, in one contiguous run
    // (the command headers reach the tables by physical adjacency).
    let page = alloc_port_pages(NCQ_DEPTH)?;
    let mut d = Disk {
        port,
        sectors: 0,
        model: [b' '; 40],
        serial: [b' '; 20],
        page,
        bounce: 0,
        ncq: false,
        table_pages: NCQ_DEPTH,
    };
    // Bounce memory: NCQ_DEPTH chunks of BOUNCE_PAGES (plus the first
    // chunk doubles as the slot-0 buffer for IDENTIFY/FLUSH).
    let mut got = 0usize;
    let mut bounce = 0u64;
    for _ in 0..NCQ_DEPTH * BOUNCE_PAGES {
        match crate::mem::pmm::alloc_page_low() {
            Some(p) => {
                if got == 0 {
                    bounce = p;
                }
                got += 1;
            }
            None => break,
        }
    }
    if got < BOUNCE_PAGES {
        if got > 0 {
            for i in 0..got {
                crate::mem::pmm::free_page(bounce + 4096 * i as u64);
            }
        }
        for i in 0..NCQ_DEPTH + 1 {
            crate::mem::pmm::free_page(page + 4096 * i as u64);
        }
        return None;
    }
    d.bounce = bounce;
    // Register now (partially built) so IDENTIFY's completion path can
    // find the disk; removed again on failure.  (The early `d` copies are
    // fine: only the final Some(d) below keeps the table_pages field.)
    st().disks[idx] = Some(d);
    pwr(port, P_CLB, (page + OFF_CMD_LIST as u64) as u32);
    pwr(port, P_CLBU, 0);
    pwr(port, P_FB, (page + OFF_FIS as u64) as u32);
    pwr(port, P_FBU, 0);
    pwr(port, P_SERR, 0xFFFF_FFFF);
    pwr(port, P_IS, 0xFFFF_FFFF);
    // Completion interrupts: device-to-host FIS + Set Device Bits (NCQ).
    pwr(port, P_IE, IS_DHRS | IS_SDBS | IS_PSS | IS_DPS);
    if !start_port(port) {
        st().disks[idx] = None;
        free_disk_pages(&d, got);
        return None;
    }

    // IDENTIFY DEVICE into the first bounce chunk (slot 0, non-NCQ).
    if exec_simple(idx, ATA_IDENTIFY, 512, false).is_err() {
        st().disks[idx] = None;
        free_disk_pages(&d, got);
        return None;
    }
    // SAFETY: the first bounce chunk is our low page holding the identify data.
    let id = unsafe { core::slice::from_raw_parts(d.bounce as *const u8, 512) };
    let lba48 = le16(id, 83) & (1 << 10) != 0;
    let sectors = if lba48 {
        (le16(id, 100) as u64) | (le16(id, 101) as u64) << 16 | (le16(id, 102) as u64) << 32 | (le16(id, 103) as u64) << 48
    } else {
        (le16(id, 60) as u64) | (le16(id, 61) as u64) << 16
    };
    // Logical sector size other than 512 bytes: not supported.
    let w106 = le16(id, 106);
    if w106 & 0xC000 == 0x4000 && w106 & (1 << 12) != 0 {
        let words = (le16(id, 117) as u32) | (le16(id, 118) as u32) << 16;
        if words != 256 {
            st().disks[idx] = None;
            free_disk_pages(&d, got);
            return None;
        }
    }
    if !lba48 || sectors == 0 {
        st().disks[idx] = None;
        free_disk_pages(&d, got);
        return None;
    }
    // NCQ: word 76 bit 8 (or queue depth in word 77 for SATA 2.5+).
    let mut d = d;
    d.ncq = le16(id, 76) & (1 << 8) != 0 || le16(id, 77) & 0x1F >= 1;
    if got < NCQ_DEPTH * BOUNCE_PAGES {
        d.ncq = false; // not enough bounce memory to queue
    }
    d.sectors = sectors;
    ata_string(id, 27, &mut d.model);
    ata_string(id, 10, &mut d.serial);
    Some(d)
}

/// Probe the AHCI controller and its disks.  Returns the number of disks.
/// Needs PCI enumerated, the system tick running and interrupts enabled
/// (completions arrive as MSIs).
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

    let msi = setup_msi(dev);
    if msi {
        // Global interrupt enable; per-port masks were set in setup_disk.
        wr(hba + HBA_GHC, rd(hba + HBA_GHC) | GHC_IE);
    }

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
        st().done[n].store(0, Ordering::Release);
        st().inflight[n].store(0, Ordering::Release);
        if let Some(d) = setup_disk(port, n) {
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
    pub msi: bool,
    pub irqs: u32,
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
        msi: (rd(s.hba + HBA_GHC) & GHC_IE) != 0,
        irqs: s.pub_irqs.load(Ordering::Relaxed),
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

    pub fn ncq(&self) -> bool {
        self.d().ncq
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

    /// Read `buf` (whole sectors): with NCQ, up to NCQ_DEPTH FPDMA reads
    /// run concurrently; without it, slot-0 DMA EXT commands one at a time.
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        let d = self.d();
        if buf.len() % SECTOR != 0 {
            return Err(BlkError::Io);
        }
        let total = (buf.len() / SECTOR) as u64;
        if lba.checked_add(total).map_or(true, |e| e > d.sectors) {
            return Err(BlkError::OutOfRange);
        }
        if !d.ncq {
            // Legacy path: one command at a time in slot 0.
            let (mut lba, mut done) = (lba, 0usize);
            while done < buf.len() {
                let n = ((buf.len() - done) / SECTOR).min(128);
                let bytes = n * SECTOR;
                unsafe { build_cmd(d, 0, 0x25, lba, n as u32, false, bytes, false) };
                fence(Ordering::SeqCst);
                if !wait(1000, || prd(d.port, P_TFD) & (TFD_BSY | TFD_DRQ) == 0) {
                    recover(d.port);
                    return Err(BlkError::Timeout);
                }
                let idx = self.0;
                st().done[idx].store(0, Ordering::Release);
                st().inflight[idx].store(1, Ordering::Release);
                pwr(d.port, P_IS, 0xFFFF_FFFF);
                pwr(d.port, P_CI, 1);
                wait_slots(idx, 1, 10_000)?;
                st().inflight[idx].store(0, Ordering::Release);
                // SAFETY: bounce pages are ours; the destination is in range.
                unsafe {
                    for i in 0..(bytes + 4095) / 4096 {
                        let len = (bytes - i * 4096).min(4096);
                        core::ptr::copy_nonoverlapping(
                            (d.bounce + 4096 * i as u64) as *const u8,
                            buf.as_mut_ptr().add(done + i * 4096),
                            len,
                        );
                    }
                }
                done += bytes;
                lba += n as u64;
            }
            return Ok(());
        }
        self.fpdma(lba, buf.as_mut_ptr(), buf.len(), false)
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
        self.write_legacy(lba, buf)
    }

    fn flush(&self) -> Result<(), BlkError> {
        exec_simple(self.0, ATA_FLUSH_EXT, 0, false)
    }
}

impl AhciDisk {
    /// Slot-0 DMA EXT write (also the !ncq fallback).
    fn write_legacy(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
        let d = self.d();
        let (mut lba, mut done) = (lba, 0usize);
        while done < buf.len() {
            let n = ((buf.len() - done) / SECTOR).min(128);
            let bytes = n * SECTOR;
            // SAFETY: bounce pages are ours; the source slice is in range.
            unsafe {
                for i in 0..(bytes + 4095) / 4096 {
                    let len = (bytes - i * 4096).min(4096);
                    core::ptr::copy_nonoverlapping(
                        buf.as_ptr().add(done + i * 4096),
                        (d.bounce + 4096 * i as u64) as *mut u8,
                        len,
                    );
                }
                build_cmd(d, 0, 0x35, lba, n as u32, true, bytes, false);
            }
            fence(Ordering::SeqCst);
            if !wait(1000, || prd(d.port, P_TFD) & (TFD_BSY | TFD_DRQ) == 0) {
                recover(d.port);
                return Err(BlkError::Timeout);
            }
            let idx = self.0;
            st().done[idx].store(0, Ordering::Release);
            st().inflight[idx].store(1, Ordering::Release);
            pwr(d.port, P_IS, 0xFFFF_FFFF);
            pwr(d.port, P_CI, 1);
            wait_slots(idx, 1, 10_000)?;
            st().inflight[idx].store(0, Ordering::Release);
            done += bytes;
            lba += n as u64;
        }
        Ok(())
    }

    /// NCQ engine: split the transfer into <= 128-sector chunks, keep up to
    /// `depth` FPDMA commands in flight, wait for each chunk as it lands.
    fn fpdma(&self, lba: u64, buf: *mut u8, len: usize, write: bool) -> Result<(), BlkError> {
        let d = self.d();
        let idx = self.0;
        let depth = NCQ_DEPTH.min(st().ncs as usize).min(8);
        let total_secs = len / SECTOR;
        let mut slot_sec = [0usize; 8];
        let mut slot_len = [0usize; 8];
        let mut inflight: u32 = 0;
        let mut next_slot = 0usize;
        let mut issued = 0usize; // sectors issued so far
        while issued < total_secs || inflight != 0 {
            // Fill the queue.
            while issued < total_secs && inflight.count_ones() < depth as u32 {
                let n = (total_secs - issued).min(MAX_CHUNK_SECTORS);
                let bytes = n * SECTOR;
                let slot = next_slot;
                if write {
                    // SAFETY: bounce chunk of this slot; source in range.
                    unsafe {
                        for i in 0..(bytes + 4095) / 4096 {
                            let n = (bytes - i * 4096).min(4096);
                            core::ptr::copy_nonoverlapping(
                                buf.add(issued * SECTOR + i * 4096),
                                (d.bounce + 4096 * (BOUNCE_PAGES as u64 * slot as u64 + i as u64)) as *mut u8,
                                n,
                            );
                        }
                    }
                }
                // SAFETY: slot table page and bounce chunk are ours.
                unsafe {
                    issue_ncq(
                        d,
                        slot as u8,
                        if write { ATA_WRITE_FPDMA } else { ATA_READ_FPDMA },
                        lba + issued as u64,
                        n as u32,
                        write,
                        bytes,
                    )
                };
                fence(Ordering::SeqCst);
                pwr(d.port, P_SACT, 1 << slot);
                pwr(d.port, P_CI, 1 << slot);
                inflight |= 1 << slot;
                slot_sec[slot] = issued;
                slot_len[slot] = n;
                issued += n;
                next_slot = (slot + 1) % depth;
            }
            // Wait for the lowest pending slot, then harvest everything
            // that finished.
            let first = inflight.trailing_zeros();
            wait_slots(idx, 1 << first, 20_000)?;
            let done_now = st().done[idx].load(Ordering::Acquire) & inflight;
            let mut s = 0usize;
            while s < depth {
                let bit = 1u32 << s;
                if done_now & bit != 0 {
                    let bytes = slot_len[s] * SECTOR;
                    let off = slot_sec[s] * SECTOR;
                    if !write {
                        // SAFETY: bounce chunk of this slot; dest in range.
                        unsafe {
                            for i in 0..(bytes + 4095) / 4096 {
                                let n = (bytes - i * 4096).min(4096);
                                core::ptr::copy_nonoverlapping(
                                    (d.bounce + 4096 * (BOUNCE_PAGES as u64 * s as u64 + i as u64)) as *const u8,
                                    buf.add(off + i * 4096),
                                    n,
                                );
                            }
                        }
                    }
                    inflight &= !bit;
                    slot_len[s] = 0;
                }
                s += 1;
            }
        }
        Ok(())
    }
}
