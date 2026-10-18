// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Global descriptor table.
//
// Selector map (compatible with the loader's GDT, so the cached CS stays
// valid across the swap):
//   0x08  kernel code (64-bit)
//   0x10  kernel data
//   0x18  user data (64-bit)
//   0x20  user code (64-bit, for ring 3)
//   0x28  TSS (see tss.rs)
//
// The user segments are ordered for `sysretq`: SYSRET loads
// CS = STAR[63:48]+16 and SS = STAR[63:48]+8 (STAR[63:48] = 0x10), so the
// user code selector must sit 8 above the user data selector.

use super::tss;
use core::arch::asm;
use core::ptr::addr_of_mut;

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
#[allow(dead_code)] // ring 3 later
pub const USER_DS: u16 = 0x18;
#[allow(dead_code)] // ring 3 later
pub const USER_CS: u16 = 0x20;
pub const TSS_SEL: u16 = 0x28;

// The GDT is an array of 8-byte words: code/data descriptors take one word
// each, the TSS descriptor takes two consecutive words (selector 0x28 spans
// words 5 and 6).  Total 8 words, GDTR limit = 63.

const GDT_WORDS: usize = 8;

/// LGDT/LIDT pseudo-descriptor (64-bit mode): u16 limit + u64 base packed,
/// base at byte offset 2.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub(super) struct PseudoDesc {
    pub(super) limit: u16,
    pub(super) base: u64,
}

/// Classic flat 64-bit code/data descriptor: base 0, limit 0xFFFFF,
/// 4 KiB granularity -- one 8-byte word:
///   bytes 0-1  limit[15:0]     bytes 2-4  base[23:0] (zero)
///   byte  5    access          byte  6    flags<<4 | limit[19:16]
///   byte  7    base[31:24]     (zero)
const fn flat(access: u8, flags: u8) -> u64 {
    0xFFFFu64 // limit[15:0]
        | ((access as u64) << 40)
        | (0xFu64 << 48) // limit[19:16]
        | ((flags as u64) << 52)
}

#[repr(C, align(16))]
struct Gdt {
    words: [u64; GDT_WORDS],
    gdtr: PseudoDesc,
}

static mut GDT: Gdt = Gdt {
    words: [
        0,               // 0x00 null
        flat(0x9A, 0xA), // 0x08 kernel code (L=1)
        flat(0x92, 0xC), // 0x10 kernel data
        flat(0xF2, 0xC), // 0x18 user data (sysretq: SS = STAR[63:48]+8)
        flat(0xFA, 0xA), // 0x20 user code (sysretq: CS = STAR[63:48]+16)
        0,               // 0x28 TSS low (patched at init)
        0,               //      TSS high
        0,               // padding
    ],
    gdtr: PseudoDesc { limit: (GDT_WORDS * 8 - 1) as u16, base: 0 },
};

/// Install the GDT/TSS.  Selector values match the loader's, so the live CS
/// descriptor stays valid; only the data registers are reloaded, plus LTR.
pub fn init() {
    let (tss_addr, tss_limit) = tss::init();
    // SAFETY: single-CPU boot context, no concurrency yet.
    let g = unsafe { &mut *addr_of_mut!(GDT) };
    g.words[5] = tss::desc_low(tss_addr, tss_limit);
    g.words[6] = tss::desc_high(tss_addr);
    g.gdtr.base = g.words.as_ptr() as u64;

    // SAFETY: the tables live in the kernel image (the PMM never hands
    // those pages out) and are mapped before and after the CR3 switch.
    unsafe {
        asm!("lgdt [{}]", in(reg) &g.gdtr as *const PseudoDesc, options(preserves_flags));
        asm!(
            "mov ax, {ds}\nmov ds, ax\nmov es, ax\nmov ss, ax\nmov gs, ax\nmov fs, ax",
            ds = const KERNEL_DS,
            out("ax") _,
        );
        asm!("ltr ax", in("ax") TSS_SEL, options(preserves_flags));
    }
}
