// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// xHCI (USB 3.x host controller) driver, polling mode.
//
// What it does: takes the controller from the firmware (BIOS handoff),
// resets it, runs the command / event rings, and enumerates every device
// plugged straight into a root port (no hubs yet): address, descriptors,
// configuration.  Two device classes are then brought up:
//   * HID boot keyboard  -> an interrupt IN endpoint that stays armed; the
//     reports go to `usbhid`, which turns them into key events;
//   * Mass storage (Bulk-Only, SCSI transparent) -> handed to `usbmsc`,
//     which drives it through `bulk` / `control` below.
//
// Everything is synchronous under one spin lock per controller; there are no
// interrupts: `poll()` runs from the timer tick (try_lock, never waits) to
// collect keyboard reports, and the synchronous paths pump the event ring
// themselves while they wait.  Rings and contexts live in low (<4 GiB)
// identity-mapped DMA pages, one page per ring (256 TRBs).

#![allow(dead_code)]

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::hw::hal::{self, mem_rd32, mem_rd64, mem_wr32, mem_wr64};
use crate::hw::pci;
use crate::kprintln;
use crate::hw::{usbhid, usbmsc};
use crate::lib::spin::SpinLock;

pub const MAX_CTRL: usize = 2;
const MAX_SLOT: usize = 16;
const RING_TRBS: u32 = 256;
const MAX_KBD: usize = 4;

// ------------------------------------------------------------------ registers

const CAP_HCSPARAMS1: u64 = 0x04;
const CAP_HCSPARAMS2: u64 = 0x08;
const CAP_HCCPARAMS1: u64 = 0x10;
const CAP_DBOFF: u64 = 0x14;
const CAP_RTSOFF: u64 = 0x18;

const OP_USBCMD: u64 = 0x00;
const OP_USBSTS: u64 = 0x04;
const OP_PAGESIZE: u64 = 0x08;
const OP_CRCR: u64 = 0x18;
const OP_DCBAAP: u64 = 0x30;
const OP_CONFIG: u64 = 0x38;
const OP_PORTS: u64 = 0x400;

const CMD_RS: u32 = 1 << 0;
const CMD_HCRST: u32 = 1 << 1;
const STS_HCH: u32 = 1 << 0;
const STS_CNR: u32 = 1 << 11;

const IR_IMAN: u64 = 0x00;
const IR_ERSTSZ: u64 = 0x08;
const IR_ERSTBA: u64 = 0x10;
const IR_ERDP: u64 = 0x18;
const ERDP_EHB: u64 = 1 << 3;

const PORT_CCS: u32 = 1 << 0;
const PORT_PED: u32 = 1 << 1;
const PORT_PR: u32 = 1 << 4;
const PORT_PP: u32 = 1 << 9;
const PORT_PRC: u32 = 1 << 21;
/// CSC, PEC, WRC, OCC, PRC, PLC, CEC: write 1 to clear.
const PORT_CHANGE: u32 = 0x00FE_0000;
/// The only bit worth preserving when writing PORTSC: PP.  Everything else
/// is either read-only, write-1-to-clear (a read-modify-write would clear it
/// by accident) or PED, where writing 1 *disables* the port.
const PORT_KEEP: u32 = PORT_PP;

// TRB types.
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const CMD_ENABLE_SLOT: u32 = 9;
const CMD_DISABLE_SLOT: u32 = 10;
const CMD_ADDRESS_DEV: u32 = 11;
const CMD_CONFIG_EP: u32 = 12;
const CMD_EVAL_CTX: u32 = 13;
const CMD_RESET_EP: u32 = 14;
const CMD_STOP_EP: u32 = 15;
const CMD_SET_TR_DEQ: u32 = 16;
const EV_TRANSFER: u32 = 32;
const EV_CMD_DONE: u32 = 33;
const EV_PORT_CHANGE: u32 = 34;

// Completion codes.
const CC_SUCCESS: u8 = 1;
const CC_STALL: u8 = 6;
const CC_SHORT: u8 = 13;
const CC_CTX_STATE: u8 = 19;

// TRB control bits.
const TRB_ISP: u32 = 1 << 2;
const TRB_CHAIN: u32 = 1 << 4;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;

// ---------------------------------------------------------------------- types

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XErr {
    Timeout,
    Stall,
    NoDev,
    NoMem,
    Fail(u8),
}

/// Names one enumerated device: controller index and slot id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Handle {
    pub ctrl: u8,
    pub slot: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Other,
    Hid,
    Msc,
}

/// A class driver to hand a fresh device to once the controller lock is
/// released.
pub struct Found {
    pub h: Handle,
    pub iface: u8,
    pub in_dci: u8,
    pub out_dci: u8,
}

/// One TRB ring (a single page: 255 TRBs and a link back to the start).
#[derive(Clone, Copy)]
struct Ring {
    base: u64,
    enq: u32,
    cycle: bool,
    prev_chain: bool,
}

impl Ring {
    fn new() -> Option<Ring> {
        let base = hal::dma_alloc(1)?; // zeroed
        Some(Ring { base, enq: 0, cycle: true, prev_chain: false })
    }

    fn free(&self) {
        hal::dma_free(self.base, 1);
    }

    fn put(addr: u64, param: u64, status: u32, control: u32) {
        // SAFETY: addr is inside the ring page we own.
        unsafe {
            mem_wr64(addr, param);
            mem_wr32(addr + 8, status);
        }
        hal::mb();
        // SAFETY: as above; the control dword (cycle bit) goes last.
        unsafe { mem_wr32(addr + 12, control) };
    }

    /// Append a TRB; returns its bus address.  `control` carries the type and
    /// flags but not the cycle bit.
    fn push(&mut self, param: u64, status: u32, control: u32) -> u64 {
        if self.enq == RING_TRBS - 1 {
            // The link slot: written now, with the cycle bit of the pass we
            // are finishing.  It carries the chain bit of the TD in flight.
            let ctl = (TRB_LINK << 10)
                | (1 << 1) // toggle cycle
                | if self.prev_chain { TRB_CHAIN } else { 0 }
                | self.cycle as u32;
            Self::put(self.base + (self.enq as u64) * 16, self.base, 0, ctl);
            self.cycle = !self.cycle;
            self.enq = 0;
        }
        let addr = self.base + (self.enq as u64) * 16;
        Self::put(addr, param, status, control | self.cycle as u32);
        self.prev_chain = control & TRB_CHAIN != 0;
        self.enq += 1;
        addr
    }

