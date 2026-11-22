// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The ELF loader of the Linux layer (Linux: fs/binfmt_elf.c), minimal.
//
// What it does, in Linux's order:
//
//   1. header.rs   checks the ELF header, reads the program headers, finds
//                  PT_INTERP and PT_GNU_STACK;
//   2. layout.rs   places the image: load bias, page runs and protections,
//                  program break, AT_PHDR (and the same for the program
//                  interpreter, which gets its own base);
//   3. (this file) maps the runs in a fresh address space and fills them
//                  from the file;
//   4. stack.rs    builds the initial stack: argc, argv, envp and the
//                  auxiliary vector Linux's C libraries expect.
//
// What it leaves out, on purpose: relocations (a dynamic linker or a static
// PIE's start code does them, as on Linux), TLS (the C library's job),
// ASLR, the vDSO (AT_SYSINFO_EHDR; glibc and musl work without), core dump
// notes, `binfmt_misc`, and the mmap-based mapping -- file bytes are copied.
//
// Differences from Linux, all deliberate:
//
//   * a segment that is both writable and executable is refused;
//   * a program is placed inside PrismOS's user address space (from 512 GiB
//     up): a non-PIE `ET_EXEC` linked at the usual 0x400000 is refused,
//     PIE and static-PIE programs load;
//   * the stack is the fixed size every PrismOS process gets.
//
// Nothing calls `load_elf_binary` yet: see linux/mod.rs.

pub mod error;
pub mod header;
pub mod layout;
pub mod stack;

use crate::linux::uapi::errno::*;
use crate::mem::vmm::{AddressSpace, PAGE, USER_BASE, USER_CODE_BASE, USER_INTERP_BASE, USER_SIGCODE, USER_STACK_TOP};
use crate::user::elf::{read_program, ExecError, Loaded, Source, ELF_STACK_SIZE};
use alloc::vec::Vec;
use error::LoadError;
use layout::{Layout, Limits};

/// Largest span of one image (the same cap the PrismOS loader uses).
const MAX_IMAGE_SPAN: u64 = 64 * 1024 * 1024;
/// File bytes copied per read.
const COPY_CHUNK: usize = 64 * 1024;
/// Room kept between the arguments and the bottom of the stack.
const STACK_HEADROOM: u64 = 4 * PAGE;
/// `times()` ticks per second, what AT_CLKTCK reports.
const CLKTCK: u64 = 100;

impl From<LoadError> for ExecError {
    fn from(e: LoadError) -> ExecError {
        ExecError::File(e.what, e.errno as i64)
    }
}

fn from_exec(e: ExecError) -> LoadError {
    LoadError::new(e.errno() as i32, e.what())
}

