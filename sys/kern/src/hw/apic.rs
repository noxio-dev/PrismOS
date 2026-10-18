// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Local APIC + I/O APIC.
//
//  * LAPIC: enabled through IA32_APIC_BASE and the spurious-vector register;
//    works in xAPIC (MMIO) or x2APIC (MSR) mode, whichever the firmware left.
//  * IOAPIC: every redirection entry starts masked; ISA IRQs are routed on
//    demand to vector 0x20 + irq (the same vectors the 8259 used, so the IRQ
//    stubs and handler registry in int.rs are unchanged), honouring the MADT
//    interrupt source overrides.
//  * Timer: the LAPIC timer is calibrated against the HPET (or PIT channel 2
//    when there is none) and then runs periodically at pit::HZ on vector
//    0x30; it replaces the PIT as the system tick.
//
// After init() the 8259 is fully masked and EOIs go to the LAPIC.

#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::arch::io::{rdmsr, wrmsr};
use crate::hw::acpi;

pub const TIMER_VECTOR: u8 = 0x30;
pub const SPURIOUS_VECTOR: u8 = 0xFF;

const MSR_APIC_BASE: u32 = 0x1B;
const BASE_ENABLE: u64 = 1 << 11;
const BASE_X2APIC: u64 = 1 << 10;

const REG_ID: u32 = 0x20;
const REG_VERSION: u32 = 0x30;
const REG_TPR: u32 = 0x80;
const REG_EOI: u32 = 0xB0;
const REG_SVR: u32 = 0xF0;
const REG_ESR: u32 = 0x280;
const REG_LVT_TIMER: u32 = 0x320;
const REG_LVT_LINT0: u32 = 0x350;
const REG_LVT_LINT1: u32 = 0x360;
const REG_LVT_ERROR: u32 = 0x370;
const REG_TIMER_INIT: u32 = 0x380;
const REG_TIMER_CUR: u32 = 0x390;
const REG_TIMER_DIV: u32 = 0x3E0;

const LVT_MASKED: u32 = 1 << 16;
const LVT_PERIODIC: u32 = 1 << 17;

static ENABLED: AtomicBool = AtomicBool::new(false);
static X2APIC: AtomicBool = AtomicBool::new(false);
static LAPIC_VA: AtomicU64 = AtomicU64::new(0);
static LAPIC_PHYS: AtomicU64 = AtomicU64::new(0);
static TIMER_PER_MS: AtomicU64 = AtomicU64::new(0);
static TIMER_INITIAL: AtomicU32 = AtomicU32::new(0);
static CAL_SOURCE: AtomicU32 = AtomicU32::new(0); // 1 = HPET, 2 = PIT

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

pub fn is_x2apic() -> bool {
    X2APIC.load(Ordering::Relaxed)
}

pub fn lapic_phys() -> u64 {
    LAPIC_PHYS.load(Ordering::Relaxed)
}

/// LAPIC timer counts per millisecond (at divider 16), 0 before calibration.
pub fn timer_per_ms() -> u64 {
    TIMER_PER_MS.load(Ordering::Relaxed)
}

pub fn calibration_source() -> &'static str {
    match CAL_SOURCE.load(Ordering::Relaxed) {
        1 => "HPET",
        2 => "PIT ch2",
        _ => "none",
    }
}

// ------------------------------------------------------------------ LAPIC

fn lread(reg: u32) -> u32 {
    // SAFETY: the LAPIC window / MSR range was validated in init().
    unsafe {
        if X2APIC.load(Ordering::Relaxed) {
            rdmsr(0x800 + (reg >> 4)) as u32
        } else {
            ((LAPIC_VA.load(Ordering::Relaxed) + reg as u64) as *const u32).read_volatile()
        }
    }
}

fn lwrite(reg: u32, val: u32) {
    // SAFETY: as above.
    unsafe {
        if X2APIC.load(Ordering::Relaxed) {
            wrmsr(0x800 + (reg >> 4), val as u64);
        } else {
            ((LAPIC_VA.load(Ordering::Relaxed) + reg as u64) as *mut u32).write_volatile(val);
        }
    }
}

/// APIC ID of the running CPU.
pub fn lapic_id() -> u32 {
    if !enabled() {
        return 0;
    }
    let id = lread(REG_ID);
    if is_x2apic() { id } else { id >> 24 }
}

pub fn lapic_version() -> u32 {
    if enabled() { lread(REG_VERSION) & 0xFF } else { 0 }
}

pub fn lapic_eoi() {
    lwrite(REG_EOI, 0);
}

