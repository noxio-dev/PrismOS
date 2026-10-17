// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Memory commands: the loader memory map, PMM / VMM / heap statistics, the
// heap stress test and raw peek / poke / dump.

use super::{bad_number, boot_info, parse_u64, usage, Args, Size};
use crate::boot::{
    MEM_ACPI_NVS, MEM_ACPI_RECL, MEM_BAD, MEM_KERNEL, MEM_LOADER, MEM_RESERVED, MEM_USABLE,
};

/// Width suffix shared by peek/poke: bits, or a size letter.
fn parse_width(s: &str) -> Option<u32> {
    match s {
        "8" | "b" => Some(8),
        "16" | "w" => Some(16),
        "32" | "d" => Some(32),
        "64" | "q" => Some(64),
        _ => None,
    }
}

/// Optional trailing width argument for peek/poke; defaults to 64 bits.
fn width_arg(args: &mut Args, cmd: &str) -> Option<u32> {
    match args.next() {
        None => Some(64),
        Some(tok) => match parse_width(tok) {
            Some(w) => Some(w),
            None => {
                crate::kprintln!("{}: bad width '{}' (use 8, 16, 32 or 64)", cmd, tok);
                None
            }
        },
    }
}

pub(super) fn cmd_lsmem(_args: &mut Args) {
    let Some(bi) = boot_info() else {
        crate::kprintln!("lsmem: no boot info");
        return;
    };
    let mmap = bi.memory_map();
    if mmap.is_empty() {
        crate::kprintln!("lsmem: no memory map");
        return;
    }
    crate::kprintln!("        base           end        size      type");
    let mut entries = 0usize;
    let mut usable = 0u64;
    let mut regions = 0usize;
    for e in mmap {
        entries += 1;
        let end = e.base.wrapping_add(e.length).wrapping_sub(1); // last byte
        crate::kprintln!(
            "{:#012x}  {:#012x}  {:>7} KiB  {}",
            e.base,
            end,
            e.length >> 10,
            kind_name(e.kind)
        );
        if e.kind == MEM_USABLE || e.kind == MEM_LOADER {
            usable += e.length;
            regions += 1;
        }
    }
    crate::kprintln!("{} entries, usable: {} in {} region(s)", entries, Size(usable), regions);
}

// ----------------------------------------------------------- raw memory

/// Volatile read of `w` bits at `addr`.
///
/// The address must be mapped and aligned to the access width; a bad
/// address faults (and, without a page fault handler, resets the machine).
fn read_mem(addr: u64, w: u32) -> u64 {
    // SAFETY: peek/poke validated alignment and width; whether the address
    // is mapped is the user's call -- this is a poking shell.
    match w {
        8 => unsafe { (addr as *const u8).read_volatile() as u64 },
        16 => unsafe { (addr as *const u16).read_volatile() as u64 },
        32 => unsafe { (addr as *const u32).read_volatile() as u64 },
        _ => unsafe { (addr as *const u64).read_volatile() },
    }
}

/// Volatile write of `val` to `addr` with width `w`; same contract as read_mem.
fn write_mem(addr: u64, val: u64, w: u32) {
    // SAFETY: same contract as read_mem.
    match w {
        8 => unsafe { (addr as *mut u8).write_volatile(val as u8) },
        16 => unsafe { (addr as *mut u16).write_volatile(val as u16) },
        32 => unsafe { (addr as *mut u32).write_volatile(val as u32) },
        _ => unsafe { (addr as *mut u64).write_volatile(val) },
    }
}

pub(super) fn cmd_peek(args: &mut Args) {
    let Some(tok) = args.next() else { usage("peek"); return };
    let Some(addr) = parse_u64(tok) else { bad_number("peek", tok); return };
    let Some(w) = width_arg(args, "peek") else { return };
    let bytes = u64::from(w / 8);
    if addr % bytes != 0 {
        crate::kprintln!("peek: {:#x} is not {}-byte aligned", addr, bytes);
        return;
    }
    let val = read_mem(addr, w);
    match w {
        8 => crate::kprintln!("{:#018x}: {:#04x}", addr, val),
        16 => crate::kprintln!("{:#018x}: {:#06x}", addr, val),
        32 => crate::kprintln!("{:#018x}: {:#010x}", addr, val),
        _ => crate::kprintln!("{:#018x}: {:#018x}", addr, val),
    }
}

pub(super) fn cmd_poke(args: &mut Args) {
    let Some(atok) = args.next() else { usage("poke"); return };
    let Some(addr) = parse_u64(atok) else { bad_number("poke", atok); return };
    let Some(vtok) = args.next() else { usage("poke"); return };
    let Some(val) = parse_u64(vtok) else { bad_number("poke", vtok); return };
    let Some(w) = width_arg(args, "poke") else { return };
    if w < 64 && val >> w != 0 {
        crate::kprintln!("poke: {:#x} does not fit in {} bits", val, w);
        return;
    }
    let bytes = u64::from(w / 8);
    if addr % bytes != 0 {
        crate::kprintln!("poke: {:#x} is not {}-byte aligned", addr, bytes);
        return;
    }
    let old = read_mem(addr, w);
    write_mem(addr, val, w);
    crate::kprintln!("{:#018x}: {:#x} -> {:#x}", addr, old, val);
}

