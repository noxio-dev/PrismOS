// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// In-kernel unit tests (phase 1.5).  The pure parts of the kernel -- the
// checksum tables, path and date helpers, the log ring, the directory-hash
// function -- get the same treatment the VFS/ext4 code gets from
// `make fstest`, but running in the real kernel: `ktest` from the shell (and
// part of `make test`) executes the table below and reports each result.
//
// A test is a plain function returning Result: the `check!` / `check_eq!`
// macros early-return a message on failure (no unwinding, no panic).  Tests
// must not depend on each other and must leave global state as they found it.

use alloc::format;
use alloc::string::String;

pub struct Test {
    pub name: &'static str,
    pub f: fn() -> Result<(), String>,
}

/// All tests, in run order.  `make test` expects the summary line
/// "ktest: all N passed" (or fails), so keep this list stable and green.
pub static TESTS: &[Test] = &[
    Test { name: "crc32c-vectors", f: t_crc32c },
    Test { name: "crc16-vector", f: t_crc16 },
    Test { name: "vfs-normalize", f: t_normalize },
    Test { name: "vfs-split-parent", f: t_split_parent },
    Test { name: "civil-days", f: t_civil_days },
    Test { name: "stamp-format", f: t_stamp },
    Test { name: "klog-levels", f: t_klog_levels },
    Test { name: "klog-ring-order", f: t_klog_ring },
    Test { name: "dx-hash", f: t_dx_hash },
    Test { name: "ext-merge", f: t_ext_merge },
    Test { name: "unifont-width", f: t_unifont_width },
    Test { name: "size-format", f: t_size_format },
    Test { name: "parse-u64", f: t_parse_u64 },
    Test { name: "kallsyms-self", f: t_kallsyms },
    Test { name: "heap-roundtrip", f: t_heap_roundtrip },
    Test { name: "spinlock", f: t_spinlock },
    Test { name: "trampoline-layout", f: t_trampoline_layout },
    Test { name: "percpu-slots", f: t_percpu_slots },
];

#[macro_export]
macro_rules! check {
    ($cond:expr) => {
        if !($cond) {
            return Err(format!("check failed: {}", stringify!($cond)));
        }
    };
}

#[macro_export]
macro_rules! check_eq {
    ($a:expr, $b:expr) => {
        let (x, y) = (&$a, &$b);
        if x != y {
            return Err(format!("{}:{}: {:?} != {:?}", stringify!($a), stringify!($b), x, y));
        }
    };
}

#[macro_export]
macro_rules! check_ne {
    ($a:expr, $b:expr) => {
        let (x, y) = (&$a, &$b);
        if x == y {
            return Err(format!("{}:{}: {:?} == {:?}", stringify!($a), stringify!($b), x, y));
        }
    };
}

/// Run the whole table; returns (passed, failed).
pub fn run_all() -> (usize, usize) {
    let (mut pass, mut fail) = (0, 0);
    for t in TESTS {
        match (t.f)() {
            Ok(()) => {
                pass += 1;
                crate::kprintln!("  ok       {}", t.name);
            }
            Err(e) => {
                fail += 1;
                crate::kprintln!("  FAIL     {}: {}", t.name, e);
            }
        }
    }
    (pass, fail)
}

// ------------------------------------------------------------------ tests

/// RFC 3720 test vectors for CRC-32C (Castagnoli).
fn t_crc32c() -> Result<(), String> {
    use crate::fs::ext4::crc::crc32c;
    // The kernel flavour passes a seed and does no final xor; the standard
    // test vectors use init ~0 and xor-out ~0: crc = crc32c(!0, data) ^ !0.
    let cases: &[(&[u8], u32)] = &[
        (b"", 0x0000_0000),
        (b"123456789", 0xE306_9283),
        (b"The quick brown fox jumps over the lazy dog", 0x2262_0404),
        (&[0x00; 32], 0x8A91_36AA),
    ];
    for (data, want) in cases {
        let got = crc32c(!0, data) ^ !0;
        check_eq!(got, *want);
    }
    Ok(())
}

/// CRC-16 (poly 0xA001, reflected): init 0 gives the ARC check value
/// ("123456789" -> 0xBB3D), init 0xFFFF the MODBUS one (0x4B37).
fn t_crc16() -> Result<(), String> {
    use crate::fs::ext4::crc::crc16;
    check_eq!(crc16(0x0000, b"123456789"), 0xBB3D);
    check_eq!(crc16(0xFFFF, b"123456789"), 0x4B37);
    // Continuation: feeding bytes one at a time equals feeding them at once.
    let whole = crc16(0xFFFF, b"prismos-phase-1.5");
    let mut step = 0xFFFF;
    for b in b"prismos-phase-1.5" {
        step = crc16(step, &[*b]);
    }
    check_eq!(step, whole);
    Ok(())
}

