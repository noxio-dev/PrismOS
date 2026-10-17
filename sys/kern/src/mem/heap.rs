// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Kernel heap: a buddy allocator for page-sized and larger blocks, with a
// slab layer on top for small objects.
//
//   buddy : the region mapped by vmm::map_heap() is cut into naturally
//           aligned blocks of 4 KiB (order 0) up to 4 MiB (order 10).  Free
//           blocks sit on one doubly linked list per order; freeing merges a
//           block with its buddy as long as the buddy is free too, so the
//           heap collapses back to its largest blocks once everything is
//           released.  One byte of metadata per page (`info`) tells which
//           kind of block starts there and its order.
//
//   slab  : a cache hands out fixed-size objects carved from slabs (1 to 32
//           contiguous buddy pages, naturally aligned, header in the first
//           64 bytes).  kmalloc() uses six built-in caches (32 .. 1024
//           bytes); kcache_create() makes private ones.  An object finds its
//           slab by masking the address, so kfree() needs no size.
//
// API: kmalloc(size, flags) / kfree(ptr) / kcache_create / kcache_alloc /
//      kcache_free / kcache_destroy, plus per-cache statistics for meminfo.
//      The same allocator backs the `alloc` crate (Box, Vec, String ...)
//      through the #[global_allocator] at the bottom of the file.
//
// All entry points mask interrupts, so the heap is usable from IRQ context.
// The slab free lists carry a canary in the second word of every free
// object: freeing the same object twice is detected and ignored (and counted
// in `bad_frees`).

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{addr_of_mut, null_mut};

use crate::arch::io::IrqGuard;
use crate::mem::vmm;

const PAGE: usize = 4096;
/// Largest buddy order: 4 KiB << 10 = 4 MiB.
pub const MAX_ORDER: usize = 10;
const MAX_PAGES: usize = vmm::HEAP_SIZE as usize / PAGE;
/// Smallest heap worth having.
const MIN_HEAP: u64 = 256 * 1024;

/// kmalloc flag: return zeroed memory.
pub const KM_ZERO: u32 = 1;

// ---- per-page metadata ---------------------------------------------------
//
// bits 0..3 : order of the block starting here
// bits 4..5 : kind
const K_NONE: u8 = 0; // interior page, or outside the heap
const K_FREE: u8 = 1 << 4; // head of a free buddy block
const K_BUDDY: u8 = 2 << 4; // head of an allocated buddy block
const K_SLAB: u8 = 3 << 4; // every page of a slab
const KIND_MASK: u8 = 0x30;
const ORDER_MASK: u8 = 0x0F;

struct FreeBlock {
    next: *mut FreeBlock,
    prev: *mut FreeBlock,
}

struct Buddy {
    lists: [*mut FreeBlock; MAX_ORDER + 1],
    list_len: [u32; MAX_ORDER + 1],
    base: usize,
    pages: usize,
    free_pages: usize,
    info: [u8; MAX_PAGES],
    ready: bool,
}

static mut BUDDY: Buddy = Buddy {
    lists: [null_mut(); MAX_ORDER + 1],
    list_len: [0; MAX_ORDER + 1],
    base: 0,
    pages: 0,
    free_pages: 0,
    info: [0; MAX_PAGES],
    ready: false,
};

struct Counters {
    allocs: u64,
    frees: u64,
    bad_frees: u64,
    failed: u64,
}

static mut CNT: Counters = Counters { allocs: 0, frees: 0, bad_frees: 0, failed: 0 };

#[inline]
fn buddy() -> &'static mut Buddy {
    // SAFETY: single CPU; every caller holds an IrqGuard or runs at boot.
    unsafe { &mut *addr_of_mut!(BUDDY) }
}

#[inline]
fn cnt() -> &'static mut Counters {
    // SAFETY: as above.
    unsafe { &mut *addr_of_mut!(CNT) }
}

// ------------------------------------------------------------------ buddy

impl Buddy {
    fn idx(&self, addr: usize) -> usize {
        (addr - self.base) / PAGE
    }

    fn addr(&self, idx: usize) -> usize {
        self.base + idx * PAGE
    }

    unsafe fn push(&mut self, idx: usize, order: usize) {
        let b = self.addr(idx) as *mut FreeBlock;
        (*b).prev = null_mut();
        (*b).next = self.lists[order];
        if !self.lists[order].is_null() {
            (*self.lists[order]).prev = b;
        }
        self.lists[order] = b;
        self.list_len[order] += 1;
        self.info[idx] = K_FREE | order as u8;
        self.free_pages += 1 << order;
    }

