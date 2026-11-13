// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// NVMe driver (NVM Express 1.x over PCIe), polling mode.
//
// One admin queue pair and one I/O queue pair per controller, one command in
// flight at a time (a lock serializes callers).  Completion is found by
// polling the phase bit of the completion queue, so no interrupt vector is
// needed: the controller's interrupts are masked (INTMS) and PCI INTx is
// disabled.  An NVMe command completes in tens of microseconds, so `hal::
// wait_event` spins first and only sleeps (`hlt`) for slow devices.
//
// Data moves through a physically contiguous bounce buffer below 4 GiB: the
// kernel heap is not identity mapped, so a caller's slice has no stable
// physical address a device could use.  Up to 128 KiB per command (less when
// the controller's MDTS says so); larger requests are split.
//
// Namespaces are exposed through `NvmeNs` (implements `BlockDev` in
// 512-byte sectors on top of the namespace's own logical block size).

#![allow(dead_code)]

use crate::hw::blk::{BlkError, BlockDev};
use crate::hw::disk::{self, RawBlocks};
use crate::hw::hal::{self, Global, PAGE};
use crate::hw::pci;
use crate::kprintln;
use crate::lib::spin::SpinLock;
use core::sync::atomic::{AtomicBool, Ordering};

pub const MAX_NAMESPACES: usize = 4;
const MAX_CTRL: usize = 2;

// ---------------------------------------------------------------- registers

const REG_CAP: u64 = 0x00;
const REG_VS: u64 = 0x08;
const REG_INTMS: u64 = 0x0C;
const REG_CC: u64 = 0x14;
const REG_CSTS: u64 = 0x1C;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;
const REG_DOORBELL: u64 = 0x1000;

const CC_EN: u32 = 1;
const CC_IOSQES: u32 = 6 << 16; // 64-byte submission entries
const CC_IOCQES: u32 = 4 << 20; // 16-byte completion entries
const CSTS_RDY: u32 = 1;
const CSTS_CFS: u32 = 2;

// Admin opcodes
const ADM_DELETE_SQ: u32 = 0x00;
const ADM_CREATE_SQ: u32 = 0x01;
const ADM_DELETE_CQ: u32 = 0x04;
const ADM_CREATE_CQ: u32 = 0x05;
const ADM_IDENTIFY: u32 = 0x06;
// NVM opcodes
const NVM_FLUSH: u32 = 0x00;
const NVM_WRITE: u32 = 0x01;
const NVM_READ: u32 = 0x02;

const ADMIN_DEPTH: u16 = 16;
const IO_DEPTH: u16 = 64;
/// Largest transfer per command (bounce buffer size is chosen at init).
const MAX_CHUNK_PAGES: usize = 32;

const T_ADMIN_MS: u64 = 3_000;
const T_IO_MS: u64 = 10_000;

// ----------------------------------------------------------------- queues

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NvmeError {
    Timeout,
    /// Controller reported a non-zero status (SCT << 8 | SC).
    Status(u16),
    Dead,
}

impl From<NvmeError> for BlkError {
    fn from(e: NvmeError) -> BlkError {
        match e {
            NvmeError::Timeout => BlkError::Timeout,
            _ => BlkError::Io,
        }
    }
}

struct Queue {
    sq: u64,
    cq: u64,
    depth: u16,
    tail: u16,
    head: u16,
    phase: u32,
    sq_db: u64,
    cq_db: u64,
    cid: u16,
}

impl Queue {
    const fn empty() -> Queue {
        Queue { sq: 0, cq: 0, depth: 0, tail: 0, head: 0, phase: 1, sq_db: 0, cq_db: 0, cid: 0 }
    }

