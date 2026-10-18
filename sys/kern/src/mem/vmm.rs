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
//   0x0000_0080_0000_0000 .. 0x0000_4000_0000_0000  user space (PML4 slots
//         1..127, per process -- phase 2):
//         USER_CODE_BASE  first user page (program image)
//         USER_STACK_TOP  initial user RSP, the stack grows down from here
//   0xFFFF_8880_0000_0000 .. +RAM                    direct map (phys -> virt)
//   0xFFFF_FFFF_8002_0000 ..                         kernel image, W^X:
//         .text r-x   .rodata r--(NX)   .data/.bss rw-(NX)
//   0xFFFF_FFFF_9000_0000 .. +16 MiB                 kernel heap (rw-, NX)
//   0xFFFF_FFFF_A000_0000 .. +<=48 MiB               framebuffer back buffer (rw-, NX)
//
// While the tables are being built we still run on the loader's tables, so
// table pages are reached through the identity map; after the switch the
// map_4k/map_2m helpers use the direct map.
//
// `AddressSpace` (bottom of the file) is the per-process half: one PML4 per
// user program, kernel slots shared, user slots owned and freed on drop.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::boot::{BootInfo, FLAG_FB};
use crate::arch::intr::IsrFrame;
use crate::arch::io::{rdmsr, wrmsr};

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

/// First virtual address of user space (PML4 slot 1).
pub const USER_BASE: u64 = 1 << 39;
/// End (exclusive) of user space: PML4 slots 1..127.
pub const USER_END: u64 = 128 << 39;
/// The flat program image is copied here (mapped r-x).
pub const USER_CODE_BASE: u64 = USER_BASE;
/// Initial user RSP; the stack grows down, the page under it is a guard.
pub const USER_STACK_TOP: u64 = USER_END;

// PTE bits
const P: u64 = 1;
const RW: u64 = 2;
const US: u64 = 4;
const PS: u64 = 0x80;
const NX: u64 = 1 << 63;