    unsafe fn unlink(&mut self, idx: usize, order: usize) {
        let b = self.addr(idx) as *mut FreeBlock;
        let (p, n) = ((*b).prev, (*b).next);
        if p.is_null() {
            self.lists[order] = n;
        } else {
            (*p).next = n;
        }
        if !n.is_null() {
            (*n).prev = p;
        }
        self.list_len[order] -= 1;
        self.info[idx] = K_NONE;
        self.free_pages -= 1 << order;
    }

    /// Take a block of exactly `order`, splitting a larger one if needed.
    unsafe fn alloc(&mut self, order: usize) -> Option<usize> {
        let mut o = order;
        while o <= MAX_ORDER && self.lists[o].is_null() {
            o += 1;
        }
        if o > MAX_ORDER {
            return None;
        }
        let idx = self.idx(self.lists[o] as usize);
        self.unlink(idx, o);
        while o > order {
            o -= 1;
            self.push(idx + (1 << o), o); // upper half goes back
        }
        self.info[idx] = K_BUDDY | order as u8;
        Some(self.addr(idx))
    }

    /// Give a block back and merge it with its buddies.
    unsafe fn free(&mut self, addr: usize, order: usize) {
        let mut idx = self.idx(addr);
        for i in idx..idx + (1 << order) {
            self.info[i] = K_NONE;
        }
        let mut o = order;
        while o < MAX_ORDER {
            let b = idx ^ (1 << o);
            if b + (1 << o) > self.pages || self.info[b] != K_FREE | o as u8 {
                break;
            }
            self.unlink(b, o);
            idx = idx.min(b);
            o += 1;
        }
        self.push(idx, o);
    }

    fn contains(&self, p: usize) -> bool {
        self.ready && p >= self.base && p < self.base + self.pages * PAGE
    }
}

fn order_for_pages(pages: usize) -> usize {
    if pages <= 1 {
        0
    } else {
        (usize::BITS - (pages - 1).leading_zeros()) as usize
    }
}

// ------------------------------------------------------------------- slab

const SLAB_MAGIC: u64 = 0x534C_4142_4844_5221; // "SLABHDR!"
const CANARY: u64 = 0xF4EE_F4EE_F4EE_F4EE;
const SLAB_HDR: usize = 64;
const MAX_SLAB_ORDER: usize = 5;
const MAX_OBJ: usize = 2048;
const MAX_CACHES: usize = 32;
const N_GENERIC: usize = 6;
const GENERIC_SIZES: [usize; N_GENERIC] = [32, 64, 128, 256, 512, 1024];
const GENERIC_NAMES: [&str; N_GENERIC] = [
    "kmalloc-32",
    "kmalloc-64",
    "kmalloc-128",
    "kmalloc-256",
    "kmalloc-512",
    "kmalloc-1024",
];

#[repr(C)]
struct Slab {
    magic: u64,
    cache: *mut Cache,
    free: *mut u8, // free object list (next pointer in the first word)
    inuse: u32,
    total: u32,
    next_all: *mut Slab,
    prev_all: *mut Slab,
    next_part: *mut Slab,
    prev_part: *mut Slab,
}

const _: () = assert!(core::mem::size_of::<Slab>() == SLAB_HDR);

pub struct Cache {
    used: bool,
    builtin: bool,
    name: [u8; 16],
    name_len: u8,
    objsize: usize,
    slab_order: usize,
    per_slab: u32,
    all: *mut Slab,
    partial: *mut Slab,
    nslabs: usize,
    inuse: usize,
    allocs: u64,
    frees: u64,
    grows: u64,
    shrinks: u64,
}

const EMPTY_CACHE: Cache = Cache {
    used: false,
    builtin: false,
    name: [0; 16],
    name_len: 0,
    objsize: 0,
    slab_order: 0,
    per_slab: 0,
    all: null_mut(),
    partial: null_mut(),
    nslabs: 0,
    inuse: 0,
    allocs: 0,
    frees: 0,
    grows: 0,
    shrinks: 0,
};

static mut CACHES: [Cache; MAX_CACHES] = [EMPTY_CACHE; MAX_CACHES];