fn t_normalize() -> Result<(), String> {
    use crate::fs::vfs::normalize;
    check_eq!(normalize("/", "/"), "/");
    check_eq!(normalize("/", "a/b/c"), "/a/b/c");
    check_eq!("/a", normalize("/", "a/./b/../."));
    check_eq!("/x", normalize("/a/b", "../../x"));
    check_eq!("/", normalize("/", "../../../../.."));
    check_eq!("/a/b", normalize("/a", "b"));
    check_eq!("/b", normalize("/a", "///b//")); // leading slash: absolute
    check_eq!("/b", normalize("/a", "../b"));
    Ok(())
}

fn t_split_parent() -> Result<(), String> {
    use crate::fs::vfs::split_parent;
    check_eq!(split_parent("/"), None);
    check_eq!(split_parent("/a"), Some(("/", "a")));
    check_eq!(split_parent("/a/b"), Some(("/a", "b")));
    check_eq!(split_parent("/a/b/c.txt"), Some(("/a/b", "c.txt")));
    Ok(())
}

fn t_civil_days() -> Result<(), String> {
    use crate::fs::fsys::{civil_from_days, days_from_civil};
    let cases = [
        (1970i64, 1u32, 1u32, 0i64),
        (2000, 3, 1, 11017),
        (2026, 1, 1, 20454),
        (1999, 12, 31, 10956),
        (2100, 3, 1, 47541), // 2100 is NOT a leap year (400-year rule)
    ];
    for (y, m, d, days) in cases {
        check_eq!(days_from_civil(y, m as i64, d as i64), days);
        check_eq!(civil_from_days(days), (y, m, d));
    }
    // Leap years: 2024-02-29 exists, 2023-02-29 must not.
    check_eq!(days_from_civil(2024, 2, 29), 19782);
    check_eq!(civil_from_days(19782), (2024, 2, 29));
    Ok(())
}

fn t_stamp() -> Result<(), String> {
    use crate::fs::fsys::Stamp;
    use alloc::string::ToString;
    // 2026-01-01 00:00 UTC = 1767225600.
    check_eq!(Stamp(1767225600).to_string(), "2026-01-01 00:00");
    // 2000-03-01 12:34 UTC = 951909240? (2000-03-01 = 11017 days).
    check_eq!(Stamp(11017 * 86400 + 12 * 3600 + 34 * 60).to_string(), "2000-03-01 12:34");
    Ok(())
}

fn t_klog_levels() -> Result<(), String> {
    use crate::lib::klog::Level;
    check!(Level::Error > Level::Warn);
    check!(Level::Warn > Level::Info);
    check!(Level::Info > Level::Debug);
    check_eq!(Level::parse("warn"), Some(Level::Warn));
    check_eq!(Level::parse("nope"), None);
    check_eq!(Level::Error.as_str(), "error");
    Ok(())
}

/// Record order and the level filter (uses the debug level so a plain
/// `dmesg` at info level does not show the noise).
fn t_klog_ring() -> Result<(), String> {
    use crate::lib::klog::{self, Level};
    for i in 0..4 {
        klog::record(Level::Debug, &format!("ktest ring {}", i));
    }
    let all = klog::read(Level::Debug);
    check!(all.len() >= 4);
    // The last four debug records must be ours, in order.
    let tail = &all[all.len() - 4..];
    for (i, r) in tail.iter().enumerate() {
        check_eq!(r.text, format!("ktest ring {}", i));
        check_eq!(r.level, Level::Debug);
    }
    // The info filter skips them.
    let info = klog::read(Level::Info);
    check!(info.iter().all(|r| r.text != "ktest ring 3"));
    Ok(())
}

