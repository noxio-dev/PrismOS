// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Virtual memory manager: builds the kernel's own page tables, switches to
// them, and provides map/translate helpers plus the direct map.
//
// Final layout (long mode, 4 KiB / 2 MiB / 1 GiB pages, NX enabled):
//
//   0x0000_0000_0000_0000 .. 0x0000_0100_0000_0000  identity, first 4 GiB
//         (boot info, memory map, VGA text, MMIO, legacy)
//   0x5000_0000_0000_0000                           crash-test page (on demand)
//   0xFFFF_8880_0000_0000 .. +RAM                    direct map (phys -> virt)
//   0xFFFF_FFFF_8002_0000 ..                         kernel image, W^X:
//         .text r-x   .rodata r--(NX)   .data/.bss rw-(NX)
//   0xFFFF_FFFF_9000_0000 .. +16 MiB                 kernel heap (rw-, NX)
//
// While the tables are being built we still run on the loader's tables, so
// table pages are reached through the identity map; after the switch the
// map_4k/map_2m helpers use the direct map.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::boot::{BootInfo, FLAG_FB};
use crate::int::IsrFrame;

pub const PAGE: u64 = 4096;
pub const PAGE_2M: u64 = 2 * 1024 * 1024;
pub const PAGE_1G: u64 = 1024 * 1024 * 1024;
pub const KERNBASE: u64 = 0xFFFF_FFFF_8000_0000;
pub const DIRECT_MAP_BASE: u64 = 0xFFFF_8880_0000_0000;
pub const HEAP_BASE: u64 = KERNBASE + 0x1000_0000; // +256 MiB above KERNBASE
pub const HEAP_SIZE: u64 = 16 * 1024 * 1024;

/// Address the `crash page` command faults on; recovered by the #PF handler.
/// Canonical, unmapped, far from anything we build.
pub const CRASH_TEST_ADDR: u64 = 0x0000_5000_0000_0000;

// PTE bits
const P: u64 = 1;
const RW: u64 = 2;
const PS: u64 = 0x80;
const NX: u64 = 1 << 63;

const PHYS_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const PHYS_MASK_2M: u64 = 0x000F_FFFF_FFE0_0000;
const PHYS_MASK_1G: u64 = 0x0000_FFFF_C000_0000;

/// Physical address of our PML4 (0 until the switch).
static mut PML4: u64 = 0;
static READY: AtomicBool = AtomicBool::new(false);
static mut PAGES_MAPPED: u64 = 0;

// Linker symbols describing the kernel image layout (linker.ld).
unsafe extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __bss_end: u8;
}

unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi);
    ((hi as u64) << 32) | lo as u64
}

unsafe fn wrmsr(msr: u32, val: u64) {
    asm!("wrmsr", in("ecx") msr, in("eax") (val & 0xFFFF_FFFF) as u32, in("edx") (val >> 32) as u32);
}

// ------------------------------------------------------ table access helpers
// Two access modes: identity (before the CR3 switch -- the loader maps the
// first 4 GiB 1:1) and direct map (after the switch, works everywhere).

#[inline]
unsafe fn tw_boot(table: u64, idx: usize, val: u64) {
    ((table + (idx as u64) * 8) as *mut u64).write_volatile(val);
}

#[inline]
unsafe fn tr_boot(table: u64, idx: usize) -> u64 {
    ((table + (idx as u64) * 8) as *const u64).read_volatile()
}

#[inline]
unsafe fn tw_live(table: u64, idx: usize, val: u64) {
    ((DIRECT_MAP_BASE + table + (idx as u64) * 8) as *mut u64).write_volatile(val);
}

#[inline]
unsafe fn tr_live(table: u64, idx: usize) -> u64 {
    ((DIRECT_MAP_BASE + table + (idx as u64) * 8) as *const u64).read_volatile()
}

/// Walk one level using the live (direct map) tables.  Allocates the next
/// table when missing.  Returns the next table's physical address, or None
/// when a huge page blocks the walk or allocation failed.
unsafe fn walk_live(table: u64, idx: usize) -> Option<u64> {
    let e = tr_live(table, idx);
    if e & P == 0 {
        let t = crate::pmm::alloc_page()?;
        tw_live(table, idx, t | P | RW);
        Some(t)
    } else {
        if e & PS != 0 {
            return None;
        }
        Some(e & PHYS_MASK)
    }
}

