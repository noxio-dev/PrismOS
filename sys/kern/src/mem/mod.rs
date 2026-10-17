// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Memory management: physical page allocator (bitmap), virtual memory
// (own page tables, direct map, W^X) and the kernel heap (buddy + slab).

pub mod heap;
pub mod pmm;
pub mod vmm;
