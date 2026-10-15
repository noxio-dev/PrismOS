// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Physical memory manager: a bitmap over every 4 KiB page of RAM (1 = used).
//
// Only MEM_USABLE regions from the loader memory map are managed, and never
// anything below 1 MiB: page 0 and the legacy area (IVT, BIOS data, EBDA,
// video memory) stay out of the allocator.  That matters on UEFI, where the
// firmware reports 0..640 KiB as usable; handing out physical page 0 made the
// first table of the VMM (the PML4) live at address 0, which the VMM took for
// "no tables yet" and the heap could never be mapped.
//
// The kernel image and the bitmap itself are marked used.  MEM_LOADER regions
// (boot info, memory map) stay untouched.
//
// The bitmap lives in physical memory below 4 GiB, which is identity mapped
// both by the loader and by the kernel's own tables, so it is reachable at
// every stage of the boot.  (It used to be a 12 MiB static array in .bss,
// which also inflated the kernel image to 14 MiB.)

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::boot::{BootInfo, MEM_USABLE};
use crate::io::IrqGuard;

const PAGE: u64 = 4096;
/// Nothing below this address is ever handed out.
const LOW_RESERVED: u64 = 0x10_0000;
/// Up to 1 TiB of RAM (32 MiB of bitmap).
const MAX_PAGES: u64 = 1 << 28;
const FOUR_GIB: u64 = 1 << 32;

pub struct Stats {
    pub total_pages: u64,
    pub free_pages: AtomicU64,
    pub base: u64, // lowest managed physical address
    pub top: u64,  // highest managed physical address + 1
}

static mut STATS: Stats = Stats {
    total_pages: 0,
    free_pages: AtomicU64::new(0),
    base: 0,
    top: 0,
};
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);

/// Physical address of the bitmap (0 until init succeeds).
static BITMAP_PHYS: AtomicU64 = AtomicU64::new(0);
/// Number of 64-bit words in the bitmap.
static BITMAP_WORDS: AtomicUsize = AtomicUsize::new(0);
/// Word index where the next search starts.
static HINT: AtomicUsize = AtomicUsize::new(0);

/// While true (until the VMM switches to its own tables) only pages below
/// 4 GiB are handed out: they are reachable through the identity map, which
/// the table-building code still relies on.
static LOW_ONLY: AtomicBool = AtomicBool::new(true);

pub fn allow_high() {
    LOW_ONLY.store(false, Ordering::Release);
}

#[inline]
fn bm_ptr() -> *mut u64 {
    BITMAP_PHYS.load(Ordering::Relaxed) as *mut u64
}

#[inline]
fn bm_read(word: usize) -> u64 {
    // SAFETY: word < BITMAP_WORDS; the bitmap is identity mapped RAM.
    unsafe { bm_ptr().add(word).read_volatile() }
}

#[inline]
fn bm_write(word: usize, val: u64) {
    // SAFETY: as above.
    unsafe { bm_ptr().add(word).write_volatile(val) }
}

fn set_used(pfn: u64) {
    let w = (pfn >> 6) as usize;
    bm_write(w, bm_read(w) | (1u64 << (pfn & 63)));
}

fn set_free(pfn: u64) {
    let w = (pfn >> 6) as usize;
    bm_write(w, bm_read(w) & !(1u64 << (pfn & 63)));
}

/// Initialize from the loader memory map.  Returns the final stats.
pub fn init(bi: &BootInfo) -> Stats {
    let mmap = bi.memory_map();

    let kern_base = bi.kernel_phys;
    let kern_top = (bi.kernel_phys + bi.kernel_size + PAGE - 1) & !(PAGE - 1);

    // Extent of the managed range: usable memory at or above 1 MiB.
    let mut top: u64 = 0;
    let mut base: u64 = u64::MAX;
    for e in mmap.iter().filter(|e| e.kind == MEM_USABLE) {
        let s = (e.base + PAGE - 1).max(LOW_RESERVED) & !(PAGE - 1);
        let t = (e.base + e.length) & !(PAGE - 1);
        if t <= s {
            continue;
        }
        top = top.max(t);
        base = base.min(s);
    }
    if base == u64::MAX || top == 0 {
        return stats_snapshot();
    }

    let total_pages = (top / PAGE).min(MAX_PAGES);
    let words = ((total_pages + 63) / 64) as usize;
    let bitmap_bytes = words as u64 * 8;
    let bitmap_pages = (bitmap_bytes + PAGE - 1) / PAGE;

    // Find a home for the bitmap below 4 GiB: first above the kernel, then in
    // the gap between 1 MiB and the kernel.
    let mut bitmap_phys: u64 = 0;
    'search: for pass in 0..2 {
        for e in mmap.iter().filter(|e| e.kind == MEM_USABLE) {
            let (start, end) = if pass == 0 {
                (
                    (e.base.max(kern_top).max(LOW_RESERVED) + PAGE - 1) & !(PAGE - 1),
                    (e.base + e.length).min(FOUR_GIB),
                )
            } else {
                (
                    (e.base.max(LOW_RESERVED) + PAGE - 1) & !(PAGE - 1),
                    (e.base + e.length).min(kern_base).min(FOUR_GIB),
                )
            };
            if end > start && end - start >= bitmap_pages * PAGE {
                bitmap_phys = start;
                break 'search;
            }
        }
    }
    if bitmap_phys == 0 {
        return stats_snapshot();
    }

    BITMAP_PHYS.store(bitmap_phys, Ordering::Relaxed);
    BITMAP_WORDS.store(words, Ordering::Relaxed);

    // Everything used (this also covers the padding bits of the last word),
    // then open exactly the usable pages above 1 MiB.
    for w in 0..words {
        bm_write(w, !0u64);
    }
    for e in mmap.iter().filter(|e| e.kind == MEM_USABLE) {
        let s = (e.base + PAGE - 1).max(LOW_RESERVED) & !(PAGE - 1);
        let t = (e.base + e.length) & !(PAGE - 1);
        let mut p = s;
        while p < t && p / PAGE < total_pages {
            set_free(p / PAGE);
            p += PAGE;
        }
    }
    // Close the kernel image and the bitmap itself.
    for pfn in kern_base / PAGE..kern_top / PAGE {
        if pfn < total_pages {
            set_used(pfn);
        }
    }
    for pfn in bitmap_phys / PAGE..bitmap_phys / PAGE + bitmap_pages {
        set_used(pfn);
    }

    let mut free = 0u64;
    for w in 0..words {
        free += bm_read(w).count_zeros() as u64;
    }

    // SAFETY: boot context, single CPU, interrupts masked; single plain
    // write of the whole stats block.
    unsafe {
        let sp = core::ptr::addr_of_mut!(STATS);
        sp.write(Stats {
            total_pages,
            free_pages: AtomicU64::new(free),
            base,
            top: total_pages * PAGE,
        });
    }
    HINT.store((base / PAGE / 64) as usize, Ordering::Relaxed);
    stats_snapshot()
}