    /// Post `sqe` and wait for its completion.  Returns completion dword 0.
    fn exec(&mut self, sqe: &mut [u32; 16], ms: u64) -> Result<u32, NvmeError> {
        self.cid = self.cid.wrapping_add(1);
        let cid = self.cid;
        sqe[0] = (sqe[0] & 0xFFFF) | (cid as u32) << 16;
        let slot = self.sq + self.tail as u64 * 64;
        for (i, d) in sqe.iter().enumerate() {
            // SAFETY: slot is inside the SQ page we own (tail < depth).
            unsafe { hal::mem_wr32(slot + 4 * i as u64, *d) };
        }
        self.tail = (self.tail + 1) % self.depth;
        hal::mb();
        hal::wr32(self.sq_db, self.tail as u32);

        let cqe = self.cq + self.head as u64 * 16;
        let phase = self.phase;
        // SAFETY: cqe is inside the CQ page we own (head < depth).
        let arrived = hal::wait_event(ms, || unsafe { (hal::mem_rd32(cqe + 12) >> 16) & 1 == phase });
        if !arrived {
            return Err(NvmeError::Timeout);
        }
        hal::mb();
        // SAFETY: as above.
        let (dw0, dw3) = unsafe { (hal::mem_rd32(cqe), hal::mem_rd32(cqe + 12)) };
        self.head += 1;
        if self.head == self.depth {
            self.head = 0;
            self.phase ^= 1;
        }
        hal::wr32(self.cq_db, self.head as u32);
        if dw3 & 0xFFFF != cid as u32 {
            return Err(NvmeError::Status(0xFFFF));
        }
        let sf = (dw3 >> 17) as u16 & 0x7FFF;
        if sf != 0 {
            // SCT in bits 10:8, SC in bits 7:0 of the status field.
            return Err(NvmeError::Status(sf & 0x7FF));
        }
        Ok(dw0)
    }
}

/// Everything a command needs, behind the controller lock.
struct Inner {
    admin: Queue,
    io: Queue,
    /// Bounce buffer (identity mapped, below 4 GiB).
    buf: u64,
    /// One page of PRP list entries.
    list: u64,
    chunk: usize,
}

struct Ctrl {
    regs: u64,
    version: u32,
    mdts_bytes: usize,
    model: [u8; 40],
    serial: [u8; 20],
    nn: u32,
    pci: (u8, u8, u8, u16, u16),
    dead: AtomicBool,
    inner: SpinLock<Inner>,
}

#[derive(Clone, Copy)]
struct Ns {
    ctrl: u8,
    nsid: u32,
    blocks: u64,
    bs: u32,
}

struct State {
    ctrls: [Option<Ctrl>; MAX_CTRL],
    ns: [Option<Ns>; MAX_NAMESPACES],
}

static STATE: Global<State> = Global::new(State { ctrls: [None, None], ns: [None; MAX_NAMESPACES] });

fn st() -> &'static State {
    // SAFETY: mutated only by `init` at boot, shared reads afterwards.
    unsafe { STATE.get() }
}

// ------------------------------------------------------------------ helpers

fn trim(s: &[u8]) -> &str {
    let mut end = s.len();
    while end > 0 && (s[end - 1] == b' ' || s[end - 1] == 0) {
        end -= 1;
    }
    let mut start = 0;
    while start < end && s[start] == b' ' {
        start += 1;
    }
    core::str::from_utf8(&s[start..end]).unwrap_or("?")
}

fn cmd(op: u32, nsid: u32) -> [u32; 16] {
    let mut c = [0u32; 16];
    c[0] = op;
    c[1] = nsid;
    c
}

fn set_prp(c: &mut [u32; 16], prp1: u64, prp2: u64) {
    c[6] = prp1 as u32;
    c[7] = (prp1 >> 32) as u32;
    c[8] = prp2 as u32;
    c[9] = (prp2 >> 32) as u32;
}

fn identify(inner: &mut Inner, cns: u32, nsid: u32) -> Result<(), NvmeError> {
    let mut c = cmd(ADM_IDENTIFY, nsid);
    set_prp(&mut c, inner.buf, 0);
    c[10] = cns;
    // The result lands in page 0 of the bounce buffer; clear stale data so a
    // short answer cannot be mistaken for an old one.
    // SAFETY: the buffer is ours and at least one page.
    unsafe { core::ptr::write_bytes(inner.buf as *mut u8, 0, PAGE) };
    inner.admin.exec(&mut c, T_ADMIN_MS).map(|_| ())
}

