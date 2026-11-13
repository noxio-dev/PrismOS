// xHCI controller model + USB devices (HID boot keyboard, bulk-only mass
// storage).  Enforces the rules real controllers enforce: TRB cycle bits,
// 64 KiB TRB boundaries, context layouts, endpoint states, port power.

#![allow(dead_code)]

use crate::models::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const CAPLEN: u64 = 0x20;
const OP: u64 = CAPLEN;
const PORTS: u64 = OP + 0x400;
const RT: u64 = 0x600;
const IR0: u64 = RT + 0x20;
const DB: u64 = 0x800;
const LEGACY: u64 = 0x40;
const NPORTS: usize = 4;
const NSLOTS: usize = 8;

// ---------------------------------------------------------------- devices

pub struct Msc {
    pub disk: Vec<u8>,
    pub bs: usize,
    phase: Phase,
    halt_in: bool,
    halt_out: bool,
    /// TEST UNIT READY fails this many times first (unit attention).
    pub tur_fail: u32,
    /// Stall the next data-in stage once.
    pub stall_next_data_in: bool,
    /// REQUEST SENSE returns fewer bytes than asked (short packet).
    pub short_sense: bool,
    pub cmds: u64,
    pub stalls: u64,
}

enum Phase {
    Idle,
    DataIn(Vec<u8>, u32 /*tag*/, u32 /*dlen*/, u8 /*status*/),
    DataOut { lba: usize, len: usize, tag: u32, dlen: u32 },
    Csw(u32, u32, u8),
}

pub struct Kbd {
    pub reports: VecDeque<[u8; 8]>,
    pub stall_set_idle: bool,
    pub protocol_set: Option<u16>,
}

pub enum Dev {
    Kbd(Kbd),
    Msc(Msc),
}

impl Msc {
    pub fn new(bs: usize, bytes: usize) -> Msc {
        let mut disk = vec![0u8; bytes];
        for (i, b) in disk.iter_mut().enumerate() { *b = (i * 13 + i / 4096) as u8; }
        Msc { disk, bs, phase: Phase::Idle, halt_in: false, halt_out: false, tur_fail: 2,
              stall_next_data_in: false, short_sense: true, cmds: 0, stalls: 0 }
    }

    fn cbw(&mut self, b: &[u8]) {
        assert_eq!(b.len(), 31, "CBW must be 31 bytes");
        assert_eq!(&b[0..4], &0x43425355u32.to_le_bytes(), "CBW signature");
        let tag = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        let dlen = u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
        let dir_in = b[12] & 0x80 != 0;
        let cb = &b[15..15 + b[14] as usize];
        self.cmds += 1;
        let (mut data, mut status) = (Vec::new(), 0u8);
        match cb[0] {
            0x00 => {
                if self.tur_fail > 0 { self.tur_fail -= 1; status = 1; }
            }
            0x03 => {
                data = vec![0x70, 0, 6, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0x29, 0, 0, 0, 0, 0];
                if self.short_sense { data.truncate(14); }
            }
            0x12 => {
                data = vec![0u8; 36];
                data[1] = 0x80;
                data[4] = 31;
                data[8..16].copy_from_slice(b"PRISMOS ");
                data[16..32].copy_from_slice(b"USB TEST DISK   ");
            }
            0x25 => {
                let last = (self.disk.len() / self.bs - 1) as u32;
                data.extend_from_slice(&last.to_be_bytes());
                data.extend_from_slice(&(self.bs as u32).to_be_bytes());
            }
            0x28 | 0x2A => {
                let lba = u32::from_be_bytes([cb[2], cb[3], cb[4], cb[5]]) as usize;
                let n = u16::from_be_bytes([cb[7], cb[8]]) as usize;
                assert_eq!(dlen as usize, n * self.bs, "CBW length vs block count");
                if (lba + n) * self.bs > self.disk.len() { status = 1; }
                else if cb[0] == 0x28 { data = self.disk[lba * self.bs..(lba + n) * self.bs].to_vec(); }
                else {
                    self.phase = Phase::DataOut { lba: lba * self.bs, len: n * self.bs, tag, dlen };
                    return;
                }
            }
            0x35 => {}
            _ => status = 1,
        }
        if dir_in && dlen > 0 {
            self.phase = Phase::DataIn(data, tag, dlen, status);
        } else {
            self.phase = Phase::Csw(tag, dlen, status);
        }
    }