/// Take one free physical page (4 KiB), zeroed.  Returns None when no page
/// is left (or none reachable in the current boot phase).
pub fn alloc_page() -> Option<u64> {
    alloc_page_inner(false)
}

/// Like alloc_page(), but the page is guaranteed to sit below 4 GiB: for DMA
/// engines that only handle 32-bit addresses (and reachable through the
/// identity map).
pub fn alloc_page_low() -> Option<u64> {
    alloc_page_inner(true)
}

fn alloc_page_inner(force_low: bool) -> Option<u64> {
    let words = BITMAP_WORDS.load(Ordering::Relaxed);
    if words == 0 {
        return None;
    }
    let _guard = IrqGuard::new();
    // Before the CR3 switch only the first 4 GiB is reachable.
    let limit = if force_low || LOW_ONLY.load(Ordering::Acquire) {
        words.min((FOUR_GIB / PAGE / 64) as usize)
    } else {
        words
    };
    let start = HINT.load(Ordering::Relaxed).min(limit.saturating_sub(1));
    for i in 0..limit {
        let w = (start + i) % limit;
        let bits = bm_read(w);
        if bits == !0u64 {
            continue;
        }
        let bit = (!bits).trailing_zeros() as u64;
        bm_write(w, bits | (1u64 << bit));
        HINT.store(w, Ordering::Relaxed);
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        // SAFETY: atomic update on our own static.
        unsafe {
            (*core::ptr::addr_of_mut!(STATS)).free_pages.fetch_sub(1, Ordering::Relaxed);
        }
        let phys = (w as u64 * 64 + bit) * PAGE;
        zero_page(phys);
        return Some(phys);
    }
    None
}

/// Zero a physical page.  Low memory is identity mapped both by the loader
/// and by our own tables; high memory goes through the direct map.
fn zero_page(phys: u64) {
    let va = if phys < FOUR_GIB { phys } else { crate::vmm::DIRECT_MAP_BASE + phys };
    let p = va as *mut u64;
    for w in 0..(PAGE / 8) as usize {
        // SAFETY: freshly allocated page, exclusively ours.
        unsafe { p.add(w).write_volatile(0) };
    }
}

/// Give a page back.  Pages the allocator does not own (outside the managed
/// range, or already free) are ignored.
pub fn free_page(phys: u64) {
    if phys & (PAGE - 1) != 0 {
        return;
    }
    let pfn = phys / PAGE;
    // SAFETY: scalar read of our own static.
    let total_pages = unsafe { core::ptr::addr_of!(STATS).read().total_pages };
    if pfn >= total_pages || phys < LOW_RESERVED {
        return;
    }
    let _guard = IrqGuard::new();
    let w = (pfn >> 6) as usize;
    let bit = 1u64 << (pfn & 63);
    let bits = bm_read(w);
    if bits & bit == 0 {
        return; // double free
    }
    bm_write(w, bits & !bit);
    if w < HINT.load(Ordering::Relaxed) {
        HINT.store(w, Ordering::Relaxed);
    }
    // SAFETY: atomic increment of the free counter.
    unsafe {
        (*core::ptr::addr_of_mut!(STATS)).free_pages.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn alloc_count() -> u64 {
    ALLOC_COUNT.load(Ordering::Relaxed)
}

pub fn stats_snapshot() -> Stats {
    // SAFETY: read-only snapshot via a raw read.
    unsafe {
        let s = core::ptr::addr_of!(STATS).read();
        Stats {
            total_pages: s.total_pages,
            free_pages: AtomicU64::new(s.free_pages.load(Ordering::Relaxed)),
            base: s.base,
            top: s.top,
        }
    }
}
