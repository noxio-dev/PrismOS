// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// ACPI table parser: RSDP (from the boot info) -> XSDT (or RSDT on ACPI 1.0)
// -> MADT (CPUs, IOAPICs, interrupt source overrides), FADT and HPET.
//
// Only what the interrupt and timer setup needs is decoded; AML is never
// touched.  Tables are read in place through vmm::phys_access().

#![allow(dead_code)]

use core::ptr::addr_of;

use crate::boot::BootInfo;

pub const MAX_CPUS: usize = 16;
pub const MAX_IOAPICS: usize = 4;
pub const MAX_OVERRIDES: usize = 16;
pub const MAX_TABLES: usize = 32;

#[derive(Clone, Copy)]
pub struct CpuEntry {
    pub acpi_id: u8,
    pub apic_id: u32,
    pub enabled: bool,
}

#[derive(Clone, Copy)]
pub struct IoApicEntry {
    pub id: u8,
    pub addr: u32,
    pub gsi_base: u32,
}

/// MADT "interrupt source override": ISA IRQ `irq` is wired to `gsi`.
#[derive(Clone, Copy)]
pub struct Override {
    pub irq: u8,
    pub gsi: u32,
    /// MPS INTI flags: bits 0-1 polarity, bits 2-3 trigger mode.
    pub flags: u16,
}

pub struct Acpi {
    pub rsdp_rev: u8,
    pub oem: [u8; 6],
    pub xsdt: bool,
    pub tables: [[u8; 4]; MAX_TABLES],
    pub n_tables: usize,

    pub madt: bool,
    pub lapic_addr: u64,
    pub pcat_compat: bool,
    pub cpus: [CpuEntry; MAX_CPUS],
    pub n_cpus: usize,
    pub ioapics: [IoApicEntry; MAX_IOAPICS],
    pub n_ioapics: usize,
    pub overrides: [Override; MAX_OVERRIDES],
    pub n_overrides: usize,

    pub fadt: bool,
    pub sci_irq: u16,
    pub pm_timer_port: u32,
    pub fadt_flags: u32,

    pub hpet: bool,
    pub hpet_addr: u64,
    pub hpet_min_tick: u16,
}

const NO_CPU: CpuEntry = CpuEntry { acpi_id: 0, apic_id: 0, enabled: false };
const NO_IOAPIC: IoApicEntry = IoApicEntry { id: 0, addr: 0, gsi_base: 0 };
const NO_OVERRIDE: Override = Override { irq: 0, gsi: 0, flags: 0 };

static mut ACPI: Acpi = Acpi {
    rsdp_rev: 0,
    oem: [0; 6],
    xsdt: false,
    tables: [[0; 4]; MAX_TABLES],
    n_tables: 0,
    madt: false,
    lapic_addr: 0,
    pcat_compat: false,
    cpus: [NO_CPU; MAX_CPUS],
    n_cpus: 0,
    ioapics: [NO_IOAPIC; MAX_IOAPICS],
    n_ioapics: 0,
    overrides: [NO_OVERRIDE; MAX_OVERRIDES],
    n_overrides: 0,
    fadt: false,
    sci_irq: 0,
    pm_timer_port: 0,
    fadt_flags: 0,
    hpet: false,
    hpet_addr: 0,
    hpet_min_tick: 0,
};
static mut FOUND: bool = false;

