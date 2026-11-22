// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of user/abi.rs: telling a PrismOS executable from a Linux
// one by its program headers (no QEMU needed).
//
//   rustc --edition 2021 --test tests/abi_host.rs -o /tmp/abi_host \
//     && /tmp/abi_host --test-threads=1

#![allow(dead_code)]

#[path = "../sys/kern/src/user/abi.rs"]
mod abi;

use abi::{classify, Abi};

const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_NOTE: u32 = 4;

/// One program header to build: (type, payload, p_align).
struct Ph {
    p_type: u32,
    data: Vec<u8>,
    align: u64,
}

fn interp(path: &str) -> Ph {
    let mut d = path.as_bytes().to_vec();
    d.push(0);
    Ph { p_type: PT_INTERP, data: d, align: 1 }
}

fn load() -> Ph {
    Ph { p_type: PT_LOAD, data: vec![0x90; 16], align: 4096 }
}

/// One ELF note; name and descriptor are padded to `align`.
fn note(name: &[u8], ntype: u32, desc: &[u8], align: usize) -> Vec<u8> {
    let pad = |v: &mut Vec<u8>| while v.len() % align != 0 { v.push(0) };
    let mut n = Vec::new();
    n.extend_from_slice(&(name.len() as u32).to_le_bytes());
    n.extend_from_slice(&(desc.len() as u32).to_le_bytes());
    n.extend_from_slice(&ntype.to_le_bytes());
    n.extend_from_slice(name);
    pad(&mut n);
    n.extend_from_slice(desc);
    pad(&mut n);
    n
}

/// glibc's .note.ABI-tag: OS word, then the minimum kernel version.
fn abi_tag(os: u32) -> Vec<u8> {
    let mut d = Vec::new();
    for w in [os, 3, 2, 0] {
        d.extend_from_slice(&w.to_le_bytes());
    }
    note(b"GNU\0", 1, &d, 4)
}

fn build_id() -> Vec<u8> {
    note(b"GNU\0", 3, &[0xAB; 20], 4)
}

fn notes(data: Vec<u8>) -> Ph {
    Ph { p_type: PT_NOTE, data, align: 4 }
}

