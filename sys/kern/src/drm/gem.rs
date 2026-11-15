// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// GEM objects: the buffers behind dumb buffers and framebuffers.  An object
// is a list of physical pages from the PMM (zeroed, never contiguous: user
// mappings and the scanout blit both work page by page).
//
// Ownership rides on the page refcount of mem/cow.rs, the same table `fork`
// and `munmap` already use: the object holds the implicit first reference of
// every page, each user mapping `track`s one more, and whoever drops the
// last one frees the frame.  A buffer that is still mapped when its last
// handle closes therefore stays alive until `munmap` -- the Linux behaviour
// (the VMA keeps the object), with no new bookkeeping.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::mem::{cow, pmm, vmm};

pub const PAGE: usize = 4096;

/// Pages held by live objects, and the ceiling: DRM buffers are pinned (no
/// swap), so a client must not be able to drain a 256 MiB machine.
static LIVE_PAGES: AtomicUsize = AtomicUsize::new(0);
const MAX_LIVE_PAGES: usize = (128 << 20) / PAGE;
/// Frames always left to the rest of the system.
const KEEP_FREE_PAGES: u64 = 2048;

pub struct Gem {
    /// Object id: the high half of the fake mmap offset (see `Gem::mmap_offset`).
    pub id: u32,
    /// Requested size in bytes (the pages round it up).
    pub size: usize,
    pages: Vec<u64>,
}

impl Gem {
    /// A zeroed object of `size` bytes, or None when memory is short.
    pub fn new(id: u32, size: usize) -> Option<Gem> {
        let n = size.div_ceil(PAGE);
        if n == 0 || LIVE_PAGES.load(Ordering::Relaxed) + n > MAX_LIVE_PAGES {
            return None;
        }
        let free = pmm::stats_snapshot().free_pages.load(Ordering::Relaxed);
        if free < n as u64 + KEEP_FREE_PAGES {
            return None;
        }
        let mut pages: Vec<u64> = Vec::new();
        pages.try_reserve_exact(n).ok()?;
        for _ in 0..n {
            let Some(p) = pmm::alloc_page() else {
                // Hand back what we took: Drop of the half-built object.
                // Not counted yet: give the frames back directly.
                for q in pages.drain(..) {
                    if cow::release(q) {
                        pmm::free_page(q);
                    }
                }
                return None;
            };
            // `alloc_page` hands out zeroed frames, so the object starts black.
            if vmm::phys_access(p).is_none() {
                pmm::free_page(p);
                for q in pages.drain(..) {
                    if cow::release(q) {
                        pmm::free_page(q);
                    }
                }
                return None;
            }
            pages.push(p);
        }
        LIVE_PAGES.fetch_add(n, Ordering::Relaxed);
        Some(Gem { id, size, pages })
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Physical address of page `i`.
    pub fn page_phys(&self, i: usize) -> Option<u64> {
        self.pages.get(i).copied()
    }

    /// The fake mmap offset userspace passes back to `mmap(2)` on the DRM
    /// fd: the object id in the high half, the byte offset below.  Always
    /// page aligned and never 0 (id starts at 1).
    pub fn mmap_offset(&self) -> u64 {
        (self.id as u64) << 32
    }

    /// Copy `out.len()` bytes starting at byte `off` of the object into
    /// `out`.  Reads past the end return false.
    pub fn read(&self, off: usize, out: &mut [u8]) -> bool {
        let Some(end) = off.checked_add(out.len()) else { return false };
        if end > self.pages.len() * PAGE {
            return false;
        }
        let mut done = 0;
        while done < out.len() {
            let pos = off + done;
            let (pi, po) = (pos / PAGE, pos % PAGE);
            let n = (PAGE - po).min(out.len() - done);
            let Some(va) = vmm::phys_access(self.pages[pi]) else { return false };
            // SAFETY: inside one page of ours, `n` bytes.
            unsafe { core::ptr::copy_nonoverlapping((va as *const u8).add(po), out.as_mut_ptr().add(done), n) };
            done += n;
        }
        true
    }
}

impl Drop for Gem {
    fn drop(&mut self) {
        LIVE_PAGES.fetch_sub(self.pages.len(), Ordering::Relaxed);
        for &p in &self.pages {
            // `release` answers true when we were the last owner.
            if cow::release(p) {
                pmm::free_page(p);
            }
        }
    }
}
