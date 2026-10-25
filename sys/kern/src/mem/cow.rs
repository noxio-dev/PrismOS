// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Shared physical pages (phase 2.5): the refcount table behind copy-on-write.
//
// The PMM is a bitmap: it knows only "used / free", not how many owners a page
// has.  `fork` (process.rs) duplicates a whole address space by mapping the
// *same* physical pages into the child, so ownership of every user page
// becomes shared between two (or, with several forks, more) address spaces.
// This module keeps the count for exactly those pages:
//
//   * an entry exists only while the page is mapped by two or more address
//     spaces (`track` inserts it at 2, `track` again bumps it);
//   * `release` drops one owner: the entry is removed when one owner remains
//     (the survivor frees the page itself, through the plain PMM path, when
//     its own address space dies) and returns whether the caller was the last
//     owner (and so must free the page);
//   * `refcount` is what the #PF handler consults: a COW fault on a page with
//     no entry means "already exclusively mine" (just make it writable
//     again), with one it means "copy before writing".
//
// Single CPU, no SMP: every mutation runs with interrupts masked (fork and
// exit paths, the #PF handler), so the table needs no lock of its own.  All
// operations still take an IrqGuard: the paths that reach us are already
// masked, and a stray caller from an interrupt context must not tear a
// rehash apart.
//
// The table is open-addressing with linear probing, allocated from the kernel
// heap, grown (doubled) at a load factor of 2/3.  Empty slots hold count 0;
// deleted slots hold the DELETED marker so probe chains stay intact.

use core::ptr::addr_of_mut;

use crate::arch::io::IrqGuard;

const PAGE: u64 = 4096;

/// One entry: a physical page and how many address spaces map it.
#[derive(Clone, Copy)]
struct Entry {
    phys: u64,
    count: u32,
}

const DELETED: u32 = u32::MAX;

/// Initial capacity (a power of two, one heap page worth of entries).
const INITIAL_CAP: usize = 512;

struct Table {
    slots: *mut Entry,
    cap: usize,
    used: usize, // live + deleted
    live: usize,
}

static mut TABLE: Table = Table { slots: core::ptr::null_mut(), cap: 0, used: 0, live: 0 };

fn alloc_slots(cap: usize) -> *mut Entry {
    let Ok(layout) = core::alloc::Layout::array::<Entry>(cap) else { return core::ptr::null_mut() };
    // SAFETY: the layout is non-zero (cap is a power of two >= 512); a fresh
    // allocation, zeroed by `alloc_zeroed` so every slot reads "empty".  A
    // null result (out of memory) is returned as is, never written through.
    unsafe {
        let p = alloc::alloc::alloc_zeroed(layout);
        if p.is_null() {
            return core::ptr::null_mut();
        }
        p.cast::<Entry>()
    }
}

/// Index of the first empty-or-matching slot for `phys` (linear probing).
///
/// # Safety
/// `slots` must be a valid array of `cap` entries.
unsafe fn probe(slots: *mut Entry, cap: usize, phys: u64) -> usize {
    let mut i = ((phys / PAGE) as usize) & (cap - 1);
    loop {
        // SAFETY: index within the array.
        let e = unsafe { slots.add(i).read() };
        if e.count == 0 || e.phys == phys {
            return i;
        }
        i = (i + 1) & (cap - 1);
    }
}

/// Rebuild the table with twice the capacity (or from scratch).
///
/// # Safety
/// Called with interrupts masked and the old table consistent.
unsafe fn grow() -> bool {
    unsafe {
        let t = &mut *addr_of_mut!(TABLE);
        let cap = if t.cap == 0 { INITIAL_CAP } else { t.cap * 2 };
        let slots = alloc_slots(cap);
        if slots.is_null() {
            return false;
        }
        if !t.slots.is_null() {
            for k in 0..t.cap {
                let e = t.slots.add(k).read();
                if e.count != 0 && e.count != DELETED {
                    let i = probe(slots, cap, e.phys);
                    slots.add(i).write(e);
                }
            }
            let old_layout =
                core::alloc::Layout::array::<Entry>(t.cap).expect("old layout was valid");
            alloc::alloc::dealloc(t.slots.cast(), old_layout);
        }
        t.slots = slots;
        t.cap = cap;
        t.used = t.live;
        true
    }
}

/// A page becomes shared by one more address space (fork).  Returns false
/// when the table cannot be grown -- the caller must then abort the fork
/// (an untracked shared page would be freed by both owners later).
pub fn track(phys: u64) -> bool {
    let _g = IrqGuard::new();
    // SAFETY: interrupts masked; the table is consistent.
    unsafe {
        let t = &mut *addr_of_mut!(TABLE);
        if t.slots.is_null() && !grow() {
            return false;
        }
        let t = &mut *addr_of_mut!(TABLE);
        let i = probe(t.slots, t.cap, phys);
        let e = t.slots.add(i).read();
        if e.count == 0 {
            // The page leaves single ownership.
            t.slots.add(i).write(Entry { phys, count: 2 });
            t.live += 1;
            t.used += 1;
            if t.used * 3 >= t.cap * 2 {
                if !grow() {
                    // Undo the insert: better a failed fork than a page
                    // freed twice.
                    t.slots.add(i).write(Entry { phys, count: 0 });
                    t.live -= 1;
                    t.used -= 1;
                    return false;
                }
            }
            true
        } else if e.count != DELETED {
            // Already shared (fork of a fork): one more owner.
            t.slots.add(i).write(Entry { phys, count: e.count.saturating_add(1) });
            true
        } else {
            // DELETED tombstone: reuse the slot.
            t.slots.add(i).write(Entry { phys, count: 2 });
            t.live += 1;
            true
        }
    }
}

/// How many address spaces map `phys` (0 = single owner, untracked).
pub fn refcount(phys: u64) -> u32 {
    let _g = IrqGuard::new();
    // SAFETY: read-only probe.
    unsafe {
        let t = &*addr_of_mut!(TABLE);
        if t.slots.is_null() {
            return 0;
        }
        let i = probe(t.slots, t.cap, phys);
        let e = t.slots.add(i).read();
        if e.count != 0 && e.count != DELETED {
            e.count
        } else {
            0
        }
    }
}

/// One address space stops mapping `phys`.  Returns true when the caller was
/// the last owner and must hand the page back to the PMM itself; false when
/// other owners remain (or the entry bookkeeping ended).
pub fn release(phys: u64) -> bool {
    let _g = IrqGuard::new();
    // SAFETY: interrupts masked; single-owner pages (no entry) are the common
    // case and leave the table untouched.
    unsafe {
        let t = &mut *addr_of_mut!(TABLE);
        if t.slots.is_null() {
            return true;
        }
        let i = probe(t.slots, t.cap, phys);
        let e = t.slots.add(i).read();
        if e.count == 0 || e.count == DELETED {
            return true; // single owner: the caller frees it
        }
        if e.count > 2 {
            t.slots.add(i).write(Entry { phys, count: e.count - 1 });
            return false;
        }
        if e.count == 2 {
            // One other owner remains: below the tracking threshold.
            t.slots.add(i).write(Entry { phys, count: DELETED });
            t.live -= 1;
            return false;
        }
        // e.count == 1 cannot happen (invariant: entries hold >= 2), but if
        // it ever did, treat the caller as the last owner.
        t.slots.add(i).write(Entry { phys, count: DELETED });
        t.live -= 1;
        true
    }
}

/// Live entries, for `meminfo` diagnostics.
pub fn shared_pages() -> usize {
    // SAFETY: scalar read.
    unsafe { (*addr_of_mut!(TABLE)).live }
}