    /// Dequeue pointer value (with the cycle state bit) that restarts the
    /// ring at the current enqueue position.
    fn restart_ptr(&self) -> u64 {
        self.base + (self.enq as u64) * 16 | self.cycle as u64
    }
}

#[derive(Clone, Copy)]
struct Dev {
    port: u8,
    speed: u8,
    kind: Kind,
    out_ctx: u64,
    in_ctx: u64,
    /// One page: control data bounce at 0, HID report at 2048.
    buf: u64,
    rings: [Option<Ring>; 32],
    vid: u16,
    pid: u16,
    class: u8,
    hid_dci: u8,
    hid_idx: u8,
    hid_errs: u8,
}

enum Ev {
    Cmd { ptr: u64, code: u8, slot: u8 },
    Xfer { ptr: u64, code: u8, resid: u32, slot: u8, dci: u8 },
    Port,
    Other,
}

struct Xhci {
    present: bool,
    cap: u64,
    op: u64,
    rt: u64,
    db: u64,
    cs: usize, // context size: 32 or 64 bytes
    max_ports: u8,
    max_slots: u8,
    dcbaa: u64,
    scratch_arr: u64,
    scratch_n: usize,
    cmd: Option<Ring>,
    ev_base: u64,
    erst: u64,
    ev_deq: u32,
    ev_ccs: bool,
    devs: [Option<Dev>; MAX_SLOT],
}

impl Xhci {
    const NEW: Xhci = Xhci {
        present: false,
        cap: 0,
        op: 0,
        rt: 0,
        db: 0,
        cs: 32,
        max_ports: 0,
        max_slots: 0,
        dcbaa: 0,
        scratch_arr: 0,
        scratch_n: 0,
        cmd: None,
        ev_base: 0,
        erst: 0,
        ev_deq: 0,
        ev_ccs: true,
        devs: [const { None }; MAX_SLOT],
    };
}

static CTRL: [SpinLock<Xhci>; MAX_CTRL] = [SpinLock::new(Xhci::NEW), SpinLock::new(Xhci::NEW)];
static ANY: AtomicBool = AtomicBool::new(false);
static PORT_EVENT: AtomicBool = AtomicBool::new(false);

/// Public description of an enumerated device, for the shell.
pub struct DevInfo {
    pub h: Handle,
    pub port: u8,
    pub speed: &'static str,
    pub vid: u16,
    pub pid: u16,
    pub class: &'static str,
}

fn speed_name(s: u8) -> &'static str {
    match s {
        1 => "full",
        2 => "low",
        3 => "high",
        4 => "super",
        5 => "super+",
        _ => "?",
    }
}

fn class_name(c: u8, kind: Kind) -> &'static str {
    match (kind, c) {
        (Kind::Hid, _) => "keyboard",
        (Kind::Msc, _) => "mass storage",
        (_, 0x09) => "hub (unsupported)",
        (_, 0x03) => "hid",
        (_, 0x08) => "storage",
        (_, 0x01) => "audio",
        (_, 0x0E) => "video",
        (_, 0xE0) => "wireless",
        _ => "other",
    }
}

// ------------------------------------------------------------- ring plumbing

impl Xhci {
    fn cw(&self, base: u64, off: usize, v: u32) {
        // SAFETY: base is a context page we own, off within it.
        unsafe { mem_wr32(base + off as u64, v) }
    }

    fn cr(&self, base: u64, off: usize) -> u32 {
        // SAFETY: as above.
        unsafe { mem_rd32(base + off as u64) }
    }

    fn doorbell(&self, slot: u8, target: u8) {
        hal::mb();
        hal::wr32(self.db + 4 * slot as u64, target as u32);
    }

    /// Next pending event, if any (consumed, ERDP advanced).
    fn next_event(&mut self) -> Option<Ev> {
        let a = self.ev_base + self.ev_deq as u64 * 16;
        // SAFETY: a is inside the event ring page.
        let ctl = unsafe { mem_rd32(a + 12) };
        if (ctl & 1 != 0) != self.ev_ccs {
            return None;
        }
        hal::mb();
        // SAFETY: as above.
        let (ptr, st) = unsafe { (mem_rd64(a), mem_rd32(a + 8)) };
        self.ev_deq += 1;
        if self.ev_deq == RING_TRBS {
            self.ev_deq = 0;
            self.ev_ccs = !self.ev_ccs;
        }
        let ir = self.rt + 0x20;
        hal::wr64(ir + IR_ERDP, (self.ev_base + self.ev_deq as u64 * 16) | ERDP_EHB);
        let code = (st >> 24) as u8;
        let slot = (ctl >> 24) as u8;
        Some(match (ctl >> 10) & 0x3F {
            EV_CMD_DONE => Ev::Cmd { ptr, code, slot },
            EV_TRANSFER => Ev::Xfer { ptr, code, resid: st & 0xFF_FFFF, slot, dci: ((ctl >> 16) & 0x1F) as u8 },
            EV_PORT_CHANGE => Ev::Port,
            _ => Ev::Other,
        })
    }

    /// Handle an event that is nobody's reply: keyboard reports, port
    /// changes.  Returns the event back when it may belong to a waiter.
    fn dispatch(&mut self, ev: Ev) -> Option<Ev> {
        match ev {
            Ev::Port => {
                PORT_EVENT.store(true, Ordering::Release);
                None
            }
            Ev::Xfer { code, slot, dci, .. } => {
                let s = slot as usize;
                if s < MAX_SLOT {
                    if let Some(d) = self.devs[s].as_mut() {
                        if d.kind == Kind::Hid && d.hid_dci == dci {
                            self.hid_event(slot, code);
                            return None;
                        }
                    }
                }
                Some(ev)
            }
            other => Some(other),
        }
    }