    fn out(&mut self, b: &[u8]) -> Result<(), ()> {
        if self.halt_out { return Err(()); }
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Idle => self.cbw(b),
            Phase::DataOut { lba, len, tag, dlen } => {
                assert_eq!(b.len(), len, "data-out length");
                self.disk[lba..lba + len].copy_from_slice(b);
                self.phase = Phase::Csw(tag, dlen - len as u32, 0);
            }
            _ => panic!("unexpected bulk OUT in this BOT phase"),
        }
        Ok(())
    }

    /// Bulk IN of up to `want` bytes.  Err = STALL.
    fn input(&mut self, want: usize) -> Result<Vec<u8>, ()> {
        if self.halt_in { return Err(()); }
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::DataIn(mut data, tag, dlen, status) => {
                if self.stall_next_data_in {
                    self.stall_next_data_in = false;
                    self.halt_in = true;
                    self.stalls += 1;
                    self.phase = Phase::Csw(tag, dlen, 1);
                    return Err(());
                }
                let n = want.min(data.len());
                let out: Vec<u8> = data.drain(..n).collect();
                if data.is_empty() { self.phase = Phase::Csw(tag, dlen - n as u32, status); }
                else { self.phase = Phase::DataIn(data, tag, dlen, status); }
                Ok(out)
            }
            Phase::Csw(tag, resid, status) => {
                assert!(want >= 13, "CSW read shorter than 13");
                let mut c = Vec::new();
                c.extend_from_slice(&0x53425355u32.to_le_bytes());
                c.extend_from_slice(&tag.to_le_bytes());
                c.extend_from_slice(&resid.to_le_bytes());
                c.push(status);
                Ok(c)
            }
            _ => panic!("unexpected bulk IN in this BOT phase"),
        }
    }
}

// ------------------------------------------------------------- controller

#[derive(Clone, Copy, Default)]
struct Ep {
    on: bool,
    ty: u32,
    mps: u32,
    deq: u64,
    cycle: bool,
    halted: bool,
    stopped: bool,
}

#[derive(Default)]
struct TdTrb { addr: u64, len: u32, trb: u64, isp: bool, ioc: bool }

#[derive(Default)]
struct Slot {
    used: bool,
    port: usize,
    addressed: bool,
    configured: bool,
    ctx_out: u64,
    ep: [Ep; 32],
    setup: Option<(u64, [u8; 8])>,
    data: Option<TdTrb>,
    td: Vec<TdTrb>,
    armed: VecDeque<TdTrb>,
}

#[derive(Default)]
struct Port {
    dev: Option<Dev>,
    speed: u8,
    pp: bool,
    ped: bool,
    csc: bool,
    prc: bool,
    usb3: bool,
    resets: u32,
}

pub struct Xhci {
    csz64: bool,
    pub double_event: bool,
    ports: Vec<Port>,
    slots: Vec<Slot>,
    running: bool,
    crcr: u64,
    cmd_deq: u64,
    cmd_cycle: bool,
    dcbaap: u64,
    config: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    ev_enq: u32,
    ev_cycle: bool,
    bios_owned: bool,
    os_owned: bool,
    pub saw_eval_ctx: bool,
    pub cmds_seen: Vec<u32>,
    pub ev_posted: u64,
}

impl Xhci {
    pub fn new(csz64: bool) -> Xhci {
        let mut ports = Vec::new();
        for _ in 0..NPORTS { ports.push(Port::default()); }
        let mut slots = Vec::new();
        for _ in 0..=NSLOTS { slots.push(Slot::default()); }
        Xhci { csz64, double_event: false, ports, slots, running: false, crcr: 0, cmd_deq: 0, cmd_cycle: true,
               dcbaap: 0, config: 0, erstsz: 0, erstba: 0, erdp: 0, ev_enq: 0, ev_cycle: true,
               bios_owned: true, os_owned: false, saw_eval_ctx: false, cmds_seen: Vec::new(), ev_posted: 0 }
    }

