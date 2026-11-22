// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Which system-call ABI an ELF executable was built for.
//
// PrismOS has its own system call numbering, so a program built for Linux
// cannot run here, whatever its ELF header says.  `execve` therefore looks
// at the program before mapping it: a PrismOS program is loaded as always,
// a Linux one is handed to the compatibility handler (`LINUX_COMPAT`), the
// same way a `#!` script is handed to its interpreter.  Today the handler
// only says that Linux programs are not supported; a real Linux layer can
// replace it later without touching the kernel.
//
// The decision is pure (it reads the program headers already in memory), so
// the host tests include this file as it is (tests/abi_host.rs).
//
// A program is called Linux only on positive evidence, so nothing PrismOS
// builds can be caught by mistake:
//
//   * its PT_INTERP names a Linux dynamic linker (`ld-linux*` for glibc,
//     `ld-musl*` for musl; PrismOS's own is `/lib/ld.so`), or
//   * it carries the GNU ABI tag note (NT_GNU_ABI_TAG, OS = Linux) that
//     glibc puts in every program, static ones included.
//
// A static musl program has neither mark and is not recognised; marking
// PrismOS programs themselves (a note from crt1) is the way to close that.

/// The ABI a program was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abi {
    Prism,
    Linux,
}

/// Where the kernel sends Linux programs: `LINUX_COMPAT <program> args...`.
pub const LINUX_COMPAT: &str = "/usr/sbin/linux-compat";

const PT_INTERP: u32 = 3;
const PT_NOTE: u32 = 4;
const NT_GNU_ABI_TAG: u32 = 1;
const ABI_TAG_OS_LINUX: u32 = 0;

const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const MAX_PHDRS: usize = 64;
/// Notes looked at per PT_NOTE segment: a bound, not a limit anyone reaches.
const MAX_NOTES: usize = 16;

fn rd16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o.checked_add(2)?)?.try_into().ok()?))
}
fn rd32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o.checked_add(4)?)?.try_into().ok()?))
}
fn rd64(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o.checked_add(8)?)?.try_into().ok()?))
}

/// `image[off .. off + len]`, or None when it is not all in `image`.
fn slice(image: &[u8], off: u64, len: u64) -> Option<&[u8]> {
    let end = off.checked_add(len)?;
    image.get(usize::try_from(off).ok()?..usize::try_from(end).ok()?)
}

fn round_up(v: usize, align: usize) -> Option<usize> {
    v.checked_add(align - 1).map(|x| x & !(align - 1))
}

/// A Linux dynamic linker: the file name part of a PT_INTERP path.
fn is_linux_interp(path: &[u8]) -> bool {
    let path = match path.iter().position(|&b| b == 0) {
        Some(n) => &path[..n],
        None => path,
    };
    let base = match path.iter().rposition(|&b| b == b'/') {
        Some(n) => &path[n + 1..],
        None => path,
    };
    base.starts_with(b"ld-linux") || base.starts_with(b"ld-musl")
}

/// Does this PT_NOTE segment hold the GNU ABI tag of a Linux program?
fn has_linux_abi_tag(notes: &[u8], align: usize) -> bool {
    let mut off = 0usize;
    for _ in 0..MAX_NOTES {
        // Also keeps every `off + n` below from overflowing.
        if off >= notes.len() {
            return false;
        }
        let (Some(namesz), Some(descsz), Some(ntype)) = (rd32(notes, off), rd32(notes, off + 4), rd32(notes, off + 8)) else {
            return false;
        };
        let name_at = off + 12;
        let Some(desc_at) = round_up(name_at.saturating_add(namesz as usize), align) else { return false };
        let Some(next) = round_up(desc_at.saturating_add(descsz as usize), align) else { return false };
        if ntype == NT_GNU_ABI_TAG && namesz == 4 && descsz >= 16 {
            if notes.get(name_at..name_at + 4) == Some(b"GNU\0") && rd32(notes, desc_at) == Some(ABI_TAG_OS_LINUX) {
                return true;
            }
        }
        if next <= off {
            return false;
        }
        off = next;
    }
    false
}

/// Classify the ELF whose first bytes are `image` (the file head, which
/// holds the program headers).  Anything that is not a well formed ELF64 is
/// `Abi::Prism`: the loader reports the real error itself.
pub fn classify(image: &[u8]) -> Abi {
    if image.len() < EHDR_SIZE || &image[..4] != b"\x7fELF" || image[4] != 2 || image[5] != 1 {
        return Abi::Prism;
    }
    let (Some(phoff), Some(phentsize), Some(phnum)) = (rd64(image, 32), rd16(image, 54), rd16(image, 56)) else {
        return Abi::Prism;
    };
    let (phentsize, phnum) = (phentsize as usize, phnum as usize);
    if phentsize != PHDR_SIZE || phnum == 0 || phnum > MAX_PHDRS {
        return Abi::Prism;
    }
    for i in 0..phnum {
        // The whole entry must lie in the image: this also keeps every
        // `at + n` below from overflowing.
        let Some(at) = usize::try_from(phoff).ok().and_then(|p| p.checked_add(i * PHDR_SIZE)) else {
            return Abi::Prism;
        };
        if at.checked_add(PHDR_SIZE).map_or(true, |end| end > image.len()) {
            return Abi::Prism;
        }
        let (Some(p_type), Some(p_off), Some(p_filesz), Some(p_align)) =
            (rd32(image, at), rd64(image, at + 8), rd64(image, at + 32), rd64(image, at + 48))
        else {
            return Abi::Prism;
        };
        match p_type {
            PT_INTERP => {
                if let Some(path) = slice(image, p_off, p_filesz) {
                    if is_linux_interp(path) {
                        return Abi::Linux;
                    }
                }
            }
            PT_NOTE => {
                if let Some(notes) = slice(image, p_off, p_filesz) {
                    let align = if p_align >= 8 { 8 } else { 4 };
                    if has_linux_abi_tag(notes, align) {
                        return Abi::Linux;
                    }
                }
            }
            _ => {}
        }
    }
    Abi::Prism
}