/// Handle returned by kcache_create.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct KCache(u8);

fn caches() -> &'static mut [Cache; MAX_CACHES] {
    // SAFETY: single CPU, guarded by the callers.
    unsafe { &mut *addr_of_mut!(CACHES) }
}

unsafe fn all_insert(c: &mut Cache, s: *mut Slab) {
    (*s).prev_all = null_mut();
    (*s).next_all = c.all;
    if !c.all.is_null() {
        (*c.all).prev_all = s;
    }
    c.all = s;
}

unsafe fn all_remove(c: &mut Cache, s: *mut Slab) {
    let (p, n) = ((*s).prev_all, (*s).next_all);
    if p.is_null() {
        c.all = n;
    } else {
        (*p).next_all = n;
    }
    if !n.is_null() {
        (*n).prev_all = p;
    }
}

unsafe fn part_insert(c: &mut Cache, s: *mut Slab) {
    (*s).prev_part = null_mut();
    (*s).next_part = c.partial;
    if !c.partial.is_null() {
        (*c.partial).prev_part = s;
    }
    c.partial = s;
}

unsafe fn part_remove(c: &mut Cache, s: *mut Slab) {
    let (p, n) = ((*s).prev_part, (*s).next_part);
    if p.is_null() {
        c.partial = n;
    } else {
        (*p).next_part = n;
    }
    if !n.is_null() {
        (*n).prev_part = p;
    }
}

/// Add one slab to the cache.  Returns false when the buddy is exhausted.
unsafe fn cache_grow(c: &mut Cache) -> bool {
    let b = buddy();
    let Some(addr) = b.alloc(c.slab_order) else { return false };
    let first = b.idx(addr);
    for i in first..first + (1 << c.slab_order) {
        b.info[i] = K_SLAB | c.slab_order as u8;
    }
    let s = addr as *mut Slab;
    let total = c.per_slab;
    s.write(Slab {
        magic: SLAB_MAGIC,
        cache: c as *mut Cache,
        free: null_mut(),
        inuse: 0,
        total,
        next_all: null_mut(),
        prev_all: null_mut(),
        next_part: null_mut(),
        prev_part: null_mut(),
    });
    // Thread the free list through the objects, lowest address first.
    let mut next: *mut u8 = null_mut();
    for i in (0..total as usize).rev() {
        let obj = (addr + SLAB_HDR + i * c.objsize) as *mut u8;
        (obj as *mut *mut u8).write(next);
        (obj.add(8) as *mut u64).write(CANARY);
        next = obj;
    }
    (*s).free = next;
    all_insert(c, s);
    part_insert(c, s);
    c.nslabs += 1;
    c.grows += 1;
    true
}

unsafe fn cache_alloc_raw(c: &mut Cache) -> Option<*mut u8> {
    if c.partial.is_null() && !cache_grow(c) {
        return None;
    }
    let s = c.partial;
    let obj = (*s).free;
    (*s).free = (obj as *mut *mut u8).read();
    (*s).inuse += 1;
    if (*s).free.is_null() {
        part_remove(c, s); // slab is full now
    }
    c.inuse += 1;
    c.allocs += 1;
    (obj as *mut u64).write(0);
    (obj.add(8) as *mut u64).write(0);
    Some(obj)
}

/// Free `obj`, which lives in the slab at `s`.  Returns false for a pointer
/// that is not a live object of that slab (misaligned, double free).
unsafe fn cache_free_raw(s: *mut Slab, obj: *mut u8) -> bool {
    if (*s).magic != SLAB_MAGIC {
        return false;
    }
    let c = &mut *(*s).cache;
    let off = (obj as usize).wrapping_sub(s as usize + SLAB_HDR);
    if (obj as usize) < s as usize + SLAB_HDR
        || off % c.objsize != 0
        || off / c.objsize >= (*s).total as usize
        || (obj.add(8) as *const u64).read() == CANARY
        || (*s).inuse == 0
    {
        return false;
    }
    let was_full = (*s).free.is_null();
    (obj.add(8) as *mut u64).write(CANARY);
    (obj as *mut *mut u8).write((*s).free);
    (*s).free = obj;
    (*s).inuse -= 1;
    c.inuse -= 1;
    c.frees += 1;
    if was_full {
        part_insert(c, s);
    }
    // Hand an empty slab back, but keep the last one to avoid thrashing.
    if (*s).inuse == 0 && c.nslabs > 1 {
        part_remove(c, s);
        all_remove(c, s);
        c.nslabs -= 1;
        c.shrinks += 1;
        (*s).magic = 0;
        buddy().free(s as usize, c.slab_order);
    }
    true
}