fn ident_buf(inner: &Inner) -> &'static [u8] {
    // SAFETY: one page of identity-mapped memory owned by the controller;
    // callers hold the controller lock while they read it.
    unsafe { core::slice::from_raw_parts(inner.buf as *const u8, PAGE) }
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn le64(b: &[u8], o: usize) -> u64 {
    le32(b, o) as u64 | (le32(b, o + 4) as u64) << 32
}

// --------------------------------------------------------------------- init

/// Probe every NVMe controller on the PCI bus.  Returns the number of
/// namespaces registered.  Safe to call once, at boot, after `pci::init`.
pub fn init() -> usize {
    let mut nctrl = 0;
    for d in pci::devices() {
        if d.class == 0x01 && d.subclass == 0x08 && d.progif == 0x02 {
            if nctrl >= MAX_CTRL {
                break;
            }
            match probe(d) {
                Some(c) => {
                    // SAFETY: boot-time, single context.
                    unsafe { STATE.get().ctrls[nctrl] = Some(c) };
                    nctrl += 1;
                    scan_namespaces(nctrl - 1);
                }
                None => kprintln!(
                    "nvme: {:02x}:{:02x}.{} controller failed to initialise",
                    d.bus,
                    d.dev,
                    d.func
                ),
            }
        }
    }
    count_ns()
}

fn count_ns() -> usize {
    st().ns.iter().flatten().count()
}

fn wait_ready(regs: u64, ready: bool, ms: u64) -> bool {
    hal::wait_event(ms, || {
        let s = hal::rd32(regs + REG_CSTS);
        s & CSTS_CFS == 0 && (s & CSTS_RDY != 0) == ready
    })
}