    pub fn plug(&mut self, port: usize, speed: u8, dev: Dev) {
        let p = &mut self.ports[port - 1];
        p.dev = Some(dev);
        p.speed = speed;
        p.usb3 = speed >= 4;
        p.csc = true;
        p.ped = p.usb3 && p.pp;
    }

    pub fn unplug(&mut self, port: usize) {
        let p = &mut self.ports[port - 1];
        p.dev = None;
        p.ped = false;
        p.csc = true;
    }

    pub fn dev(&mut self, port: usize) -> &mut Dev {
        self.ports[port - 1].dev.as_mut().unwrap()
    }

    fn cs(&self) -> u64 { if self.csz64 { 64 } else { 32 } }

    // ----------------------------------------------------------- event ring
    fn post(&mut self, ptr: u64, status: u32, ctl: u32) {
        assert!(self.erstsz >= 1, "event ring not set up");
        // SAFETY: ERST entry 0 and the segment it names are driver memory.
        let (seg, segn) = unsafe { (m64(self.erstba), m32(self.erstba + 8)) };
        let a = seg + self.ev_enq as u64 * 16;
        unsafe {
            w64(a, ptr);
            w32(a + 8, status);
            w32(a + 12, ctl | self.ev_cycle as u32);
        }
        self.ev_enq += 1;
        if self.ev_enq == segn { self.ev_enq = 0; self.ev_cycle = !self.ev_cycle; }
        self.ev_posted += 1;
    }

    fn xfer_event(&mut self, slot: usize, dci: usize, ptr: u64, code: u32, resid: u32) {
        self.post(ptr, code << 24 | resid, 32 << 10 | (dci as u32) << 16 | (slot as u32) << 24);
    }

    // -------------------------------------------------------------- commands
    fn run_commands(&mut self) {
        loop {
            // SAFETY: command ring page.
            let (p, st, ctl) = unsafe { (m64(self.cmd_deq), m32(self.cmd_deq + 8), m32(self.cmd_deq + 12)) };
            if (ctl & 1 != 0) != self.cmd_cycle { break; }
            let ty = (ctl >> 10) & 0x3F;
            let trb = self.cmd_deq;
            if ty == 6 {
                self.cmd_deq = p;
                if ctl & 2 != 0 { self.cmd_cycle = !self.cmd_cycle; }
                continue;
            }
            self.cmd_deq += 16;
            self.cmds_seen.push(ty);
            let slot = (ctl >> 24) as usize;
            let dci = ((ctl >> 16) & 0x1F) as usize;
            let (code, newslot) = self.command(ty, p, st, slot, dci);
            self.post(trb, code << 24, 33 << 10 | (newslot as u32) << 24);
        }
    }