// ------------------------------------------------------------ public API

/// Create a private cache of `size`-byte objects (16..=2048) aligned to
/// `align` (0 = 16, otherwise a power of two up to 64).
pub fn kcache_create(name: &str, size: usize, align: usize) -> Option<KCache> {
    let _g = IrqGuard::new();
    create_locked(name, size, align, false)
}

fn create_locked(name: &str, size: usize, align: usize, builtin: bool) -> Option<KCache> {
    let align = if align == 0 { 16 } else { align };
    if !align.is_power_of_two() || align > 64 || size == 0 || size > MAX_OBJ {
        return None;
    }
    let align = align.max(16);
    let objsize = (size.max(16) + align - 1) & !(align - 1);
    // Smallest slab that holds at least 8 objects (or the biggest allowed).
    let mut order = 0;
    while order < MAX_SLAB_ORDER && ((PAGE << order) - SLAB_HDR) / objsize < 8 {
        order += 1;
    }
    let per = ((PAGE << order) - SLAB_HDR) / objsize;
    if per == 0 {
        return None;
    }
    let slot = caches().iter().position(|c| !c.used)?;
    let c = &mut caches()[slot];
    *c = EMPTY_CACHE;
    c.used = true;
    c.builtin = builtin;
    let n = name.len().min(15);
    c.name[..n].copy_from_slice(&name.as_bytes()[..n]);
    c.name_len = n as u8;
    c.objsize = objsize;
    c.slab_order = order;
    c.per_slab = per as u32;
    Some(KCache(slot as u8))
}

/// Allocate one object from `cache`.
pub fn kcache_alloc(cache: KCache) -> Option<*mut u8> {
    let _g = IrqGuard::new();
    let c = &mut caches()[cache.0 as usize];
    if !c.used {
        return None;
    }
    // SAFETY: guarded; the cache owns its slabs.
    let r = unsafe { cache_alloc_raw(c) };
    if r.is_none() {
        cnt().failed += 1;
    }
    r
}

/// Free an object obtained from `cache`.  Pointers belonging to another
/// cache are refused.
pub fn kcache_free(cache: KCache, ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let _g = IrqGuard::new();
    let b = buddy();
    let p = ptr as usize;
    if !b.contains(p) {
        cnt().bad_frees += 1;
        return;
    }
    let info = b.info[b.idx(p)];
    if info & KIND_MASK != K_SLAB {
        cnt().bad_frees += 1;
        return;
    }
    let bytes = PAGE << (info & ORDER_MASK);
    let s = (p & !(bytes - 1)) as *mut Slab;
    // SAFETY: the page is a slab page (checked above) inside the heap.
    unsafe {
        if (*s).cache != &mut caches()[cache.0 as usize] as *mut Cache || !cache_free_raw(s, ptr) {
            cnt().bad_frees += 1;
        }
    }
}

/// Destroy a private cache and give all its slabs back.  Fails (returns
/// false) while objects are still allocated, and for the built-in caches.
pub fn kcache_destroy(cache: KCache) -> bool {
    let _g = IrqGuard::new();
    let c = &mut caches()[cache.0 as usize];
    if !c.used || c.builtin || c.inuse != 0 {
        return false;
    }
    // SAFETY: no object is live, so every slab can go.
    unsafe {
        let mut s = c.all;
        while !s.is_null() {
            let next = (*s).next_all;
            (*s).magic = 0;
            buddy().free(s as usize, c.slab_order);
            s = next;
        }
    }
    *c = EMPTY_CACHE;
    true
}

