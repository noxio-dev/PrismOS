// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Physical memory manager: a bitmap over every 4 KiB page of RAM.
//
// Only MEM_USABLE regions from the loader memory map are managed.  The
// kernel image is marked used, and so is the bitmap itself.  MEM_LOADER
// regions (boot info, memory map, the stack we are running on) stay
// untouched -- they get reclaimed once the kernel owns its stack and has
// copied the boot tables.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::boot::{BootInfo, MEM_USABLE};

const PAGE: u64 = 4096;
/// Up to 256 GiB of RAM (9.6 MiB of bitmap).
const MAX_PAGES: u64 = 0x6400000;

static mut BITMAP: [u64; (MAX_PAGES / 64) as usize] = [0; (MAX_PAGES / 64) as usize];

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
/// While true (until the VMM switches to its own tables) only pages below
/// 4 GiB are handed out: they are reachable through the loader's identity
/// map, which the table-building code still relies on.
static LOW_ONLY: AtomicBool = AtomicBool::new(true);

pub fn allow_high() {
    LOW_ONLY.store(false, Ordering::Release);
}

/// Free regions carved at init: (first pfn, page count).
static mut REGIONS: [(u64, u64); 64] = [(0, 0); 64];
static REGION_COUNT: AtomicUsize = AtomicUsize::new(0);
static REGION_IDX: AtomicUsize = AtomicUsize::new(0);
static WITHIN: AtomicU64 = AtomicU64::new(0);

// SAFETY note: every bitmap access happens with interrupts masked (boot
// context, or the exception path where IF is clear); no SMP yet.
//
// Implementation note: all accesses go through one raw pointer with
// volatile loads/stores.  Taking `&mut` references into a `static mut`
// from several nested helpers is undefined behaviour (aliasing), and the
// optimizer is free to break such code -- which it did.

#[inline(never)]
fn bm_ptr() -> *mut u64 {
    core::ptr::addr_of_mut!(BITMAP) as *mut u64
}

#[inline(never)]
fn bm_read(word: usize) -> u64 {
    // SAFETY: word < BITMAP.len(); volatile read of our own static.
    unsafe { bm_ptr().add(word).read_volatile() }
}

#[inline(never)]
fn bm_write(word: usize, val: u64) {
    // SAFETY: as above.
    unsafe { bm_ptr().add(word).write_volatile(val) }
}

fn mark_range(pfn: u64, n: u64) {
    for i in 0..n {
        let p = pfn + i;
        let w = (p >> 6) as usize;
        bm_write(w, bm_read(w) | (1u64 << (p & 63)));
    }
}

fn clear(pfn: u64) {
    let w = (pfn >> 6) as usize;
    let bit = 1u64 << (pfn & 63);
    bm_write(w, bm_read(w) & !bit);
}

/// Initialize from the loader memory map.  Returns the final stats.
pub fn init(bi: &BootInfo) -> Stats {
    let mmap = bi.memory_map();

    // Physical extent of the kernel image (loaded at a 2 MiB boundary).
    let kern_base = bi.kernel_phys;
    let kern_top = (bi.kernel_phys + bi.kernel_size + PAGE - 1) & !(PAGE - 1);

    let mut top: u64 = 0;
    let mut base: u64 = u64::MAX;
    for e in mmap.iter().filter(|e| e.kind == MEM_USABLE && e.length >= PAGE) {
        let end = e.base + e.length;
        if end > top {
            top = end;
        }
        if e.base < base {
            base = e.base;
        }
    }
    if base == u64::MAX || top == 0 {
        return stats_snapshot();
    }

    let total_pages = (top / PAGE).min(MAX_PAGES);
    let bitmap_bytes = (total_pages + 7) / 8;

    // Find a home for the bitmap in low usable memory: prefer above the
    // kernel, fall back to the gap between 1 MiB and the kernel start.
    let mut bitmap_phys: u64 = 0;
    for pass in 0..2 {
        for e in mmap.iter().filter(|e| e.kind == MEM_USABLE) {
            let (start, end) = if pass == 0 {
                ((e.base.max(kern_top) + PAGE - 1) & !(PAGE - 1), e.base + e.length)
            } else {
                ((e.base.max(0x100000) + PAGE - 1) & !(PAGE - 1), (e.base + e.length).min(kern_base))
            };
            if end > start && end - start >= bitmap_bytes {
                bitmap_phys = start;
                break;
            }
        }
        if bitmap_phys != 0 {
            break;
        }
    }
    if bitmap_phys == 0 {
        return stats_snapshot();
    }

    // Mark everything used, then open exactly the usable pages.
    mark_range(0, total_pages);
    for e in mmap.iter().filter(|e| e.kind == MEM_USABLE) {
        let s = (e.base + PAGE - 1) & !(PAGE - 1);
        let t = e.base + e.length;
        let mut p = s;
        while p + PAGE <= t && (p / PAGE) < total_pages {
            clear(p / PAGE);
            p += PAGE;
        }
    }
    // Close the kernel image and the bitmap itself.
    mark_range(kern_base / PAGE, (kern_top - kern_base) / PAGE);
    mark_range(bitmap_phys / PAGE, (bitmap_bytes + PAGE - 1) / PAGE);

    // Count what is left.
    let words = ((total_pages + 63) / 64) as usize;
    let mut free = 0u64;
    for i in 0..words {
        free += bm_read(i).count_zeros() as u64;
    }
    if total_pages % 64 != 0 {
        free -= 64 - (total_pages % 64); // trim padding bits of the last word
    }

    // SAFETY: boot context, single CPU, interrupts masked; single plain
    // write of the whole stats block.
    unsafe {
        let sp = core::ptr::addr_of_mut!(STATS);
        sp.write(Stats {
            total_pages,
            free_pages: AtomicU64::new(free),
            base,
            top: base + total_pages * PAGE,
        });
    }

    // Carve the free regions (single linear pass, self-consistent reads).
    // SAFETY: boot context, single CPU.
    unsafe {
        let rp = core::ptr::addr_of_mut!(REGIONS) as *mut (u64, u64);
        let mut n = 0usize;
        let mut run_start: u64 = u64::MAX;
        let mut run_len: u64 = 0;
        let words = ((total_pages as usize) + 63) / 64;
        let mut i = 0usize;
        while i < words {
            let w = bm_read(i);
            let mut b = 0usize;
            while b < 64 {
                let pfn = (i * 64 + b) as u64;
                if pfn >= total_pages {
                    break;
                }
                if w & (1u64 << b) == 0 {
                    if run_len == 0 {
                        run_start = pfn;
                    }
                    run_len += 1;
                } else if run_len > 0 {
                    if n < 64 {
                        rp.add(n).write((run_start, run_len));
                        n += 1;
                    }
                    run_len = 0;
                }
                b += 1;
            }
            i += 1;
        }
        if run_len > 0 && n < 64 {
            rp.add(n).write((run_start, run_len));
            n += 1;
        }
        REGION_COUNT.store(n, Ordering::Relaxed);
    }
    REGION_IDX.store(0, Ordering::Relaxed);
    WITHIN.store(0, Ordering::Relaxed);
    stats_snapshot()
}