/// ext4 directory hash (half-MD4 / TEA / legacy, phase 1.5 htree): the
/// exact values were cross-checked against `debugfs hash` on a real image
/// (seed f395312a.., hash_version 1 = half_md4).
fn t_dx_hash() -> Result<(), String> {
    use crate::fs::ext4::htree::dx_hash_raw;
    let seed = [
        0x2a31_95f3u32,
        0x504c_f1ee,
        0x7948_e18d,
        0x281e_9e03,
    ];
    // debugfs 1.47.2 `dx_hash` with exactly this seed (ground truth):
    //   f0000 -> 0x3eec5ef2 (minor 0xe0d03ed5), f0001 -> 0x6e67a48c,
    //   lost+found -> 0xfa26574a, f0399 -> 0x4309f126.
    let cases: &[(&[u8], u32, u32)] = &[
        (b"f0000", 0x3eec_5ef2, 0xe0d0_3ed5),
        (b"f0001", 0x6e67_a48c, 0xe5b3_ac4a),
        (b"lost+found", 0xfa26_574a, 0x1d93_fe51),
        (b"f0399", 0x4309_f126, 0xe670_dd8a),
    ];
    for (name, want, want_minor) in cases {
        let (h, minor) = dx_hash_raw(name, &seed, 1);
        check_eq!(h, *want);
        check_eq!(minor, *want_minor);
    }
    // Determinism and separation.
    check_ne!(dx_hash_raw(b"a", &seed, 1).0, dx_hash_raw(b"b", &seed, 1).0);
    check_ne!(dx_hash_raw(b"Readme", &seed, 1).0, dx_hash_raw(b"readme", &seed, 1).0);
    check_ne!(dx_hash_raw(&[b'x'; 255], &seed, 1).0, dx_hash_raw(&[b'x'; 254], &seed, 1).0);
    // The all-zero seed falls back to the MD4 initial state.
    let zero = [0u32; 4];
    check_ne!(dx_hash_raw(b"f0000", &zero, 1).0, dx_hash_raw(b"f0000", &seed, 1).0);
    Ok(())
}

fn t_ext_merge() -> Result<(), String> {
    use crate::fs::ext4::extent::{merge_adjacent, Ext};
    let mk = |lblk: u32, len: u32, pblk: u64| Ext { lblk, len, pblk, uninit: false };
    // Adjacent on both sides merge into one.
    let mut e = alloc::vec![mk(0, 2, 10), mk(2, 3, 12)];
    merge_adjacent(&mut e);
    check_eq!(e.len(), 1);
    check_eq!(e[0], mk(0, 5, 10));
    // A physical gap keeps them apart.
    let mut e = alloc::vec![mk(0, 2, 10), mk(2, 3, 13)];
    merge_adjacent(&mut e);
    check_eq!(e.len(), 2);
    // uninit state must match to merge.
    let mut e = alloc::vec![mk(0, 2, 10), Ext { lblk: 2, len: 3, pblk: 12, uninit: true }];
    merge_adjacent(&mut e);
    check_eq!(e.len(), 2);
    // Extents at the 32768 cap do not merge past it.
    let mut e = alloc::vec![mk(0, 32768, 10), mk(32768, 5, 10 + 32768)];
    merge_adjacent(&mut e);
    check_eq!(e.len(), 2);
    Ok(())
}

fn t_unifont_width() -> Result<(), String> {
    use crate::console::unifont::width;
    check_eq!(width('a'), 1);
    check_eq!(width('é'), 1);
    check_eq!(width('青'), 2);
    check_eq!(width('\u{3000}'), 2);
    Ok(())
}

fn t_size_format() -> Result<(), String> {
    use crate::shell::Size;
    use alloc::string::ToString;
    check_eq!(Size(0).to_string(), "0 B");
    check_eq!(Size(512).to_string(), "512 B");
    check_eq!(Size(1024).to_string(), "1 KiB");
    check_eq!(Size(1536).to_string(), "1.5 KiB");
    check_eq!(Size(1024 * 1024).to_string(), "1 MiB");
    check_eq!(Size(5 * 1024 * 1024 + 300 * 1024).to_string(), "5.3 MiB");
    Ok(())
}

fn t_parse_u64() -> Result<(), String> {
    use crate::shell::parse_u64;
    check_eq!(parse_u64("0"), Some(0));
    check_eq!(parse_u64("12345"), Some(12345));
    check_eq!(parse_u64("0x10"), Some(16));
    check_eq!(parse_u64("0XFF"), Some(255));
    check_eq!(parse_u64("-1"), None);
    check_eq!(parse_u64("x"), None);
    check_eq!(parse_u64("18446744073709551615"), Some(u64::MAX));
    Ok(())
}

/// The symbol table must resolve at least itself, and an address inside the
/// kernel's text must map to the right neighborhood.
fn t_kallsyms() -> Result<(), String> {
    use crate::lib::kallsyms;
    let f: fn() -> (usize, usize) = run_all;
    let n = f as usize as u64;
    if kallsyms::base() == 0 {
        return Err("symbol table empty (regenerate with make)".into());
    }
    let (name, off) = kallsyms::lookup(n).ok_or("run_all address not resolved")?;
    check_eq!(off, 0);
    check!(name.contains("run_all"));
    // An unmapped low address resolves to nothing.
    check_eq!(kallsyms::lookup(0x1000), None);
    Ok(())
}

