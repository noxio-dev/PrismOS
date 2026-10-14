// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Kernel heap: a first-fit allocator over the region mapped by
// vmm::map_heap().  Free blocks form an address-ordered singly linked list,
// so freeing merges a block with both neighbours in one pass and the heap
// always collapses back to a single block once everything is freed.  All
// entry points mask interrupts, so the heap is usable from IRQ context.
//
// init() sizes the heap from the memory actually available (a quarter of
// the free pages, at most vmm::HEAP_SIZE) and keeps whatever it managed to
// map, instead of failing outright on a small or fragmented machine.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::io::IrqGuard;
use crate::vmm;

const MAGIC_USED: u64 = 0x50524953_4D555345; // "PRISMUSE"
const MAGIC_FREE: u64 = 0x50524953_46524545; // "PRISFREE"
const ALIGN: usize = 16;
const HDR: usize = core::mem::size_of::<Header>(); // 16
const MIN_BLOCK: usize = HDR + ALIGN; // header + minimal payload
/// Smallest heap worth having.
const MIN_HEAP: u64 = 256 * 1024;

#[repr(C)]
struct Header {
    magic: u64,
    size: usize, // payload size, multiple of ALIGN
}

struct Heap {
    free: *mut Header, // head of the free list, ordered by address
    base: usize,
    size: usize,
    initialized: bool,
}

static mut HEAP: Heap = Heap { free: core::ptr::null_mut(), base: 0, size: 0, initialized: false };
static STAT_USED: AtomicU64 = AtomicU64::new(0);
static STAT_FREE: AtomicU64 = AtomicU64::new(0);
static STAT_ALLOCS: AtomicU64 = AtomicU64::new(0);
static STAT_FREES: AtomicU64 = AtomicU64::new(0);

/// Map the heap region and seed it with one big free block.  Returns false
/// only when not even MIN_HEAP bytes could be mapped.
pub fn init() -> bool {
    let free_bytes = crate::pmm::stats_snapshot().free_pages.load(Ordering::Relaxed) * 4096;
    let want = (free_bytes / 4).clamp(MIN_HEAP, vmm::HEAP_SIZE);
    let size = vmm::map_heap(want);
    if size < MIN_HEAP {
        return false;
    }
    let hdr = vmm::HEAP_BASE as *mut Header;
    // SAFETY: the freshly mapped heap region is exclusively ours.
    unsafe {
        hdr.write(Header { magic: MAGIC_FREE, size: size as usize - HDR });
        set_next(hdr, core::ptr::null_mut());
        let heap = &mut *addr_of_mut!(HEAP);
        heap.free = hdr;
        heap.base = vmm::HEAP_BASE as usize;
        heap.size = size as usize;
        heap.initialized = true;
    }
    STAT_FREE.store(size, Ordering::Relaxed);
    true
}

/// Size of the mapped heap in bytes (0 before init).
pub fn size() -> u64 {
    // SAFETY: scalar read.
    unsafe { (*addr_of_mut!(HEAP)).size as u64 }
}

/// The free list threads the next pointer through the first payload word.
#[inline]
unsafe fn next_of(block: *mut Header) -> *mut Header {
    ((block as *mut u8).add(HDR) as *mut *mut Header).read()
}

#[inline]
unsafe fn set_next(block: *mut Header, next: *mut Header) {
    ((block as *mut u8).add(HDR) as *mut *mut Header).write(next);
}

#[inline]
unsafe fn block_end(block: *mut Header) -> usize {
    block as usize + HDR + (*block).size
}

/// Allocate `size` bytes (rounded up to 16).  Returns None when out of
/// memory.
pub fn kalloc(size: usize) -> Option<*mut u8> {
    let _guard = IrqGuard::new();
    // SAFETY: heap state is guarded above; single CPU.
    unsafe {
        let heap = &mut *addr_of_mut!(HEAP);
        if !heap.initialized || size == 0 || size > heap.size {
            return None;
        }
        let need = (size + ALIGN - 1) & !(ALIGN - 1);
        let mut prev: *mut Header = core::ptr::null_mut();
        let mut cur = heap.free;
        while !cur.is_null() {
            if (*cur).magic != MAGIC_FREE {
                return None; // free list corrupted: refuse to continue
            }
            if (*cur).size >= need {
                let next = next_of(cur);
                let replacement = if (*cur).size >= need + MIN_BLOCK {
                    // Split: the tail stays on the list in this block's place.
                    let rest = (cur as *mut u8).add(HDR + need) as *mut Header;
                    rest.write(Header { magic: MAGIC_FREE, size: (*cur).size - need - HDR });
                    set_next(rest, next);
                    (*cur).size = need;
                    rest
                } else {
                    next
                };
                if prev.is_null() {
                    heap.free = replacement;
                } else {
                    set_next(prev, replacement);
                }
                (*cur).magic = MAGIC_USED;
                STAT_USED.fetch_add(((*cur).size + HDR) as u64, Ordering::Relaxed);
                STAT_FREE.fetch_sub(((*cur).size + HDR) as u64, Ordering::Relaxed);
                STAT_ALLOCS.fetch_add(1, Ordering::Relaxed);
                return Some((cur as *mut u8).add(HDR));
            }
            prev = cur;
            cur = next_of(cur);
        }
    }
    None
}