/// A minimal ELF64 x86-64 image holding `phs` (headers first, then payloads).
fn elf(phs: &[Ph]) -> Vec<u8> {
    let mut img = vec![0u8; 64 + 56 * phs.len()];
    img[..4].copy_from_slice(b"\x7fELF");
    img[4] = 2; // ELFCLASS64
    img[5] = 1; // little endian
    img[6] = 1;
    img[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    img[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    img[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    img[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    img[56..58].copy_from_slice(&(phs.len() as u16).to_le_bytes());
    for (i, ph) in phs.iter().enumerate() {
        let off = img.len() as u64;
        let at = 64 + 56 * i;
        img[at..at + 4].copy_from_slice(&ph.p_type.to_le_bytes());
        img[at + 8..at + 16].copy_from_slice(&off.to_le_bytes());
        img[at + 32..at + 40].copy_from_slice(&(ph.data.len() as u64).to_le_bytes());
        img[at + 40..at + 48].copy_from_slice(&(ph.data.len() as u64).to_le_bytes());
        img[at + 48..at + 56].copy_from_slice(&ph.align.to_le_bytes());
        img.extend_from_slice(&ph.data);
    }
    img
}

// ------------------------------------------------------------ PrismOS

#[test]
fn static_prism_program_is_prism() {
    assert_eq!(classify(&elf(&[load()])), Abi::Prism);
}

#[test]
fn prism_dynamic_program_is_prism() {
    assert_eq!(classify(&elf(&[interp("/lib/ld.so"), load()])), Abi::Prism);
}

#[test]
fn build_id_note_alone_is_not_linux() {
    assert_eq!(classify(&elf(&[notes(build_id()), load()])), Abi::Prism);
}

#[test]
fn non_linux_abi_tag_is_not_linux() {
    // OS word 1 = Hurd, 2 = Solaris, 3 = FreeBSD: not ours to call Linux.
    for os in [1, 2, 3] {
        assert_eq!(classify(&elf(&[notes(abi_tag(os)), load()])), Abi::Prism, "os {os}");
    }
}

// -------------------------------------------------------------- Linux

#[test]
fn glibc_interpreter_is_linux() {
    let img = elf(&[interp("/lib64/ld-linux-x86-64.so.2"), load()]);
    assert_eq!(classify(&img), Abi::Linux);
}

#[test]
fn musl_interpreter_is_linux() {
    assert_eq!(classify(&elf(&[interp("/lib/ld-musl-x86_64.so.1"), load()])), Abi::Linux);
}

#[test]
fn interpreter_is_matched_on_its_file_name_only() {
    // A directory called ld-linux does not make the linker one.
    assert_eq!(classify(&elf(&[interp("/ld-linux/ld.so"), load()])), Abi::Prism);
    assert_eq!(classify(&elf(&[interp("/opt/lib/ld-linux-x86-64.so.2"), load()])), Abi::Linux);
}

#[test]
fn static_glibc_program_is_linux_by_its_abi_tag() {
    // No PT_INTERP: only the note gives a static glibc binary away.
    let mut n = build_id();
    n.extend(abi_tag(0));
    assert_eq!(classify(&elf(&[notes(n), load()])), Abi::Linux);
}

#[test]
fn abi_tag_in_an_eight_byte_aligned_segment() {
    let mut n = note(b"GNU\0", 5, &[0; 16], 8);
    n.extend(note(b"GNU\0", 1, &[0u8; 16], 8));
    let ph = Ph { p_type: PT_NOTE, data: n, align: 8 };
    assert_eq!(classify(&elf(&[ph, load()])), Abi::Linux);
}

#[test]
fn the_note_may_come_after_the_load_segment() {
    assert_eq!(classify(&elf(&[load(), notes(abi_tag(0))])), Abi::Linux);
}

// ------------------------------------------------- malformed input

#[test]
fn not_an_elf_is_left_to_the_loader() {
    assert_eq!(classify(b""), Abi::Prism);
    assert_eq!(classify(b"#!/bin/sh\necho hi\n"), Abi::Prism);
    assert_eq!(classify(&[0x7f, b'E', b'L', b'F']), Abi::Prism);
}

#[test]
fn elf32_and_big_endian_are_left_to_the_loader() {
    let mut img = elf(&[interp("/lib64/ld-linux-x86-64.so.2")]);
    img[4] = 1; // ELFCLASS32
    assert_eq!(classify(&img), Abi::Prism);
    let mut img = elf(&[interp("/lib64/ld-linux-x86-64.so.2")]);
    img[5] = 2; // big endian
    assert_eq!(classify(&img), Abi::Prism);
}

#[test]
fn truncated_images_never_panic() {
    let full = elf(&[interp("/lib64/ld-linux-x86-64.so.2"), notes(abi_tag(0)), load()]);
    for cut in 0..full.len() {
        let _ = classify(&full[..cut]);
    }
}

#[test]
fn wild_header_fields_never_panic() {
    let base = elf(&[interp("/lib64/ld-linux-x86-64.so.2"), notes(abi_tag(0))]);
    // Every byte of the headers set to hostile values, one at a time.
    for i in 0..(64 + 56 * 2) {
        for v in [0x00u8, 0x01, 0x7f, 0x80, 0xff] {
            let mut img = base.clone();
            img[i] = v;
            let _ = classify(&img);
        }
    }
    let mut img = base.clone();
    img[32..40].copy_from_slice(&u64::MAX.to_le_bytes()); // e_phoff
    assert_eq!(classify(&img), Abi::Prism);
}

#[test]
fn bad_note_sizes_never_loop_or_panic() {
    let mut n = Vec::new();
    n.extend_from_slice(&u32::MAX.to_le_bytes()); // namesz
    n.extend_from_slice(&u32::MAX.to_le_bytes()); // descsz
    n.extend_from_slice(&1u32.to_le_bytes());
    n.extend_from_slice(&[0; 16]);
    assert_eq!(classify(&elf(&[notes(n), load()])), Abi::Prism);
    // A zero-sized note must not spin either.
    assert_eq!(classify(&elf(&[notes(vec![0u8; 64]), load()])), Abi::Prism);
}

#[test]
fn unterminated_interpreter_is_still_judged() {
    let ph = Ph { p_type: PT_INTERP, data: b"/lib64/ld-linux-x86-64.so.2".to_vec(), align: 1 };
    assert_eq!(classify(&elf(&[ph, load()])), Abi::Linux);
}

// ------------------------------------------------ a real Linux binary

/// The host's own programs are the real thing: glibc or musl, dynamic or
/// static, with whatever notes the distribution's toolchain adds.
#[test]
fn host_binaries_are_linux() {
    let mut seen = 0;
    for p in ["/bin/ls", "/usr/bin/ls", "/bin/sh", "/usr/bin/env", "/bin/busybox"] {
        let Ok(bytes) = std::fs::read(p) else { continue };
        let head = &bytes[..bytes.len().min(64 * 1024)];
        if head.len() > 20 && &head[..4] == b"\x7fELF" {
            assert_eq!(classify(head), Abi::Linux, "{p}");
            seen += 1;
        }
    }
    eprintln!("host binaries checked: {seen}");
}