    fn command(&mut self, ty: u32, param: u64, _st: u32, slot: usize, dci: usize) -> (u32, usize) {
        let cs = self.cs();
        match ty {
            9 => {
                for s in 1..=NSLOTS {
                    if !self.slots[s].used {
                        self.slots[s] = Slot { used: true, ..Default::default() };
                        return (1, s);
                    }
                }
                (9, 0)
            }
            10 => {
                self.slots[slot] = Slot::default();
                (1, slot)
            }
            11 => {
                assert!(self.slots[slot].used, "Address Device on unused slot");
                // SAFETY: the driver's input context page.
                let (drop, add) = unsafe { (m32(param), m32(param + 4)) };
                assert_eq!(drop, 0);
                assert_eq!(add, 0b11, "Address Device must add slot + EP0");
                let (s0, s1) = unsafe { (m32(param + cs), m32(param + cs + 4)) };
                let port = ((s1 >> 16) & 0xFF) as usize;
                assert_eq!(s0 & 0xFFFFF, 0, "route string");
                assert_eq!(s0 >> 27, 1, "context entries at address time");
                assert!(port >= 1 && port <= NPORTS && self.ports[port - 1].ped, "Address Device on a port that is not enabled");
                assert_eq!(((s0 >> 20) & 0xF) as u8, self.ports[port - 1].speed, "slot speed");
                let e = param + 2 * cs;
                let (e1, e2, e3) = unsafe { (m32(e + 4), m32(e + 8), m32(e + 12)) };
                assert_eq!((e1 >> 3) & 7, 4, "EP0 type");
                assert_eq!((e1 >> 1) & 3, 3, "EP0 CErr");
                let deq = e2 as u64 | (e3 as u64) << 32;
                let dcb = unsafe { m64(self.dcbaap + 8 * slot as u64) };
                assert_ne!(dcb, 0, "DCBAA entry not set before Address Device");
                let sl = &mut self.slots[slot];
                sl.port = port;
                sl.addressed = true;
                sl.ctx_out = dcb;
                sl.ep[1] = Ep { on: true, ty: 4, mps: e1 >> 16, deq: deq & !0xF, cycle: deq & 1 != 0, ..Default::default() };
                // SAFETY: the output context page.
                unsafe {
                    w32(dcb, s0);
                    w32(dcb + 4, s1);
                    w32(dcb + 12, slot as u32 | 2 << 27); // address, state=Addressed
                }
                (1, slot)
            }
            12 => {
                assert!(self.slots[slot].addressed, "Configure Endpoint before Address Device");
                let (drop, add) = unsafe { (m32(param), m32(param + 4)) };
                assert_eq!(drop, 0);
                assert_eq!(add & 1, 1, "A0 must be set");
                let s0 = unsafe { m32(param + cs) };
                let mut top = 0;
                for d in 2..32 {
                    if add & (1 << d) != 0 {
                        top = d;
                        let e = param + (d as u64 + 1) * cs;
                        let (e0, e1, e2, e3) = unsafe { (m32(e), m32(e + 4), m32(e + 8), m32(e + 12)) };
                        let ty = (e1 >> 3) & 7;
                        let mps = e1 >> 16;
                        assert!(mps > 0, "endpoint with mps 0");
                        assert_eq!((e1 >> 1) & 3, 3, "CErr");
                        let _ = e0;
                        let deq = e2 as u64 | (e3 as u64) << 32;
                        assert_eq!(deq & 0xE, 0, "dequeue pointer alignment");
                        self.slots[slot].ep[d] = Ep { on: true, ty, mps, deq: deq & !0xF, cycle: deq & 1 != 0, ..Default::default() };
                    }
                }
                assert!(s0 >> 27 >= top as u32, "context entries {} < top endpoint {}", s0 >> 27, top);
                self.slots[slot].configured = true;
                (1, slot)
            }
            13 => {
                let add = unsafe { m32(param + 4) };
                assert_eq!(add, 0b10, "Evaluate Context: only EP0");
                let e1 = unsafe { m32(param + 2 * cs + 4) };
                self.slots[slot].ep[1].mps = e1 >> 16;
                self.saw_eval_ctx = true;
                (1, slot)
            }
            14 => {
                let ep = &mut self.slots[slot].ep[dci];
                if !ep.halted { return (19, slot); }
                ep.halted = false;
                ep.stopped = true;
                (1, slot)
            }
            15 => {
                let ep = &mut self.slots[slot].ep[dci];
                if ep.halted || ep.stopped { return (19, slot); }
                ep.stopped = true;
                (1, slot)
            }
            16 => {
                let s = &mut self.slots[slot];
                let ep = &mut s.ep[dci];
                if !ep.stopped { return (19, slot); }
                ep.deq = param & !0xF;
                ep.cycle = param & 1 != 0;
                ep.stopped = false;
                s.setup = None;
                s.data = None;
                s.td.clear();
                (1, slot)
            }
            t => panic!("unexpected command type {}", t),
        }
    }

    // ------------------------------------------------------------ transfers
    fn doorbell(&mut self, slot: usize, dci: usize) {
        if slot == 0 {
            if self.running { self.run_commands(); }
            return;
        }
        assert!(self.slots[slot].used && self.slots[slot].ep[dci].on, "doorbell for an unconfigured endpoint {}:{}", slot, dci);
        self.run_ep(slot, dci);
    }