fn probe(d: &pci::PciDev) -> Option<Ctrl> {
    let bar = d.bar(0)?;
    if bar.io || bar.size < 0x1008 {
        return None;
    }
    d.enable(pci::CMD_MEM | pci::CMD_BUS_MASTER | pci::CMD_INTX_DISABLE);
    let regs = hal::map_bar(bar.addr, bar.size.min(0x10000))?;

    let cap = hal::rd64(regs + REG_CAP);
    if cap == u64::MAX {
        return None; // device not responding
    }
    let mqes = (cap & 0xFFFF) as u16; // 0-based
    let to_ms = (((cap >> 24) & 0xFF).max(1)) * 500;
    let dstrd = ((cap >> 32) & 0xF) as u32;
    let css = (cap >> 37) & 0xFF;
    let mpsmin = (cap >> 48) & 0xF;
    if css & 1 == 0 || mpsmin != 0 {
        kprintln!("nvme: controller lacks NVM command set or 4 KiB pages");
        return None;
    }
    let stride = 4u64 << dstrd;
    // Highest doorbell we touch: admin + one I/O queue pair.
    let db_end = REG_DOORBELL + 4 * stride;
    if db_end + 4 > bar.size.min(0x10000) {
        kprintln!("nvme: doorbells beyond the mapped BAR");
        return None;
    }
    let version = hal::rd32(regs + REG_VS);

    // Quiesce: disable, wait for RDY=0, mask every interrupt.
    let cc = hal::rd32(regs + REG_CC);
    if cc & CC_EN != 0 {
        hal::wr32(regs + REG_CC, cc & !CC_EN);
    }
    if !wait_ready(regs, false, to_ms) {
        return None;
    }
    hal::wr32(regs + REG_INTMS, 0xFFFF_FFFF);

    // Memory: queues, bounce buffer, PRP list.
    let admin_depth = ADMIN_DEPTH.min(mqes.saturating_add(1));
    let io_depth = IO_DEPTH.min(mqes.saturating_add(1));
    if admin_depth < 2 || io_depth < 2 {
        return None;
    }
    let mut pages = MAX_CHUNK_PAGES;
    let (asq, acq, isq, icq, list, buf);
    {
        let a = hal::dma_alloc(5)?; // asq, acq, isq, icq, list
        asq = a;
        acq = a + PAGE as u64;
        isq = a + 2 * PAGE as u64;
        icq = a + 3 * PAGE as u64;
        list = a + 4 * PAGE as u64;
    }
    loop {
        match hal::dma_alloc(pages) {
            Some(b) => {
                buf = b;
                break;
            }
            None if pages > 2 => pages /= 4,
            None => {
                hal::dma_free(asq, 5);
                return None;
            }
        }
    }
    let mut inner = Inner {
        admin: Queue::empty(),
        io: Queue::empty(),
        buf,
        list,
        chunk: pages * PAGE,
    };
    inner.admin = Queue {
        sq: asq,
        cq: acq,
        depth: admin_depth,
        tail: 0,
        head: 0,
        phase: 1,
        sq_db: regs + REG_DOORBELL,
        cq_db: regs + REG_DOORBELL + stride,
        cid: 0,
    };

    hal::wr32(regs + REG_AQA, (admin_depth as u32 - 1) | (admin_depth as u32 - 1) << 16);
    hal::wr64(regs + REG_ASQ, asq);
    hal::wr64(regs + REG_ACQ, acq);
    hal::wr32(regs + REG_CC, CC_EN | CC_IOSQES | CC_IOCQES); // MPS=0 (4 KiB), CSS=0 (NVM)
    if !wait_ready(regs, true, to_ms) {
        kprintln!("nvme: controller never became ready");
        return None;
    }

    // I/O queue pair 1: completion queue first, then its submission queue.
    let mut c = cmd(ADM_CREATE_CQ, 0);
    set_prp(&mut c, icq, 0);
    c[10] = (io_depth as u32 - 1) << 16 | 1;
    c[11] = 1; // physically contiguous, interrupts disabled
    if let Err(e) = inner.admin.exec(&mut c, T_ADMIN_MS) {
        kprintln!("nvme: create I/O CQ failed: {:?}", e);
        return None;
    }
    let mut c = cmd(ADM_CREATE_SQ, 0);
    set_prp(&mut c, isq, 0);
    c[10] = (io_depth as u32 - 1) << 16 | 1;
    c[11] = 1 << 16 | 1; // CQ 1, contiguous
    if let Err(e) = inner.admin.exec(&mut c, T_ADMIN_MS) {
        kprintln!("nvme: create I/O SQ failed: {:?}", e);
        return None;
    }
    inner.io = Queue {
        sq: isq,
        cq: icq,
        depth: io_depth,
        tail: 0,
        head: 0,
        phase: 1,
        sq_db: regs + REG_DOORBELL + 2 * stride,
        cq_db: regs + REG_DOORBELL + 3 * stride,
        cid: 0,
    };

    // Identify controller.
    if let Err(e) = identify(&mut inner, 1, 0) {
        kprintln!("nvme: identify controller failed: {:?}", e);
        return None;
    }
    let id = ident_buf(&inner);
    let mut serial = [0u8; 20];
    let mut model = [0u8; 40];
    serial.copy_from_slice(&id[4..24]);
    model.copy_from_slice(&id[24..64]);
    let mdts = id[77] as u32;
    let nn = le32(id, 516);
    let mdts_bytes = if mdts == 0 || mdts > 20 { usize::MAX } else { PAGE << mdts };
    inner.chunk = inner.chunk.min(mdts_bytes);

    Some(Ctrl {
        regs,
        version,
        mdts_bytes,
        model,
        serial,
        nn,
        pci: (d.bus, d.dev, d.func, d.vendor, d.device),
        dead: AtomicBool::new(false),
        inner: SpinLock::new(inner),
    })
}

