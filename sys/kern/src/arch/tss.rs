// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Task state segment: the 64-bit hardware TSS plus the dedicated double-fault
// stack (IST1).  Only RSP0 (for a future ring 3) and IST1 are used.  The GDT
// descriptor pointing at the TSS is built here and installed by gdt.rs.

use core::ptr::addr_of_mut;

const DF_STACK_SIZE: usize = 8192;

/// Hardware task state (64-bit layout, 100 bytes packed).
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Tss {
    reserved1: u32, // 0x00
    rsp0: u64,      // 0x04
    rsp1: u64,      // 0x0C
    rsp2: u64,      // 0x14
    reserved2: u64, // 0x1C
    ist: [u64; 7],  // 0x24
    reserved3: u32, // 0x5C
    reserved4: u16, // 0x60
    iopb: u16,      // 0x62
}

/// IST1: dedicated #DF stack.
#[repr(C, align(16))]
struct DfStack([u8; DF_STACK_SIZE]);

static mut TSS: Tss = Tss {
    reserved1: 0,
    rsp0: 0,
    rsp1: 0,
    rsp2: 0,
    reserved2: 0,
    ist: [0; 7],
    reserved3: 0,
    reserved4: 0,
    iopb: 100, // >= TSS limit: no I/O permission bitmap
};

static mut DF_STACK: DfStack = DfStack([0; DF_STACK_SIZE]);

/// 16-byte system-segment descriptor pointing at the TSS -> two GDT words:
///   word 0: limit[15:0] | base[23:0]<<16 | access(0x89)<<40 | (flags+limit[19:16])<<48 | base[31:24]<<56
///   word 1: base[63:32]
pub(super) const fn desc_low(base: u64, limit: u32) -> u64 {
    (limit & 0xFFFF) as u64
        | ((base & 0xFF_FFFF) << 16)
        | (0x89u64 << 40) // present, 64-bit TSS available
        | (((limit >> 16) & 0xF) as u64) << 48
        | (((base >> 24) & 0xFF) as u64) << 56
}

pub(super) const fn desc_high(base: u64) -> u64 {
    (base >> 32) & 0xFFFF_FFFF
}

/// Fill in the stack pointers and return the TSS (address, limit) for its
/// GDT descriptor.
pub(super) fn init() -> (u64, u32) {
    // SAFETY: single-CPU boot context, no concurrency yet.
    let (tss, stack) = unsafe { (&mut *addr_of_mut!(TSS), &*addr_of_mut!(DF_STACK)) };
    tss.rsp0 = 0; // no ring 3 yet
    tss.ist[0] = stack.0.as_ptr() as u64 + DF_STACK_SIZE as u64;
    (tss as *const Tss as u64, (core::mem::size_of::<Tss>() - 1) as u32)
}