/// PML4 slots (0..256) the kernel owns in every address space.  Filled once
/// at the end of `init` so user spaces share them and never reuse them.
static mut KERNEL_SLOTS: [u64; 4] = [0; 4];

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
        let t = crate::mem::pmm::alloc_page()?;
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

    let Some(pml4) = crate::mem::pmm::alloc_page() else { return false };

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
                match crate::mem::pmm::alloc_page() {
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
                match crate::mem::pmm::alloc_page() {
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
                match crate::mem::pmm::alloc_page() {
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
        let Some(pdpt) = crate::mem::pmm::alloc_page() else { return false };
        tw_boot(pml4, 0, pdpt | P | RW);
        for gib in 0..4u64 {
            let Some(pd) = crate::mem::pmm::alloc_page() else { return false };
            tw_boot(pdpt, gib as usize, pd | P | RW);
            for i in 0..512usize {
                let phys = (gib << 30) + ((i as u64) << 21);
                tw_boot(pd, i, phys | P | RW | PS);
            }
        }
    }

    // ---- direct map over managed RAM (NX, RW) ----------------------------
    let top = crate::mem::pmm::stats_snapshot().top.max(1 << 32);
    let pdpe1g = crate::arch::cpu::Cpu::detect().has(crate::arch::cpu::Feature::Pdpe1Gb);
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
                match crate::mem::pmm::alloc_page() {
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
                match crate::mem::pmm::alloc_page() {
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
                    match crate::mem::pmm::alloc_page() {
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
                    match crate::mem::pmm::alloc_page() {
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
        crate::arch::regs::write_cr3(pml4);
    }
    // From here on page-table edits go through the direct map; high memory
    // can be handed out by the PMM too.
    crate::mem::pmm::allow_high();
    READY.store(true, Ordering::Release);

    // Remember which lower-half slots the kernel owns (identity map, maybe a
    // framebuffer above 4 GiB) so user address spaces share and never reuse
    // them.
    // SAFETY: read-only walk of the live PML4 through the direct map, at
    // boot with interrupts off.
    unsafe {
        for i in 0..256usize {
            if tr_live(pml4, i) & P != 0 {
                KERNEL_SLOTS[i / 64] |= 1u64 << (i % 64);
            }
        }
    }
    ok
}

/// Map up to `size` bytes of the heap region with 4 KiB pages (called by
/// heap::init).  Stops at the first page the PMM cannot supply and returns
/// the number of bytes actually mapped, contiguous from HEAP_BASE.
pub fn map_heap(size: u64) -> u64 {
    let end = HEAP_BASE + (size.min(HEAP_SIZE) & !(PAGE - 1));
    let mut v = HEAP_BASE;
    while v < end {
        let Some(phys) = crate::mem::pmm::alloc_page() else { break };
        if !map_4k(v, phys, RW | NX) {
            crate::mem::pmm::free_page(phys);
            break;
        }
        v += PAGE;
    }
    v - HEAP_BASE
}

/// Back `size` bytes at `virt` with freshly allocated, zeroed 4 KiB pages
/// (read/write, no-execute).  The PMM is checked up front, with a margin kept
/// for page tables and other users: when it cannot cover the whole region
/// nothing is mapped and 0 is returned.  Returns the number of bytes mapped.
pub fn map_region(virt: u64, size: u64) -> u64 {
    let pages = size.div_ceil(PAGE);
    let free = crate::mem::pmm::stats_snapshot().free_pages.load(Ordering::Relaxed);
    // Keep 1 MiB + the page-table pages we may need for ourselves.
    if !READY.load(Ordering::Acquire) || virt & (PAGE - 1) != 0 || free < pages + 256 + pages / 512 {
        return 0;
    }
    for i in 0..pages {
        let Some(phys) = crate::mem::pmm::alloc_page() else { return 0 };
        if !map_4k(virt + i * PAGE, phys, RW | NX) {
            crate::mem::pmm::free_page(phys);
            return 0;
        }
    }
    pages * PAGE
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

/// Kernel virtual address through which physical address `phys` can be
/// accessed: the identity map below 4 GiB, the direct map above (RAM only).
/// None when the address is not reachable.
pub fn phys_access(phys: u64) -> Option<u64> {
    if phys < (1 << 32) {
        return Some(phys);
    }
    if READY.load(Ordering::Acquire) && phys < crate::mem::pmm::stats_snapshot().top {
        return Some(DIRECT_MAP_BASE + phys);
    }
    None
}

pub fn pages_mapped() -> u64 {
    // SAFETY: read-only counter.
    unsafe { PAGES_MAPPED }
}

// ============================================================================
// User address spaces (phase 2)
// ============================================================================

/// Physical address of the kernel's own PML4 (0 before `init`).
pub fn kernel_cr3() -> u64 {
    // SAFETY: scalar read of a static written once at boot.
    unsafe { PML4 }
}

/// Switch back to the kernel's own page tables.
pub fn activate_kernel() {
    let pml4 = kernel_cr3();
    if pml4 != 0 {
        // SAFETY: the kernel tables map the running code, stacks and heap.
        unsafe { crate::arch::regs::write_cr3(pml4) };
    }
}

fn current_cr3() -> u64 {
    // SAFETY: plain register read.
    crate::arch::regs::read_cr3() & PHYS_MASK
}

/// True when PML4 slot `i` is owned by the kernel (shared, never user-mapped).
fn slot_is_kernel(i: usize) -> bool {
    // SAFETY: bitmap written once at the end of `init`.
    i >= 256 || unsafe { (KERNEL_SLOTS[i / 64] >> (i % 64)) & 1 != 0 }
}

/// Walk one level of a user table, creating the next table with U/S set.
/// Returns None when a huge page blocks the walk or allocation failed.
unsafe fn walk_user(table: u64, idx: usize) -> Option<u64> {
    let e = tr_live(table, idx);
    if e & P == 0 {
        let t = crate::mem::pmm::alloc_page()?;
        tw_live(table, idx, t | P | RW | US);
        Some(t)
    } else if e & PS != 0 {
        None
    } else {
        Some(e & PHYS_MASK)
    }
}

/// Free a page-table subtree and every leaf page below it.  `level` is the
/// level of `table` itself (3 = PDPT, 2 = PD, 1 = PT whose entries are the
/// user's own pages).
unsafe fn free_tree(table: u64, level: u32) {
    for i in 0..512usize {
        let e = tr_live(table, i);
        if e & P == 0 {
            continue;
        }
        let child = e & PHYS_MASK;
        if level == 1 {
            crate::mem::pmm::free_page(child);
        } else if e & PS == 0 {
            free_tree(child, level - 1);
        }
    }
    crate::mem::pmm::free_page(table);
}

/// `len` bytes at `virt` lie entirely inside user space.
fn user_range_ok(virt: u64, len: usize) -> bool {
    len == 0
        || (virt >= USER_BASE
            && virt.checked_add(len as u64).map_or(false, |end| end <= USER_END))
}

/// One process's page tables.  The kernel half is shared with the kernel
/// PML4, the user half is private and freed on drop.
pub struct AddressSpace {
    pml4: u64,
}

impl AddressSpace {
    /// New address space: kernel mappings shared, user half empty.
    pub fn new() -> Option<AddressSpace> {
        let kpml4 = kernel_cr3();
        if kpml4 == 0 {
            return None;
        }
        let pml4 = crate::mem::pmm::alloc_page()?;
        // SAFETY: both tables are reached through the direct map; the new
        // one is exclusively ours.
        unsafe {
            for i in 0..512usize {
                if slot_is_kernel(i) {
                    let e = tr_live(kpml4, i);
                    if e & P != 0 {
                        tw_live(pml4, i, e);
                    }
                }
            }
        }
        Some(AddressSpace { pml4 })
    }

    /// Value to load into CR3.
    #[allow(dead_code)] // the scheduler switches through `activate`
    pub fn cr3(&self) -> u64 {
        self.pml4
    }

    /// Make these the live page tables.
    ///
    /// # Safety
    /// The kernel half must be shared (it always is), so the running code
    /// stays mapped; callers switch CR3 on purpose.
    pub unsafe fn activate(&self) {
        crate::arch::regs::write_cr3(self.pml4);
    }

    /// True when the kernel half (identity map and kernel image) is shared
    /// with the kernel's own PML4.
    pub fn shares_kernel(&self) -> bool {
        let kpml4 = kernel_cr3();
        // SAFETY: read-only walks through the direct map.
        unsafe {
            (0..512usize)
                .filter(|&i| slot_is_kernel(i))
                .all(|i| tr_live(kpml4, i) == tr_live(self.pml4, i))
        }
    }

    /// Map the physical page `phys` at `virt` for user access.  `w`: writable,
    /// `x`: executable.  Fails if `virt` is outside user space, unaligned, in
    /// a kernel-owned slot, already mapped, or out of memory.
    pub fn map_page(&mut self, virt: u64, phys: u64, w: bool, x: bool) -> bool {
        if virt & (PAGE - 1) != 0 || phys & (PAGE - 1) != 0 || !user_range_ok(virt, PAGE as usize) {
            return false;
        }
        let i4 = ((virt >> 39) & 0x1FF) as usize;
        if slot_is_kernel(i4) {
            return false;
        }
        let i3 = ((virt >> 30) & 0x1FF) as usize;
        let i2 = ((virt >> 21) & 0x1FF) as usize;
        let i1 = ((virt >> 12) & 0x1FF) as usize;
        // SAFETY: table walks through the direct map on tables we own.
        unsafe {
            let Some(pdpt) = walk_user(self.pml4, i4) else { return false };
            let Some(pd) = walk_user(pdpt, i3) else { return false };
            let Some(pt) = walk_user(pd, i2) else { return false };
            if tr_live(pt, i1) & P != 0 {
                return false;
            }
            let mut f = P | US;
            if w {
                f |= RW;
            }
            if !x {
                f |= NX;
            }
            tw_live(pt, i1, phys | f);
        }
        true
    }

    /// Allocate zeroed pages covering `[virt, virt + len)` and map them.
    /// The pages belong to the address space and are freed with it.
    pub fn alloc_map(&mut self, virt: u64, len: u64, w: bool, x: bool) -> bool {
        let start = virt & !(PAGE - 1);
        let Some(end) = virt.checked_add(len).and_then(|e| e.checked_add(PAGE - 1)) else {
            return false;
        };
        let end = end & !(PAGE - 1);
        let mut v = start;
        while v < end {
            let Some(phys) = crate::mem::pmm::alloc_page() else { return false };
            if !self.map_page(v, phys, w, x) {
                crate::mem::pmm::free_page(phys);
                return false;
            }
            v += PAGE;
        }
        true
    }

    /// Physical address behind a user byte and whether the page is writable.
    /// None for anything that is not a present user-accessible page.
    pub fn translate(&self, virt: u64) -> Option<(u64, bool)> {
        if !user_range_ok(virt, 1) {
            return None;
        }
        let i4 = ((virt >> 39) & 0x1FF) as usize;
        if slot_is_kernel(i4) {
            return None;
        }
        let i3 = ((virt >> 30) & 0x1FF) as usize;
        let i2 = ((virt >> 21) & 0x1FF) as usize;
        let i1 = ((virt >> 12) & 0x1FF) as usize;
        // SAFETY: read-only walk through the direct map.
        unsafe {
            let e = tr_live(self.pml4, i4);
            if e & P == 0 {
                return None;
            }
            let e = tr_live(e & PHYS_MASK, i3);
            if e & P == 0 || e & PS != 0 {
                return None;
            }
            let e = tr_live(e & PHYS_MASK, i2);
            if e & P == 0 || e & PS != 0 {
                return None;
            }
            let e = tr_live(e & PHYS_MASK, i1);
            if e & P == 0 || e & US == 0 {
                return None;
            }
            Some(((e & PHYS_MASK) | (virt & (PAGE - 1)), e & RW != 0))
        }
    }

    /// Copy user memory into `buf` by walking this address space's tables
    /// (the kernel never dereferences a user pointer directly).
    pub fn read(&self, virt: u64, buf: &mut [u8]) -> bool {
        if !user_range_ok(virt, buf.len()) {
            return false;
        }
        let mut done = 0usize;
        while done < buf.len() {
            let va = virt + done as u64;
            let Some((phys, _)) = self.translate(va) else { return false };
            let Some(k) = phys_access(phys) else { return false };
            let off = (va & (PAGE - 1)) as usize;
            let n = (PAGE as usize - off).min(buf.len() - done);
            // SAFETY: `k` is the kernel alias of a present page and the copy
            // stays inside it.
            unsafe { core::ptr::copy_nonoverlapping(k as *const u8, buf.as_mut_ptr().add(done), n) };
            done += n;
        }
        true
    }

    /// Copy `data` into user memory.  Without `force`, every page must be
    /// user-writable (what a syscall may do for the user); with `force` the
    /// write ignores the user permission bits (what the loader does to fill
    /// a read-only code page).
    pub fn write(&self, virt: u64, data: &[u8], force: bool) -> bool {
        if !user_range_ok(virt, data.len()) {
            return false;
        }
        let mut done = 0usize;
        while done < data.len() {
            let va = virt + done as u64;
            let Some((phys, writable)) = self.translate(va) else { return false };
            if !writable && !force {
                return false;
            }
            let Some(k) = phys_access(phys) else { return false };
            let off = (va & (PAGE - 1)) as usize;
            let n = (PAGE as usize - off).min(data.len() - done);
            // SAFETY: as in `read`.
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr().add(done), k as *mut u8, n) };
            done += n;
        }
        true
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // Never free the tables we are running on.
        if current_cr3() == self.pml4 {
            activate_kernel();
        }
        // SAFETY: the user half is exclusively ours; kernel slots are shared
        // and skipped.
        unsafe {
            for i in 0..256usize {
                if slot_is_kernel(i) {
                    continue;
                }
                let e = tr_live(self.pml4, i);
                if e & P != 0 {
                    free_tree(e & PHYS_MASK, 3);
                }
            }
        }
        crate::mem::pmm::free_page(self.pml4);
    }
}

/// #PF handler: the `crash page` smoke test faults on CRASH_TEST_ADDR and
/// is recovered by mapping a zeroed page; everything else is fatal.
pub fn handle_page_fault(frame: &mut IsrFrame) {
    let cr2 = crate::arch::regs::read_cr2();
    let err = frame.errcode;

    if cr2 == CRASH_TEST_ADDR && err & 1 == 0 {
        // The deliberate not-present fault: map a fresh zero page RW+NX and
        // return -- the faulting instruction re-executes and reads zero.
        if let Some(phys) = crate::mem::pmm::alloc_page() {
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
    crate::arch::intr::park();
}
