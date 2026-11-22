// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Second step: decide where an image goes.  From the program headers this
// computes the load bias, the page runs to map and with which protection,
// the program break, and the values the auxiliary vector needs (entry point,
// address of the program header table).  The kernel never relocates
// anything: a dynamic linker, or a static PIE's own start code, does that.
//
// Pure: no address space is touched, so every rule here is tested on the
// host.

use super::error::LoadError;
use super::header::Headers;
use crate::linux::uapi::elf::*;
use crate::linux::uapi::errno::*;
use alloc::vec::Vec;

pub const PAGE: u64 = 4096;

/// Where an image may be placed.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// First usable address (inclusive) and end of the usable range
    /// (exclusive): the image must fit in `[lo, hi)`.
    pub lo: u64,
    pub hi: u64,
    /// Largest span from the first to the last page of an image.
    pub max_span: u64,
}

/// Pages mapped with one protection (always readable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// Page aligned, biased: `[start, end)`.
    pub start: u64,
    pub end: u64,
    pub w: bool,
    pub x: bool,
}

/// File bytes to copy into the runs: `filesz` bytes from file offset `offset`
/// to the (biased) address `vaddr`.  The rest of the segment up to its
/// memory size stays zero (the bss).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
}

#[derive(Debug, Clone)]
pub struct Layout {
    /// Added to every address of the file: 0 for `ET_EXEC`, the distance
    /// from the link-time base to where an `ET_DYN` lands.
    pub bias: u64,
    pub runs: Vec<Run>,
    pub fills: Vec<Fill>,
    /// First and one past the last page of the image (biased).
    pub lo: u64,
    pub hi: u64,
    /// Biased entry point.
    pub entry: u64,
    /// Where the program break starts: the page after the last segment.
    pub brk: u64,
    /// Biased address of the program header table in memory (`AT_PHDR`).
    pub phdr: Option<u64>,
}

fn page_down(x: u64) -> u64 {
    x & !(PAGE - 1)
}
fn page_up(x: u64) -> Option<u64> {
    x.checked_add(PAGE - 1).map(page_down)
}

/// Lay out the image described by `h`.  An `ET_DYN` is placed so that its
/// first page lands on `anchor` (page aligned); an `ET_EXEC` stays at its
/// link-time addresses.
pub fn plan(h: &Headers, anchor: u64, lim: &Limits) -> Result<Layout, LoadError> {
    let mut loads: Vec<Phdr> = h.loads().copied().collect();
    loads.sort_by_key(|p| p.p_vaddr);
    for w in loads.windows(2) {
        if w[0].p_vaddr + w[0].p_memsz > w[1].p_vaddr {
            return Err(LoadError::new(ENOEXEC, "overlapping segments"));
        }
    }
    let first = loads.first().ok_or(LoadError::new(ENOEXEC, "no loadable segment"))?;
    let lo = page_down(first.p_vaddr);
    let hi_raw = loads.iter().map(|p| p.p_vaddr + p.p_memsz).max().unwrap();
    let hi = page_up(hi_raw).ok_or(LoadError::new(ENOEXEC, "segment wraps around the address space"))?;
    if hi - lo > lim.max_span {
        return Err(LoadError::new(ENOMEM, "image too large"));
    }

    let bias = if h.ehdr.e_type == ET_DYN {
        if anchor & (PAGE - 1) != 0 {
            return Err(LoadError::new(EINVAL, "unaligned load address"));
        }
        anchor.wrapping_sub(lo)
    } else {
        0
    };
    let (load_lo, load_hi) = (lo.wrapping_add(bias), hi.wrapping_add(bias));
    if load_lo < lim.lo || load_hi > lim.hi || load_hi <= load_lo {
        return Err(LoadError::new(ENOMEM, "segments outside the user address space"));
    }

    // Page runs: segments that share a page share its mapping, with the
    // union of their protections.
    let mut runs: Vec<Run> = Vec::new();
    for p in &loads {
        let start = page_down(p.p_vaddr).wrapping_add(bias);
        let end = page_up(p.p_vaddr + p.p_memsz).unwrap().wrapping_add(bias);
        let (w, x) = (p.p_flags & PF_W != 0, p.p_flags & PF_X != 0);
        match runs.last_mut() {
            Some(last) if start < last.end => {
                last.end = last.end.max(end);
                last.w |= w;
                last.x |= x;
            }
            _ => runs.push(Run { start, end, w, x }),
        }
    }
    if runs.iter().any(|r| r.w && r.x) {
        return Err(LoadError::new(ENOEXEC, "writable and executable segment"));
    }

    let fills = loads
        .iter()
        .filter(|p| p.p_filesz > 0)
        .map(|p| Fill { offset: p.p_offset, vaddr: p.p_vaddr.wrapping_add(bias), filesz: p.p_filesz })
        .collect();

    let e_entry = h.ehdr.e_entry;
    if !loads
        .iter()
        .any(|p| p.p_flags & PF_X != 0 && e_entry >= p.p_vaddr && e_entry < p.p_vaddr + p.p_memsz)
    {
        return Err(LoadError::new(ENOEXEC, "entry point outside an executable segment"));
    }

    // AT_PHDR: the program header table as the segment that holds it maps it.
    let table_len = h.ehdr.e_phnum as u64 * PHDR_SIZE as u64;
    let phdr = loads.iter().find_map(|p| {
        let off = h.ehdr.e_phoff;
        let end = off.checked_add(table_len)?;
        let seg_end = p.p_offset.checked_add(p.p_filesz)?;
        (off >= p.p_offset && end <= seg_end).then(|| (off - p.p_offset + p.p_vaddr).wrapping_add(bias))
    });

    Ok(Layout {
        bias,
        runs,
        fills,
        lo: load_lo,
        hi: load_hi,
        entry: e_entry.wrapping_add(bias),
        brk: load_hi,
        phdr,
    })
}