/// Build the address space of the ELF program `src` for Linux.  `execfn` is
/// the path the program sees (`AT_EXECFN`).  Returns what `execve` needs to
/// start it: the entry point (the interpreter's, when there is one), the
/// initial `rsp`, and where the program break starts (`image_end`).
pub fn load_elf_binary(
    src: &dyn Source,
    execfn: &[u8],
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
) -> Result<Loaded, LoadError> {
    let head = src.head();
    let hdr = header::parse(head, src.len())?;

    let stack_bottom = USER_STACK_TOP - ELF_STACK_SIZE;
    let limits = Limits { lo: USER_BASE, hi: stack_bottom - PAGE, max_span: MAX_IMAGE_SPAN };
    let main = layout::plan(&hdr, USER_CODE_BASE, &limits)?;
    keep_clear_of_sigcode(&main)?;

    // The program interpreter, read before any address-space work so that a
    // bad one fails the exec cleanly.  Whatever is wrong with it is
    // ELIBBAD, except a missing or unreadable file, which keeps its errno.
    let interp = match hdr.interp_path(head) {
        None => None,
        Some(path) => {
            let path = core::str::from_utf8(path).map_err(|_| LoadError::new(ELIBBAD, "non-UTF-8 PT_INTERP path"))?;
            let prog = read_program(path).map_err(from_exec)?;
            let ih = header::parse(prog.head(), prog.len()).map_err(|e| LoadError::new(ELIBBAD, e.what))?;
            let il = layout::plan(&ih, USER_INTERP_BASE, &limits).map_err(|e| LoadError::new(ELIBBAD, e.what))?;
            keep_clear_of_sigcode(&il)?;
            if !(main.hi <= il.lo || il.hi <= main.lo) {
                return Err(LoadError::new(ELIBBAD, "the program and its interpreter overlap"));
            }
            Some((prog, il))
        }
    };

    let mut aspace = AddressSpace::new().ok_or(LoadError::new(ENOMEM, "out of memory"))?;
    map_image(&mut aspace, src, &main)?;
    if let Some((prog, il)) = &interp {
        map_image(&mut aspace, prog, il)?;
    }

    // ---- the stack
    if !aspace.alloc_map(stack_bottom, ELF_STACK_SIZE, true, hdr.exec_stack) {
        return Err(LoadError::new(ENOMEM, "out of memory"));
    }
    let (uid, euid, gid, egid) = match crate::user::process::current() {
        Some(p) => (p.cred.uid, p.cred.euid, p.cred.gid, p.cred.egid),
        None => (0, 0, 0, 0),
    };
    // SAFETY: CPUID leaf 1 exists on every x86-64 CPU.  (Recent compilers
    // made the intrinsic safe; older ones still need the block.)
    #[allow(unused_unsafe)]
    let hwcap = unsafe { core::arch::x86_64::__cpuid(1) }.edx as u64;
    let aux = stack::Aux {
        phdr: main.phdr.unwrap_or(0),
        phnum: hdr.ehdr.e_phnum as u64,
        entry: main.entry,
        base: interp.as_ref().map_or(0, |(_, il)| il.bias),
        hwcap,
        hwcap2: 0,
        clktck: CLKTCK,
        uid: uid as u64,
        euid: euid as u64,
        gid: gid as u64,
        egid: egid as u64,
        secure: false,
    };
    let image = stack::build(
        USER_STACK_TOP,
        stack_bottom + STACK_HEADROOM,
        argv,
        envp,
        execfn,
        &aux,
        crate::random::bytes16(),
    )?;
    if !aspace.write(image.rsp, &image.bytes, false) {
        return Err(LoadError::new(EINVAL, "cannot write the initial stack"));
    }

    // PrismOS's signal-return trampoline page, mapped in every process.
    if !aspace.map_sigcode() {
        return Err(LoadError::new(ENOMEM, "out of memory"));
    }

    let entry = interp.as_ref().map_or(main.entry, |(_, il)| il.entry);
    Ok(Loaded { aspace, entry, rsp: image.rsp, argc: argv.len(), image_end: main.brk })
}

/// The trampoline page sits at a fixed address; an image reaching it would
/// collide with the kernel's code.
fn keep_clear_of_sigcode(l: &Layout) -> Result<(), LoadError> {
    let page = USER_SIGCODE & !(PAGE - 1);
    if l.lo <= page + PAGE && page < l.hi {
        return Err(LoadError::new(ENOMEM, "segments overlap the signal trampoline page"));
    }
    Ok(())
}

/// Map the runs of `l` and fill them from `src`.
fn map_image(aspace: &mut AddressSpace, src: &dyn Source, l: &Layout) -> Result<(), LoadError> {
    for r in &l.runs {
        if !aspace.alloc_map(r.start, r.end - r.start, r.w, r.x) {
            return Err(LoadError::new(ENOMEM, "out of memory"));
        }
    }
    // File bytes in chunks, never the whole file at once.  `force`: a code
    // page is not user-writable, the loader fills it anyway.
    let mut buf: Vec<u8> = Vec::new();
    buf.try_reserve_exact(COPY_CHUNK).map_err(|_| LoadError::new(ENOMEM, "out of memory"))?;
    buf.resize(COPY_CHUNK, 0);
    for f in &l.fills {
        let mut done = 0u64;
        while done < f.filesz {
            let want = buf.len().min((f.filesz - done) as usize);
            let got = src.read_at(f.offset + done, &mut buf[..want]).map_err(from_exec)?;
            if got == 0 {
                return Err(LoadError::new(ENOEXEC, "segment past the end of the file"));
            }
            if !aspace.write(f.vaddr.wrapping_add(done), &buf[..got], true) {
                return Err(LoadError::new(EINVAL, "cannot copy a segment"));
            }
            done += got as u64;
        }
    }
    Ok(())
}