/// End of interrupt for ISA line `irq`: LAPIC when the APIC is in charge,
/// 8259 otherwise.
pub fn eoi(irq: u8) {
    if enabled() {
        lapic_eoi();
    } else {
        crate::hw::pic::eoi(irq);
    }
}

// ----------------------------------------------------------------- IOAPIC

struct IoApic {
    va: u64,
    gsi_base: u32,
    entries: u32,
    id: u8,
}

const NO_IOAPIC: IoApic = IoApic { va: 0, gsi_base: 0, entries: 0, id: 0 };
static mut IOAPICS: [IoApic; acpi::MAX_IOAPICS] = [NO_IOAPIC; acpi::MAX_IOAPICS];
static mut N_IOAPICS: usize = 0;

fn ioapics() -> &'static [IoApic] {
    // SAFETY: written once in init(), read-only afterwards.
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(IOAPICS) as *const IoApic, N_IOAPICS) }
}

fn io_read(io: &IoApic, reg: u32) -> u32 {
    // SAFETY: MMIO window validated in init().
    unsafe {
        (io.va as *mut u32).write_volatile(reg);
        ((io.va + 0x10) as *const u32).read_volatile()
    }
}

fn io_write(io: &IoApic, reg: u32, val: u32) {
    // SAFETY: as above.
    unsafe {
        (io.va as *mut u32).write_volatile(reg);
        ((io.va + 0x10) as *mut u32).write_volatile(val);
    }
}

fn io_for_gsi(gsi: u32) -> Option<&'static IoApic> {
    ioapics().iter().find(|io| gsi >= io.gsi_base && gsi < io.gsi_base + io.entries)
}

pub fn n_ioapics() -> usize {
    ioapics().len()
}

/// (id, physical address, first GSI, entries) of IOAPIC `i`.
pub fn ioapic_info(i: usize) -> Option<(u8, u64, u32, u32)> {
    let io = ioapics().get(i)?;
    Some((io.id, io.va, io.gsi_base, io.entries))
}

/// Route ISA `irq` to vector 0x20 + irq on this CPU and unmask it.
pub fn unmask(irq: u8) {
    set_isa_mask(irq, false);
}

/// Mask ISA `irq` at the IOAPIC.
pub fn mask(irq: u8) {
    set_isa_mask(irq, true);
}

fn set_isa_mask(irq: u8, masked: bool) {
    let (gsi, low, level) = acpi::isa_route(irq);
    let Some(io) = io_for_gsi(gsi) else { return };
    let n = gsi - io.gsi_base;
    let mut lo = 0x20 + irq as u32; // fixed delivery, physical destination
    if low {
        lo |= 1 << 13;
    }
    if level {
        lo |= 1 << 15;
    }
    if masked {
        lo |= LVT_MASKED;
    }
    let _g = crate::arch::io::IrqGuard::new();
    io_write(io, 0x10 + 2 * n + 1, lapic_id() << 24);
    io_write(io, 0x10 + 2 * n, lo);
}

// ------------------------------------------------------------------ timer

/// Wait `ms` using the HPET main counter.  False if it does not tick.
fn hpet_wait_ms(a: &acpi::Acpi, ms: u64) -> bool {
    let Some(va) = crate::mem::vmm::phys_access(a.hpet_addr) else { return false };
    let base = va as *mut u8;
    // SAFETY: HPET register block, 1 KiB of MMIO.
    unsafe {
        let cap = (base as *const u64).read_volatile();
        let period_fs = cap >> 32;
        if period_fs == 0 || period_fs > 100_000_000 {
            return false;
        }
        let wide = cap & (1 << 13) != 0;
        let mask: u64 = if wide { u64::MAX } else { 0xFFFF_FFFF };
        let cfg = base.add(0x10) as *mut u64;
        cfg.write_volatile(cfg.read_volatile() | 1); // ENABLE_CNF
        let cnt = base.add(0xF0) as *const u64;
        let ticks = ms * 1_000_000_000_000 / period_fs;
        let start = cnt.read_volatile();
        let mut spins = 0u64;
        while cnt.read_volatile().wrapping_sub(start) & mask < ticks {
            spins += 1;
            if spins > 100_000_000 {
                return false;
            }
        }
        true
    }
}