/// Map one physical page at `virt` with `flags` in the live tables.
pub fn map_4k(virt: u64, phys: u64, flags: u64) -> bool {
    if virt & (PAGE - 1) != 0 || phys & (PAGE - 1) != 0 {
        return false;
    }
    // SAFETY: table accesses go through the direct map; single CPU with
    // interrupts masked at every call site.
    unsafe {
        let pml4 = PML4;
        if pml4 == 0 {
            return false;
        }
        let i4 = ((virt >> 39) & 0x1FF) as usize;
        let i3 = ((virt >> 30) & 0x1FF) as usize;
        let i2 = ((virt >> 21) & 0x1FF) as usize;
        let i1 = ((virt >> 12) & 0x1FF) as usize;

        let Some(pdpt) = walk_live(pml4, i4) else { return false };
        let Some(pd) = walk_live(pdpt, i3) else { return false };
        let Some(pt) = walk_live(pd, i2) else { return false };
        if tr_live(pt, i1) & P != 0 {
            return false; // already mapped
        }
        tw_live(pt, i1, phys | P | flags);
        PAGES_MAPPED += 1;
        true
    }
}

/// Map a 2 MiB huge page at `virt` in the live tables.
#[allow(dead_code)] // 2 MiB mapping arrives with the slab allocator
pub fn map_2m(virt: u64, phys: u64, flags: u64) -> bool {
    if virt & (PAGE_2M - 1) != 0 || phys & (PAGE_2M - 1) != 0 {
        return false;
    }
    // SAFETY: as map_4k.
    unsafe {
        let pml4 = PML4;
        if pml4 == 0 {
            return false;
        }
        let i4 = ((virt >> 39) & 0x1FF) as usize;
        let i3 = ((virt >> 30) & 0x1FF) as usize;
        let i2 = ((virt >> 21) & 0x1FF) as usize;

        let Some(pdpt) = walk_live(pml4, i4) else { return false };
        let Some(pd) = walk_live(pdpt, i3) else { return false };
        if tr_live(pd, i2) & P != 0 {
            return false;
        }
        tw_live(pd, i2, phys | P | RW | PS | flags);
        PAGES_MAPPED += 512;
        true
    }
}

/// Physical address currently mapped at `virt`, if any.
pub fn translate(virt: u64) -> Option<u64> {
    // SAFETY: read-only walk of the live tables.
    unsafe {
        let pml4 = PML4;
        if pml4 == 0 {
            return None;
        }
        let i4 = ((virt >> 39) & 0x1FF) as usize;
        let i3 = ((virt >> 30) & 0x1FF) as usize;
        let i2 = ((virt >> 21) & 0x1FF) as usize;
        let i1 = ((virt >> 12) & 0x1FF) as usize;

        let e = tr_live(pml4, i4);
        if e & P == 0 {
            return None;
        }
        let e = tr_live(e & PHYS_MASK, i3);
        if e & P == 0 {
            return None;
        }
        if e & PS != 0 {
            return Some((e & PHYS_MASK_1G) | (virt & (PAGE_1G - 1)));
        }
        let e = tr_live(e & PHYS_MASK, i2);
        if e & P == 0 {
            return None;
        }
        if e & PS != 0 {
            return Some((e & PHYS_MASK_2M) | (virt & (PAGE_2M - 1)));
        }
        let e = tr_live(e & PHYS_MASK, i1);
        if e & P == 0 {
            return None;
        }
        Some((e & PHYS_MASK) | (virt & (PAGE - 1)))
    }
}

