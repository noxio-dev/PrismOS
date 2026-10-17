// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The two checksums ext4 uses: CRC-32C (metadata_csum) and CRC-16/ARC
// (uninit_bg group descriptor checksums).  Both are the "raw" kernel flavours:
// no pre- or post-inversion, the caller passes the seed.

const fn table32() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

const fn table16() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xA001 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static T32: [u32; 256] = table32();
static T16: [u16; 256] = table16();

/// CRC-32C continuing from `seed` (the kernel's `ext4_chksum`).
pub fn crc32c(seed: u32, data: &[u8]) -> u32 {
    let mut c = seed;
    for &b in data {
        c = T32[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

/// CRC-16 (poly 0xA001, reflected) continuing from `seed`.
pub fn crc16(seed: u16, data: &[u8]) -> u16 {
    let mut c = seed;
    for &b in data {
        c = T16[((c ^ b as u16) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}