/// Zeroed allocation.
#[allow(dead_code)]
pub fn kcalloc(n: usize, size: usize) -> Option<*mut u8> {
    let total = n.checked_mul(size)?;
    let p = kalloc(total)?;
    // SAFETY: freshly allocated block of at least `total` bytes.
    unsafe { core::ptr::write_bytes(p, 0, total) };
    Some(p)
}

/// Free an allocation returned by kalloc/kcalloc.  Pointers that are not
/// live allocations (foreign, already freed) are ignored.
pub fn kfree(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let _guard = IrqGuard::new();
    // SAFETY: heap state guarded; the pointer is validated below.
    unsafe {
        let heap = &mut *addr_of_mut!(HEAP);
        if !heap.initialized {
            return;
        }
        let a = ptr as usize;
        if a < heap.base + HDR || a >= heap.base + heap.size || a % ALIGN != 0 {
            return;
        }
        let hdr = ptr.sub(HDR) as *mut Header;
        if (*hdr).magic != MAGIC_USED {
            return;
        }
        (*hdr).magic = MAGIC_FREE;
        STAT_USED.fetch_sub(((*hdr).size + HDR) as u64, Ordering::Relaxed);
        STAT_FREE.fetch_add(((*hdr).size + HDR) as u64, Ordering::Relaxed);
        STAT_FREES.fetch_add(1, Ordering::Relaxed);

        // Find the neighbours in address order.
        let mut prev: *mut Header = core::ptr::null_mut();
        let mut next = heap.free;
        while !next.is_null() && (next as usize) < (hdr as usize) {
            prev = next;
            next = next_of(next);
        }
        // Merge forward, then backward.
        if !next.is_null() && block_end(hdr) == next as usize {
            (*hdr).size += HDR + (*next).size;
            next = next_of(next);
        }
        set_next(hdr, next);
        if !prev.is_null() && block_end(prev) == hdr as usize {
            (*prev).size += HDR + (*hdr).size;
            set_next(prev, next);
        } else if prev.is_null() {
            heap.free = hdr;
        } else {
            set_next(prev, hdr);
        }
    }
}

pub struct Stats {
    pub used: u64,
    pub free: u64,
    pub allocs: u64,
    pub frees: u64,
    /// Blocks on the free list (1 = fully coalesced).
    pub free_blocks: u64,
}

pub fn stats() -> Stats {
    let _guard = IrqGuard::new();
    let mut blocks = 0;
    // SAFETY: guarded read-only walk of the free list.
    unsafe {
        let mut cur = (*addr_of_mut!(HEAP)).free;
        while !cur.is_null() && blocks < 1 << 20 {
            blocks += 1;
            cur = next_of(cur);
        }
    }
    Stats {
        used: STAT_USED.load(Ordering::Relaxed),
        free: STAT_FREE.load(Ordering::Relaxed),
        allocs: STAT_ALLOCS.load(Ordering::Relaxed),
        frees: STAT_FREES.load(Ordering::Relaxed),
        free_blocks: blocks,
    }
}

/// Self-test used by the `meminfo` command: data integrity, hand-back of
/// the same block, and full coalescing after frees in scrambled order.
pub fn self_test() -> bool {
    let blocks_before = stats().free_blocks;
    let Some(a) = kalloc(64) else { return false };
    // SAFETY: our own fresh allocation.
    unsafe {
        a.write(0xAA);
        a.add(63).write(0x55);
        if a.read() != 0xAA || a.add(63).read() != 0x55 {
            kfree(a);
            return false;
        }
    }
    kfree(a);
    let Some(b) = kalloc(64) else { return false };
    let same = a == b;
    kfree(b);

    let mut p = [core::ptr::null_mut::<u8>(); 8];
    for (i, slot) in p.iter_mut().enumerate() {
        match kalloc(24 + i * 40) {
            Some(x) => *slot = x,
            None => return false,
        }
    }
    for i in [3, 0, 7, 1, 6, 2, 5, 4] {
        kfree(p[i]);
    }
    same && stats().free_blocks == blocks_before
}
