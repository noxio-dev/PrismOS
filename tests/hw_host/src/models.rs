// Software models of an NVMe controller and an xHCI controller with a few USB
// devices behind it.  The kernel drivers talk to them through the hostsim_*
// hooks of hw/hal.rs: register accesses land in `rd32/wr32` below, and the
// models read and write the drivers' DMA memory directly (bus address ==
// host pointer).  Commands complete synchronously on the doorbell write, like
// QEMU's emulation does.

#![allow(dead_code)]

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ------------------------------------------------------------ host plumbing

pub trait Mmio: Send {
    fn rd32(&mut self, off: u64) -> u32;
    fn wr32(&mut self, off: u64, v: u32);
    fn rd8(&mut self, off: u64) -> u8 {
        (self.rd32(off & !3) >> ((off & 3) * 8)) as u8
    }
    fn wr8(&mut self, _off: u64, _v: u8) {}
}

struct Region {
    base: u64,
    size: u64,
    dev: Box<dyn Mmio>,
}

static REGIONS: Mutex<Vec<Region>> = Mutex::new(Vec::new());
static TIME_MS: AtomicU64 = AtomicU64::new(1000);
static CALLS: AtomicU64 = AtomicU64::new(0);
pub static IDLES: AtomicU64 = AtomicU64::new(0);

pub fn add_region(base: u64, size: u64, dev: Box<dyn Mmio>) {
    REGIONS.lock().unwrap().push(Region { base, size, dev });
}

fn with<R>(a: u64, f: impl FnOnce(&mut dyn Mmio, u64) -> R) -> R {
    let mut g = REGIONS.lock().unwrap();
    for r in g.iter_mut() {
        if a >= r.base && a < r.base + r.size {
            return f(r.dev.as_mut(), a - r.base);
        }
    }
    panic!("MMIO access outside any BAR: {:#x}", a);
}

#[no_mangle]
pub fn hostsim_rd32(a: u64) -> u32 { with(a, |d, o| d.rd32(o)) }
#[no_mangle]
pub fn hostsim_wr32(a: u64, v: u32) { with(a, |d, o| d.wr32(o, v)) }
#[no_mangle]
pub fn hostsim_rd8(a: u64) -> u8 { with(a, |d, o| d.rd8(o)) }
#[no_mangle]
pub fn hostsim_wr8(a: u64, v: u8) { with(a, |d, o| d.wr8(o, v)) }
#[no_mangle]
pub fn hostsim_idle() {
    IDLES.fetch_add(1, Ordering::Relaxed);
    TIME_MS.fetch_add(10, Ordering::Relaxed);
}
#[no_mangle]
pub fn hostsim_now_ms() -> u64 {
    // Time also creeps forward on its own so a spin-only wait ends.
    if CALLS.fetch_add(1, Ordering::Relaxed) % 2000 == 1999 {
        TIME_MS.fetch_add(1, Ordering::Relaxed);
    }
    TIME_MS.load(Ordering::Relaxed)
}

static DMA_LIVE: AtomicU64 = AtomicU64::new(0);
#[no_mangle]
pub fn hostsim_dma_alloc(n: usize) -> u64 {
    DMA_LIVE.fetch_add(n as u64, Ordering::Relaxed);
    // SAFETY: valid layout; zeroed memory handed to the "device".
    unsafe { alloc_zeroed(Layout::from_size_align(n * 4096, 4096).unwrap()) as u64 }
}
#[no_mangle]
pub fn hostsim_dma_free(p: u64, n: usize) {
    DMA_LIVE.fetch_sub(n as u64, Ordering::Relaxed);
    // SAFETY: allocated by hostsim_dma_alloc with the same layout (callers
    // free in the same granularity they allocated: pages one by one is also
    // fine for the model because we never merge, see below).
    let _ = (p, n);
}
pub fn dma_live() -> u64 { DMA_LIVE.load(Ordering::Relaxed) }

// DMA memory helpers for the models.
pub unsafe fn m32(a: u64) -> u32 { (a as *const u32).read_volatile() }
pub unsafe fn w32(a: u64, v: u32) { (a as *mut u32).write_volatile(v) }
pub unsafe fn m64(a: u64) -> u64 { (a as *const u64).read_volatile() }
pub unsafe fn w64(a: u64, v: u64) { (a as *mut u64).write_volatile(v) }
pub unsafe fn mbytes(a: u64, n: usize) -> &'static mut [u8] { std::slice::from_raw_parts_mut(a as *mut u8, n) }

// -------------------------------------------------------------------- NVMe

#[derive(Default, Clone, Copy)]
struct Sq { base: u64, size: u32, head: u32, cq: u32, on: bool }
#[derive(Default, Clone, Copy)]
struct Cq { base: u64, size: u32, tail: u32, phase: bool, on: bool }