/// Build the kernel page tables and switch CR3.
pub fn init(bi: &BootInfo) -> bool {
    // NX first: a PTE with bit 63 set faults as "reserved bit" when NXE
    // is off, so the bit must be live before any NX entry exists.
    // SAFETY: MSR access in boot context.
    unsafe {
        let efer = rdmsr(0xC000_0080);
        wrmsr(0xC000_0080, efer | (1 << 11));
    }

    let Some(pml4) = crate::pmm::alloc_page() else { return false };

    // ---- kernel image, W^X, from the linker symbols ---------------------
    let kv = bi.kernel_virt;
    let kp = bi.kernel_phys;
    // Linker symbols live in our own image; reading their addresses is safe.
    let (text_s, text_e) = (
        core::ptr::addr_of!(__text_start) as u64,
        core::ptr::addr_of!(__text_end) as u64,
    );
    let (ro_s, ro_e) = (
        core::ptr::addr_of!(__rodata_start) as u64,
        core::ptr::addr_of!(__rodata_end) as u64,
    );
    let (data_s, data_e) = (
        core::ptr::addr_of!(__data_start) as u64,
        core::ptr::addr_of!(__bss_end) as u64,
    );

    let mut ok = true;
    // SAFETY: boot-time table building through the identity map.
    unsafe fn map_boot(root: u64, from: u64, to: u64, kv: u64, kp: u64, flags: u64) -> bool {
        let mut all_ok = true;
        let mut v = from & !(PAGE - 1);
        while v < to {
            let phys = v - kv + kp;
            let i4 = ((v >> 39) & 0x1FF) as usize;
            let i3 = ((v >> 30) & 0x1FF) as usize;
            let i2 = ((v >> 21) & 0x1FF) as usize;
            let i1 = ((v >> 12) & 0x1FF) as usize;

            let e = tr_boot(root, i4);
            let pdpt = if e & P == 0 {
                match crate::pmm::alloc_page() {
                    Some(t) => {
                        tw_boot(root, i4, t | P | RW);
                        t
                    }
                    None => {
                        all_ok = false;
                        break;
                    }
                }
            } else {
                e & PHYS_MASK
            };
            let e = tr_boot(pdpt, i3);
            let pd = if e & P == 0 {
                match crate::pmm::alloc_page() {
                    Some(t) => {
                        tw_boot(pdpt, i3, t | P | RW);
                        t
                    }
                    None => {
                        all_ok = false;
                        break;
                    }
                }
            } else {
                e & PHYS_MASK
            };
            let e = tr_boot(pd, i2);
            let pt = if e & P == 0 {
                match crate::pmm::alloc_page() {
                    Some(t) => {
                        tw_boot(pd, i2, t | P | RW);
                        t
                    }
                    None => {
                        all_ok = false;
                        break;
                    }
                }
            } else {
                e & PHYS_MASK
            };
            if tr_boot(pt, i1) & P == 0 {
                tw_boot(pt, i1, phys | P | flags);
                PAGES_MAPPED += 1;
            }
            v += PAGE;
        }
        all_ok
    }

    // .text: present, executable, read-only.
    // SAFETY: boot-time table building through the identity map.
    unsafe {
        ok &= map_boot(pml4, text_s, text_e, kv, kp, 0);
        // .rodata: present, no execute.
        ok &= map_boot(pml4, ro_s, ro_e, kv, kp, NX);
        // .data + .bss: present, writable, no execute.
        ok &= map_boot(pml4, data_s, data_e, kv, kp, RW | NX);
    }

    // ---- identity map: first 4 GiB ---------------------------------------
    // PML4[0] -> one PDPT -> one PD per GiB -> 512 x 2 MiB leaves.
    // (A PDPT entry with PS set would be a 1 GiB page: never put 2 MiB
    // addresses there, the reserved bits fault.)
    // SAFETY: entries written into freshly allocated tables.
    unsafe {
        let Some(pdpt) = crate::pmm::alloc_page() else { return false };
        tw_boot(pml4, 0, pdpt | P | RW);
        for gib in 0..4u64 {
            let Some(pd) = crate::pmm::alloc_page() else { return false };
            tw_boot(pdpt, gib as usize, pd | P | RW);
            for i in 0..512usize {
                let phys = (gib << 30) + ((i as u64) << 21);
                tw_boot(pd, i, phys | P | RW | PS);
            }
        }
    }

    // ---- direct map over managed RAM (NX, RW) ----------------------------
    let top = crate::pmm::stats_snapshot().top.max(1 << 32);
    let pdpe1g = crate::cpu::Cpu::detect().has(crate::cpu::Feature::Pdpe1Gb);
    // SAFETY: as above.
    unsafe {
        let mut covered: u64 = 0;
        while covered < top {
            let va = DIRECT_MAP_BASE + covered;
            let i4 = ((va >> 39) & 0x1FF) as usize;
            let i3 = ((va >> 30) & 0x1FF) as usize;
            let i2 = ((va >> 21) & 0x1FF) as usize;

            let e = tr_boot(pml4, i4);
            let pdpt = if e & P == 0 {
                match crate::pmm::alloc_page() {
                    Some(t) => {
                        tw_boot(pml4, i4, t | P | RW);
                        t
                    }
                    None => return false,
                }
            } else {
                e & PHYS_MASK
            };

            if pdpe1g && top - covered >= PAGE_1G {
                if tr_boot(pdpt, i3) & P == 0 {
                    tw_boot(pdpt, i3, covered | P | RW | PS | NX);
                    covered += PAGE_1G;
                    continue;
                }
            }

            let e = tr_boot(pdpt, i3);
            let pd = if e & P == 0 {
                match crate::pmm::alloc_page() {
                    Some(t) => {
                        tw_boot(pdpt, i3, t | P | RW);
                        t
                    }
                    None => return false,
                }
            } else {
                e & PHYS_MASK
            };
            if tr_boot(pd, i2) & P == 0 {
                tw_boot(pd, i2, covered | P | RW | PS | NX);
            }
            covered += PAGE_2M;
        }
    }

    // ---- identity mapping for a framebuffer above 4 GiB -------------------
    if bi.flags & FLAG_FB != 0 && bi.fb.addr >= (1 << 32) && bi.fb.addr != 0 {
        let fb_end = bi.fb.addr + (bi.fb.pitch as u64) * (bi.fb.height as u64);
        let mut v = bi.fb.addr & !(PAGE_2M - 1);
        // SAFETY: as above.
        unsafe {
            while v < fb_end {
                let i4 = ((v >> 39) & 0x1FF) as usize;
                let i3 = ((v >> 30) & 0x1FF) as usize;
                let i2 = ((v >> 21) & 0x1FF) as usize;
                let e = tr_boot(pml4, i4);
                let pdpt = if e & P == 0 {
                    match crate::pmm::alloc_page() {
                        Some(t) => {
                            tw_boot(pml4, i4, t | P | RW);
                            t
                        }
                        None => break,
                    }
                } else {
                    e & PHYS_MASK
                };
                let e = tr_boot(pdpt, i3);
                let pd = if e & P == 0 {
                    match crate::pmm::alloc_page() {
                        Some(t) => {
                            tw_boot(pdpt, i3, t | P | RW);
                            t
                        }
                        None => break,
                    }
                } else {
                    e & PHYS_MASK
                };
                if tr_boot(pd, i2) & P == 0 {
                    tw_boot(pd, i2, (v & PHYS_MASK_2M) | P | RW | PS);
                }
                v += PAGE_2M;
            }
        }
    }

    // ---- switch ------------------------------------------------------------
    // SAFETY: the new tables map the running kernel identically (same leaf
    // entries for the image), so the next instruction fetch resolves fine.
    unsafe {
        PML4 = pml4;
        asm!("mov cr3, {}", in(reg) pml4, options(preserves_flags));
    }
    // From here on page-table edits go through the direct map; high memory
    // can be handed out by the PMM too.
    crate::pmm::allow_high();
    READY.store(true, Ordering::Release);
    ok
}