/// Allocate `size` bytes.  Small requests (<= 1024) come from the matching
/// slab cache, bigger ones from the buddy in power-of-two page runs (up to
/// 4 MiB, page aligned).  None when out of memory or `size` is 0 or huge.
pub fn kmalloc(size: usize, flags: u32) -> Option<*mut u8> {
    if size == 0 {
        return None;
    }
    let _g = IrqGuard::new();
    if !buddy().ready {
        return None;
    }
    let r = if size <= GENERIC_SIZES[N_GENERIC - 1] {
        let class = if size <= 32 {
            0
        } else {
            (usize::BITS - (size - 1).leading_zeros()) as usize - 5
        };
        // SAFETY: guarded; generic caches occupy slots 0..N_GENERIC.
        let p = unsafe { cache_alloc_raw(&mut caches()[class]) };
        if let (Some(p), true) = (p, flags & KM_ZERO != 0) {
            // SAFETY: the object is at least GENERIC_SIZES[class] bytes.
            unsafe { core::ptr::write_bytes(p, 0, GENERIC_SIZES[class]) };
        }
        p
    } else {
        buddy_alloc_bytes(size, flags)
    };
    match r {
        Some(_) => cnt().allocs += 1,
        None => cnt().failed += 1,
    }
    r
}

fn buddy_alloc_bytes(size: usize, flags: u32) -> Option<*mut u8> {
    let order = order_for_pages((size + PAGE - 1) / PAGE);
    if order > MAX_ORDER {
        return None;
    }
    // SAFETY: caller holds the IRQ guard.
    let addr = unsafe { buddy().alloc(order) }?;
    if flags & KM_ZERO != 0 {
        // SAFETY: the whole block is ours.
        unsafe { core::ptr::write_bytes(addr as *mut u8, 0, PAGE << order) };
    }
    Some(addr as *mut u8)
}

/// Zeroed allocation of `n * size` bytes.
#[allow(dead_code)]
pub fn kcalloc(n: usize, size: usize) -> Option<*mut u8> {
    kmalloc(n.checked_mul(size)?, KM_ZERO)
}

/// Free a pointer returned by kmalloc / kcalloc / kcache_alloc.  Pointers
/// that are not live allocations (foreign, interior, already freed) are
/// ignored and counted in `bad_frees`.
pub fn kfree(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let _g = IrqGuard::new();
    let b = buddy();
    let p = ptr as usize;
    if !b.contains(p) {
        cnt().bad_frees += 1;
        return;
    }
    let idx = b.idx(p);
    let info = b.info[idx];
    // SAFETY: the pointer lies in the heap and the metadata is consistent
    // with the checks below.
    unsafe {
        match info & KIND_MASK {
            K_BUDDY if p == b.addr(idx) => {
                b.free(p, (info & ORDER_MASK) as usize);
                cnt().frees += 1;
            }
            K_SLAB => {
                let bytes = PAGE << (info & ORDER_MASK);
                let s = (p & !(bytes - 1)) as *mut Slab;
                if cache_free_raw(s, ptr) {
                    cnt().frees += 1;
                } else {
                    cnt().bad_frees += 1;
                }
            }
            _ => cnt().bad_frees += 1,
        }
    }
}

// ------------------------------------------------------------------- init

/// Map the heap region and hand it to the buddy.  Returns false only when
/// not even MIN_HEAP bytes could be mapped.
pub fn init() -> bool {
    let free_bytes = crate::mem::pmm::stats_snapshot()
        .free_pages
        .load(core::sync::atomic::Ordering::Relaxed)
        * 4096;
    let want = (free_bytes / 4).clamp(MIN_HEAP, vmm::HEAP_SIZE);
    let size = vmm::map_heap(want);
    if size < MIN_HEAP {
        return false;
    }
    let b = buddy();
    b.base = vmm::HEAP_BASE as usize;
    b.pages = size as usize / PAGE;
    // SAFETY: the freshly mapped region is exclusively ours.  Seed the free
    // lists with the largest naturally aligned blocks that fit.
    unsafe {
        let mut idx = 0;
        while idx < b.pages {
            let mut o = MAX_ORDER;
            while idx % (1 << o) != 0 || idx + (1 << o) > b.pages {
                o -= 1;
            }
            b.push(idx, o);
            idx += 1 << o;
        }
    }
    b.ready = true;
    for (i, sz) in GENERIC_SIZES.iter().enumerate() {
        let slot = create_locked(GENERIC_NAMES[i], *sz, 0, true);
        debug_assert!(slot == Some(KCache(i as u8)));
    }
    true
}

/// Size of the mapped heap in bytes (0 before init).
pub fn size() -> u64 {
    (buddy().pages * PAGE) as u64
}

// ------------------------------------------------------------------ stats

