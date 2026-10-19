// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Memory management: physical page allocator (bitmap), virtual memory
// (own page tables, direct map, W^X), the kernel heap (buddy + slab) and
// the shared-page refcounts behind copy-on-write (phase 2.5).

pub mod cow;
pub mod heap;
pub mod pmm;
pub mod vmm;
