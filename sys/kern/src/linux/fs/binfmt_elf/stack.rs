// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Last step: the initial stack of the new program, as Linux hands it over.
// Top of the stack to bottom:
//
//     8 zero bytes
//     the program's path       (what AT_EXECFN points to)
//     envp strings, then argv strings, each NUL terminated, in order
//     (alignment to 16)
//     16 random bytes (AT_RANDOM, 16-aligned), the platform string "x86_64"
//     (alignment to 16)
//   rsp -> argc, argv[0..argc], NULL, envp[...], NULL, the auxiliary vector
//
// `build` returns the bytes of everything from `rsp` to the stack top: the
// caller writes them into the new address space in one go.  Pure, so the
// layout is checked on the host by reading it back.

use super::error::LoadError;
use crate::linux::uapi::elf::*;
use crate::linux::uapi::errno::*;
use alloc::vec;
use alloc::vec::Vec;

/// What AT_PLATFORM names.
pub const PLATFORM: &[u8] = b"x86_64";
pub const PAGESZ: u64 = 4096;

/// The values of the auxiliary vector that depend on the program.
#[derive(Debug, Clone, Copy, Default)]
pub struct Aux {
    /// Where the program header table is in memory (0 when unknown).
    pub phdr: u64,
    pub phnum: u64,
    /// The program's own entry point (not the interpreter's).
    pub entry: u64,
    /// Where the interpreter was loaded; 0 without one.
    pub base: u64,
    pub hwcap: u64,
    pub hwcap2: u64,
    /// Ticks per second reported by `times()`.
    pub clktck: u64,
    pub uid: u64,
    pub euid: u64,
    pub gid: u64,
    pub egid: u64,
    /// Set-id exec: the C library must be careful with its environment.
    pub secure: bool,
}

/// The finished stack: `bytes` is the memory from `rsp` up to the stack top.
#[derive(Debug, Clone)]
pub struct Image {
    pub rsp: u64,
    pub bytes: Vec<u8>,
}

/// Reserve `n` bytes below `*p`, never going under `limit`.
fn take(p: &mut u64, n: usize, limit: u64) -> Result<u64, LoadError> {
    let new = p
        .checked_sub(n as u64)
        .filter(|&a| a >= limit)
        .ok_or(LoadError::new(E2BIG, "argument list too long"))?;
    *p = new;
    Ok(new)
}

/// Lay out the new program's stack under `stack_top`.  Nothing may be placed
/// below `limit` (the lowest address the arguments may reach: keep room for
/// the program under it).  `random` is the content of AT_RANDOM.
pub fn build(
    stack_top: u64,
    limit: u64,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    execfn: &[u8],
    aux: &Aux,
    random: [u8; 16],
) -> Result<Image, LoadError> {
    // (address, bytes) of everything that is not a pointer word; the stack
    // is zero filled, so a string's NUL needs no write.
    let mut pieces: Vec<(u64, &[u8])> = Vec::new();

    let mut p = stack_top;
    take(&mut p, 8, limit)?;
    let execfn_p = take(&mut p, execfn.len() + 1, limit)?;
    pieces.push((execfn_p, execfn));
    let mut env_p = vec![0u64; envp.len()];
    for (i, s) in envp.iter().enumerate().rev() {
        env_p[i] = take(&mut p, s.len() + 1, limit)?;
        pieces.push((env_p[i], s));
    }
    let mut arg_p = vec![0u64; argv.len()];
    for (i, s) in argv.iter().enumerate().rev() {
        arg_p[i] = take(&mut p, s.len() + 1, limit)?;
        pieces.push((arg_p[i], s));
    }
    p &= !15;
    let random_p = take(&mut p, 16, limit)?;
    pieces.push((random_p, &random));
    let platform_p = take(&mut p, PLATFORM.len() + 1, limit)?;
    pieces.push((platform_p, PLATFORM));

    // The auxiliary vector, in the order Linux emits it.
    let auxv: [(u64, u64); 19] = [
        (AT_HWCAP, aux.hwcap),
        (AT_PAGESZ, PAGESZ),
        (AT_CLKTCK, aux.clktck),
        (AT_PHDR, aux.phdr),
        (AT_PHENT, PHDR_SIZE as u64),
        (AT_PHNUM, aux.phnum),
        (AT_BASE, aux.base),
        (AT_FLAGS, 0),
        (AT_ENTRY, aux.entry),
        (AT_UID, aux.uid),
        (AT_EUID, aux.euid),
        (AT_GID, aux.gid),
        (AT_EGID, aux.egid),
        (AT_SECURE, aux.secure as u64),
        (AT_RANDOM, random_p),
        (AT_HWCAP2, aux.hwcap2),
        (AT_EXECFN, execfn_p),
        (AT_PLATFORM, platform_p),
        (AT_NULL, 0),
    ];

    let mut words: Vec<u64> = Vec::with_capacity(3 + argv.len() + envp.len() + auxv.len() * 2);
    words.push(argv.len() as u64);
    words.extend_from_slice(&arg_p);
    words.push(0);
    words.extend_from_slice(&env_p);
    words.push(0);
    for &(k, v) in &auxv {
        words.push(k);
        words.push(v);
    }
    let vector = (words.len() as u64)
        .checked_mul(8)
        .ok_or(LoadError::new(E2BIG, "argument list too long"))?;
    let rsp = p
        .checked_sub(vector)
        .map(|a| a & !15)
        .filter(|&a| a >= limit)
        .ok_or(LoadError::new(E2BIG, "argument list too long"))?;

    let mut bytes = vec![0u8; (stack_top - rsp) as usize];
    for (i, w) in words.iter().enumerate() {
        bytes[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    for (addr, data) in pieces {
        let at = (addr - rsp) as usize;
        bytes[at..at + data.len()].copy_from_slice(data);
    }
    Ok(Image { rsp, bytes })
}