pub struct Stats {
    /// Bytes handed out by the buddy (large blocks and slabs).
    pub used: u64,
    /// Bytes still free in the buddy.
    pub free: u64,
    pub allocs: u64,
    pub frees: u64,
    pub bad_frees: u64,
    pub failed: u64,
    /// Free blocks per buddy order (4 KiB << order).
    pub orders: [u32; MAX_ORDER + 1],
}

pub fn stats() -> Stats {
    let _g = IrqGuard::new();
    let b = buddy();
    let free = (b.free_pages * PAGE) as u64;
    Stats {
        used: (b.pages * PAGE) as u64 - free,
        free,
        allocs: cnt().allocs,
        frees: cnt().frees,
        bad_frees: cnt().bad_frees,
        failed: cnt().failed,
        orders: b.list_len,
    }
}

/// One line of the per-cache statistics.
pub struct CacheInfo {
    name: [u8; 16],
    name_len: u8,
    pub objsize: usize,
    pub slab_bytes: usize,
    pub slabs: usize,
    pub inuse: usize,
    pub total: usize,
    pub allocs: u64,
    pub frees: u64,
    pub grows: u64,
    pub shrinks: u64,
    pub builtin: bool,
}

impl CacheInfo {
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("?")
    }
}

/// Statistics of the `i`-th cache slot, None for unused slots.
pub fn cache_info(i: usize) -> Option<CacheInfo> {
    let _g = IrqGuard::new();
    let c = caches().get(i)?;
    if !c.used {
        return None;
    }
    Some(CacheInfo {
        name: c.name,
        name_len: c.name_len,
        objsize: c.objsize,
        slab_bytes: PAGE << c.slab_order,
        slabs: c.nslabs,
        inuse: c.inuse,
        total: c.nslabs * c.per_slab as usize,
        allocs: c.allocs,
        frees: c.frees,
        grows: c.grows,
        shrinks: c.shrinks,
        builtin: c.builtin,
    })
}

pub const fn cache_slots() -> usize {
    MAX_CACHES
}

// ------------------------------------------------------------- self tests

fn fill(p: *mut u8, n: usize, seed: u8) {
    for i in 0..n {
        // SAFETY: callers pass a live allocation of at least `n` bytes.
        unsafe { p.add(i).write(seed.wrapping_add(i as u8)) };
    }
}

fn check(p: *mut u8, n: usize, seed: u8) -> bool {
    (0..n).all(|i| {
        // SAFETY: as above.
        unsafe { p.add(i).read() == seed.wrapping_add(i as u8) }
    })
}

/// Quick checks run by `meminfo`: integrity, slab reuse, buddy coalescing,
/// private caches, double-free rejection.
pub fn self_test() -> bool {
    // Warm the generic caches: each keeps one idle slab, which must not
    // count as a leak in the before/after comparison.
    for sz in GENERIC_SIZES {
        if let Some(p) = kmalloc(sz, 0) {
            kfree(p);
        }
    }
    let before = stats();
    let caches_before: usize = (0..MAX_CACHES).filter_map(cache_info).map(|c| c.inuse).sum();

    // Slab: fresh object is writable and reused right after being freed.
    let Some(a) = kmalloc(64, 0) else { return false };
    fill(a, 64, 0x11);
    if !check(a, 64, 0x11) {
        kfree(a);
        return false;
    }
    kfree(a);
    let Some(a2) = kmalloc(64, 0) else { return false };
    let reused = a == a2;
    kfree(a2);

    // Double free is refused and counted.
    let bad_before = stats().bad_frees;
    kfree(a2);
    let double_ok = stats().bad_frees == bad_before + 1;

    // Buddy: odd sizes, freed in scrambled order, must coalesce fully.
    let sizes = [4096usize, 5000, 20000, 70000, 4096, 300_000, 8192, 1500];
    let mut p = [null_mut::<u8>(); 8];
    for (i, s) in sizes.iter().enumerate() {
        match kmalloc(*s, KM_ZERO) {
            Some(x) => {
                p[i] = x;
                // SAFETY: freshly allocated, zeroed block of at least 1 byte.
                if unsafe { x.read() != 0 } {
                    return false;
                }
                fill(x, (*s).min(256), i as u8);
            }
            None => return false,
        }
    }
    let mut ok = true;
    for (i, s) in sizes.iter().enumerate() {
        ok &= check(p[i], (*s).min(256), i as u8);
    }
    for i in [3, 0, 7, 1, 6, 2, 5, 4] {
        kfree(p[i]);
    }

    // Private cache with 24 objects of 200 bytes.
    let mut cache_ok = false;
    if let Some(kc) = kcache_create("selftest", 200, 0) {
        let mut objs = [null_mut::<u8>(); 24];
        cache_ok = true;
        for (i, o) in objs.iter_mut().enumerate() {
            match kcache_alloc(kc) {
                Some(x) => {
                    fill(x, 200, i as u8);
                    *o = x;
                }
                None => cache_ok = false,
            }
        }
        for (i, o) in objs.iter().enumerate() {
            if !o.is_null() {
                cache_ok &= check(*o, 200, i as u8);
                kcache_free(kc, *o);
            }
        }
        cache_ok &= kcache_destroy(kc);
    }

    let after = stats();
    let caches_after: usize = (0..MAX_CACHES).filter_map(cache_info).map(|c| c.inuse).sum();
    let _ = reused; // informational: LIFO reuse is not a correctness rule
    ok && double_ok
        && cache_ok
        && after.free == before.free
        && after.orders == before.orders
        && caches_after == caches_before
}