pub(super) fn cmd_dump(args: &mut Args) {
    const MAX_LEN: u64 = 4096;
    let Some(atok) = args.next() else { usage("dump"); return };
    let Some(addr) = parse_u64(atok) else { bad_number("dump", atok); return };
    let len = match args.next() {
        None => 64,
        Some(tok) => match parse_u64(tok) {
            Some(n) if n >= 1 && n <= MAX_LEN => n,
            _ => {
                crate::kprintln!("dump: bad length '{}' (1..={})", tok, MAX_LEN);
                return;
            }
        },
    };
    if len > u64::MAX - addr {
        crate::kprintln!("dump: range wraps the address space");
        return;
    }
    let base = addr as *const u8;
    let mut off = 0u64;
    while off < len {
        let n = (len - off).min(16) as usize;
        // Read the row first so every byte is touched exactly once; this
        // keeps MMIO reads sane and the two columns consistent.
        let mut row = [0u8; 16];
        // SAFETY: raw memory access asked for by the user; may fault.
        for i in 0..n {
            unsafe { row[i] = base.add(off as usize + i).read_volatile() };
        }
        crate::kprint!("{:#010x}  ", addr + off);
        for i in 0..16 {
            if i < n {
                crate::kprint!("{:02x} ", row[i]);
            } else {
                crate::kprint!("   ");
            }
            if i == 7 {
                crate::kprint!(" ");
            }
        }
        crate::kprint!("|");
        for i in 0..n {
            let b = row[i];
            let c = if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' };
            crate::kprint!("{}", c);
        }
        for _ in n..16 {
            crate::kprint!(" ");
        }
        crate::kprintln!("|");
        off += n as u64;
    }
}

pub(super) fn cmd_meminfo(_args: &mut Args) {
    let ps = crate::mem::pmm::stats_snapshot();
    let free = ps.free_pages.load(core::sync::atomic::Ordering::Relaxed);
    crate::kprintln!("pmm      : {} pages total, {} free, {} used, {} allocated",
        ps.total_pages,
        free,
        ps.total_pages - free,
        crate::mem::pmm::alloc_count()
    );
    crate::kprintln!("           managed {:#x}..{:#x} ({}/{} MiB)",
        ps.base, ps.top, (ps.top - ps.base) >> 20, (ps.top - ps.base) >> 20);
    crate::kprintln!("vmm      : {} pages mapped, direct map {:#x}",
        crate::mem::vmm::pages_mapped(), crate::mem::vmm::DIRECT_MAP_BASE);
    let hs = crate::mem::heap::stats();
    crate::kprintln!("heap     : {} used / {} free of {}, {} allocs, {} frees, {} failed, {} bad frees",
        Size(hs.used), Size(hs.free), Size(crate::mem::heap::size()), hs.allocs, hs.frees, hs.failed, hs.bad_frees);
    crate::kprint!("           buddy free blocks by order:");
    for (o, n) in hs.orders.iter().enumerate() {
        if *n != 0 {
            crate::kprint!(" {}K x{}", 4u64 << o, n);
        }
    }
    crate::kprintln!();
    crate::kprintln!("           {:<14} {:>5} {:>6} {:>11} {:>9} {:>9}", "cache", "size", "slabs", "inuse/total", "allocs", "frees");
    for i in 0..crate::mem::heap::cache_slots() {
        if let Some(c) = crate::mem::heap::cache_info(i) {
            crate::kprintln!("           {:<14} {:>5} {:>6} {:>5}/{:<5} {:>9} {:>9}",
                c.name(), c.objsize, c.slabs, c.inuse, c.total, c.allocs, c.frees);
        }
    }
    crate::kprintln!("           self-test: {}",
        if crate::mem::heap::self_test() { "ok" } else { "FAILED" });
}

pub(super) fn cmd_heaptest(args: &mut Args) {
    let n = args.next().and_then(parse_u64).unwrap_or(100_000) as usize;
    let r = crate::mem::heap::stress(n);
    crate::kprintln!("heaptest : {} objects allocated/freed, {} alloc failures, {}",
        r.objects, r.failed_allocs, if r.ok { "ok, no corruption" } else { "CORRUPTION DETECTED" });
}

fn kind_name(kind: u32) -> &'static str {
    match kind {
        MEM_USABLE => "usable",
        MEM_RESERVED => "reserved",
        MEM_ACPI_RECL => "ACPI reclaimable",
        MEM_ACPI_NVS => "ACPI NVS",
        MEM_BAD => "bad memory",
        MEM_LOADER => "loader",
        MEM_KERNEL => "kernel",
        _ => "unknown",
    }
}
