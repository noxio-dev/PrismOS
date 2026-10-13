// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Kernel heap: a first-fit free-list allocator over the region mapped by
// vmm::map_heap().  Blocks carry a header with a magic + size; free blocks
// merge with their neighbours.  All entry points mask interrupts so the
// heap can also be used from exception context later.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::io::IrqGuard;
use crate::vmm;

const MAGIC_USED: u64 = 0x50524953_4D555345; // "PRISMUSED"
const MAGIC_FREE: u64 = 0x50524953_46524545; // "PRISMFREE"
const ALIGN: usize = 16;
const MIN_BLOCK: usize = 32; // header (16) + minimal payload (16)

#[repr(C)]
struct Header {
    magic: u64,
    size: usize, // payload size, multiple of ALIGN
}

struct Heap {
    free: *mut Header, // head of the free list (most recently freed)
    initialized: bool,
}

static mut HEAP: Heap = Heap { free: core::ptr::null_mut(), initialized: false };
static STAT_USED: AtomicU64 = AtomicU64::new(0);
static STAT_FREE: AtomicU64 = AtomicU64::new(0);
static STAT_ALLOCS: AtomicU64 = AtomicU64::new(0);
static STAT_FREES: AtomicU64 = AtomicU64::new(0);

/// Map the heap region and seed it with one big free block.
pub fn init() -> bool {
    if !vmm::map_heap() {
        return false;
    }
    let size = vmm::HEAP_SIZE as usize;
    let hdr = vmm::HEAP_BASE as *mut Header;
    // SAFETY: the freshly mapped heap region is exclusively ours.
    unsafe {
        hdr.write_volatile(Header { magic: MAGIC_FREE, size: size - core::mem::size_of::<Header>() });
        let heap = &mut *addr_of_mut!(HEAP);
        heap.free = hdr;
        heap.initialized = true;
    }
    STAT_FREE.store(vmm::HEAP_SIZE, Ordering::Relaxed);
    true
}

/// The free list threads the next pointer through the first payload word.
#[inline]
unsafe fn next_of(block: *mut Header) -> *mut Header {
    let p = (block as *mut u8).add(core::mem::size_of::<Header>()) as *mut *mut Header;
    p.read_volatile()
}

#[inline]
unsafe fn set_next(block: *mut Header, next: *mut Header) {
    let p = (block as *mut u8).add(core::mem::size_of::<Header>()) as *mut *mut Header;
    p.write_volatile(next);
}

/// Split a free block if the remainder can stand alone.
unsafe fn split(block: *mut Header, need: usize) {
    let total = (*block).size;
    if total >= need + MIN_BLOCK {
        let rest = (block as *mut u8).add(core::mem::size_of::<Header>() + need) as *mut Header;
        (*rest).magic = MAGIC_FREE;
        (*rest).size = total - need - core::mem::size_of::<Header>();
        (*block).size = need;
        // Push the remainder as its own free block.
        push_free(rest);
    }
}

unsafe fn push_free(block: *mut Header) {
    let heap = &mut *addr_of_mut!(HEAP);
    // Simple singly-linked free list threaded through the block payload.
    set_next(block, heap.free);
    heap.free = block;
}

#[allow(dead_code)]
unsafe fn pop_free() -> Option<*mut Header> {
    let heap = &mut *addr_of_mut!(HEAP);
    let head = heap.free;
    if head.is_null() {
        return None;
    }
    // The next pointer is the first payload word.
    heap.free = next_of(head);
    Some(head)
}

/// Try to merge `block` back with adjacent free blocks: because we always
/// merge on free and the region is contiguous, checking the whole list is
/// the simple correct approach for now (the heap is small).
unsafe fn coalesce() {
    // Rebuild the free list by address, merging neighbours.
    let heap = &mut *addr_of_mut!(HEAP);
    let mut blocks: [*mut Header; 4096] = [core::ptr::null_mut(); 4096];
    let mut n = 0usize;
    let mut cur = heap.free;
    while !cur.is_null() && n < blocks.len() {
        blocks[n] = cur;
        n += 1;
        cur = next_of(cur);
    }
    if n == 0 {
        return;
    }
    // Sort by address (insertion sort; n is small).
    for i in 1..n {
        let mut j = i;
        while j > 0 && blocks[j - 1] > blocks[j] {
            blocks.swap(j - 1, j);
            j -= 1;
        }
    }
    // Merge.
    for i in 0..n {
        if blocks[i].is_null() {
            continue;
        }
        let mut end =
            (blocks[i] as usize) + core::mem::size_of::<Header>() + (*blocks[i]).size;
        let mut k = i + 1;
        while k < n {
            let start = blocks[k] as usize;
            if start == end {
                // Neighbour: absorb.
                (*blocks[i]).size += core::mem::size_of::<Header>() + (*blocks[k]).size;
                end = (blocks[i] as usize) + core::mem::size_of::<Header>() + (*blocks[i]).size;
                blocks[k] = core::ptr::null_mut();
            }
            k += 1;
        }
    }
    // Relink.
    let mut head: *mut Header = core::ptr::null_mut();
    for i in (0..n).rev() {
        if !blocks[i].is_null() {
            let next = head;
            set_next(blocks[i], next);
            head = blocks[i];
        }
    }
    heap.free = head;
}