    fn run_ep(&mut self, slot: usize, dci: usize) {
        loop {
            let ep = self.slots[slot].ep[dci];
            if ep.halted || ep.stopped { return; }
            // SAFETY: transfer ring page.
            let (p, st, ctl) = unsafe { (m64(ep.deq), m32(ep.deq + 8), m32(ep.deq + 12)) };
            if (ctl & 1 != 0) != ep.cycle { return; }
            let ty = (ctl >> 10) & 0x3F;
            let trb = ep.deq;
            let e = &mut self.slots[slot].ep[dci];
            if ty == 6 {
                e.deq = p;
                if ctl & 2 != 0 { e.cycle = !e.cycle; }
                continue;
            }
            e.deq += 16;
            let t = TdTrb { addr: p, len: st & 0x1FFFF, trb, isp: ctl & 4 != 0, ioc: ctl & 0x20 != 0 };
            match (dci, ty) {
                (1, 2) => {
                    assert!(ctl & 0x40 != 0, "Setup TRB without IDT");
                    assert_eq!(st & 0x1FFFF, 8);
                    self.slots[slot].setup = Some((trb, p.to_le_bytes()));
                }
                (1, 3) => self.slots[slot].data = Some(t),
                (1, 4) => self.control(slot, t, ctl),
                (_, 1) => {
                    assert!(t.len <= 0x10000, "TRB longer than 64 KiB");
                    assert!(t.len == 0 || (t.addr & 0xFFFF) + t.len as u64 <= 0x10000, "TRB crosses a 64 KiB boundary");
                    let ty_ep = self.slots[slot].ep[dci].ty;
                    if ty_ep == 7 {
                        self.slots[slot].armed.push_back(t);
                        self.deliver_kbd(slot, dci);
                    } else {
                        let chain = ctl & 0x10 != 0;
                        self.slots[slot].td.push(t);
                        if !chain {
                            self.bulk_td(slot, dci);
                        }
                    }
                }
                other => panic!("unexpected TRB {:?} on ep {}", other, dci),
            }
        }
    }

    fn halt(&mut self, slot: usize, dci: usize, ptr: u64) {
        self.slots[slot].ep[dci].halted = true;
        self.xfer_event(slot, dci, ptr, 6, 0);
    }

    fn control(&mut self, slot: usize, status: TdTrb, ctl: u32) {
        let (_, s) = self.slots[slot].setup.take().expect("Status stage without Setup");
        let data = self.slots[slot].data.take();
        let (bm, req, val, idx, len) = (s[0], s[1], u16::from_le_bytes([s[2], s[3]]), u16::from_le_bytes([s[4], s[5]]), u16::from_le_bytes([s[6], s[7]]));
        let dir_in = bm & 0x80 != 0;
        if len > 0 { assert_eq!(data.is_some(), true, "wLength without Data stage"); }
        let port = self.slots[slot].port;
        let status_dir_in = ctl & (1 << 16) != 0;
        assert_eq!(status_dir_in, !(len > 0 && dir_in), "status stage direction");
        let mut resp: Vec<u8> = Vec::new();
        let mut ok = true;
        let mps0 = self.slots[slot].ep[1].mps;
        let speed = self.ports[port - 1].speed;
        let dev = self.ports[port - 1].dev.as_mut().expect("control on an unplugged device");
        match (bm, req) {
            (0x80, 6) => match val >> 8 {
                1 => {
                    let real_mps0 = if speed <= 2 { 64u8 } else if speed == 3 { 64 } else { 9 };
                    if speed <= 2 && len == 8 { /* first look: initial mps was 8 */ }
                    resp = vec![18, 1, 0x00, if speed >= 4 { 3 } else { 2 }, 0, 0, 0, real_mps0, 0x34, 0x12,
                                match dev { Dev::Kbd(_) => 0x01, Dev::Msc(_) => 0x02 }, 0, 0, 1, 1, 2, 3, 1];
                }
                2 => resp = config_desc(dev, speed),
                t => panic!("descriptor type {}", t),
            },
            (0x00, 9) => assert_eq!(val, 1, "configuration value"),
            (0x21, 0x0B) => if let Dev::Kbd(k) = dev { k.protocol_set = Some(val); assert_eq!(val, 0, "boot protocol") },
            (0x21, 0x0A) => if let Dev::Kbd(k) = dev { if k.stall_set_idle { ok = false } },
            (0x21, 0xFF) => if let Dev::Msc(m) = dev { m.phase = Phase::Idle; },
            (0x02, 1) => if let Dev::Msc(m) = dev {
                match idx { 0x81 => m.halt_in = false, 0x02 => m.halt_out = false, i => panic!("clear halt on {:#x}", i) }
            },
            other => panic!("unexpected control request {:?}", other),
        }
        let _ = mps0;
        if !ok {
            if let Some(d) = &data { self.halt(slot, 1, d.trb); } else { self.halt(slot, 1, status.trb); }
            return;
        }
        let mut short = false;
        if let Some(d) = data {
            if dir_in {
                let n = resp.len().min(len as usize);
                // SAFETY: the driver's control buffer.
                unsafe { mbytes(d.addr, n).copy_from_slice(&resp[..n]) };
                if n < len as usize {
                    short = true;
                    if d.isp { self.xfer_event(slot, 1, d.trb, 13, len as u32 - n as u32); }
                }
            }
        }
        if status.ioc { self.xfer_event(slot, 1, status.trb, 1, 0); }
        let _ = short;
    }