/// Map up to `size` bytes of the heap region with 4 KiB pages (called by
/// heap::init).  Stops at the first page the PMM cannot supply and returns
/// the number of bytes actually mapped, contiguous from HEAP_BASE.
pub fn map_heap(size: u64) -> u64 {
    let end = HEAP_BASE + (size.min(HEAP_SIZE) & !(PAGE - 1));
    let mut v = HEAP_BASE;
    while v < end {
        let Some(phys) = crate::pmm::alloc_page() else { break };
        if !map_4k(v, phys, RW | NX) {
            crate::pmm::free_page(phys);
            break;
        }
        v += PAGE;
    }
    v - HEAP_BASE
}

#[allow(dead_code)]
pub fn direct_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Virtual address of a physical page through the direct map.
#[allow(dead_code)]
pub fn phys_to_virt(phys: u64) -> u64 {
    DIRECT_MAP_BASE + phys
}

pub fn pages_mapped() -> u64 {
    // SAFETY: read-only counter.
    unsafe { PAGES_MAPPED }
}

/// #PF handler: the `crash page` smoke test faults on CRASH_TEST_ADDR and
/// is recovered by mapping a zeroed page; everything else is fatal.
pub fn handle_page_fault(frame: &mut IsrFrame) {
    // SAFETY: plain register read.
    let cr2: u64;
    unsafe {
        asm!("mov {}, cr2", out(reg) cr2, options(nomem, preserves_flags));
    }
    let err = frame.errcode;

    if cr2 == CRASH_TEST_ADDR && err & 1 == 0 {
        // The deliberate not-present fault: map a fresh zero page RW+NX and
        // return -- the faulting instruction re-executes and reads zero.
        if let Some(phys) = crate::pmm::alloc_page() {
            if map_4k(cr2 & !(PAGE - 1), phys, RW | NX) {
                crate::kprintln!(
                    "\npage fault #PF at {:#x} (cr2 {:#x}): page mapped, continuing",
                    frame.rip,
                    cr2
                );
                return;
            }
        }
    }

    let why = match () {
        _ if err & 1 == 0 => "not present",
        _ if err & 2 != 0 => "write to read-only",
        _ if err & 4 != 0 => "user access",
        _ if err & 16 != 0 => "instruction fetch",
        _ => "reserved bit",
    };
    crate::kprintln!(
        "\npage fault #PF at {:#x}, cr2 {:#x} ({}), errcode {:#x}",
        frame.rip,
        cr2,
        why,
        err
    );
    match translate(cr2) {
        Some(p) => crate::kprintln!("cr2 maps to physical {:#x}", p),
        None => crate::kprintln!("cr2 is unmapped"),
    }
    crate::kprintln!(
        "rax {:#018x}  rbx {:#018x}  rcx {:#018x}  rdx {:#018x}\n\
         rsi {:#018x}  rdi {:#018x}  rbp {:#018x}  rsp {:#018x}",
        frame.rax, frame.rbx, frame.rcx, frame.rdx, frame.rsi, frame.rdi, frame.rbp, frame.rsp
    );
    crate::int::park();
}