/// Take one free physical page (4 KiB), zeroed.  Returns None when the
/// last free page is gone (or the last one reachable in the current
/// boot phase).
pub fn alloc_page() -> Option<u64> {
    // Region-based bump allocation: the free regions were carved once at
    // init (a single-pass scan whose reads are self-consistent), so the
    // hot path never reads the bitmap back.  The bitmap is kept as the
    // allocation map for diagnostics and for free_page().
    // SAFETY: scalar reads of our own statics.
    let total_pages = unsafe { core::ptr::addr_of_mut!(STATS).read().total_pages };
    if total_pages == 0 {
        return None;
    }
    let low_only = LOW_ONLY.load(Ordering::Acquire);

    let mut region = REGION_IDX.load(Ordering::Relaxed);
    while region < REGION_COUNT.load(Ordering::Relaxed) {
        // SAFETY: region < REGION_COUNT <= REGIONS.len().
        let (start, count) = unsafe {
            core::ptr::addr_of_mut!(REGIONS).cast::<(u64, u64)>().add(region).read()
        };
        let used = WITHIN.load(Ordering::Relaxed);
        if used < count {
            let pfn = start + used;
            if pfn as u64 >= total_pages {
                return None;
            }
            if low_only && pfn >= (1 << 32) / PAGE {
                // High page while the direct map is not up yet: skip the
                // whole region.
                region += 1;
                REGION_IDX.store(region, Ordering::Relaxed);
                WITHIN.store(0, Ordering::Relaxed);
                continue;
            }
            WITHIN.store(used + 1, Ordering::Relaxed);
            // Mark the page in the bitmap (write-only as far as the
            // allocator is concerned).
            let w = (pfn >> 6) as usize;
            let bit = 1u64 << (pfn & 63);
            bm_write(w, bm_read(w) | bit);
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            // SAFETY: atomic update on our own static.
            unsafe {
                let sp = core::ptr::addr_of_mut!(STATS);
                (*sp).free_pages.fetch_sub(1, Ordering::Relaxed);
            }
            let phys = pfn * PAGE;
            zero_page(phys);
            return Some(phys);
        }
        region += 1;
        REGION_IDX.store(region, Ordering::Relaxed);
        WITHIN.store(0, Ordering::Relaxed);
    }
    None
}

/// Zero a physical page.  Low memory is identity mapped both by the loader
/// and by our own tables; high memory goes through the direct map.
fn zero_page(phys: u64) {
    let va = if phys < (1 << 32) { phys } else { crate::vmm::DIRECT_MAP_BASE + phys };
    let p = va as *mut u64;
    for w in 0..(PAGE / 8) as usize {
        // SAFETY: freshly allocated page, exclusively ours.
        unsafe { p.add(w).write_volatile(0) };
    }
}

/// Give a page back.
#[allow(dead_code)] // page recycling comes with the slab/vfs work
pub fn free_page(phys: u64) {
    if phys & (PAGE - 1) != 0 {
        return; // not a page: refuse
    }
    // SAFETY: scalar reads of our own static.
    let total_pages = unsafe { core::ptr::addr_of_mut!(STATS).read().total_pages };
    let pfn = phys / PAGE;
    if pfn >= total_pages {
        return;
    }
    clear(pfn);
    // SAFETY: atomic increment of the free counter.
    unsafe {
        let sp = core::ptr::addr_of_mut!(STATS);
        (*sp).free_pages.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn alloc_count() -> u64 {
    ALLOC_COUNT.load(Ordering::Relaxed)
}

pub fn stats_snapshot() -> Stats {
    // SAFETY: read-only snapshot via a raw read.
    unsafe {
        let s = core::ptr::addr_of_mut!(STATS).read();
        Stats {
            total_pages: s.total_pages,
            free_pages: AtomicU64::new(s.free_pages.load(Ordering::Relaxed)),
            base: s.base,
            top: s.top,
        }
    }
}