    fn bulk_td(&mut self, slot: usize, dci: usize) {
        let td = std::mem::take(&mut self.slots[slot].td);
        let port = self.slots[slot].port;
        let dir_in = dci & 1 == 1;
        let total: usize = td.iter().map(|t| t.len as usize).sum();
        let last = td.last().unwrap();
        let (last_trb, last_ioc) = (last.trb, last.ioc);
        let dev = self.ports[port - 1].dev.as_mut().unwrap();
        let Dev::Msc(m) = dev else { panic!("bulk on a non-storage device") };
        if !dir_in {
            let mut bytes = Vec::with_capacity(total);
            for t in &td {
                // SAFETY: the driver's DMA buffer.
                bytes.extend_from_slice(unsafe { mbytes(t.addr, t.len as usize) });
            }
            match m.out(&bytes) {
                Ok(()) => { if last_ioc { self.xfer_event(slot, dci, last_trb, 1, 0) } }
                Err(()) => self.halt(slot, dci, td[0].trb),
            }
            return;
        }
        match m.input(total) {
            Err(()) => {
                m.halt_in = true;
                self.halt(slot, dci, td[0].trb);
            }
            Ok(data) => {
                let mut off = 0;
                let mut end_trb = None;
                for (i, t) in td.iter().enumerate() {
                    let n = (t.len as usize).min(data.len() - off);
                    // SAFETY: the driver's DMA buffer.
                    unsafe { mbytes(t.addr, n).copy_from_slice(&data[off..off + n]) };
                    off += n;
                    if n < t.len as usize && end_trb.is_none() { end_trb = Some((i, t.len - n as u32)); }
                }
                match end_trb {
                    None => { if last_ioc { self.xfer_event(slot, dci, last_trb, 1, 0) } }
                    Some((i, resid)) => {
                        assert!(td[i].isp, "short packet on a TRB without ISP");
                        self.xfer_event(slot, dci, td[i].trb, 13, resid);
                        if self.double_event && i != td.len() - 1 && last_ioc {
                            self.xfer_event(slot, dci, last_trb, 1, 0);
                        }
                    }
                }
            }
        }
    }

    fn deliver_kbd(&mut self, slot: usize, dci: usize) {
        let port = self.slots[slot].port;
        loop {
            let Some(t) = self.slots[slot].armed.front() else { return };
            let Dev::Kbd(k) = self.ports[port - 1].dev.as_mut().unwrap() else { panic!() };
            let Some(r) = k.reports.pop_front() else { return };
            let (addr, trb, len) = (t.addr, t.trb, t.len);
            assert_eq!(len, 8);
            // SAFETY: the driver's report buffer.
            unsafe { mbytes(addr, 8).copy_from_slice(&r) };
            self.slots[slot].armed.pop_front();
            self.xfer_event(slot, dci, trb, 1, 0);
        }
    }

