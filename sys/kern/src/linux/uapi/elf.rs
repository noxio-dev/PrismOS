// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The ELF ABI as Linux user space sees it (include/uapi/linux/elf.h),
// narrowed to ELF64 little endian / x86-64 and to what the loader reads.
// The values are those of the System V ABI, the x86-64 psABI and the
// auxiliary vector keys Linux defines.

// ---- e_ident
pub const EI_CLASS: usize = 4;
pub const EI_DATA: usize = 5;
pub const ELFCLASS64: u8 = 2;
pub const ELFDATA2LSB: u8 = 1;

// ---- e_type / e_machine
pub const ET_EXEC: u16 = 2;
pub const ET_DYN: u16 = 3;
pub const EM_X86_64: u16 = 62;

// ---- p_type
pub const PT_LOAD: u32 = 1;
pub const PT_INTERP: u32 = 3;
pub const PT_GNU_STACK: u32 = 0x6474_e551;

// ---- p_flags
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

// ---- auxiliary vector keys
pub const AT_NULL: u64 = 0;
pub const AT_PHDR: u64 = 3;
pub const AT_PHENT: u64 = 4;
pub const AT_PHNUM: u64 = 5;
pub const AT_PAGESZ: u64 = 6;
pub const AT_BASE: u64 = 7;
pub const AT_FLAGS: u64 = 8;
pub const AT_ENTRY: u64 = 9;
pub const AT_UID: u64 = 11;
pub const AT_EUID: u64 = 12;
pub const AT_GID: u64 = 13;
pub const AT_EGID: u64 = 14;
pub const AT_PLATFORM: u64 = 15;
pub const AT_HWCAP: u64 = 16;
pub const AT_CLKTCK: u64 = 17;
pub const AT_SECURE: u64 = 23;
pub const AT_RANDOM: u64 = 25;
pub const AT_HWCAP2: u64 = 26;
pub const AT_EXECFN: u64 = 31;

pub const EHDR_SIZE: usize = 64;
pub const PHDR_SIZE: usize = 56;

fn rd16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o.checked_add(2)?)?.try_into().ok()?))
}
fn rd32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o.checked_add(4)?)?.try_into().ok()?))
}
fn rd64(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o.checked_add(8)?)?.try_into().ok()?))
}

/// `Elf64_Ehdr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ehdr {
    pub ident: [u8; 16],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

impl Ehdr {
    /// Decode the header at the start of `b`; None when `b` is too short.
    pub fn parse(b: &[u8]) -> Option<Ehdr> {
        Some(Ehdr {
            ident: b.get(..16)?.try_into().ok()?,
            e_type: rd16(b, 16)?,
            e_machine: rd16(b, 18)?,
            e_version: rd32(b, 20)?,
            e_entry: rd64(b, 24)?,
            e_phoff: rd64(b, 32)?,
            e_shoff: rd64(b, 40)?,
            e_flags: rd32(b, 48)?,
            e_ehsize: rd16(b, 52)?,
            e_phentsize: rd16(b, 54)?,
            e_phnum: rd16(b, 56)?,
            e_shentsize: rd16(b, 58)?,
            e_shnum: rd16(b, 60)?,
            e_shstrndx: rd16(b, 62)?,
        })
    }
}

/// `Elf64_Phdr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

impl Phdr {
    /// Decode the program header at the start of `b`.
    pub fn parse(b: &[u8]) -> Option<Phdr> {
        Some(Phdr {
            p_type: rd32(b, 0)?,
            p_flags: rd32(b, 4)?,
            p_offset: rd64(b, 8)?,
            p_vaddr: rd64(b, 16)?,
            p_paddr: rd64(b, 24)?,
            p_filesz: rd64(b, 32)?,
            p_memsz: rd64(b, 40)?,
            p_align: rd64(b, 48)?,
        })
    }
}
