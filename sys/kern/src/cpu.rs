// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev

use core::arch::asm;
use core::str::from_utf8;

#[derive(Clone, Copy)]
pub struct Cpuid {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

#[inline]
pub fn cpuid(leaf: u32, subleaf: u32) -> Cpuid {
    let (a, b, c, d);
    unsafe {
        asm!(
            "xchg rbx, {b}",
            "cpuid",
            "xchg rbx, {b}",
            b = out(reg) b,
            inout("eax") leaf => a,
            inout("ecx") subleaf => c,
            out("edx") d,
            options(nomem, nostack)
        );
    }
    Cpuid { eax: a, ebx: b, ecx: c, edx: d }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Fpu, Tsc, Msr, Apic, Cmov, Sse, Sse2,
    Sse3, Ssse3, Sse41, Sse42, Popcnt, Aes, Avx, Rdrand,
    Avx2, Erms, Rdseed,
}

impl Feature {
    const ALL: [Feature; 18] = [
        Feature::Fpu, Feature::Tsc, Feature::Msr, Feature::Apic, Feature::Cmov,
        Feature::Sse, Feature::Sse2, Feature::Sse3, Feature::Ssse3, Feature::Sse41,
        Feature::Sse42, Feature::Popcnt, Feature::Aes, Feature::Avx, Feature::Rdrand,
        Feature::Avx2, Feature::Erms, Feature::Rdseed,
    ];

    fn name(self) -> &'static str {
        match self {
            Feature::Fpu => "fpu",
            Feature::Tsc => "tsc",
            Feature::Msr => "msr",
            Feature::Apic => "apic",
            Feature::Cmov => "cmov",
            Feature::Sse => "sse",
            Feature::Sse2 => "sse2",
            Feature::Sse3 => "sse3",
            Feature::Ssse3 => "ssse3",
            Feature::Sse41 => "sse4.1",
            Feature::Sse42 => "sse4.2",
            Feature::Popcnt => "popcnt",
            Feature::Aes => "aes",
            Feature::Avx => "avx",
            Feature::Rdrand => "rdrand",
            Feature::Avx2 => "avx2",
            Feature::Erms => "erms",
            Feature::Rdseed => "rdseed",
        }
    }

    fn bit(self) -> u32 {
        match self {
            Feature::Fpu => 1 << 0,
            Feature::Tsc => 1 << 4,
            Feature::Msr => 1 << 5,
            Feature::Apic => 1 << 9,
            Feature::Cmov => 1 << 15,
            Feature::Sse => 1 << 25,
            Feature::Sse2 => 1 << 26,
            Feature::Sse3 => 1 << 0,
            Feature::Ssse3 => 1 << 9,
            Feature::Sse41 => 1 << 19,
            Feature::Sse42 => 1 << 20,
            Feature::Popcnt => 1 << 23,
            Feature::Aes => 1 << 25,
            Feature::Avx => 1 << 28,
            Feature::Rdrand => 1 << 30,
            Feature::Avx2 => 1 << 5,
            Feature::Erms => 1 << 9,
            Feature::Rdseed => 1 << 18,
        }
    }

    fn word(self, cpu: &Cpu) -> u32 {
        match self {
            Feature::Fpu | Feature::Tsc | Feature::Msr | Feature::Apic | Feature::Cmov
            | Feature::Sse | Feature::Sse2 => cpu.f1_edx,
            Feature::Sse3 | Feature::Ssse3 | Feature::Sse41 | Feature::Sse42
            | Feature::Popcnt | Feature::Aes | Feature::Avx | Feature::Rdrand => cpu.f1_ecx,
            Feature::Avx2 | Feature::Erms | Feature::Rdseed => cpu.f7_ebx,
        }
    }
}

pub struct Cpu {
    vendor: [u8; 12],
    brand: [u8; 48],
    eax1: u32,
    f1_edx: u32,
    f1_ecx: u32,
    f7_ebx: u32,
}

impl Cpu {
    pub fn detect() -> Cpu {
        let l0 = cpuid(0, 0);

        let mut vendor = [0u8; 12];
        vendor[0..4].copy_from_slice(&l0.ebx.to_le_bytes());
        vendor[4..8].copy_from_slice(&l0.edx.to_le_bytes());
        vendor[8..12].copy_from_slice(&l0.ecx.to_le_bytes());

        let l1 = cpuid(1, 0);
        let f7_ebx = if l0.eax >= 7 { cpuid(7, 0).ebx } else { 0 };

        let mut brand = [0u8; 48];
        if cpuid(0x8000_0000, 0).eax >= 0x8000_0004 {
            for i in 0..3usize {
                let r = cpuid(0x8000_0002 + i as u32, 0);
                let o = i * 16;
                brand[o..o + 4].copy_from_slice(&r.eax.to_le_bytes());
                brand[o + 4..o + 8].copy_from_slice(&r.ebx.to_le_bytes());
                brand[o + 8..o + 12].copy_from_slice(&r.ecx.to_le_bytes());
                brand[o + 12..o + 16].copy_from_slice(&r.edx.to_le_bytes());
            }
        }

        Cpu { vendor, brand, eax1: l1.eax, f1_edx: l1.edx, f1_ecx: l1.ecx, f7_ebx }
    }

    pub fn vendor(&self) -> &str {
        let end = self.vendor.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        from_utf8(&self.vendor[..end]).unwrap_or("<invalid>")
    }

    pub fn brand(&self) -> &str {
        let end = self.brand.iter().rposition(|&b| b != 0 && b != b' ').map_or(0, |i| i + 1);
        from_utf8(&self.brand[..end]).unwrap_or("")
    }

    pub fn family(&self) -> u32 {
        let base = (self.eax1 >> 8) & 0xF;
        let ext = (self.eax1 >> 20) & 0xFF;
        base + if base == 0xF { ext } else { 0 }
    }

    pub fn model(&self) -> u32 {
        let fam = (self.eax1 >> 8) & 0xF;
        let base = (self.eax1 >> 4) & 0xF;
        let ext = (self.eax1 >> 16) & 0xF;
        base + if fam == 0x6 || fam == 0xF { ext << 4 } else { 0 }
    }

    pub fn stepping(&self) -> u32 {
        self.eax1 & 0xF
    }

    pub fn has(&self, f: Feature) -> bool {
        f.word(self) & f.bit() != 0
    }

    pub fn features_into<'a>(&self, buf: &'a mut [u8]) -> &'a str {
        let mut n = 0;
        for &f in Feature::ALL.iter() {
            if !self.has(f) {
                continue;
            }
            if n > 0 && n < buf.len() {
                buf[n] = b' ';
                n += 1;
            }
            for &b in f.name().as_bytes() {
                if n == buf.len() {
                    break;
                }
                buf[n] = b;
                n += 1;
            }
        }
        from_utf8(&buf[..n]).unwrap_or("")
    }
}