    fn hid_event(&mut self, slot: u8, code: u8) {
        let Some(d) = self.devs[slot as usize].as_mut() else { return };
        let ok = code == CC_SUCCESS || code == CC_SHORT;
        if ok {
            d.hid_errs = 0;
            let mut rep = [0u8; 8];
            // SAFETY: report area of the device page, 8 bytes inside it.
            for (i, b) in rep.iter_mut().enumerate() {
                *b = unsafe { ((d.buf + 2048 + i as u64) as *const u8).read_volatile() };
            }
            usbhid::on_report(d.hid_idx as usize, &rep);
        } else {
            d.hid_errs = d.hid_errs.saturating_add(1);
            if d.hid_errs >= 8 {
                return; // dead endpoint: stop re-arming
            }
        }
        self.arm_hid(slot);
    }

    fn arm_hid(&mut self, slot: u8) {
        let db = self.db;
        let Some(d) = self.devs[slot as usize].as_mut() else { return };
        let dci = d.hid_dci;
        let buf = d.buf + 2048;
        let Some(r) = d.rings[dci as usize].as_mut() else { return };
        r.push(buf, 8, (TRB_NORMAL << 10) | TRB_IOC | TRB_ISP);
        hal::mb();
        hal::wr32(db + 4 * slot as u64, dci as u32);
    }

    /// Pump events until `pick` accepts one or `ms` elapse.
    fn wait_for<T>(&mut self, ms: u64, mut pick: impl FnMut(&Ev) -> Option<T>) -> Option<T> {
        let mut out = None;
        hal::wait_event(ms, || {
            while let Some(ev) = self.next_event() {
                if let Some(ev) = self.dispatch(ev) {
                    if let Some(v) = pick(&ev) {
                        out = Some(v);
                        return true;
                    }
                }
            }
            false
        });
        out
    }

    /// Queue a command and wait for its completion event.
    fn command(&mut self, param: u64, status: u32, control: u32) -> Result<(u8, u8), XErr> {
        let Some(ring) = self.cmd.as_mut() else { return Err(XErr::NoDev) };
        let trb = ring.push(param, status, control);
        self.doorbell(0, 0);
        match self.wait_for(5000, |ev| match ev {
            Ev::Cmd { ptr, code, slot } if *ptr == trb => Some((*code, *slot)),
            _ => None,
        }) {
            Some((c, s)) => Ok((c, s)),
            None => Err(XErr::Timeout),
        }
    }

    fn command_ok(&mut self, param: u64, status: u32, control: u32) -> Result<u8, XErr> {
        let (code, slot) = self.command(param, status, control)?;
        if code == CC_SUCCESS {
            Ok(slot)
        } else {
            Err(XErr::Fail(code))
        }
    }

    /// After a stall or a timeout: get the endpoint running again with an
    /// empty ring (Reset Endpoint if halted, else Stop; then Set TR Dequeue).
    fn recover_ep(&mut self, slot: u8, dci: u8) {
        let ctl = |t: u32| (t << 10) | (dci as u32) << 16 | (slot as u32) << 24;
        match self.command(0, 0, ctl(CMD_RESET_EP)) {
            Ok((CC_CTX_STATE, _)) => {
                let _ = self.command(0, 0, ctl(CMD_STOP_EP));
            }
            _ => {}
        }
        let Some(d) = self.devs[slot as usize].as_mut() else { return };
        let Some(r) = d.rings[dci as usize].as_ref() else { return };
        let ptr = r.restart_ptr();
        let _ = self.command(ptr, 0, ctl(CMD_SET_TR_DEQ));
    }
}

// --------------------------------------------------------------- transfers

impl Xhci {
    fn control_raw(&mut self, slot: u8, bm: u8, req: u8, val: u16, idx: u16, len: u16, data: u64) -> Result<usize, XErr> {
        let dev_in = bm & 0x80 != 0;
        let setup = bm as u64 | (req as u64) << 8 | (val as u64) << 16 | (idx as u64) << 32 | (len as u64) << 48;
        let trt: u32 = if len == 0 {
            0
        } else if dev_in {
            3
        } else {
            2
        };
        let Some(d) = self.devs.get_mut(slot as usize).and_then(|d| d.as_mut()) else { return Err(XErr::NoDev) };
        let Some(ring) = d.rings[1].as_mut() else { return Err(XErr::NoDev) };
        ring.push(setup, 8, (TRB_SETUP << 10) | TRB_IDT | (trt << 16));
        let mut data_trb = 0;
        if len > 0 {
            let dir = if dev_in { (1 << 16) | TRB_ISP } else { 0 };
            data_trb = ring.push(data, len as u32, (TRB_DATA << 10) | dir);
        }
        let status_dir = if len > 0 && dev_in { 0 } else { 1 << 16 };
        let st = ring.push(0, 0, (TRB_STATUS << 10) | TRB_IOC | status_dir);
        self.doorbell(slot, 1);

        let mut resid = 0u32;
        let r = self.wait_for(5000, |ev| match ev {
            Ev::Xfer { ptr, code, resid: rs, slot: s, dci: 1 } if *s == slot => {
                if data_trb != 0 && *ptr == data_trb {
                    if *code == CC_SHORT {
                        resid = *rs;
                        return None; // the status stage event follows
                    }
                    if *code == CC_SUCCESS {
                        return None;
                    }
                    return Some(*code);
                }
                if *ptr == st {
                    return Some(if *code == CC_SHORT { CC_SUCCESS } else { *code });
                }
                None
            }
            _ => None,
        });
        match r {
            Some(CC_SUCCESS) => Ok(len as usize - (resid as usize).min(len as usize)),
            Some(code) => {
                self.recover_ep(slot, 1);
                Err(if code == CC_STALL { XErr::Stall } else { XErr::Fail(code) })
            }
            None => {
                self.recover_ep(slot, 1);
                Err(XErr::Timeout)
            }
        }
    }