fn scan_namespaces(ci: usize) {
    let c = st().ctrls[ci].as_ref().unwrap();
    let mut inner = c.inner.lock();
    let mut nsids = [0u32; 32];
    let mut n = 0;

    if c.version >= 0x0001_0100 {
        // Active namespace ID list (CNS 2): up to 1024 ids, ascending.
        if identify(&mut inner, 2, 0).is_ok() {
            let b = ident_buf(&inner);
            while n < nsids.len() && n < 1024 {
                let id = le32(b, 4 * n);
                if id == 0 {
                    break;
                }
                nsids[n] = id;
                n += 1;
            }
        }
    } else {
        for id in 1..=c.nn.min(nsids.len() as u32) {
            nsids[n] = id;
            n += 1;
        }
    }

    for &nsid in &nsids[..n] {
        if identify(&mut inner, 0, nsid).is_err() {
            continue;
        }
        let b = ident_buf(&inner);
        let nsze = le64(b, 0);
        let nlbaf = b[25] as usize + 1;
        let flbas = b[26] as usize;
        let mut fi = flbas & 0xF;
        if nlbaf > 16 {
            fi |= ((flbas >> 5) & 3) << 4;
        }
        if nsze == 0 || fi >= nlbaf || fi >= 64 {
            continue;
        }
        let lbaf = le32(b, 128 + 4 * fi);
        let ms = lbaf & 0xFFFF;
        let lbads = (lbaf >> 16) & 0xFF;
        if ms != 0 || !(9..=16).contains(&lbads) {
            continue; // per-block metadata / odd sizes: not supported
        }
        let bs = 1u32 << lbads;
        if bs as usize > inner.chunk {
            continue;
        }
        // Register the namespace in the first free slot.
        // SAFETY: boot-time, single context.
        let ns = unsafe { &mut STATE.get().ns };
        if let Some(slot) = ns.iter_mut().find(|s| s.is_none()) {
            *slot = Some(Ns { ctrl: ci as u8, nsid, blocks: nsze, bs });
        }
    }
}

// -------------------------------------------------------------- public API

pub fn controllers() -> usize {
    st().ctrls.iter().flatten().count()
}

pub struct Info {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub version: (u16, u8, u8),
    pub model: &'static str,
    pub serial: &'static str,
    pub max_transfer: usize,
    pub failed: bool,
}

pub fn info(i: usize) -> Option<Info> {
    let c = st().ctrls.get(i)?.as_ref()?;
    Some(Info {
        bus: c.pci.0,
        dev: c.pci.1,
        func: c.pci.2,
        vendor: c.pci.3,
        device: c.pci.4,
        version: ((c.version >> 16) as u16, (c.version >> 8) as u8, c.version as u8),
        model: trim(&c.model),
        serial: trim(&c.serial),
        max_transfer: c.mdts_bytes,
        failed: c.dead.load(Ordering::Relaxed),
    })
}

/// Namespace handle `i` (0-based over all controllers).
pub fn namespace(i: usize) -> Option<NvmeNs> {
    if i < MAX_NAMESPACES && st().ns[i].is_some() {
        Some(NvmeNs(i))
    } else {
        None
    }
}

#[derive(Clone, Copy)]
pub struct NvmeNs(pub usize);

impl NvmeNs {
    fn ns(&self) -> &'static Ns {
        st().ns[self.0].as_ref().expect("nvme: stale namespace handle")
    }

    fn ctrl(&self) -> &'static Ctrl {
        st().ctrls[self.ns().ctrl as usize].as_ref().unwrap()
    }

    pub fn nsid(&self) -> u32 {
        self.ns().nsid
    }

    pub fn model(&self) -> &'static str {
        trim(&self.ctrl().model)
    }

    pub fn serial(&self) -> &'static str {
        trim(&self.ctrl().serial)
    }

    pub fn block_size(&self) -> usize {
        self.ns().bs as usize
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
        let c = self.ctrl();
        if c.dead.load(Ordering::Relaxed) {
            return Err(BlkError::Io);
        }
        let mut inner = c.inner.lock();
        let mut cm = cmd(NVM_FLUSH, self.ns().nsid);
        run(c, &mut inner, &mut cm)
    }
}

impl RawBlocks for NvmeNs {
    fn block_size(&self) -> usize {
        self.ns().bs as usize
    }

    fn blocks(&self) -> u64 {
        self.ns().blocks
    }

    fn read_blocks(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        self.transfer(lba, buf.len(), false, |off, dma, n| {
            // SAFETY: dma holds n bytes just written by the device.
            let src = unsafe { core::slice::from_raw_parts(dma as *const u8, n) };
            buf[off..off + n].copy_from_slice(src);
        })
    }

    fn write_blocks(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
        // Copy-in happens inside `transfer` before the command is built.
        let mut src = buf;
        self.transfer_out(lba, &mut src)
    }
}

