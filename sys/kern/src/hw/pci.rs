// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// PCI bus enumeration through the legacy 0xCF8 / 0xCFC configuration
// mechanism.  Bus 0 is scanned, and every PCI-to-PCI bridge found on the way
// has its secondary bus scanned too.  Devices are kept in a static table.

#![allow(dead_code)]

use crate::arch::io::{inl, outl};

const CONFIG_ADDR: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;
pub const MAX_DEVICES: usize = 64;

pub const CMD_IO: u16 = 1 << 0;
pub const CMD_MEM: u16 = 1 << 1;
pub const CMD_BUS_MASTER: u16 = 1 << 2;
pub const CMD_INTX_DISABLE: u16 = 1 << 10;

#[derive(Clone, Copy)]
pub struct PciDev {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub progif: u8,
    pub revision: u8,
    pub header_type: u8,
    pub irq_line: u8,
    pub irq_pin: u8,
    pub bars: [u32; 6],
}

#[derive(Clone, Copy)]
pub struct Bar {
    pub addr: u64,
    pub size: u64,
    pub io: bool,
    pub prefetch: bool,
    pub is64: bool,
}

const EMPTY: PciDev = PciDev {
    bus: 0,
    dev: 0,
    func: 0,
    vendor: 0xFFFF,
    device: 0,
    class: 0,
    subclass: 0,
    progif: 0,
    revision: 0,
    header_type: 0,
    irq_line: 0,
    irq_pin: 0,
    bars: [0; 6],
};

static mut DEVICES: [PciDev; MAX_DEVICES] = [EMPTY; MAX_DEVICES];
static mut COUNT: usize = 0;

// ------------------------------------------------------- config space access

fn addr(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    0x8000_0000 | (bus as u32) << 16 | (dev as u32) << 11 | (func as u32) << 8 | (off as u32 & 0xFC)
}

pub fn cfg_read32(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    let _g = crate::arch::io::IrqGuard::new();
    // SAFETY: standard configuration mechanism #1.
    unsafe {
        outl(CONFIG_ADDR, addr(bus, dev, func, off));
        inl(CONFIG_DATA)
    }
}

pub fn cfg_write32(bus: u8, dev: u8, func: u8, off: u8, val: u32) {
    let _g = crate::arch::io::IrqGuard::new();
    // SAFETY: as above.
    unsafe {
        outl(CONFIG_ADDR, addr(bus, dev, func, off));
        outl(CONFIG_DATA, val);
    }
}

pub fn cfg_read16(bus: u8, dev: u8, func: u8, off: u8) -> u16 {
    (cfg_read32(bus, dev, func, off) >> ((off & 2) * 8)) as u16
}

pub fn cfg_read8(bus: u8, dev: u8, func: u8, off: u8) -> u8 {
    (cfg_read32(bus, dev, func, off) >> ((off & 3) * 8)) as u8
}

pub fn cfg_write16(bus: u8, dev: u8, func: u8, off: u8, val: u16) {
    let shift = (off & 2) * 8;
    let old = cfg_read32(bus, dev, func, off);
    let new = (old & !(0xFFFF << shift)) | ((val as u32) << shift);
    cfg_write32(bus, dev, func, off, new);
}

// ------------------------------------------------------------------- scan

fn record(bus: u8, dev: u8, func: u8) -> bool {
    let id = cfg_read32(bus, dev, func, 0);
    if id & 0xFFFF == 0xFFFF {
        return false;
    }
    let class_reg = cfg_read32(bus, dev, func, 8);
    let header_type = cfg_read8(bus, dev, func, 0x0E) & 0x7F;
    let mut bars = [0u32; 6];
    let nbars = match header_type {
        0 => 6,
        1 => 2,
        _ => 0,
    };
    for (i, b) in bars.iter_mut().enumerate().take(nbars) {
        *b = cfg_read32(bus, dev, func, 0x10 + 4 * i as u8);
    }
    let irq = cfg_read16(bus, dev, func, 0x3C);
    // SAFETY: boot context, single CPU.
    unsafe {
        if COUNT < MAX_DEVICES {
            (*core::ptr::addr_of_mut!(DEVICES))[COUNT] = PciDev {
                bus,
                dev,
                func,
                vendor: id as u16,
                device: (id >> 16) as u16,
                class: (class_reg >> 24) as u8,
                subclass: (class_reg >> 16) as u8,
                progif: (class_reg >> 8) as u8,
                revision: class_reg as u8,
                header_type,
                irq_line: irq as u8,
                irq_pin: (irq >> 8) as u8,
                bars,
            };
            COUNT += 1;
        }
    }
    true
}

fn scan_bus(bus: u8, visited: &mut [bool; 256]) {
    if visited[bus as usize] {
        return;
    }
    visited[bus as usize] = true;
    for dev in 0..32u8 {
        if cfg_read32(bus, dev, 0, 0) & 0xFFFF == 0xFFFF {
            continue;
        }
        let multi = cfg_read8(bus, dev, 0, 0x0E) & 0x80 != 0;
        for func in 0..if multi { 8 } else { 1 } {
            if !record(bus, dev, func) {
                continue;
            }
            let class = cfg_read16(bus, dev, func, 0x0A);
            if class == 0x0604 {
                let secondary = cfg_read8(bus, dev, func, 0x19);
                if secondary != 0 {
                    scan_bus(secondary, visited);
                }
            }
        }
    }
}

