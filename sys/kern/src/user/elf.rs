// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// ELF64 loader (phase 2.4): turns an executable file of the VFS into a ready
// to run address space.
//
// What is accepted
//   * ELF64, little endian, x86-64, `ET_EXEC` or `ET_DYN`.  A static PIE
//     (`ET_DYN`, linked with `-pie --no-dynamic-linker`) is the natural
//     format here: user space starts at USER_BASE (512 GiB), far above the
//     2 GiB the small code model reaches, so the image is loaded at
//     USER_CODE_BASE and its `R_X86_64_RELATIVE` relocations are applied.
//     An `ET_EXEC` must be linked inside user space (USER_BASE..) already.
//   * `PT_LOAD` segments, mapped page by page with their own protection
//     (W^X: a segment that is both writable and executable is refused), the
//     file part copied, the rest (`.bss`) left zero.
//   * No `PT_INTERP`, no `DT_NEEDED`: programs are fully static.
//
// The initial stack follows the System V x86-64 process entry convention
// (what `_start` of any libc expects): at the entry `rsp` is 16-byte aligned
// and points at `argc`, followed by the `argv` pointers, a NULL, the `envp`
// pointers, a NULL and the auxiliary vector (`AT_PHDR`, `AT_PHENT`, `AT_PHNUM`,
// `AT_PAGESZ`, `AT_ENTRY`, `AT_RANDOM`, `AT_EXECFN`, ... terminated by
// `AT_NULL`); the strings themselves sit above, at the top of the stack.

use crate::mem::vmm::{AddressSpace, PAGE, USER_BASE, USER_CODE_BASE, USER_END, USER_SIGCODE, USER_STACK_TOP};
use alloc::string::String;
use alloc::vec::Vec;

/// Stack given to ELF programs (the guard page below stays unmapped).
pub const ELF_STACK_SIZE: u64 = 16 * PAGE;
/// Largest executable the loader reads.
pub const MAX_ELF_FILE: usize = 4 * 1024 * 1024;
/// Largest memory footprint of the loaded segments.
const MAX_IMAGE_SPAN: u64 = 64 * 1024 * 1024;
/// Limits on what `execve` copies onto the new stack.
pub const MAX_ARGS: usize = 64;
pub const MAX_STRING: usize = 4096;
pub const MAX_ARG_BYTES: usize = 24 * 1024;
const MAX_PHDRS: usize = 32;
const MAX_RELOCS: u64 = 1 << 16;

const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_RELAENT: u64 = 9;
const DT_REL: u64 = 17;
const DT_JMPREL: u64 = 23;
const R_X86_64_NONE: u32 = 0;
const R_X86_64_RELATIVE: u32 = 8;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_FLAGS: u64 = 8;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