pub struct Nvme {
    pub bs: usize,
    pub disk: Vec<u8>,
    dstrd: u32,
    cc: u32,
    csts: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    sq: [Sq; 3],
    cq: [Cq; 3],
    pub io_cmds: u64,
    pub max_blocks_seen: usize,
}

impl Nvme {
    pub fn new(bs: usize, blocks: usize, dstrd: u32) -> Nvme {
        let mut disk = vec![0u8; bs * blocks];
        for (i, b) in disk.iter_mut().enumerate() { *b = (i * 7 + i / 512) as u8; }
        Nvme { bs, disk, dstrd, cc: 0, csts: 0, aqa: 0, asq: 0, acq: 0,
               sq: Default::default(), cq: Default::default(), io_cmds: 0, max_blocks_seen: 0 }
    }

    fn post(&mut self, qid: usize, cid: u32, result: u32, status: u32) {
        let sqh = self.sq[qid].head;
        let cq = &mut self.cq[self.sq[qid].cq as usize];
        let a = cq.base + cq.tail as u64 * 16;
        // SAFETY: the driver's CQ page.
        unsafe {
            w32(a, result);
            w32(a + 4, 0);
            w32(a + 8, sqh | (qid as u32) << 16);
            w32(a + 12, cid | (cq.phase as u32) << 16 | status << 17);
        }
        cq.tail += 1;
        if cq.tail == cq.size { cq.tail = 0; cq.phase = !cq.phase; }
    }

    /// Resolve the PRP pair of a command into (address, length) pieces.
    fn prps(&self, prp1: u64, prp2: u64, len: usize) -> Vec<(u64, usize)> {
        let mut v = Vec::new();
        let first = (4096 - (prp1 as usize & 4095)).min(len);
        v.push((prp1, first));
        let mut rem = len - first;
        if rem == 0 { return v; }
        if rem <= 4096 {
            assert_eq!(prp2 & 4095, 0, "PRP2 not page aligned");
            v.push((prp2, rem));
            return v;
        }
        assert_eq!(prp2 & 4095, 0, "PRP list not page aligned");
        let mut i = 0;
        while rem > 0 {
            assert!(i < 512, "PRP list longer than one page");
            // SAFETY: the driver's PRP list page.
            let p = unsafe { m64(prp2 + 8 * i) };
            assert_eq!(p & 4095, 0, "PRP entry not page aligned");
            let n = rem.min(4096);
            v.push((p, n));
            rem -= n;
            i += 1;
        }
        v
    }

    fn exec(&mut self, qid: usize, c: [u32; 16]) {
        let (op, cid) = (c[0] & 0xFF, c[0] >> 16);
        let prp1 = c[6] as u64 | (c[7] as u64) << 32;
        let prp2 = c[8] as u64 | (c[9] as u64) << 32;
        if qid == 0 {
            match op {
                0x06 => {
                    let cns = c[10] & 0xFF;
                    // SAFETY: the driver's 4 KiB identify buffer.
                    let b = unsafe { mbytes(prp1, 4096) };
                    b.fill(0);
                    match cns {
                        1 => {
                            b[4..24].copy_from_slice(b"SN-NVME-TEST-0001   ");
                            b[24..64].copy_from_slice(b"PRISM VIRTUAL NVME DISK                 ");
                            b[77] = 5; // MDTS: 2^5 pages = 128 KiB
                            b[516..520].copy_from_slice(&1u32.to_le_bytes()); // NN
                        }
                        2 => b[0..4].copy_from_slice(&1u32.to_le_bytes()),
                        0 => {
                            let n = (self.disk.len() / self.bs) as u64;
                            b[0..8].copy_from_slice(&n.to_le_bytes());
                            b[8..16].copy_from_slice(&n.to_le_bytes());
                            b[25] = 0; // nlbaf
                            b[26] = 0; // flbas
                            b[128 + 2] = self.bs.trailing_zeros() as u8; // LBAF0.LBADS
                        }
                        _ => return self.post(qid, cid, 0, 2),
                    }
                    self.post(qid, cid, 0, 0);
                }
                0x05 => {
                    let qid2 = (c[10] & 0xFFFF) as usize;
                    let size = (c[10] >> 16) + 1;
                    assert!(qid2 == 1 && c[11] & 1 == 1, "CQ create: bad id / not physically contiguous");
                    self.cq[qid2] = Cq { base: prp1, size, tail: 0, phase: true, on: true };
                    self.post(qid, cid, 0, 0);
                }
                0x01 => {
                    let qid2 = (c[10] & 0xFFFF) as usize;
                    let size = (c[10] >> 16) + 1;
                    let cqid = c[11] >> 16;
                    assert!(qid2 == 1 && self.cq[cqid as usize].on && c[11] & 1 == 1, "SQ create: bad");
                    self.sq[qid2] = Sq { base: prp1, size, head: 0, cq: cqid, on: true };
                    self.post(qid, cid, 0, 0);
                }
                _ => self.post(qid, cid, 0, 1), // invalid opcode
            }
            return;
        }
        self.io_cmds += 1;
        match op {
            0x00 => self.post(qid, cid, 0, 0),
            0x01 | 0x02 => {
                let lba = c[10] as u64 | (c[11] as u64) << 32;
                let n = (c[12] & 0xFFFF) as usize + 1;
                self.max_blocks_seen = self.max_blocks_seen.max(n);
                assert!(n * self.bs <= 128 * 1024, "transfer above MDTS");
                if (lba as usize + n) * self.bs > self.disk.len() { return self.post(qid, cid, 0, 0x80); }
                let mut off = lba as usize * self.bs;
                for (a, l) in self.prps(prp1, prp2, n * self.bs) {
                    // SAFETY: a PRP piece inside the driver's bounce buffer.
                    let m = unsafe { mbytes(a, l) };
                    if op == 0x02 { m.copy_from_slice(&self.disk[off..off + l]); }
                    else { self.disk[off..off + l].copy_from_slice(m); }
                    off += l;
                }
                self.post(qid, cid, 0, 0);
            }
            _ => self.post(qid, cid, 0, 1),
        }
    }