/// Parsed tables, None when the firmware gave no usable ACPI.
pub fn info() -> Option<&'static Acpi> {
    // SAFETY: written once at boot by init(), read-only afterwards.
    unsafe {
        if FOUND {
            Some(&*addr_of!(ACPI))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------- helpers

unsafe fn rd8(base: *const u8, off: usize) -> u8 {
    base.add(off).read_unaligned()
}
unsafe fn rd16(base: *const u8, off: usize) -> u16 {
    (base.add(off) as *const u16).read_unaligned()
}
unsafe fn rd32(base: *const u8, off: usize) -> u32 {
    (base.add(off) as *const u32).read_unaligned()
}
unsafe fn rd64(base: *const u8, off: usize) -> u64 {
    (base.add(off) as *const u64).read_unaligned()
}

fn checksum_ok(p: *const u8, len: usize) -> bool {
    let mut sum = 0u8;
    for i in 0..len {
        // SAFETY: callers pass a mapped range of `len` bytes.
        sum = sum.wrapping_add(unsafe { p.add(i).read() });
    }
    sum == 0
}

/// Pointer to a table at `phys` whose header is valid and whose checksum
/// matches; returns (pointer, length).
fn table_at(phys: u64) -> Option<(*const u8, usize)> {
    let va = crate::mem::vmm::phys_access(phys)?;
    let p = va as *const u8;
    // SAFETY: the address is mapped; the header is 36 bytes.
    let len = unsafe { rd32(p, 4) } as usize;
    if !(36..=(4 << 20)).contains(&len) {
        return None;
    }
    // The whole table must be reachable too (it is when below 4 GiB).
    let last = crate::mem::vmm::phys_access(phys + len as u64 - 1)?;
    if last != va + len as u64 - 1 {
        return None;
    }
    if !checksum_ok(p, len) {
        return None;
    }
    Some((p, len))
}

fn sig(p: *const u8) -> [u8; 4] {
    // SAFETY: header of a validated table.
    unsafe { [rd8(p, 0), rd8(p, 1), rd8(p, 2), rd8(p, 3)] }
}

// ------------------------------------------------------------------- init

/// Parse the ACPI tables.  Returns true when at least the MADT was found.
pub fn init(bi: &BootInfo) -> bool {
    if bi.rsdp_phys == 0 {
        return false;
    }
    let Some(va) = crate::mem::vmm::phys_access(bi.rsdp_phys) else { return false };
    let rsdp = va as *const u8;
    // SAFETY: the loader reports a mapped RSDP; reads stay inside its 36 bytes.
    let a = unsafe { &mut *core::ptr::addr_of_mut!(ACPI) };
    unsafe {
        if core::slice::from_raw_parts(rsdp, 8) != b"RSD PTR " || !checksum_ok(rsdp, 20) {
            return false;
        }
        a.rsdp_rev = rd8(rsdp, 15);
        for i in 0..6 {
            a.oem[i] = rd8(rsdp, 9 + i);
        }
        let (root, entry) = if a.rsdp_rev >= 2 && checksum_ok(rsdp, rd32(rsdp, 20) as usize) {
            a.xsdt = true;
            (rd64(rsdp, 24), 8)
        } else {
            (rd32(rsdp, 16) as u64, 4)
        };
        let Some((rt, rlen)) = table_at(root) else { return false };
        let n = (rlen - 36) / entry;
        for i in 0..n {
            let pa = if entry == 8 { rd64(rt, 36 + i * 8) } else { rd32(rt, 36 + i * 4) as u64 };
            let Some((t, len)) = table_at(pa) else { continue };
            let s = sig(t);
            if a.n_tables < MAX_TABLES {
                a.tables[a.n_tables] = s;
                a.n_tables += 1;
            }
            match &s {
                b"APIC" => parse_madt(a, t, len),
                b"FACP" => parse_fadt(a, t, len),
                b"HPET" => parse_hpet(a, t, len),
                _ => {}
            }
        }
        FOUND = a.madt;
    }
    a.madt
}

unsafe fn parse_madt(a: &mut Acpi, t: *const u8, len: usize) {
    a.madt = true;
    a.lapic_addr = rd32(t, 36) as u64;
    a.pcat_compat = rd32(t, 40) & 1 != 0;
    let mut off = 44;
    while off + 2 <= len {
        let ty = rd8(t, off);
        let l = rd8(t, off + 1) as usize;
        if l < 2 || off + l > len {
            break;
        }
        match ty {
            0 if l >= 8 && a.n_cpus < MAX_CPUS => {
                let flags = rd32(t, off + 4);
                // bit 0 enabled, bit 1 online-capable
                a.cpus[a.n_cpus] = CpuEntry {
                    acpi_id: rd8(t, off + 2),
                    apic_id: rd8(t, off + 3) as u32,
                    enabled: flags & 3 != 0,
                };
                a.n_cpus += 1;
            }
            1 if l >= 12 && a.n_ioapics < MAX_IOAPICS => {
                a.ioapics[a.n_ioapics] = IoApicEntry {
                    id: rd8(t, off + 2),
                    addr: rd32(t, off + 4),
                    gsi_base: rd32(t, off + 8),
                };
                a.n_ioapics += 1;
            }
            2 if l >= 10 && a.n_overrides < MAX_OVERRIDES => {
                a.overrides[a.n_overrides] = Override {
                    irq: rd8(t, off + 3),
                    gsi: rd32(t, off + 4),
                    flags: rd16(t, off + 8),
                };
                a.n_overrides += 1;
            }
            5 if l >= 12 => a.lapic_addr = rd64(t, off + 4),
            9 if l >= 16 && a.n_cpus < MAX_CPUS => {
                // x2APIC CPU
                let flags = rd32(t, off + 8);
                a.cpus[a.n_cpus] = CpuEntry {
                    acpi_id: rd32(t, off + 12) as u8,
                    apic_id: rd32(t, off + 4),
                    enabled: flags & 3 != 0,
                };
                a.n_cpus += 1;
            }
            _ => {}
        }
        off += l;
    }
}

unsafe fn parse_fadt(a: &mut Acpi, t: *const u8, len: usize) {
    a.fadt = true;
    if len >= 48 {
        a.sci_irq = rd16(t, 46);
    }
    if len >= 80 {
        a.pm_timer_port = rd32(t, 76);
    }
    if len >= 116 {
        a.fadt_flags = rd32(t, 112);
    }
}

unsafe fn parse_hpet(a: &mut Acpi, t: *const u8, len: usize) {
    // header(36) + event timer block id(4) + GAS(12: space, width, offset,
    // access, address u64) + number(1) + min tick(2) + attributes(1)
    if len < 56 {
        return;
    }
    let space = rd8(t, 40);
    if space != 0 {
        return; // only memory-mapped HPETs
    }
    a.hpet = true;
    a.hpet_addr = rd64(t, 44);
    a.hpet_min_tick = rd16(t, 53);
}

/// Resolve an ISA IRQ to (GSI, active-low, level-triggered) using the MADT
/// overrides.  ISA defaults: identity, active high, edge.
pub fn isa_route(irq: u8) -> (u32, bool, bool) {
    if let Some(a) = info() {
        for o in &a.overrides[..a.n_overrides] {
            if o.irq == irq {
                let pol = o.flags & 3;
                let trig = (o.flags >> 2) & 3;
                return (o.gsi, pol == 3, trig == 3);
            }
        }
    }
    (irq as u32, false, false)
}