/// Submit a prepared I/O command; a timeout kills the controller (a late
/// completion would otherwise land in a queue we have given up on).
fn run(c: &Ctrl, inner: &mut Inner, cm: &mut [u32; 16]) -> Result<(), BlkError> {
    match inner.io.exec(cm, T_IO_MS) {
        Ok(_) => Ok(()),
        Err(NvmeError::Timeout) => {
            c.dead.store(true, Ordering::Relaxed);
            Err(BlkError::Timeout)
        }
        Err(e) => Err(e.into()),
    }
}

impl NvmeNs {
    /// Build PRP1/PRP2 for `bytes` bytes at the start of the bounce buffer.
    fn prps(inner: &Inner, bytes: usize) -> (u64, u64) {
        let pages = bytes.div_ceil(PAGE);
        match pages {
            0 | 1 => (inner.buf, 0),
            2 => (inner.buf, inner.buf + PAGE as u64),
            _ => {
                for i in 0..pages - 1 {
                    // SAFETY: the list page holds 512 entries; pages <= 33.
                    unsafe { hal::mem_wr64(inner.list + 8 * i as u64, inner.buf + (i as u64 + 1) * PAGE as u64) };
                }
                (inner.buf, inner.list)
            }
        }
    }

    fn io_cmd(&self, op: u32, lba: u64, nblocks: usize, prp: (u64, u64)) -> [u32; 16] {
        let mut cm = cmd(op, self.ns().nsid);
        set_prp(&mut cm, prp.0, prp.1);
        cm[10] = lba as u32;
        cm[11] = (lba >> 32) as u32;
        cm[12] = (nblocks as u32 - 1) & 0xFFFF;
        cm
    }

    /// Read `len` bytes at block `lba`, handing each finished chunk to `sink`
    /// as (offset in request, bounce address, length).
    fn transfer(&self, lba: u64, len: usize, write: bool, mut sink: impl FnMut(usize, u64, usize)) -> Result<(), BlkError> {
        debug_assert!(!write);
        let c = self.ctrl();
        if c.dead.load(Ordering::Relaxed) {
            return Err(BlkError::Io);
        }
        let bs = self.ns().bs as usize;
        let mut inner = c.inner.lock();
        let per = (inner.chunk / bs).max(1);
        let total = len / bs;
        let (mut done, mut cur) = (0usize, lba);
        while done < total {
            let n = per.min(total - done);
            let prp = Self::prps(&inner, n * bs);
            let mut cm = self.io_cmd(NVM_READ, cur, n, prp);
            run(c, &mut inner, &mut cm)?;
            hal::mb();
            sink(done * bs, inner.buf, n * bs);
            done += n;
            cur += n as u64;
        }
        Ok(())
    }

    fn transfer_out(&self, lba: u64, src: &mut &[u8]) -> Result<(), BlkError> {
        let c = self.ctrl();
        if c.dead.load(Ordering::Relaxed) {
            return Err(BlkError::Io);
        }
        let bs = self.ns().bs as usize;
        let mut inner = c.inner.lock();
        let per = (inner.chunk / bs).max(1);
        let total = src.len() / bs;
        let (mut done, mut cur) = (0usize, lba);
        while done < total {
            let n = per.min(total - done);
            let bytes = n * bs;
            // SAFETY: the bounce buffer holds at least `chunk >= bytes`.
            unsafe {
                core::ptr::copy_nonoverlapping(src[done * bs..].as_ptr(), inner.buf as *mut u8, bytes);
            }
            let prp = Self::prps(&inner, bytes);
            let mut cm = self.io_cmd(NVM_WRITE, cur, n, prp);
            hal::mb();
            run(c, &mut inner, &mut cm)?;
            done += n;
            cur += n as u64;
        }
        Ok(())
    }
}

impl BlockDev for NvmeNs {
    fn sectors(&self) -> u64 {
        NvmeNs::sectors(self)
    }

    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        NvmeNs::read(self, lba, buf)
    }

    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
        NvmeNs::write(self, lba, buf)
    }

    fn flush(&self) -> Result<(), BlkError> {
        NvmeNs::flush(self)
    }
}