fn calibrate(ms: u32) -> Option<u64> {
    lwrite(REG_TIMER_DIV, 0x3); // divide by 16
    lwrite(REG_LVT_TIMER, LVT_MASKED | TIMER_VECTOR as u32); // one-shot, masked
    let mut src = 0;
    lwrite(REG_TIMER_INIT, 0xFFFF_FFFF);
    let mut ok = false;
    if let Some(a) = acpi::info() {
        if a.hpet && hpet_wait_ms(a, ms as u64) {
            ok = true;
            src = 1;
        }
    }
    if !ok && crate::hw::pit::wait_ms_ch2(ms) {
        ok = true;
        src = 2;
    }
    let cur = lread(REG_TIMER_CUR);
    lwrite(REG_TIMER_INIT, 0);
    if !ok {
        return None;
    }
    let elapsed = 0xFFFF_FFFFu64 - cur as u64;
    let per_ms = elapsed / ms as u64;
    if per_ms < 100 {
        return None;
    }
    CAL_SOURCE.store(src, Ordering::Relaxed);
    Some(per_ms)
}

/// Periodic tick: this handler is the new PIT.
pub fn on_timer(from_user: bool) {
    lapic_eoi();
    crate::hw::pit::on_tick(from_user);
}

// ------------------------------------------------------------------- init

/// Bring up the LAPIC and IOAPICs, calibrate and start the timer, then mask
/// the 8259.  Returns false (leaving the 8259 setup untouched) when the
/// machine has no usable APIC or ACPI MADT.
pub fn init() -> bool {
    use crate::arch::cpu::{Cpu, Feature};
    let Some(a) = acpi::info() else { return false };
    if a.n_ioapics == 0 || !Cpu::detect().has(Feature::Apic) {
        return false;
    }

    // SAFETY: APIC base MSR access in boot context.
    let base = unsafe { rdmsr(MSR_APIC_BASE) };
    let phys = if a.lapic_addr != 0 { a.lapic_addr } else { base & 0xFFFF_FFFF_FFFF_F000 };
    let x2 = base & BASE_X2APIC != 0;
    if !x2 {
        let Some(va) = crate::mem::vmm::phys_access(phys) else { return false };
        LAPIC_VA.store(va, Ordering::Relaxed);
        // Make sure the hardware enable bit is set at the address we use.
        // SAFETY: as above.
        unsafe { wrmsr(MSR_APIC_BASE, (base & 0xFFF) | (phys & !0xFFF) | BASE_ENABLE) };
    } else if base & BASE_ENABLE == 0 {
        return false;
    }
    X2APIC.store(x2, Ordering::Relaxed);
    LAPIC_PHYS.store(phys, Ordering::Relaxed);
    ENABLED.store(true, Ordering::Release);

    lwrite(REG_TPR, 0);
    lwrite(REG_SVR, 0x100 | SPURIOUS_VECTOR as u32);
    lwrite(REG_LVT_LINT0, LVT_MASKED); // the 8259 is going away
    lwrite(REG_LVT_LINT1, 0x400); // NMI
    lwrite(REG_LVT_ERROR, 0xFE);
    if !x2 {
        lwrite(REG_ESR, 0);
        lwrite(REG_ESR, 0);
    }

    // IOAPICs: map, size, mask every entry.
    for e in &a.ioapics[..a.n_ioapics] {
        let Some(va) = crate::mem::vmm::phys_access(e.addr as u64) else { continue };
        let io = IoApic { va, gsi_base: e.gsi_base, entries: 0, id: e.id };
        let entries = ((io_read(&io, 1) >> 16) & 0xFF) + 1;
        let io = IoApic { entries, ..io };
        for n in 0..entries {
            io_write(&io, 0x10 + 2 * n, LVT_MASKED);
            io_write(&io, 0x10 + 2 * n + 1, 0);
        }
        // SAFETY: boot context, single CPU.
        unsafe {
            let n = N_IOAPICS;
            (*core::ptr::addr_of_mut!(IOAPICS))[n] = io;
            N_IOAPICS = n + 1;
        }
    }
    if n_ioapics() == 0 {
        ENABLED.store(false, Ordering::Release);
        return false;
    }

    // Timer.
    let Some(per_ms) = calibrate(10) else {
        ENABLED.store(false, Ordering::Release);
        return false;
    };
    TIMER_PER_MS.store(per_ms, Ordering::Relaxed);
    let initial = (per_ms * 1000 / crate::hw::pit::HZ) as u32;
    TIMER_INITIAL.store(initial, Ordering::Relaxed);

    // Point of no return: silence the 8259 and the PIT heartbeat, then let
    // the LAPIC timer tick.
    let _g = crate::arch::io::IrqGuard::new();
    // SAFETY: mask both 8259s completely.
    unsafe {
        crate::arch::io::outb(0x21, 0xFF);
        crate::arch::io::outb(0xA1, 0xFF);
    }
    crate::hw::pit::stop();
    lwrite(REG_TIMER_DIV, 0x3);
    lwrite(REG_LVT_TIMER, LVT_PERIODIC | TIMER_VECTOR as u32);
    lwrite(REG_TIMER_INIT, initial);
    true
}