    fn bulk_raw(&mut self, slot: u8, dci: u8, phys: u64, len: usize, dir_in: bool, mps: usize) -> Result<usize, XErr> {
        // Split at 64 KiB boundaries: a TRB must not cross one.
        let mut parts: Vec<(u64, u32)> = Vec::new();
        let (mut a, mut rem) = (phys, len);
        loop {
            let n = rem.min(0x1_0000 - (a as usize & 0xFFFF));
            parts.push((a, n as u32));
            a += n as u64;
            rem -= n;
            if rem == 0 {
                break;
            }
        }
        let Some(d) = self.devs.get_mut(slot as usize).and_then(|d| d.as_mut()) else { return Err(XErr::NoDev) };
        let Some(ring) = d.rings.get_mut(dci as usize).and_then(|r| r.as_mut()) else { return Err(XErr::NoDev) };
        let k = parts.len();
        let mut trbs: Vec<u64> = Vec::with_capacity(k);
        let mut after = len;
        for (i, &(addr, n)) in parts.iter().enumerate() {
            after -= n as usize;
            let td = (after.div_ceil(mps.max(1))).min(31) as u32;
            let last = i == k - 1;
            let mut ctl = TRB_NORMAL << 10;
            if !last {
                ctl |= TRB_CHAIN;
            } else {
                ctl |= TRB_IOC;
            }
            if dir_in {
                ctl |= TRB_ISP;
            }
            trbs.push(ring.push(addr, n | td << 17, ctl));
        }
        self.doorbell(slot, dci);

        let mut actual = 0usize;
        let res = self.wait_for(10_000, |ev| match ev {
            Ev::Xfer { ptr, code, resid, slot: s, dci: dc } if *s == slot && *dc == dci => {
                let Some(i) = trbs.iter().position(|t| t == ptr) else { return None }; // stale
                match *code {
                    CC_SUCCESS if i == k - 1 => {
                        actual = len;
                        Some(CC_SUCCESS)
                    }
                    CC_SUCCESS => None,
                    CC_SHORT => {
                        let before: usize = parts[..i].iter().map(|p| p.1 as usize).sum();
                        actual = before + (parts[i].1 as usize).saturating_sub(*resid as usize);
                        Some(CC_SUCCESS)
                    }
                    c => Some(c),
                }
            }
            _ => None,
        });
        match res {
            Some(CC_SUCCESS) => Ok(actual),
            Some(code) => {
                self.recover_ep(slot, dci);
                Err(if code == CC_STALL { XErr::Stall } else { XErr::Fail(code) })
            }
            None => {
                self.recover_ep(slot, dci);
                Err(XErr::Timeout)
            }
        }
    }
}

fn ctrl_lock(h: Handle) -> Option<crate::lib::spin::Guard<'static, Xhci>> {
    let c = CTRL.get(h.ctrl as usize)?;
    let g = c.lock();
    if g.present {
        Some(g)
    } else {
        None
    }
}

/// Control transfer on endpoint 0 of `h`.  `data` is the OUT payload or the
/// IN buffer (at most 2 KiB); returns the byte count transferred.
pub fn control(h: Handle, bm: u8, req: u8, val: u16, idx: u16, data: &mut [u8]) -> Result<usize, XErr> {
    if data.len() > 2048 {
        return Err(XErr::Fail(0));
    }
    let mut g = ctrl_lock(h).ok_or(XErr::NoDev)?;
    let buf = g.devs.get(h.slot as usize).and_then(|d| d.as_ref()).ok_or(XErr::NoDev)?.buf;
    if bm & 0x80 == 0 && !data.is_empty() {
        // SAFETY: bounce area of the device page, length checked above.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, data.len()) };
    }
    let n = g.control_raw(h.slot, bm, req, val, idx, data.len() as u16, buf)?;
    if bm & 0x80 != 0 && n > 0 {
        // SAFETY: as above.
        unsafe { core::ptr::copy_nonoverlapping(buf as *const u8, data.as_mut_ptr(), n.min(data.len())) };
    }
    Ok(n)
}

/// Bulk transfer of `len` bytes between the device and the DMA buffer at bus
/// address `phys` (identity mapped, below 4 GiB).  Returns bytes moved.
pub fn bulk(h: Handle, dci: u8, phys: u64, len: usize, dir_in: bool) -> Result<usize, XErr> {
    let mut g = ctrl_lock(h).ok_or(XErr::NoDev)?;
    let speed = g.devs.get(h.slot as usize).and_then(|d| d.as_ref()).ok_or(XErr::NoDev)?.speed;
    let mps = match speed {
        4 | 5 => 1024,
        3 => 512,
        _ => 64,
    };
    g.bulk_raw(h.slot, dci, phys, len, dir_in, mps)
}

/// CLEAR_FEATURE(ENDPOINT_HALT) on the device and the controller's side of
/// the same recovery (the ring was already re-pointed on the error path).
pub fn clear_halt(h: Handle, dci: u8) -> Result<(), XErr> {
    let ep_addr = (dci / 2) | if dci & 1 != 0 { 0x80 } else { 0 };
    {
        let mut g = ctrl_lock(h).ok_or(XErr::NoDev)?;
        g.recover_ep(h.slot, dci);
    }
    control(h, 0x02, 1, 0, ep_addr as u16, &mut []).map(|_| ())
}

// ---------------------------------------------------------------- bring-up

struct EpSpec {
    dci: u8,
    ty: u32,
    mps: u32,
    interval: u32,
    burst: u32,
}

impl Xhci {
    fn portsc(&self, port: u8) -> u64 {
        self.op + OP_PORTS + 0x10 * (port as u64 - 1)
    }