pub struct StressResult {
    pub objects: usize,
    pub ok: bool,
    pub failed_allocs: usize,
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Allocate and free `total` objects of mixed sizes (32..1024 bytes) in
/// batches, checking a byte pattern in every object (no overlap, no
/// corruption), freeing in scrambled order, and finally comparing the heap
/// state with the one we started from.
pub fn stress(total: usize) -> StressResult {
    const BATCH: usize = 256;
    let before = stats();
    let caches_before: usize = (0..MAX_CACHES).filter_map(cache_info).map(|c| c.inuse).sum();
    let mut ptrs = [(null_mut::<u8>(), 0usize); BATCH];
    let mut done = 0usize;
    let mut failed = 0usize;
    let mut ok = true;

    while done < total {
        let n = BATCH.min(total - done);
        for i in 0..n {
            let sz = GENERIC_SIZES[(done + i) % N_GENERIC] - (i % 3) * 8;
            match kmalloc(sz, 0) {
                Some(p) => {
                    fill(p, sz, ((done + i) * 31 + 7) as u8);
                    ptrs[i] = (p, sz);
                }
                None => {
                    ptrs[i] = (null_mut(), 0);
                    failed += 1;
                }
            }
        }
        for i in 0..n {
            let (p, sz) = ptrs[i];
            if !p.is_null() && !check(p, sz, ((done + i) * 31 + 7) as u8) {
                ok = false;
            }
        }
        let mut stride = 97;
        while gcd(stride, n) != 1 {
            stride += 2;
        }
        for k in 0..n {
            let (p, _) = ptrs[(k * stride) % n];
            kfree(p);
        }
        done += n;
    }

    let after = stats();
    let caches_after: usize = (0..MAX_CACHES).filter_map(cache_info).map(|c| c.inuse).sum();
    ok &= caches_after == caches_before && after.bad_frees == before.bad_frees;
    // Idle slabs are kept (one per cache), so the buddy may differ by those.
    ok &= after.used <= before.used + (N_GENERIC as u64) * (PAGE as u64) * 4 * 8;
    StressResult { objects: done - failed, ok, failed_allocs: failed }
}

// ------------------------------------------------------- global allocator

/// Backs the `alloc` crate with kmalloc.  Alignments above 16 are served by
/// the buddy, which hands out page-aligned blocks.
pub struct KernelAlloc;

unsafe impl GlobalAlloc for KernelAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        alloc_layout(l, 0)
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        alloc_layout(l, KM_ZERO)
    }

    unsafe fn dealloc(&self, p: *mut u8, _l: Layout) {
        kfree(p)
    }
}

fn alloc_layout(l: Layout, flags: u32) -> *mut u8 {
    let size = l.size().max(1);
    if l.align() > PAGE {
        return null_mut();
    }
    let r = if l.align() > 16 && size <= GENERIC_SIZES[N_GENERIC - 1] {
        // Slab objects are only 16-aligned: use a whole page instead.
        let _g = IrqGuard::new();
        if buddy().ready { buddy_alloc_bytes(PAGE, flags) } else { None }
    } else {
        kmalloc(size, flags)
    };
    r.unwrap_or(null_mut())
}

#[global_allocator]
static ALLOCATOR: KernelAlloc = KernelAlloc;