    pub fn kbd_report(&mut self, port: usize, r: [u8; 8]) {
        let Dev::Kbd(k) = self.ports[port - 1].dev.as_mut().unwrap() else { panic!() };
        k.reports.push_back(r);
        for s in 1..=NSLOTS {
            if self.slots[s].used && self.slots[s].port == port && self.slots[s].configured {
                self.deliver_kbd(s, 3);
            }
        }
    }

    pub fn slots_in_use(&self) -> usize { self.slots.iter().filter(|s| s.used).count() }
}

fn config_desc(dev: &Dev, speed: u8) -> Vec<u8> {
    let mut d = vec![9, 2, 0, 0, 1, 1, 0, 0x80, 50];
    match dev {
        Dev::Kbd(_) => {
            d.extend_from_slice(&[9, 4, 0, 0, 1, 3, 1, 1, 0]);
            d.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0]); // HID descriptor
            d.extend_from_slice(&[7, 5, 0x81, 3, 8, 0, 10]);
        }
        Dev::Msc(_) => {
            d.extend_from_slice(&[9, 4, 0, 0, 2, 8, 6, 0x50, 0]);
            let mps: u16 = if speed >= 4 { 1024 } else { 512 };
            // Bulk IN first, OUT second (order is up to the device).
            d.extend_from_slice(&[7, 5, 0x81, 2, mps as u8, (mps >> 8) as u8, 0]);
            if speed >= 4 { d.extend_from_slice(&[6, 0x30, 3, 0, 0, 0]); }
            d.extend_from_slice(&[7, 5, 0x02, 2, mps as u8, (mps >> 8) as u8, 0]);
            if speed >= 4 { d.extend_from_slice(&[6, 0x30, 3, 0, 0, 0]); }
        }
    }
    let n = d.len() as u16;
    d[2] = n as u8;
    d[3] = (n >> 8) as u8;
    d
}

impl Mmio for Xhci {
    fn rd32(&mut self, off: u64) -> u32 {
        match off {
            0x00 => 0x0100_0000 | CAPLEN as u32,
            0x04 => NSLOTS as u32 | 1 << 8 | (NPORTS as u32) << 24,
            0x08 => 2 << 27, // two scratchpad buffers (low field)
            0x10 => 1 | (self.csz64 as u32) << 2 | ((LEGACY / 4) as u32) << 16,
            0x14 => DB as u32,
            0x18 => RT as u32,
            o if o == LEGACY => 1 | (self.bios_owned as u32) << 16 | (self.os_owned as u32) << 24,
            o if o == LEGACY + 4 => 0,
            o if o == OP => self.running as u32,
            o if o == OP + 4 => (!self.running) as u32, // HCH
            o if o == OP + 8 => 1,
            o if o == OP + 0x18 => (self.crcr as u32 & !0x3F) | self.cmd_cycle as u32 & 0, // CRR reads 0
            o if o == OP + 0x38 => self.config,
            o if o >= PORTS && o < PORTS + 0x10 * NPORTS as u64 => {
                let i = ((o - PORTS) / 0x10) as usize;
                if (o - PORTS) % 0x10 != 0 { return 0; }
                let p = &self.ports[i];
                let ccs = p.pp && p.dev.is_some();
                (ccs as u32) | (p.ped as u32) << 1 | (p.pp as u32) << 9
                    | if ccs { (p.speed as u32) << 10 } else { 0 }
                    | (p.csc as u32) << 17 | (p.prc as u32) << 21
            }
            o if o == IR0 + 8 => self.erstsz,
            o if o == IR0 + 0x18 => self.erdp as u32,
            o if o == IR0 + 0x1C => (self.erdp >> 32) as u32,
            _ => 0,
        }
    }

    fn rd8(&mut self, off: u64) -> u8 {
        (self.rd32(off & !3) >> ((off & 3) * 8)) as u8
    }