    fn ring_sq(&mut self, qid: usize, tail: u32) {
        while self.sq[qid].on && self.sq[qid].head != tail {
            let a = self.sq[qid].base + self.sq[qid].head as u64 * 64;
            let mut c = [0u32; 16];
            for (i, w) in c.iter_mut().enumerate() { *w = unsafe { m32(a + 4 * i as u64) }; }
            self.sq[qid].head = (self.sq[qid].head + 1) % self.sq[qid].size;
            self.exec(qid, c);
        }
    }
}

impl Mmio for Nvme {
    fn rd32(&mut self, off: u64) -> u32 {
        match off {
            0x00 => 63 | 2 << 24 | 0, // MQES 63, TO 2 (x500ms)
            0x04 => (self.dstrd) | 1 << 5, // DSTRD (bits 35:32 -> dword 1 bits 3:0), CSS bit 37 -> bit 5
            0x08 => 0x0001_0400,
            0x14 => self.cc,
            0x1C => self.csts,
            0x24 => self.aqa,
            _ => 0,
        }
    }

    fn wr32(&mut self, off: u64, v: u32) {
        match off {
            0x14 => {
                let was = self.cc & 1 != 0;
                self.cc = v;
                if v & 1 != 0 && !was {
                    assert_eq!((v >> 16) & 0xF, 6, "IOSQES");
                    assert_eq!((v >> 20) & 0xF, 4, "IOCQES");
                    let (asz, csz) = ((self.aqa & 0xFFF) + 1, ((self.aqa >> 16) & 0xFFF) + 1);
                    self.sq = Default::default();
                    self.cq = Default::default();
                    self.sq[0] = Sq { base: self.asq, size: asz, head: 0, cq: 0, on: true };
                    self.cq[0] = Cq { base: self.acq, size: csz, tail: 0, phase: true, on: true };
                    self.csts = 1;
                } else if v & 1 == 0 && was {
                    self.csts = 0;
                }
            }
            0x24 => { assert_eq!(self.csts & 1, 0, "AQA while enabled"); self.aqa = v }
            0x28 => self.asq = self.asq & !0xFFFF_FFFF | v as u64,
            0x2C => self.asq = self.asq & 0xFFFF_FFFF | (v as u64) << 32,
            0x30 => self.acq = self.acq & !0xFFFF_FFFF | v as u64,
            0x34 => self.acq = self.acq & 0xFFFF_FFFF | (v as u64) << 32,
            o if o >= 0x1000 => {
                let stride = 4u64 << self.dstrd;
                let idx = (o - 0x1000) / stride;
                assert_eq!((o - 0x1000) % stride, 0, "misaligned doorbell");
                let (qid, is_cq) = ((idx / 2) as usize, idx % 2 == 1);
                assert!(qid < 3, "doorbell of unknown queue");
                if !is_cq { self.ring_sq(qid, v); }
            }
            _ => {}
        }
    }
}

pub struct NvmeHandle(pub Arc<Mutex<Nvme>>);
impl Mmio for NvmeHandle {
    fn rd32(&mut self, o: u64) -> u32 { self.0.lock().unwrap().rd32(o) }
    fn wr32(&mut self, o: u64, v: u32) { self.0.lock().unwrap().wr32(o, v) }
}
