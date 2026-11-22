// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// First step of loading an ELF program: decide it is one we can run and
// read what the later steps need from its headers -- the program headers,
// the interpreter's path, whether the stack must be executable.
//
// Pure: it works on the head of the file (its first bytes, where the ELF
// header and the program header table live) and knows nothing of address
// spaces.

use super::error::LoadError;
use crate::linux::uapi::elf::*;
use crate::linux::uapi::errno::*;
use alloc::vec::Vec;

/// The program header table may not exceed 64 KiB (a bound, not a goal).
const MAX_PHNUM: usize = 65536 / PHDR_SIZE;
/// Longest program interpreter path.
const PATH_MAX: u64 = 4096;

/// What the headers say about a program.
#[derive(Debug, Clone)]
pub struct Headers {
    pub ehdr: Ehdr,
    pub phdrs: Vec<Phdr>,
    /// The `PT_INTERP` path as a byte range of the file head, without its
    /// NUL.  Use `interp_path`.
    interp: Option<(usize, usize)>,
    /// `PT_GNU_STACK` asked for an executable stack.  Without the header the
    /// stack is not executable (the x86-64 default).
    pub exec_stack: bool,
}

impl Headers {
    /// The program interpreter's path, when the program asks for one.
    pub fn interp_path<'a>(&self, head: &'a [u8]) -> Option<&'a [u8]> {
        self.interp.map(|(s, e)| &head[s..e])
    }

    /// The `PT_LOAD` segments that occupy memory.
    pub fn loads(&self) -> impl Iterator<Item = &Phdr> {
        self.phdrs.iter().filter(|p| p.p_type == PT_LOAD && p.p_memsz > 0)
    }
}

/// Check that `head` starts an ELF64 x86-64 `ET_EXEC` / `ET_DYN` program
/// and decode its program headers.  `file_len` is the size of the whole
/// file: segments must lie inside it.
pub fn parse(head: &[u8], file_len: u64) -> Result<Headers, LoadError> {
    let ehdr = Ehdr::parse(head).ok_or(LoadError::new(ENOEXEC, "not an ELF file"))?;
    if &ehdr.ident[..4] != b"\x7fELF" {
        return Err(LoadError::new(ENOEXEC, "not an ELF file"));
    }
    if ehdr.ident[EI_CLASS] != ELFCLASS64 || ehdr.ident[EI_DATA] != ELFDATA2LSB {
        return Err(LoadError::new(ENOEXEC, "not a little-endian ELF64 file"));
    }
    if ehdr.e_machine != EM_X86_64 {
        return Err(LoadError::new(ENOEXEC, "not an x86-64 executable"));
    }
    if ehdr.e_type != ET_EXEC && ehdr.e_type != ET_DYN {
        return Err(LoadError::new(ENOEXEC, "not an executable (ET_EXEC / ET_DYN)"));
    }
    let phnum = ehdr.e_phnum as usize;
    if ehdr.e_phentsize as usize != PHDR_SIZE || phnum == 0 || phnum > MAX_PHNUM {
        return Err(LoadError::new(ENOEXEC, "bad program header table"));
    }
    let (table_off, _) = usize::try_from(ehdr.e_phoff)
        .ok()
        .and_then(|o| o.checked_add(phnum * PHDR_SIZE).map(|e| (o, e)))
        .filter(|&(_, e)| e <= head.len())
        .ok_or(LoadError::new(ENOEXEC, "program headers outside the start of the file"))?;

    let mut phdrs = Vec::new();
    phdrs
        .try_reserve_exact(phnum)
        .map_err(|_| LoadError::new(ENOMEM, "out of memory"))?;
    let mut interp = None;
    let mut exec_stack = false;
    let mut loads = 0usize;
    for i in 0..phnum {
        let at = table_off + i * PHDR_SIZE;
        let ph = Phdr::parse(&head[at..at + PHDR_SIZE]).ok_or(LoadError::new(ENOEXEC, "bad program header table"))?;
        match ph.p_type {
            PT_INTERP if interp.is_none() => interp = Some(interp_range(head, &ph)?),
            // Several PT_GNU_STACK are nonsense; the last one wins.
            PT_GNU_STACK => exec_stack = ph.p_flags & PF_X != 0,
            PT_LOAD => {
                check_load(&ph, file_len)?;
                if ph.p_memsz > 0 {
                    loads += 1;
                }
            }
            _ => {}
        }
        phdrs.push(ph);
    }
    if loads == 0 {
        return Err(LoadError::new(ENOEXEC, "no loadable segment"));
    }
    Ok(Headers { ehdr, phdrs, interp, exec_stack })
}

/// Validate a `PT_INTERP` and locate its path: a NUL-terminated string
/// of at least one character, inside the file head.
fn interp_range(head: &[u8], ph: &Phdr) -> Result<(usize, usize), LoadError> {
    if ph.p_filesz < 2 || ph.p_filesz > PATH_MAX {
        return Err(LoadError::new(ENOEXEC, "bad PT_INTERP size"));
    }
    let start = usize::try_from(ph.p_offset).map_err(|_| LoadError::new(ENOEXEC, "PT_INTERP outside the file head"))?;
    let end = start
        .checked_add(ph.p_filesz as usize)
        .filter(|&e| e <= head.len())
        .ok_or(LoadError::new(ENOEXEC, "PT_INTERP outside the start of the file"))?;
    let bytes = &head[start..end];
    if bytes[bytes.len() - 1] != 0 {
        return Err(LoadError::new(ENOEXEC, "unterminated PT_INTERP path"));
    }
    // A C string: the path stops at its first NUL.
    let len = bytes.iter().position(|&b| b == 0).unwrap();
    if len == 0 {
        return Err(LoadError::new(ENOEXEC, "empty PT_INTERP path"));
    }
    Ok((start, start + len))
}

/// A `PT_LOAD` must fit its memory image and the file.
fn check_load(ph: &Phdr, file_len: u64) -> Result<(), LoadError> {
    if ph.p_filesz > ph.p_memsz {
        return Err(LoadError::new(ENOEXEC, "segment file size exceeds its memory size"));
    }
    if ph.p_vaddr.checked_add(ph.p_memsz).is_none() {
        return Err(LoadError::new(ENOEXEC, "segment wraps around the address space"));
    }
    match ph.p_offset.checked_add(ph.p_filesz) {
        Some(end) if end <= file_len => Ok(()),
        _ => Err(LoadError::new(ENOEXEC, "segment past the end of the file")),
    }
}