    fn wr8(&mut self, off: u64, v: u8) {
        if off == LEGACY + 3 && v & 1 != 0 {
            self.os_owned = true;
            self.bios_owned = false; // the firmware hands over
        } else if off == LEGACY + 2 {
            self.bios_owned = v & 1 != 0;
        }
    }

    fn wr32(&mut self, off: u64, v: u32) {
        match off {
            o if o == OP => {
                if v & 2 != 0 {
                    assert!(!self.running, "HCRST while running");
                    self.dcbaap = 0; self.crcr = 0; self.erstsz = 0; self.ev_enq = 0; self.ev_cycle = true;
                } else if v & 1 != 0 {
                    assert!(self.dcbaap != 0 && self.crcr != 0 && self.erstsz >= 1 && self.config != 0, "Run before the rings are set up");
                    assert_eq!(v & 4, 0, "driver is polling: INTE must stay 0");
                    self.running = true;
                    self.cmd_deq = self.crcr & !0x3F;
                    self.cmd_cycle = self.crcr & 1 != 0;
                } else {
                    self.running = false;
                }
            }
            o if o == OP + 0x18 => self.crcr = self.crcr & !0xFFFF_FFFF | v as u64,
            o if o == OP + 0x1C => self.crcr = self.crcr & 0xFFFF_FFFF | (v as u64) << 32,
            o if o == OP + 0x30 => self.dcbaap = self.dcbaap & !0xFFFF_FFFF | v as u64,
            o if o == OP + 0x34 => {
                self.dcbaap = self.dcbaap & 0xFFFF_FFFF | (v as u64) << 32;
                // Scratchpad array check.
                let arr = unsafe { m64(self.dcbaap) };
                assert_ne!(arr, 0, "scratchpad array missing");
                for i in 0..2 { assert_ne!(unsafe { m64(arr + 8 * i) } & !0xFFF, 0); }
            }
            o if o == OP + 0x38 => self.config = v,
            o if o == LEGACY + 4 => assert_eq!(v & 0x1FFF_FFFF, 0, "SMI enables must be cleared"),
            o if o == IR0 + 8 => self.erstsz = v,
            o if o == IR0 + 0x10 => self.erstba = self.erstba & !0xFFFF_FFFF | v as u64,
            o if o == IR0 + 0x14 => self.erstba = self.erstba & 0xFFFF_FFFF | (v as u64) << 32,
            o if o == IR0 + 0x18 => self.erdp = self.erdp & !0xFFFF_FFFF | v as u64,
            o if o == IR0 + 0x1C => self.erdp = self.erdp & 0xFFFF_FFFF | (v as u64) << 32,
            o if o >= PORTS && o < PORTS + 0x10 * NPORTS as u64 && (o - PORTS) % 0x10 == 0 => {
                let i = ((o - PORTS) / 0x10) as usize;
                let p = &mut self.ports[i];
                if v & (1 << 17) != 0 { p.csc = false; }
                if v & (1 << 21) != 0 { p.prc = false; }
                if v & 2 != 0 { p.ped = false; } // writing 1 to PED disables the port
                let was = p.pp;
                p.pp = v & (1 << 9) != 0;
                if p.pp && !was && p.dev.is_some() { p.csc = true; p.ped = p.usb3; }
                if v & (1 << 4) != 0 && p.pp && p.dev.is_some() {
                    p.resets += 1;
                    p.ped = true;
                    p.prc = true;
                }
            }
            o if o >= DB && o < DB + 4 * (NSLOTS as u64 + 1) => {
                let slot = ((o - DB) / 4) as usize;
                self.doorbell(slot, (v & 0xFF) as usize);
            }
            _ => {}
        }
    }
}

pub struct XHandle(pub Arc<Mutex<Xhci>>);
impl Mmio for XHandle {
    fn rd32(&mut self, o: u64) -> u32 { self.0.lock().unwrap().rd32(o) }
    fn wr32(&mut self, o: u64, v: u32) { self.0.lock().unwrap().wr32(o, v) }
    fn rd8(&mut self, o: u64) -> u8 { self.0.lock().unwrap().rd8(o) }
    fn wr8(&mut self, o: u64, v: u8) { self.0.lock().unwrap().wr8(o, v) }
}