    fn reset_controller(&mut self) -> bool {
        let op = self.op;
        if !hal::wait_event(1000, || hal::rd32(op + OP_USBSTS) & STS_CNR == 0) {
            return false;
        }
        let cmd = hal::rd32(op + OP_USBCMD);
        if cmd & CMD_RS != 0 {
            hal::wr32(op + OP_USBCMD, cmd & !CMD_RS);
        }
        if !hal::wait_event(1000, || hal::rd32(op + OP_USBSTS) & STS_HCH != 0) {
            return false;
        }
        hal::wr32(op + OP_USBCMD, CMD_HCRST);
        hal::wait_event(2000, || {
            hal::rd32(op + OP_USBCMD) & CMD_HCRST == 0 && hal::rd32(op + OP_USBSTS) & STS_CNR == 0
        })
    }

    fn setup(&mut self) -> bool {
        let op = self.op;
        let n = (self.max_slots as usize).min(MAX_SLOT - 1) as u32;
        hal::wr32(op + OP_CONFIG, n);

        // DCBAA, with the scratchpad array in slot 0 when the controller
        // wants scratch memory.
        let Some(dcbaa) = hal::dma_alloc(1) else { return false };
        self.dcbaa = dcbaa;
        if self.scratch_n > 0 {
            let Some(arr) = hal::dma_alloc(1) else { return false };
            self.scratch_arr = arr;
            for i in 0..self.scratch_n {
                let Some(p) = hal::dma_alloc(1) else { return false };
                // SAFETY: arr is our page, i < 512.
                unsafe { mem_wr64(arr + 8 * i as u64, p) };
            }
            // SAFETY: dcbaa is our page.
            unsafe { mem_wr64(dcbaa, arr) };
        }
        hal::wr64(op + OP_DCBAAP, dcbaa);

        let Some(cmd) = Ring::new() else { return false };
        hal::wr64(op + OP_CRCR, cmd.base | 1);
        self.cmd = Some(cmd);

        let (Some(ev), Some(erst)) = (hal::dma_alloc(1), hal::dma_alloc(1)) else { return false };
        self.ev_base = ev;
        self.erst = erst;
        self.ev_deq = 0;
        self.ev_ccs = true;
        // SAFETY: erst is our page; one 16-byte segment descriptor.
        unsafe {
            mem_wr64(erst, ev);
            mem_wr32(erst + 8, RING_TRBS);
        }
        let ir = self.rt + 0x20;
        hal::wr32(ir + IR_ERSTSZ, 1);
        hal::wr64(ir + IR_ERDP, ev);
        hal::wr64(ir + IR_ERSTBA, erst);
        hal::wr32(ir + IR_IMAN, 0b11); // IP (clear) | IE; USBCMD.INTE stays 0

        hal::wr32(op + OP_USBCMD, CMD_RS);
        hal::wait_event(1000, || hal::rd32(op + OP_USBSTS) & STS_HCH == 0)
    }

    /// Port reset; returns PORTSC afterwards when the port is enabled.
    fn reset_port(&mut self, port: u8) -> Option<u32> {
        let ps = self.portsc(port);
        let v = hal::rd32(ps);
        if v & PORT_CCS == 0 {
            return None;
        }
        let speed = (v >> 10) & 0xF;
        if speed >= 4 && v & PORT_PED != 0 {
            // USB 3 ports train by themselves and come up enabled.
            hal::wr32(ps, (v & PORT_KEEP) | PORT_CHANGE);
            return Some(v);
        }
        hal::wr32(ps, (v & PORT_KEEP) | PORT_PR);
        if !hal::wait_event(500, || hal::rd32(ps) & PORT_PRC != 0) {
            return None;
        }
        let v = hal::rd32(ps);
        hal::wr32(ps, (v & PORT_KEEP) | PORT_CHANGE);
        hal::delay_ms(10); // reset recovery
        let v = hal::rd32(ps);
        if v & PORT_PED == 0 || v & PORT_CCS == 0 {
            return None;
        }
        Some(v)
    }

    fn free_dev(&mut self, slot: u8) {
        if let Some(d) = self.devs[slot as usize].take() {
            for r in d.rings.iter().flatten() {
                r.free();
            }
            hal::dma_free(d.out_ctx, 1);
            hal::dma_free(d.in_ctx, 1);
            hal::dma_free(d.buf, 1);
        }
        // SAFETY: dcbaa is our page; slot < MAX_SLOT.
        unsafe { mem_wr64(self.dcbaa + 8 * slot as u64, 0) };
    }

    fn remove(&mut self, slot: u8) {
        let kind = self.devs[slot as usize].as_ref().map(|d| (d.kind, d.hid_idx));
        if let Some((Kind::Hid, idx)) = kind {
            usbhid::detach(idx as usize);
        }
        let _ = self.command(0, 0, (CMD_DISABLE_SLOT << 10) | (slot as u32) << 24);
        self.free_dev(slot);
    }

    fn get_desc(&mut self, slot: u8, ty: u8, len: u16, out: &mut [u8]) -> Result<usize, XErr> {
        let buf = self.devs[slot as usize].as_ref().ok_or(XErr::NoDev)?.buf;
        let n = self.control_raw(slot, 0x80, 6, (ty as u16) << 8, 0, len, buf)?;
        let n = n.min(out.len());
        // SAFETY: bounce area, n <= len <= 2048.
        unsafe { core::ptr::copy_nonoverlapping(buf as *const u8, out.as_mut_ptr(), n) };
        Ok(n)
    }

    fn set_ep0_mps(&mut self, slot: u8, mps: u32) -> Result<(), XErr> {
        let cs = self.cs;
        let d = self.devs[slot as usize].as_ref().ok_or(XErr::NoDev)?;
        let inc = d.in_ctx;
        self.cw(inc, 0, 0);
        self.cw(inc, 4, 1 << 1); // A1: the EP0 context
        let ep0 = 2 * cs;
        let dw1 = self.cr(inc, ep0 + 4);
        self.cw(inc, ep0 + 4, (dw1 & 0xFFFF) | mps << 16);
        self.command_ok(inc, 0, (CMD_EVAL_CTX << 10) | (slot as u32) << 24).map(|_| ())
    }