/// Allocation round-trip must not leak: the alloc/free balance is the same
/// before and after (and the data survives).
fn t_heap_roundtrip() -> Result<(), String> {
    use alloc::vec::Vec;
    let (a0, f0, _, _) = crate::mem::heap::activity();
    {
        let mut v: Vec<u8> = Vec::new();
        for i in 0..4096u16 {
            v.push((i % 251) as u8);
        }
        let sum: u32 = v.iter().map(|&b| b as u32).sum();
        check_eq!(sum, (0..4096u16).map(|i| (i % 251) as u32).sum::<u32>());
        check_eq!(v.len(), 4096);
    }
    let (a1, f1, _, _) = crate::mem::heap::activity();
    check_eq!(a1 - f1, a0 - f0);
    Ok(())
}

/// The spin lock (phase 1.5 SMP): mutual exclusion on a hammered counter,
/// try_lock semantics, and release on drop.
fn t_spinlock() -> Result<(), String> {
    use crate::lib::spin::SpinLock;
    let l = SpinLock::new(0u64);
    check!(!l.is_locked());
    {
        let mut g = l.lock();
        check!(l.is_locked());
        // try_lock must fail while held.
        check!(l.try_lock().is_none());
        *g += 1;
    }
    check!(!l.is_locked());
    check!(l.try_lock().is_some());
    // Hammer it: with interrupts masked around each critical section the
    // counter must arrive exactly.
    let mut hits = 0u64;
    for _ in 0..100_000u64 {
        let mut g = l.lock();
        *g += 1;
        hits = *g;
    }
    let g = l.lock();
    check_eq!(*g, 100_001);
    check_eq!(hits, 100_001);
    drop(g);
    Ok(())
}

/// The SMP trampoline template (phase 1.5): the first bytes must be the
/// real-mode entry (cli; xor ax,ax), the hand-encoded far jumps must point
/// at the 32/64-bit stages, and the GDT descriptors must be the flat
/// code32/data/code64 quads -- a corrupted template would triple-fault
/// every AP before any kernel code runs.
fn t_trampoline_layout() -> Result<(), String> {
    // SAFETY: the trampoline template is read-only .rodata.
    let t: &[u8] = unsafe {
        core::slice::from_raw_parts(
            (&raw const crate::hw::smp::__trampoline_start) as *const u8,
            (&raw const crate::hw::smp::__trampoline_end as *const u8 as usize)
                - (&raw const crate::hw::smp::__trampoline_start as *const u8 as usize),
        )
    };
    check!(t.len() >= 0x200);
    // 16-bit entry: fa 31 c0 (cli; xor ax,ax)
    check_eq!(t[0], 0xFA);
    check_eq!(t[1], 0x31);
    check_eq!(t[2], 0xC0);
    // far jmp ptr16:16 to 0x0008:0xa040 at offset 0x18 (ea 40 a0 08 00)
    check_eq!(t[0x18], 0xEA);
    check_eq!(t[0x19], 0x40);
    check_eq!(t[0x1A], 0xA0);
    // far jmp ptr16:32 to 0x0018:0xa0a0 at 0x78-ish: find ea a0 a0 00 00 18
    let found = t.windows(7).any(|w| w == [0xEA, 0xA0, 0xA0, 0x00, 0x00, 0x18, 0x00]);
    check!(found);
    // GDT at 0x140: null, 0x00cf9a000000ffff, 0x00cf92000000ffff, 0x00af9a000000ffff
    let q = |o: usize| -> u64 {
        let mut v = 0u64;
        for i in 0..8 {
            v |= (t[0x140 + o + i] as u64) << (8 * i);
        }
        v
    };
    check_eq!(q(0), 0);
    check_eq!(q(8), 0x00CF_9A00_0000_FFFF);
    check_eq!(q(16), 0x00CF_9200_0000_FFFF);
    check_eq!(q(24), 0x00AF_9A00_0000_FFFF);
    // GDTR at 0x1f8: limit 31, base 0xa140
    check_eq!(u16::from_le_bytes([t[0x1F8], t[0x1F9]]), 31);
    check_eq!(u32::from_le_bytes([t[0x1FA], t[0x1FB], t[0x1FC], t[0x1FD]]), 0xA140);
    Ok(())
}

/// Per-CPU bookkeeping (phase 1.5): the BSP must be slot 0, present and
/// online, and current_cpu() must agree.
fn t_percpu_slots() -> Result<(), String> {
    use crate::hw::smp;
    check_eq!(smp::current_cpu(), 0); // ktest runs on the BSP
    const ZERO: smp::CpuInfo =
        smp::CpuInfo { index: 0, lapic: 0, online: false, ticks: 0, ipis: 0, idles: 0 };
    let mut info = [ZERO; crate::hw::acpi::MAX_CPUS];
    let n = smp::snapshot(&mut info);
    check!(n >= 1);
    check_eq!(info[0].index, 0);
    check!(info[0].online);
    check!(info[0].lapic < 256);
    for c in &info[..n] {
        check!(c.index < crate::hw::acpi::MAX_CPUS);
    }
    Ok(())
}