/// Allocate `size` bytes (rounded up to 16).  Returns None when out of
/// memory.
pub fn kalloc(size: usize) -> Option<*mut u8> {
    let _guard = IrqGuard::new();
    // SAFETY: heap state is guarded above; single CPU.
    unsafe {
        let heap = &mut *addr_of_mut!(HEAP);
        if !heap.initialized {
            return None;
        }
        let need = (size + ALIGN - 1) & !(ALIGN - 1);
        // First fit over the free list.
        let mut prev: *mut *mut Header = core::ptr::null_mut();
        let mut cur = heap.free;
        while !cur.is_null() {
            if (*cur).magic != MAGIC_FREE {
                // Corruption: refuse to continue.
                return None;
            }
            if (*cur).size >= need {
                // Unlink from the list.
                let next = next_of(cur);
                if prev.is_null() {
                    heap.free = next;
                } else {
                    prev.write_volatile(next);
                }
                split(cur, need);
                (*cur).magic = MAGIC_USED;
                STAT_USED.fetch_add((*cur).size as u64 + 16, Ordering::Relaxed);
                STAT_FREE.fetch_sub((*cur).size as u64 + 16, Ordering::Relaxed);
                STAT_ALLOCS.fetch_add(1, Ordering::Relaxed);
                return Some((cur as *mut u8).add(core::mem::size_of::<Header>()));
            }
            prev = (cur as *mut u8).add(core::mem::size_of::<Header>()) as *mut *mut Header;
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
    // SAFETY: freshly allocated block of `total` bytes.
    unsafe {
        for i in 0..total {
            (p.add(i) as *mut u8).write_volatile(0);
        }
    }
    Some(p)
}

/// Free an allocation returned by kalloc/kcalloc.
pub fn kfree(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let _guard = IrqGuard::new();
    // SAFETY: heap state guarded; the pointer came from kalloc.
    unsafe {
        let heap = &mut *addr_of_mut!(HEAP);
        if !heap.initialized {
            return;
        }
        let hdr = ptr.sub(core::mem::size_of::<Header>()) as *mut Header;
        if (*hdr).magic != MAGIC_USED {
            return; // not ours: silently ignore
        }
        (*hdr).magic = MAGIC_FREE;
        STAT_USED.fetch_sub((*hdr).size as u64 + 16, Ordering::Relaxed);
        STAT_FREE.fetch_add((*hdr).size as u64 + 16, Ordering::Relaxed);
        STAT_FREES.fetch_add(1, Ordering::Relaxed);
        push_free(hdr);
        coalesce();
    }
}

pub struct Stats {
    pub used: u64,
    pub free: u64,
    pub allocs: u64,
    pub frees: u64,
}

pub fn stats() -> Stats {
    Stats {
        used: STAT_USED.load(Ordering::Relaxed),
        free: STAT_FREE.load(Ordering::Relaxed),
        allocs: STAT_ALLOCS.load(Ordering::Relaxed),
        frees: STAT_FREES.load(Ordering::Relaxed),
    }
}

/// Self-test used by the `meminfo` command: allocate, write, free, and
/// re-allocate to prove the allocator round-trips.
pub fn self_test() -> bool {
    let a = match kalloc(64) {
        Some(p) => p,
        None => return false,
    };
    // SAFETY: our own fresh allocation.
    unsafe {
        a.write_volatile(0xAA);
        a.add(63).write_volatile(0x55);
        if a.read_volatile() != 0xAA || a.add(63).read_volatile() != 0x55 {
            kfree(a);
            return false;
        }
    }
    kfree(a);
    let b = kalloc(64);
    let ok = b == Some(a); // first-fit + coalesce should hand it back
    if let Some(p) = b {
        kfree(p);
    }
    ok
}