    /// Enable slot, build the input context, Address Device.
    fn address_device(&mut self, port: u8, speed: u8) -> Result<u8, XErr> {
        let slot = self.command_ok(0, 0, CMD_ENABLE_SLOT << 10)?;
        if slot as usize >= MAX_SLOT || self.devs[slot as usize].is_some() {
            let _ = self.command(0, 0, (CMD_DISABLE_SLOT << 10) | (slot as u32) << 24);
            return Err(XErr::NoDev);
        }
        let mps0: u32 = match speed {
            4 | 5 => 512,
            3 => 64,
            _ => 8,
        };
        let (Some(out_ctx), Some(in_ctx), Some(buf), Some(ep0)) =
            (hal::dma_alloc(1), hal::dma_alloc(1), hal::dma_alloc(1), Ring::new())
        else {
            let _ = self.command(0, 0, (CMD_DISABLE_SLOT << 10) | (slot as u32) << 24);
            return Err(XErr::NoMem);
        };
        let mut rings = [None; 32];
        rings[1] = Some(ep0);
        self.devs[slot as usize] = Some(Dev {
            port,
            speed,
            kind: Kind::Other,
            out_ctx,
            in_ctx,
            buf,
            rings,
            vid: 0,
            pid: 0,
            class: 0,
            hid_dci: 0,
            hid_idx: 0,
            hid_errs: 0,
        });
        // SAFETY: dcbaa is our page.
        unsafe { mem_wr64(self.dcbaa + 8 * slot as u64, out_ctx) };

        let cs = self.cs;
        self.cw(in_ctx, 4, 0b11); // add slot + EP0
        self.cw(in_ctx, cs, (speed as u32) << 20 | 1 << 27); // slot: speed, 1 entry
        self.cw(in_ctx, cs + 4, (port as u32) << 16);
        let e = 2 * cs;
        self.cw(in_ctx, e + 4, (3 << 1) | (4 << 3) | mps0 << 16); // CErr 3, control
        let deq = ep0.base | 1;
        self.cw(in_ctx, e + 8, deq as u32);
        self.cw(in_ctx, e + 12, (deq >> 32) as u32);
        self.cw(in_ctx, e + 16, 8);
        match self.command_ok(in_ctx, 0, (CMD_ADDRESS_DEV << 10) | (slot as u32) << 24) {
            Ok(_) => Ok(slot),
            Err(e) => {
                self.free_dev(slot);
                let _ = self.command(0, 0, (CMD_DISABLE_SLOT << 10) | (slot as u32) << 24);
                Err(e)
            }
        }
    }

    fn configure_eps(&mut self, slot: u8, eps: &[EpSpec]) -> Result<(), XErr> {
        let cs = self.cs;
        let (inc, outc) = {
            let d = self.devs[slot as usize].as_ref().ok_or(XErr::NoDev)?;
            (d.in_ctx, d.out_ctx)
        };
        // SAFETY: both are our pages.
        unsafe { core::ptr::write_bytes(inc as *mut u8, 0, hal::PAGE) };
        let mut add = 1u32;
        let mut top = 0u8;
        for e in eps {
            add |= 1 << e.dci;
            top = top.max(e.dci);
        }
        self.cw(inc, 4, add);
        for i in 0..8 {
            let v = self.cr(outc, i * 4); // copy the slot context
            self.cw(inc, cs + i * 4, v);
        }
        let dw0 = self.cr(inc, cs);
        self.cw(inc, cs, (dw0 & 0x07FF_FFFF) | (top as u32) << 27);
        for e in eps {
            let ring = Ring::new().ok_or(XErr::NoMem)?;
            self.devs[slot as usize].as_mut().unwrap().rings[e.dci as usize] = Some(ring);
            let off = (e.dci as usize + 1) * cs;
            self.cw(inc, off, e.interval << 16);
            self.cw(inc, off + 4, (3 << 1) | e.ty << 3 | e.burst << 8 | e.mps << 16);
            let deq = ring.base | 1;
            self.cw(inc, off + 8, deq as u32);
            self.cw(inc, off + 12, (deq >> 32) as u32);
            let avg = e.mps * (e.burst + 1);
            self.cw(inc, off + 16, avg | avg << 16);
        }
        self.command_ok(inc, 0, (CMD_CONFIG_EP << 10) | (slot as u32) << 24).map(|_| ())
    }

    /// Full enumeration of the device on `port`.  Returns what to hand to a
    /// class driver once the lock is dropped.
    fn enumerate(&mut self, ctrl: u8, port: u8, found: &mut Vec<Found>) {
        let Some(v) = self.reset_port(port) else { return };
        let speed = ((v >> 10) & 0xF) as u8;
        let slot = match self.address_device(port, speed) {
            Ok(s) => s,
            Err(e) => {
                kprintln!("xhci: port {}: address device failed ({:?})", port, e);
                return;
            }
        };
        let mut desc = [0u8; 18];
        // Full speed: the real EP0 packet size is only known after the
        // first 8 bytes of the device descriptor.
        let mut ok = self.get_desc(slot, 1, 8, &mut desc).map_or(false, |n| n >= 8);
        if ok && speed <= 2 && desc[7] as u32 != 8 && matches!(desc[7], 8 | 16 | 32 | 64) {
            ok = self.set_ep0_mps(slot, desc[7] as u32).is_ok();
        }
        if ok {
            ok = self.get_desc(slot, 1, 18, &mut desc).map_or(false, |n| n >= 18);
        }
        if !ok {
            kprintln!("xhci: port {}: cannot read the device descriptor", port);
            self.remove(slot);
            return;
        }
        let vid = u16::from_le_bytes([desc[8], desc[9]]);
        let pid = u16::from_le_bytes([desc[10], desc[11]]);
        let dclass = desc[4];
        {
            let d = self.devs[slot as usize].as_mut().unwrap();
            d.vid = vid;
            d.pid = pid;
            d.class = dclass;
        }
        if !self.bring_up_class(ctrl, slot, speed, found) {
            // Nothing we drive: keep the slot (it shows in `usb`), harmless.
        }
    }