/// Why a program could not be started.  `errno()` gives the matching Linux
/// style code for `execve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecError {
    /// The file could not be read (VFS error text).
    File(&'static str, i64),
    /// Not a loadable ELF (wrong magic / class / machine ...).
    NotElf(&'static str),
    /// A valid ELF using something the loader does not do.
    Unsupported(&'static str),
    /// Header or segment table inconsistent.
    Corrupt(&'static str),
    NoMemory,
    /// argv / envp too large.
    TooBig,
}

impl ExecError {
    pub fn errno(&self) -> i64 {
        match self {
            ExecError::File(_, e) => *e,
            ExecError::NotElf(_) | ExecError::Unsupported(_) | ExecError::Corrupt(_) => 8, // ENOEXEC
            ExecError::NoMemory => 12,                                                    // ENOMEM
            ExecError::TooBig => 7,                                                       // E2BIG
        }
    }

    pub fn what(&self) -> &'static str {
        match self {
            ExecError::File(s, _) => s,
            ExecError::NotElf(s) | ExecError::Unsupported(s) | ExecError::Corrupt(s) => s,
            ExecError::NoMemory => "out of memory",
            ExecError::TooBig => "argument list too long",
        }
    }
}

/// A program ready to run: its address space (code, data, bss and stack) and
/// the initial register state.
pub struct Loaded {
    pub aspace: AddressSpace,
    pub entry: u64,
    pub rsp: u64,
    pub argc: usize,
    /// Highest address used by a loaded segment (page aligned).
    pub image_end: u64,
}

#[derive(Clone, Copy)]
struct Seg {
    off: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    flags: u32,
}

fn rd16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn rd32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn rd64(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

fn page_down(x: u64) -> u64 {
    x & !(PAGE - 1)
}
fn page_up(x: u64) -> Option<u64> {
    x.checked_add(PAGE - 1).map(page_down)
}

/// Read an executable from the VFS and load it (the `execve` front end).
pub fn load_path(path: &str, argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<Loaded, ExecError> {
    use crate::fs::vfs::{FileType, FsError};
    let vfs = crate::fs::fsys::vfs();
    let errno = |e: FsError| -> ExecError {
        let code = match e {
            FsError::NotFound | FsError::NotDir => 2, // ENOENT
            FsError::IsDir => 21,                      // EISDIR
            FsError::NameTooLong => 36,                // ENAMETOOLONG
            FsError::TooBig => 8,                      // ENOEXEC
            FsError::NoSpace => 12,
            FsError::Loop => 40,
            _ => 5, // EIO
        };
        ExecError::File(e.as_str(), code)
    };
    let st = vfs.stat(path).map_err(errno)?;
    if st.ftype != FileType::Regular {
        return Err(if st.ftype == FileType::Dir {
            ExecError::File("is a directory", 21)
        } else {
            ExecError::File("not a regular file", 13)
        });
    }
    if st.mode & 0o111 == 0 {
        return Err(ExecError::File("permission denied (not executable)", 13)); // EACCES
    }
    let image = vfs.read_file(path, MAX_ELF_FILE).map_err(errno)?;
    load(&image, path.as_bytes(), argv, envp)
}

/// Build the address space of the ELF `image`.  `execfn` is the path shown
/// to the program (`AT_EXECFN`).
pub fn load(image: &[u8], execfn: &[u8], argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<Loaded, ExecError> {
    // ---- ELF header
    if image.len() < 64 || &image[..4] != b"\x7fELF" {
        return Err(ExecError::NotElf("not an ELF file"));
    }
    if image[4] != 2 || image[5] != 1 || image[6] != 1 {
        return Err(ExecError::NotElf("not a little-endian ELF64 file"));
    }
    let e_type = rd16(image, 16).unwrap();
    if rd16(image, 18).unwrap() != EM_X86_64 {
        return Err(ExecError::NotElf("not an x86-64 executable"));
    }
    if e_type != ET_EXEC && e_type != ET_DYN {
        return Err(ExecError::NotElf("not an executable (ET_EXEC / ET_DYN)"));
    }
    let e_entry = rd64(image, 24).unwrap();
    let e_phoff = rd64(image, 32).unwrap();
    let e_phentsize = rd16(image, 54).unwrap() as usize;
    let e_phnum = rd16(image, 56).unwrap() as usize;
    if e_phentsize != 56 || e_phnum == 0 || e_phnum > MAX_PHDRS {
        return Err(ExecError::Corrupt("bad program header table"));
    }
    let ph_end = (e_phoff as usize)
        .checked_add(e_phnum * 56)
        .ok_or(ExecError::Corrupt("bad program header table"))?;
    if ph_end > image.len() {
        return Err(ExecError::Corrupt("program headers past the end of the file"));
    }

    // ---- program headers
    let mut segs: Vec<Seg> = Vec::new();
    let mut dynamic: Option<Seg> = None;
    for i in 0..e_phnum {
        let o = e_phoff as usize + i * 56;
        let p_type = rd32(image, o).unwrap();
        let s = Seg {
            flags: rd32(image, o + 4).unwrap(),
            off: rd64(image, o + 8).unwrap(),
            vaddr: rd64(image, o + 16).unwrap(),
            filesz: rd64(image, o + 32).unwrap(),
            memsz: rd64(image, o + 40).unwrap(),
        };
        match p_type {
            PT_INTERP => return Err(ExecError::Unsupported("dynamically linked (PT_INTERP): no dynamic linker")),
            PT_DYNAMIC => dynamic = Some(s),
            PT_LOAD => {
                if s.memsz == 0 {
                    continue;
                }
                if s.filesz > s.memsz {
                    return Err(ExecError::Corrupt("segment file size exceeds its memory size"));
                }
                match s.off.checked_add(s.filesz) {
                    Some(e) if e as usize <= image.len() => {}
                    _ => return Err(ExecError::Corrupt("segment past the end of the file")),
                }
                if s.flags & PF_W != 0 && s.flags & PF_X != 0 {
                    return Err(ExecError::Unsupported("writable and executable segment (W^X)"));
                }
                segs.push(s);
            }
            _ => {}
        }
    }
    if segs.is_empty() {
        return Err(ExecError::Corrupt("no loadable segment"));
    }
    segs.sort_by_key(|s| s.vaddr);

    // ---- placement
    let lo = page_down(segs[0].vaddr);
    let mut hi_raw = 0u64;
    for s in &segs {
        let end = s.vaddr.checked_add(s.memsz).ok_or(ExecError::Corrupt("segment wraps around"))?;
        hi_raw = hi_raw.max(end);
    }
    let hi = page_up(hi_raw).ok_or(ExecError::Corrupt("segment wraps around"))?;
    if hi - lo > MAX_IMAGE_SPAN {
        return Err(ExecError::Unsupported("image too large"));
    }
    let bias: u64 = if e_type == ET_DYN {
        USER_CODE_BASE.wrapping_sub(lo)
    } else {
        0
    };
    let load_lo = lo.wrapping_add(bias);
    let load_hi = hi.wrapping_add(bias);
    let stack_bottom = USER_STACK_TOP - ELF_STACK_SIZE;
    if load_lo < USER_BASE || load_hi > stack_bottom - PAGE || load_hi < load_lo {
        return Err(ExecError::Unsupported("segments outside user address space"));
    }
    // The signal-return trampoline page (phase 2.5) is mapped at a fixed
    // address; an image reaching it would collide with the kernel's code.
    if page_down(vmm_sigcode()) <= load_hi && load_lo <= page_up(vmm_sigcode()).unwrap() {
        return Err(ExecError::Unsupported("segments overlap the signal trampoline page"));
    }
    // Segments must not share a page (each page has one protection).
    for w in segs.windows(2) {
        let prev_end = page_up(w[0].vaddr + w[0].memsz).unwrap();
        if page_down(w[1].vaddr) < prev_end {
            return Err(ExecError::Unsupported("segments share a page (link with -z max-page-size=4096)"));
        }
    }
    let entry = e_entry.wrapping_add(bias);
    if !segs.iter().any(|s| {
        s.flags & PF_X != 0 && e_entry >= s.vaddr && e_entry < s.vaddr + s.memsz
    }) {
        return Err(ExecError::Corrupt("entry point outside an executable segment"));
    }

    // ---- build the address space
    let mut aspace = AddressSpace::new().ok_or(ExecError::NoMemory)?;
    for s in &segs {
        let start = page_down(s.vaddr).wrapping_add(bias);
        let len = page_up(s.vaddr + s.memsz).unwrap() - page_down(s.vaddr);
        if !aspace.alloc_map(start, len, s.flags & PF_W != 0, s.flags & PF_X != 0) {
            return Err(ExecError::NoMemory);
        }
        let data = &image[s.off as usize..(s.off + s.filesz) as usize];
        if !data.is_empty() && !aspace.write(s.vaddr.wrapping_add(bias), data, true) {
            return Err(ExecError::Corrupt("cannot copy a segment"));
        }
    }

    // ---- relocations (static PIE)
    if let Some(d) = dynamic {
        apply_relocs(&mut aspace, d, bias)?;
    }

    // ---- AT_PHDR: where the program header table ended up in memory
    let phdr_addr = segs.iter().find_map(|s| {
        if e_phoff >= s.off && e_phoff + (e_phnum as u64) * 56 <= s.off + s.filesz {
            Some(s.vaddr.wrapping_add(bias) + (e_phoff - s.off))
        } else {
            None
        }
    });

    // ---- stack
    if !aspace.alloc_map(stack_bottom, ELF_STACK_SIZE, true, false) {
        return Err(ExecError::NoMemory);
    }
    let rsp = build_stack(&aspace, stack_bottom, argv, envp, execfn, entry, phdr_addr, e_phnum)?;
    // ---- signal-return trampoline (phase 2.5): one r-x page holding the
    // `sigreturn` code a handler returns into.  A fork child inherits it
    // shared through the copy-on-write clone.
    if !aspace.map_sigcode() {
        return Err(ExecError::NoMemory);
    }
    Ok(Loaded { aspace, entry, rsp, argc: argv.len(), image_end: load_hi })
}

/// The trampoline address as a plain value (import gymnastics-free).
fn vmm_sigcode() -> u64 {
    USER_SIGCODE
}

/// Apply the `R_X86_64_RELATIVE` relocations listed by `DT_RELA`.
fn apply_relocs(aspace: &mut AddressSpace, dynseg: Seg, bias: u64) -> Result<(), ExecError> {
    let base = dynseg.vaddr.wrapping_add(bias);
    let n = (dynseg.memsz / 16).min(512);
    let (mut rela, mut relasz, mut relaent) = (0u64, 0u64, 24u64);
    for i in 0..n {
        let mut e = [0u8; 16];
        if !aspace.read(base + i * 16, &mut e) {
            return Err(ExecError::Corrupt("unreadable dynamic section"));
        }
        let tag = u64::from_le_bytes(e[..8].try_into().unwrap());
        let val = u64::from_le_bytes(e[8..].try_into().unwrap());
        match tag {
            DT_NULL => break,
            DT_NEEDED => return Err(ExecError::Unsupported("needs shared libraries (DT_NEEDED)")),
            DT_REL => return Err(ExecError::Unsupported("REL relocations (only RELA)")),
            DT_JMPREL => return Err(ExecError::Unsupported("PLT relocations (link statically)")),
            DT_RELA => rela = val,
            DT_RELASZ => relasz = val,
            DT_RELAENT => relaent = val,
            _ => {}
        }
    }
    if relasz == 0 {
        return Ok(());
    }
    if relaent != 24 || relasz % 24 != 0 || relasz / 24 > MAX_RELOCS {
        return Err(ExecError::Corrupt("bad relocation table"));
    }
    for i in 0..relasz / 24 {
        let mut r = [0u8; 24];
        if !aspace.read(rela.wrapping_add(bias) + i * 24, &mut r) {
            return Err(ExecError::Corrupt("unreadable relocation table"));
        }
        let off = u64::from_le_bytes(r[..8].try_into().unwrap());
        let info = u64::from_le_bytes(r[8..16].try_into().unwrap());
        let addend = u64::from_le_bytes(r[16..].try_into().unwrap());
        match (info & 0xFFFF_FFFF) as u32 {
            R_X86_64_NONE => {}
            R_X86_64_RELATIVE => {
                let val = bias.wrapping_add(addend);
                // `force`: RELRO / read-only data pages are patched too.
                if !aspace.write(off.wrapping_add(bias), &val.to_le_bytes(), true) {
                    return Err(ExecError::Corrupt("relocation outside the image"));
                }
            }
            _ => return Err(ExecError::Unsupported("relocation type (static PIE needs RELATIVE only)")),
        }
    }
    Ok(())
}

/// A few pseudo-random bytes for AT_RANDOM (not cryptographic: there is no
/// entropy source yet).
fn random16() -> [u8; 16] {
    // SAFETY: rdtsc is available on every x86-64 CPU.
    let mut x = unsafe { core::arch::x86_64::_rdtsc() } ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = [0u8; 16];
    for b in out.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 24) as u8;
    }
    out
}

/// Lay out strings, auxv, envp, argv and argc on the new stack and return the
/// initial `rsp`.
fn build_stack(
    aspace: &AddressSpace,
    stack_bottom: u64,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    execfn: &[u8],
    entry: u64,
    phdr: Option<u64>,
    phnum: usize,
) -> Result<u64, ExecError> {
    if argv.len() > MAX_ARGS || envp.len() > MAX_ARGS {
        return Err(ExecError::TooBig);
    }
    let total: usize = argv.iter().chain(envp.iter()).map(|s| s.len() + 1).sum::<usize>() + execfn.len() + 1;
    if total > MAX_ARG_BYTES {
        return Err(ExecError::TooBig);
    }
    let mut sp = USER_STACK_TOP;
    let put = |sp: &mut u64, s: &[u8]| -> Result<u64, ExecError> {
        *sp -= s.len() as u64 + 1;
        let mut buf = Vec::with_capacity(s.len() + 1);
        buf.extend_from_slice(s);
        buf.push(0);
        if !aspace.write(*sp, &buf, false) {
            return Err(ExecError::Corrupt("cannot write the initial stack"));
        }
        Ok(*sp)
    };
    let execfn_p = put(&mut sp, execfn)?;
    let mut env_p = Vec::with_capacity(envp.len());
    for s in envp {
        env_p.push(put(&mut sp, s)?);
    }
    let mut arg_p = Vec::with_capacity(argv.len());
    for s in argv {
        arg_p.push(put(&mut sp, s)?);
    }
    sp -= 16;
    if !aspace.write(sp, &random16(), false) {
        return Err(ExecError::Corrupt("cannot write the initial stack"));
    }
    let random_p = sp;
    sp &= !15;

    let mut aux: Vec<(u64, u64)> = Vec::new();
    if let Some(p) = phdr {
        aux.push((AT_PHDR, p));
    }
    aux.push((AT_PHENT, 56));
    aux.push((AT_PHNUM, phnum as u64));
    aux.push((AT_PAGESZ, PAGE));
    aux.push((AT_BASE, 0));
    aux.push((AT_FLAGS, 0));
    aux.push((AT_ENTRY, entry));
    aux.push((AT_UID, 0));
    aux.push((AT_EUID, 0));
    aux.push((AT_GID, 0));
    aux.push((AT_EGID, 0));
    aux.push((AT_CLKTCK, 100));
    aux.push((AT_SECURE, 0));
    aux.push((AT_RANDOM, random_p));
    aux.push((AT_EXECFN, execfn_p));
    aux.push((AT_NULL, 0));

    let mut words: Vec<u64> = Vec::new();
    words.push(argv.len() as u64);
    words.extend_from_slice(&arg_p);
    words.push(0);
    words.extend_from_slice(&env_p);
    words.push(0);
    for (k, v) in &aux {
        words.push(*k);
        words.push(*v);
    }
    let rsp = (sp - words.len() as u64 * 8) & !15;
    // Leave the program some room below the arguments.
    if rsp < stack_bottom + 4 * PAGE {
        return Err(ExecError::TooBig);
    }
    let mut bytes = Vec::with_capacity(words.len() * 8);
    for w in &words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    if !aspace.write(rsp, &bytes, false) {
        return Err(ExecError::Corrupt("cannot write the initial stack"));
    }
    Ok(rsp)
}

/// Last path component (process name shown by `ps`).
pub fn base_name(path: &str) -> String {
    String::from(path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(path))
}

const _: () = assert!(USER_END > USER_BASE);