/// Enumerate the bus.  Returns the number of functions found.
pub fn init() -> usize {
    let mut visited = [false; 256];
    // SAFETY: boot context.
    unsafe { COUNT = 0 };
    scan_bus(0, &mut visited);
    devices().len()
}

pub fn devices() -> &'static [PciDev] {
    // SAFETY: written only by init(); read-only afterwards.
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(DEVICES) as *const PciDev, COUNT) }
}

pub fn find_class(class: u8, subclass: u8, progif: Option<u8>) -> Option<&'static PciDev> {
    devices()
        .iter()
        .find(|d| d.class == class && d.subclass == subclass && progif.map_or(true, |p| d.progif == p))
}

// ---------------------------------------------------------------- helpers

impl PciDev {
    pub fn read_command(&self) -> u16 {
        cfg_read16(self.bus, self.dev, self.func, 0x04)
    }

    /// Set the given command register bits (memory space, bus master ...).
    pub fn enable(&self, bits: u16) {
        let cmd = self.read_command();
        cfg_write16(self.bus, self.dev, self.func, 0x04, cmd | bits);
    }

    /// Decode BAR `i` (probing its size).  None for unimplemented BARs.
    pub fn bar(&self, i: usize) -> Option<Bar> {
        if i >= 6 {
            return None;
        }
        let raw = self.bars[i];
        if raw == 0 {
            return None;
        }
        let off = 0x10 + 4 * i as u8;
        let io = raw & 1 != 0;
        let is64 = !io && (raw >> 1) & 3 == 2;
        let prefetch = !io && raw & 8 != 0;
        let cmd = self.read_command();
        // Decoding must be off while the BAR holds the all-ones probe value.
        cfg_write16(self.bus, self.dev, self.func, 0x04, cmd & !(CMD_IO | CMD_MEM));
        let probe = |o: u8, orig: u32| {
            cfg_write32(self.bus, self.dev, self.func, o, 0xFFFF_FFFF);
            let v = cfg_read32(self.bus, self.dev, self.func, o);
            cfg_write32(self.bus, self.dev, self.func, o, orig);
            v
        };
        let lo = probe(off, raw);
        let (addr, size);
        if io {
            addr = (raw & !3) as u64;
            let m = (lo & !3) as u64 | 0xFFFF_FFFF_0000_0000;
            size = (!m).wrapping_add(1) & 0xFFFF_FFFF;
        } else if is64 && i < 5 {
            let hi_orig = self.bars[i + 1];
            let hi = probe(off + 4, hi_orig);
            addr = ((hi_orig as u64) << 32) | (raw & !0xF) as u64;
            let m = ((hi as u64) << 32) | (lo & !0xF) as u64;
            size = (!m).wrapping_add(1);
        } else {
            addr = (raw & !0xF) as u64;
            let m = (lo & !0xF) as u64 | 0xFFFF_FFFF_0000_0000;
            size = (!m).wrapping_add(1) & 0xFFFF_FFFF;
        }
        cfg_write16(self.bus, self.dev, self.func, 0x04, cmd);
        Some(Bar { addr, size, io, prefetch, is64 })
    }
}

pub fn vendor_name(id: u16) -> &'static str {
    match id {
        0x8086 => "Intel",
        0x1234 => "QEMU/Bochs",
        0x1AF4 => "Red Hat (virtio)",
        0x1022 => "AMD",
        0x10DE => "NVIDIA",
        0x1002 => "AMD/ATI",
        0x15AD => "VMware",
        0x80EE => "VirtualBox",
        0x10EC => "Realtek",
        0x1B36 => "Red Hat (QEMU)",
        _ => "unknown",
    }
}

pub fn class_name(class: u8, subclass: u8) -> &'static str {
    match (class, subclass) {
        (0x00, _) => "unclassified",
        (0x01, 0x00) => "SCSI controller",
        (0x01, 0x01) => "IDE controller",
        (0x01, 0x05) => "ATA controller",
        (0x01, 0x06) => "SATA controller",
        (0x01, 0x08) => "NVMe controller",
        (0x01, _) => "storage controller",
        (0x02, 0x00) => "Ethernet controller",
        (0x02, _) => "network controller",
        (0x03, _) => "display controller",
        (0x04, _) => "multimedia controller",
        (0x05, _) => "memory controller",
        (0x06, 0x00) => "host bridge",
        (0x06, 0x01) => "ISA bridge",
        (0x06, 0x04) => "PCI bridge",
        (0x06, _) => "bridge",
        (0x07, _) => "communication controller",
        (0x08, _) => "system peripheral",
        (0x0C, 0x03) => "USB controller",
        (0x0C, 0x05) => "SMBus controller",
        (0x0C, _) => "serial bus controller",
        _ => "other",
    }
}