    fn bring_up_class(&mut self, ctrl: u8, slot: u8, speed: u8, found: &mut Vec<Found>) -> bool {
        let mut hdr = [0u8; 9];
        if self.get_desc(slot, 2, 9, &mut hdr).map_or(true, |n| n < 9) {
            return false;
        }
        let total = (u16::from_le_bytes([hdr[2], hdr[3]]) as usize).clamp(9, 1024);
        let mut cfg = alloc::vec![0u8; total];
        let n = match self.get_desc(slot, 2, total as u16, &mut cfg) {
            Ok(n) => n,
            Err(_) => return false,
        };
        let cfg_value = hdr[5];

        // Walk the descriptors: remember the first interface we can drive.
        struct Ep {
            addr: u8,
            attr: u8,
            mps: u16,
            interval: u8,
            burst: u8,
        }
        let mut best: Option<(u8, u8, u8, u8)> = None; // iface, class, sub, proto
        let mut eps: Vec<Ep> = Vec::new();
        let (mut cur, mut take): (u8, bool) = (0, false);
        let mut i = 0;
        while i + 2 <= n {
            let l = cfg[i] as usize;
            if l < 2 || i + l > n {
                break;
            }
            match cfg[i + 1] {
                4 if l >= 9 => {
                    let (num, alt, cl, sub, pr) = (cfg[i + 2], cfg[i + 3], cfg[i + 5], cfg[i + 6], cfg[i + 7]);
                    take = false;
                    if alt == 0 && best.is_none() {
                        let kbd = cl == 3 && sub == 1 && pr == 1;
                        let msc = cl == 8 && sub == 6 && pr == 0x50;
                        if kbd || msc {
                            best = Some((num, cl, sub, pr));
                            cur = num;
                            take = true;
                        }
                    }
                }
                5 if l >= 7 && take => eps.push(Ep {
                    addr: cfg[i + 2],
                    attr: cfg[i + 3],
                    mps: u16::from_le_bytes([cfg[i + 4], cfg[i + 5]]) & 0x7FF,
                    interval: cfg[i + 6],
                    burst: 0,
                }),
                0x30 if l >= 3 && take => {
                    if let Some(e) = eps.last_mut() {
                        e.burst = cfg[i + 2];
                    }
                }
                _ => {}
            }
            i += l;
        }
        let Some((iface, cl, _, _)) = best else {
            let d = self.devs[slot as usize].as_ref().unwrap();
            if d.class == 9 {
                kprintln!("xhci: port {}: USB hubs are not supported yet", d.port);
            }
            return false;
        };
        let _ = cur;
        let dci_of = |e: &Ep| (e.addr & 0xF) * 2 + (e.addr >> 7);
        let mut dummy = [0u8; 0];
        if self.control_raw(slot, 0x00, 9, cfg_value as u16, 0, 0, 0).is_err() {
            return false;
        }
        let _ = &mut dummy;

        if cl == 3 {
            // HID boot keyboard: one interrupt IN endpoint.
            let Some(e) = eps.iter().find(|e| e.attr & 3 == 3 && e.addr & 0x80 != 0) else { return false };
            let interval = if speed >= 3 {
                (e.interval as u32).clamp(1, 16) - 1
            } else {
                let ms8 = (e.interval.max(1) as u32) * 8;
                (31 - ms8.leading_zeros()).clamp(3, 10)
            };
            let spec = EpSpec { dci: dci_of(e), ty: 7, mps: e.mps as u32, interval, burst: e.burst as u32 };
            let dci = spec.dci;
            if self.configure_eps(slot, &[spec]).is_err() {
                return false;
            }
            // SET_PROTOCOL(boot), SET_IDLE(0): best effort, some keyboards stall.
            let _ = self.control_raw(slot, 0x21, 0x0B, 0, iface as u16, 0, 0);
            let _ = self.control_raw(slot, 0x21, 0x0A, 0, iface as u16, 0, 0);
            let Some(idx) = usbhid::attach() else {
                kprintln!("xhci: too many USB keyboards");
                return false;
            };
            let d = self.devs[slot as usize].as_mut().unwrap();
            d.kind = Kind::Hid;
            d.hid_dci = dci;
            d.hid_idx = idx as u8;
            self.arm_hid(slot);
            return true;
        }

        // Mass storage: bulk IN + bulk OUT.
        let bin = eps.iter().find(|e| e.attr & 3 == 2 && e.addr & 0x80 != 0);
        let bout = eps.iter().find(|e| e.attr & 3 == 2 && e.addr & 0x80 == 0);
        let (Some(bi), Some(bo)) = (bin, bout) else { return false };
        let specs = [
            EpSpec { dci: dci_of(bo), ty: 2, mps: bo.mps as u32, interval: 0, burst: bo.burst as u32 },
            EpSpec { dci: dci_of(bi), ty: 6, mps: bi.mps as u32, interval: 0, burst: bi.burst as u32 },
        ];
        let (in_dci, out_dci) = (specs[1].dci, specs[0].dci);
        if self.configure_eps(slot, &specs).is_err() {
            return false;
        }
        self.devs[slot as usize].as_mut().unwrap().kind = Kind::Msc;
        found.push(Found { h: Handle { ctrl, slot }, iface, in_dci, out_dci });
        true
    }

    /// Look at every port: enumerate new devices, drop the ones that went away.
    fn scan(&mut self, ctrl: u8, found: &mut Vec<Found>) {
        // Ports of a controller with port power control start unpowered.
        let mut powered = false;
        for port in 1..=self.max_ports {
            let ps = self.portsc(port);
            let v = hal::rd32(ps);
            if v & PORT_PP == 0 {
                hal::wr32(ps, (v & PORT_KEEP) | PORT_PP);
                powered = true;
            }
        }
        if powered {
            hal::delay_ms(100); // connect debounce
        }
        for port in 1..=self.max_ports {
            let ps = self.portsc(port);
            let v = hal::rd32(ps);
            if v & PORT_CHANGE != 0 {
                hal::wr32(ps, (v & PORT_KEEP) | PORT_CHANGE);
            }
            let known = (1..MAX_SLOT).find(|&s| self.devs[s].as_ref().map_or(false, |d| d.port == port));
            let connected = v & PORT_CCS != 0;
            match (known, connected) {
                (Some(s), false) => {
                    let d = self.devs[s].as_ref().unwrap();
                    if d.kind == Kind::Msc {
                        usbmsc::detach(Handle { ctrl, slot: s as u8 });
                    }
                    self.remove(s as u8);
                }
                (None, true) => self.enumerate(ctrl, port, found),
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------- public API

fn probe(d: &pci::PciDev, idx: usize) -> bool {
    let Some(bar) = d.bar(0) else { return false };
    if bar.io || bar.size < 0x1000 {
        return false;
    }
    d.enable(pci::CMD_MEM | pci::CMD_BUS_MASTER | pci::CMD_INTX_DISABLE);
    let Some(cap) = hal::map_bar(bar.addr, bar.size.min(0x10_0000)) else { return false };
    let lenver = hal::rd32(cap);
    if lenver == u32::MAX {
        return false;
    }
    let caplen = (lenver & 0xFF) as u64;
    let hcs1 = hal::rd32(cap + CAP_HCSPARAMS1);
    let hcs2 = hal::rd32(cap + CAP_HCSPARAMS2);
    let hcc = hal::rd32(cap + CAP_HCCPARAMS1);

    // BIOS -> OS ownership (USB Legacy Support capability).
    let mut p = cap + ((hcc >> 16) as u64) * 4;
    if hcc >> 16 != 0 {
        for _ in 0..64 {
            let v = hal::rd32(p);
            if v & 0xFF == 1 {
                if v & (1 << 16) != 0 {
                    hal::wr8(p + 3, 1);
                    if !hal::wait_event(1000, || hal::rd32(p) & (1 << 16) == 0) {
                        hal::wr8(p + 2, 0); // firmware did not let go: take it
                    }
                }
                hal::wr32(p + 4, 0xE000_0000); // SMIs off, status cleared
                break;
            }
            let next = ((v >> 8) & 0xFF) as u64;
            if next == 0 {
                break;
            }
            p += next * 4;
        }
    }

    let mut g = CTRL[idx].lock();
    g.cap = cap;
    g.op = cap + caplen;
    g.rt = cap + (hal::rd32(cap + CAP_RTSOFF) & !0x1F) as u64;
    g.db = cap + (hal::rd32(cap + CAP_DBOFF) & !0x3) as u64;
    g.cs = if hcc & (1 << 2) != 0 { 64 } else { 32 };
    g.max_slots = (hcs1 & 0xFF) as u8;
    g.max_ports = (hcs1 >> 24) as u8;
    g.scratch_n = ((((hcs2 >> 21) & 0x1F) << 5) | ((hcs2 >> 27) & 0x1F)) as usize;
    if g.scratch_n > 512 || hal::rd32(g.op + OP_PAGESIZE) & 1 == 0 {
        kprintln!("xhci: unsupported controller (scratchpad {} / page size)", g.scratch_n);
        return false;
    }
    if !g.reset_controller() || !g.setup() {
        kprintln!("xhci: {:02x}:{:02x}.{} failed to start", d.bus, d.dev, d.func);
        return false;
    }
    g.present = true;
    true
}

/// Probe the xHCI controllers, enumerate what is plugged in and bring up
/// keyboards and mass storage.  Returns the number of controllers running.
pub fn init() -> usize {
    let mut n = 0;
    for d in pci::devices() {
        if d.class == 0x0C && d.subclass == 0x03 && d.progif == 0x30 {
            if n >= MAX_CTRL {
                break;
            }
            if probe(d, n) {
                n += 1;
            }
        }
    }
    if n > 0 {
        ANY.store(true, Ordering::Release);
        hal::delay_ms(100); // let devices come up after power-on / reset
        rescan();
    }
    n
}

/// Re-examine the root ports (hot plug is not interrupt driven).  Returns
/// the number of mass storage devices newly attached.
pub fn rescan() -> usize {
    PORT_EVENT.store(false, Ordering::Relaxed);
    let mut attached = 0;
    for c in 0..MAX_CTRL {
        let mut found = Vec::new();
        {
            let mut g = CTRL[c].lock();
            if !g.present {
                continue;
            }
            g.scan(c as u8, &mut found);
        }
        for f in found {
            if usbmsc::attach(&f) {
                attached += 1;
            } else {
                // Not usable as a disk: leave the slot in place but inert.
                if let Some(mut g) = ctrl_lock(f.h) {
                    if let Some(d) = g.devs[f.h.slot as usize].as_mut() {
                        d.kind = Kind::Other;
                    }
                }
            }
        }
    }
    attached
}

/// Did the controller report a port change since the last `rescan`?
pub fn port_event_pending() -> bool {
    PORT_EVENT.load(Ordering::Acquire)
}

pub fn controllers() -> usize {
    CTRL.iter().filter(|c| c.try_lock().map_or(true, |g| g.present)).count()
}

pub fn list() -> Vec<DevInfo> {
    let mut v = Vec::new();
    for (c, m) in CTRL.iter().enumerate() {
        let g = m.lock();
        if !g.present {
            continue;
        }
        for s in 1..MAX_SLOT {
            if let Some(d) = g.devs[s].as_ref() {
                v.push(DevInfo {
                    h: Handle { ctrl: c as u8, slot: s as u8 },
                    port: d.port,
                    speed: speed_name(d.speed),
                    vid: d.vid,
                    pid: d.pid,
                    class: class_name(d.class, d.kind),
                });
            }
        }
    }
    v
}

/// Called from the timer tick: collect keyboard reports.  Never waits.
pub fn poll() {
    if !ANY.load(Ordering::Acquire) {
        return;
    }
    for m in CTRL.iter() {
        if let Some(mut g) = m.try_lock() {
            if g.present {
                while let Some(ev) = g.next_event() {
                    let _ = g.dispatch(ev);
                }
            }
        }
    }
    usbhid::tick();
}
