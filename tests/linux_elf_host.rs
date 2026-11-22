// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of the Linux layer's ELF loader (sys/kern/src/linux/fs/
// binfmt_elf): header checks, placement and the initial stack, on synthetic
// ELF files and on the host's own programs.  No QEMU needed.
//
//   rustc --edition 2021 --test tests/linux_elf_host.rs -o /tmp/linux_elf_host \
//     && /tmp/linux_elf_host --test-threads=1

#![allow(dead_code)]
extern crate alloc;

// `include!` is relative to this file, whatever the nesting: the modules keep
// the paths (`crate::linux::...`) they have in the kernel.
mod linux {
    pub mod uapi {
        pub mod elf {
            include!("../sys/kern/src/linux/uapi/elf.rs");
        }
        pub mod errno {
            include!("../sys/kern/src/linux/uapi/errno.rs");
        }
    }
    pub mod fs {
        pub mod binfmt_elf {
            pub mod error {
                include!("../sys/kern/src/linux/fs/binfmt_elf/error.rs");
            }
            pub mod header {
                include!("../sys/kern/src/linux/fs/binfmt_elf/header.rs");
            }
            pub mod layout {
                include!("../sys/kern/src/linux/fs/binfmt_elf/layout.rs");
            }
            pub mod stack {
                include!("../sys/kern/src/linux/fs/binfmt_elf/stack.rs");
            }
        }
    }
}

use linux::fs::binfmt_elf::{header, layout, stack};
use linux::uapi::elf::*;
use linux::uapi::errno::*;

// ------------------------------------------------------------- fixtures

#[derive(Clone)]
struct Seg {
    p_type: u32,
    flags: u32,
    vaddr: u64,
    memsz: u64,
    data: Vec<u8>,
}

fn load(vaddr: u64, flags: u32, data: Vec<u8>, memsz: u64) -> Seg {
    Seg { p_type: PT_LOAD, flags, vaddr, memsz, data }
}

fn interp(path: &str) -> Seg {
    let mut d = path.as_bytes().to_vec();
    d.push(0);
    let n = d.len() as u64;
    Seg { p_type: PT_INTERP, flags: PF_R, vaddr: 0, memsz: n, data: d }
}

fn gnu_stack(flags: u32) -> Seg {
    Seg { p_type: PT_GNU_STACK, flags, vaddr: 0, memsz: 0, data: vec![] }
}

/// An ELF64 x86-64 file: header, program headers, then each segment's data
/// at a page aligned file offset, so that `vaddr % 4096 == offset % 4096`
/// can be arranged by the caller through `vaddr`.  The first LOAD gets the
/// headers too (it starts at file offset 0, like a real linker output).
fn elf(e_type: u16, entry: u64, segs: &[Seg]) -> Vec<u8> {
    let phdr_end = 64 + 56 * segs.len();
    let mut img = vec![0u8; phdr_end];
    let mut offsets = Vec::new();
    let mut sizes = Vec::new();
    let mut first_load = true;
    for s in segs {
        if s.p_type == PT_LOAD && first_load && s.vaddr % 4096 == 0 {
            // Like a linker's output: the first LOAD starts at file offset 0
            // and covers the headers; its data overlays what follows them.
            first_load = false;
            let cur = img.len(); // headers, plus any note/interp already placed
            assert!(s.data.len() >= cur);
            img.resize(s.data.len(), 0);
            img[cur..].copy_from_slice(&s.data[cur..]);
            offsets.push(0u64);
            sizes.push(s.data.len() as u64);
            continue;
        }
        if s.p_type == PT_LOAD {
            first_load = false;
            // Keep file offset congruent to vaddr modulo the page.
            while img.len() % 4096 != (s.vaddr % 4096) as usize {
                img.push(0);
            }
        }
        offsets.push(img.len() as u64);
        sizes.push(s.data.len() as u64);
        img.extend_from_slice(&s.data);
    }
    img[..4].copy_from_slice(b"\x7fELF");
    img[4] = 2;
    img[5] = 1;
    img[6] = 1;
    img[16..18].copy_from_slice(&e_type.to_le_bytes());
    img[18..20].copy_from_slice(&62u16.to_le_bytes());
    img[24..32].copy_from_slice(&entry.to_le_bytes());
    img[32..40].copy_from_slice(&64u64.to_le_bytes());
    img[54..56].copy_from_slice(&56u16.to_le_bytes());
    img[56..58].copy_from_slice(&(segs.len() as u16).to_le_bytes());
    for (i, s) in segs.iter().enumerate() {
        let at = 64 + 56 * i;
        img[at..at + 4].copy_from_slice(&s.p_type.to_le_bytes());
        img[at + 4..at + 8].copy_from_slice(&s.flags.to_le_bytes());
        img[at + 8..at + 16].copy_from_slice(&offsets[i].to_le_bytes());
        img[at + 16..at + 24].copy_from_slice(&s.vaddr.to_le_bytes());
        img[at + 32..at + 40].copy_from_slice(&sizes[i].to_le_bytes());
        img[at + 40..at + 48].copy_from_slice(&s.memsz.to_le_bytes());
        img[at + 48..at + 56].copy_from_slice(&4096u64.to_le_bytes());
    }
    img
}

const LIM: layout::Limits = layout::Limits { lo: 1 << 39, hi: 100 << 39, max_span: 64 << 20 };
const BASE: u64 = 1 << 39;

fn simple_dyn() -> Vec<u8> {
    elf(
        ET_DYN,
        0x1000,
        &[
            load(0, PF_R | PF_X, vec![0x90; 0x2000], 0x2000), // headers + text
            load(0x3000, PF_R | PF_W, vec![1; 0x100], 0x5000), // data + bss
        ],
    )
}

fn parse(img: &[u8]) -> Result<header::Headers, linux::fs::binfmt_elf::error::LoadError> {
    header::parse(img, img.len() as u64)
}

// ------------------------------------------------------------- header.rs

#[test]
fn accepts_a_pie() {
    let img = simple_dyn();
    let h = parse(&img).unwrap();
    assert_eq!(h.ehdr.e_type, ET_DYN);
    assert_eq!(h.loads().count(), 2);
    assert!(h.interp_path(&img).is_none());
    assert!(!h.exec_stack);
}

#[test]
fn rejects_what_is_not_an_x86_64_elf64_executable() {
    let good = simple_dyn();
    let bad = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut i = good.clone();
        f(&mut i);
        assert_eq!(parse(&i).unwrap_err().errno, ENOEXEC);
    };
    bad(&|i| i[0] = 0);
    bad(&|i| i[4] = 1); // ELFCLASS32
    bad(&|i| i[5] = 2); // big endian
    bad(&|i| i[18] = 3); // EM_386
    bad(&|i| i[16] = 1); // ET_REL
    bad(&|i| i[54] = 32); // e_phentsize
    bad(&|i| i[56] = 0); // no program headers
    assert_eq!(parse(b"#!/bin/sh\n").unwrap_err().errno, ENOEXEC);
    assert_eq!(parse(b"").unwrap_err().errno, ENOEXEC);
}

#[test]
fn needs_a_load_segment() {
    let img = elf(ET_DYN, 0, &[gnu_stack(PF_R | PF_W)]);
    assert_eq!(parse(&img).unwrap_err().what, "no loadable segment");
}

#[test]
fn segment_must_fit_its_file_and_its_memory() {
    let mut img = simple_dyn();
    // First segment: p_filesz > p_memsz.
    img[64 + 32..64 + 40].copy_from_slice(&0x9000u64.to_le_bytes());
    assert_eq!(parse(&img).unwrap_err().errno, ENOEXEC);
    // p_offset + p_filesz past the end of the file.
    let mut img = simple_dyn();
    let len = img.len() as u64;
    img[64 + 8..64 + 16].copy_from_slice(&len.to_le_bytes());
    assert_eq!(parse(&img).unwrap_err().what, "segment past the end of the file");
    // p_vaddr + p_memsz wraps.
    let mut img = simple_dyn();
    img[64 + 16..64 + 24].copy_from_slice(&(u64::MAX - 0x10).to_le_bytes());
    assert_eq!(parse(&img).unwrap_err().errno, ENOEXEC);
}

#[test]
fn finds_the_interpreter() {
    let img = elf(
        ET_DYN,
        0x1000,
        &[interp("/lib64/ld-linux-x86-64.so.2"), load(0, PF_R | PF_X, vec![0x90; 0x2000], 0x2000)],
    );
    let h = parse(&img).unwrap();
    assert_eq!(h.interp_path(&img), Some(&b"/lib64/ld-linux-x86-64.so.2"[..]));
}

#[test]
fn rejects_bad_interpreters() {
    let seg = load(0, PF_R | PF_X, vec![0x90; 0x2000], 0x2000);
    // Unterminated.
    let mut s = interp("/lib/ld.so");
    s.data.pop();
    assert_eq!(parse(&elf(ET_DYN, 0x1000, &[s, seg.clone()])).unwrap_err().what, "unterminated PT_INTERP path");
    // Empty (just a NUL).
    let s = Seg { data: vec![0, 0], ..interp("x") };
    assert_eq!(parse(&elf(ET_DYN, 0x1000, &[s, seg.clone()])).unwrap_err().errno, ENOEXEC);
    // Too long.
    let mut s = interp("x");
    s.data = vec![b'a'; 5000];
    s.data.push(0);
    assert_eq!(parse(&elf(ET_DYN, 0x1000, &[s, seg])).unwrap_err().what, "bad PT_INTERP size");
}

#[test]
fn gnu_stack_decides_stack_execution() {
    let seg = || load(0, PF_R | PF_X, vec![0x90; 0x2000], 0x2000);
    assert!(parse(&elf(ET_DYN, 0x1000, &[seg(), gnu_stack(PF_R | PF_W | PF_X)])).unwrap().exec_stack);
    assert!(!parse(&elf(ET_DYN, 0x1000, &[seg(), gnu_stack(PF_R | PF_W)])).unwrap().exec_stack);
}

// ------------------------------------------------------------- layout.rs

#[test]
fn pie_is_biased_to_the_anchor() {
    let img = simple_dyn();
    let l = layout::plan(&parse(&img).unwrap(), BASE, &LIM).unwrap();
    assert_eq!(l.bias, BASE);
    assert_eq!(l.entry, BASE + 0x1000);
    assert_eq!((l.lo, l.hi), (BASE, BASE + 0x8000));
    assert_eq!(l.brk, l.hi);
    // Text r-x, data+bss rw-; the hole between is not mapped.
    assert_eq!(
        l.runs,
        vec![
            layout::Run { start: BASE, end: BASE + 0x2000, w: false, x: true },
            layout::Run { start: BASE + 0x3000, end: BASE + 0x8000, w: true, x: false },
        ]
    );
    assert_eq!(l.fills.len(), 2);
    assert_eq!(l.fills[1].vaddr, BASE + 0x3000);
    assert_eq!(l.fills[1].filesz, 0x100);
    // The program headers sit in the first segment, at offset 64.
    assert_eq!(l.phdr, Some(BASE + 64));
}

#[test]
fn exec_keeps_its_link_addresses() {
    let img = elf(ET_EXEC, BASE + 0x1000, &[load(BASE, PF_R | PF_X, vec![0x90; 0x2000], 0x2000)]);
    let l = layout::plan(&parse(&img).unwrap(), 0xdead_0000, &LIM).unwrap();
    assert_eq!(l.bias, 0);
    assert_eq!(l.entry, BASE + 0x1000);
}

#[test]
fn exec_outside_the_user_range_is_refused() {
    // The classic non-PIE base, below PrismOS's user space.
    let img = elf(ET_EXEC, 0x401000, &[load(0x400000, PF_R | PF_X, vec![0x90; 0x2000], 0x2000)]);
    assert_eq!(layout::plan(&parse(&img).unwrap(), BASE, &LIM).unwrap_err().errno, ENOMEM);
}

#[test]
fn segments_sharing_a_page_share_one_run() {
    // r-x text, then r-- rodata starting mid-page: one page, one mapping.
    let img = elf(
        ET_DYN,
        0x10,
        &[load(0, PF_R | PF_X, vec![0x90; 0x1800], 0x1800), load(0x1800, PF_R, vec![2; 0x100], 0x400)],
    );
    let l = layout::plan(&parse(&img).unwrap(), BASE, &LIM).unwrap();
    assert_eq!(l.runs, vec![layout::Run { start: BASE, end: BASE + 0x2000, w: false, x: true }]);
}

#[test]
fn data_sharing_a_page_with_text_is_refused() {
    // The same layout with rw data would make the page writable and
    // executable: refused rather than silently executable data.
    let img = elf(
        ET_DYN,
        0x10,
        &[load(0, PF_R | PF_X, vec![0x90; 0x1800], 0x1800), load(0x1800, PF_R | PF_W, vec![2; 0x100], 0x400)],
    );
    assert_eq!(
        layout::plan(&parse(&img).unwrap(), BASE, &LIM).unwrap_err().what,
        "writable and executable segment"
    );
}

#[test]
fn writable_and_executable_is_refused() {
    let img = elf(ET_DYN, 0x10, &[load(0, PF_R | PF_W | PF_X, vec![0x90; 0x1000], 0x1000)]);
    let e = layout::plan(&parse(&img).unwrap(), BASE, &LIM).unwrap_err();
    assert_eq!(e.what, "writable and executable segment");
}

#[test]
fn overlapping_segments_are_refused() {
    let img = elf(
        ET_DYN,
        0x10,
        &[load(0, PF_R | PF_X, vec![0x90; 0x2000], 0x2000), load(0x1000, PF_R | PF_W, vec![1; 0x100], 0x100)],
    );
    assert_eq!(layout::plan(&parse(&img).unwrap(), BASE, &LIM).unwrap_err().what, "overlapping segments");
}

#[test]
fn entry_must_be_executable() {
    let img = elf(ET_DYN, 0x3000, &[load(0, PF_R | PF_X, vec![0x90; 0x2000], 0x2000), load(0x3000, PF_R | PF_W, vec![1; 0x10], 0x10)]);
    assert_eq!(
        layout::plan(&parse(&img).unwrap(), BASE, &LIM).unwrap_err().what,
        "entry point outside an executable segment"
    );
}

#[test]
fn image_span_is_capped() {
    let img = elf(ET_DYN, 0x10, &[load(0, PF_R | PF_X, vec![0x90; 0x1000], 128 << 20)]);
    assert_eq!(layout::plan(&parse(&img).unwrap(), BASE, &LIM).unwrap_err().what, "image too large");
}

#[test]
fn unaligned_anchor_is_refused() {
    let img = simple_dyn();
    assert_eq!(layout::plan(&parse(&img).unwrap(), BASE + 8, &LIM).unwrap_err().errno, EINVAL);
}

// -------------------------------------------------------------- stack.rs

const TOP: u64 = 0x7fff_0000_0000;

fn rd64(m: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(m[at..at + 8].try_into().unwrap())
}

/// Read a NUL terminated string at the stack address `addr`.
fn cstr(img: &stack::Image, addr: u64) -> Vec<u8> {
    let off = (addr - img.rsp) as usize;
    let end = img.bytes[off..].iter().position(|&b| b == 0).unwrap();
    img.bytes[off..off + end].to_vec()
}

fn sample_aux() -> stack::Aux {
    stack::Aux {
        phdr: 0x1234_0040,
        phnum: 9,
        entry: 0x1234_1000,
        base: 0x5555_0000,
        hwcap: 0xbfebfbff,
        hwcap2: 0,
        clktck: 100,
        uid: 1000,
        euid: 1000,
        gid: 100,
        egid: 100,
        secure: false,
    }
}

fn args(v: &[&str]) -> Vec<Vec<u8>> {
    v.iter().map(|s| s.as_bytes().to_vec()).collect()
}

#[test]
fn stack_layout_reads_back() {
    let argv = args(&["/bin/prog", "-x", "file"]);
    let envp = args(&["HOME=/root", "TERM=linux"]);
    let rnd: [u8; 16] = core::array::from_fn(|i| 0xA0 + i as u8);
    let img = stack::build(TOP, TOP - (1 << 20), &argv, &envp, b"/bin/prog", &sample_aux(), rnd).unwrap();

    assert_eq!(img.rsp % 16, 0, "rsp is 16-byte aligned");
    assert_eq!(img.bytes.len() as u64, TOP - img.rsp);
    let m = &img.bytes;
    assert_eq!(rd64(m, 0), 3, "argc");
    for (i, a) in argv.iter().enumerate() {
        assert_eq!(cstr(&img, rd64(m, 8 + 8 * i)), *a, "argv[{i}]");
    }
    assert_eq!(rd64(m, 8 + 8 * 3), 0, "argv terminator");
    let env0 = 8 + 8 * 4;
    for (i, e) in envp.iter().enumerate() {
        assert_eq!(cstr(&img, rd64(m, env0 + 8 * i)), *e, "envp[{i}]");
    }
    assert_eq!(rd64(m, env0 + 8 * 2), 0, "envp terminator");

    // The auxiliary vector: pairs until AT_NULL.
    let mut at = env0 + 8 * 3;
    let mut aux = std::collections::BTreeMap::new();
    loop {
        let (k, v) = (rd64(m, at), rd64(m, at + 8));
        at += 16;
        if k == AT_NULL {
            break;
        }
        assert!(aux.insert(k, v).is_none(), "duplicate auxv key {k}");
    }
    assert!(at < m.len(), "the strings come after the vector, up to the stack top");
}

#[test]
fn auxv_carries_the_program_values() {
    let img = stack::build(TOP, TOP - (1 << 20), &args(&["p"]), &[], b"/p", &sample_aux(), [7; 16]).unwrap();
    let m = &img.bytes;
    let mut at = 8 + 8 * 2 + 8; // argc, argv[0], NULL, then (empty) envp NULL
    let mut aux = std::collections::BTreeMap::new();
    loop {
        let (k, v) = (rd64(m, at), rd64(m, at + 8));
        at += 16;
        if k == AT_NULL {
            break;
        }
        aux.insert(k, v);
    }
    assert_eq!(aux[&AT_PHDR], 0x1234_0040);
    assert_eq!(aux[&AT_PHENT], 56);
    assert_eq!(aux[&AT_PHNUM], 9);
    assert_eq!(aux[&AT_PAGESZ], 4096);
    assert_eq!(aux[&AT_BASE], 0x5555_0000);
    assert_eq!(aux[&AT_ENTRY], 0x1234_1000);
    assert_eq!(aux[&AT_UID], 1000);
    assert_eq!(aux[&AT_EGID], 100);
    assert_eq!(aux[&AT_CLKTCK], 100);
    assert_eq!(aux[&AT_SECURE], 0);
    assert_eq!(aux[&AT_HWCAP], 0xbfebfbff);
    assert_eq!(cstr(&img, aux[&AT_EXECFN]), b"/p");
    assert_eq!(cstr(&img, aux[&AT_PLATFORM]), b"x86_64");
    let r = (aux[&AT_RANDOM] - img.rsp) as usize;
    assert_eq!(&m[r..r + 16], &[7u8; 16], "AT_RANDOM points at the 16 random bytes");
    assert_eq!(aux[&AT_RANDOM] % 16, 0);
}

#[test]
fn empty_argv_and_envp_are_fine() {
    let img = stack::build(TOP, TOP - (1 << 20), &[], &[], b"", &sample_aux(), [0; 16]).unwrap();
    assert_eq!(rd64(&img.bytes, 0), 0, "argc 0");
    assert_eq!(img.rsp % 16, 0);
}

#[test]
fn oversized_arguments_are_e2big() {
    let big = vec![vec![b'a'; 4096]; 300];
    let e = stack::build(TOP, TOP - (1 << 20), &big, &[], b"/p", &sample_aux(), [0; 16]).unwrap_err();
    assert_eq!(e.errno, E2BIG);
    // The limit is hard: nothing below it, ever.
    for limit_room in [0u64, 8, 16, 100, 4000] {
        let r = stack::build(TOP, TOP - limit_room, &args(&["abc", "def"]), &args(&["K=V"]), b"/p", &sample_aux(), [0; 16]);
        if let Ok(img) = r {
            assert!(img.rsp >= TOP - limit_room);
        }
    }
}

// ---------------------------------------------- the host's own programs

/// Rebuild the loaded image from layout.rs's runs and fills, the way the
/// kernel glue does, as a sparse map of page -> 4096 bytes.
fn materialise(file: &[u8], l: &layout::Layout) -> std::collections::BTreeMap<u64, Vec<u8>> {
    let mut mem = std::collections::BTreeMap::new();
    for r in &l.runs {
        let mut a = r.start;
        while a < r.end {
            mem.insert(a, vec![0u8; 4096]);
            a += 4096;
        }
    }
    for f in &l.fills {
        for i in 0..f.filesz {
            let a = f.vaddr + i;
            mem.get_mut(&(a & !4095)).expect("fill outside every run")[(a & 4095) as usize] = file[(f.offset + i) as usize];
        }
    }
    mem
}

fn host_elfs() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for p in ["/bin/ls", "/usr/bin/ls", "/bin/sh", "/usr/bin/env", "/usr/bin/cat", "/bin/busybox", "/usr/bin/python3"] {
        if let Ok(b) = std::fs::read(p) {
            if b.len() > 64 && &b[..4] == b"\x7fELF" {
                out.push((p.to_string(), b));
            }
        }
    }
    out
}

#[test]
fn host_programs_parse_and_lay_out() {
    let progs = host_elfs();
    assert!(!progs.is_empty(), "no host ELF to test with");
    for (name, file) in &progs {
        let head = &file[..file.len().min(64 * 1024)];
        let h = header::parse(head, file.len() as u64).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        if h.ehdr.e_type == ET_EXEC {
            // Linked at the classic 0x400000: below PrismOS's user space.
            assert_eq!(layout::plan(&h, BASE, &LIM).unwrap_err().errno, ENOMEM, "{name}: non-PIE");
            eprintln!("{name}: non-PIE, refused as designed");
            continue;
        }
        let l = layout::plan(&h, BASE, &LIM).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        // Real linkers keep every segment inside the file's own page runs.
        assert!(l.entry >= l.lo && l.entry < l.hi, "{name}: entry inside the image");
        assert!(l.runs.iter().all(|r| r.start % 4096 == 0 && r.end % 4096 == 0 && r.start < r.end), "{name}");
        assert!(l.runs.windows(2).all(|w| w[0].end <= w[1].start), "{name}: runs ordered, disjoint");
        assert!(l.runs.iter().all(|r| !(r.w && r.x)), "{name}: W^X");

        // The entry point's page really holds the file's bytes at e_entry.
        let mem = materialise(file, &l);
        let page = mem.get(&(l.entry & !4095)).unwrap_or_else(|| panic!("{name}: entry page not mapped"));
        let seg = h.loads().find(|p| h.ehdr.e_entry >= p.p_vaddr && h.ehdr.e_entry < p.p_vaddr + p.p_filesz);
        if let Some(seg) = seg {
            let fo = (h.ehdr.e_entry - seg.p_vaddr + seg.p_offset) as usize;
            let po = (l.entry & 4095) as usize;
            let n = 16.min(4096 - po).min(file.len() - fo);
            assert_eq!(&page[po..po + n], &file[fo..fo + n], "{name}: code at the entry point");
        }

        // AT_PHDR: the program header table must read back from memory.
        if let Some(ph) = l.phdr {
            let table = &file[h.ehdr.e_phoff as usize..h.ehdr.e_phoff as usize + h.phdrs.len() * 56];
            let mut got = Vec::new();
            for i in 0..table.len() as u64 {
                let a = ph + i;
                got.push(mem[&(a & !4095)][(a & 4095) as usize]);
            }
            assert_eq!(got, table, "{name}: AT_PHDR points at the program headers");
        }
        eprintln!("{name}: {} runs, interp {:?}", l.runs.len(), h.interp_path(head).map(|p| String::from_utf8_lossy(p).to_string()));
    }
}

// ------------------------------------------------------ hostile input

#[test]
fn truncated_files_never_panic() {
    let full = elf(
        ET_DYN,
        0x1000,
        &[interp("/lib64/ld-linux-x86-64.so.2"), load(0, PF_R | PF_X, vec![0x90; 0x2000], 0x2000), gnu_stack(PF_R | PF_W)],
    );
    for cut in (0..full.len().min(600)).chain([full.len() - 1]) {
        let _ = header::parse(&full[..cut], full.len() as u64);
    }
}

#[test]
fn every_header_byte_hostile_never_panics() {
    let base = elf(
        ET_DYN,
        0x1000,
        &[interp("/lib64/ld.so"), load(0, PF_R | PF_X, vec![0x90; 0x2000], 0x2000), load(0x3000, PF_R | PF_W, vec![1; 8], 0x100)],
    );
    for i in 0..(64 + 56 * 3) {
        for v in [0x00u8, 0x01, 0x7f, 0x80, 0xff] {
            let mut img = base.clone();
            img[i] = v;
            if let Ok(h) = header::parse(&img, img.len() as u64) {
                let _ = layout::plan(&h, BASE, &LIM);
                let _ = h.interp_path(&img);
            }
        }
    }
    // 64-bit fields at their extremes.
    for field in [8usize, 16, 32, 40, 48] {
        for v in [u64::MAX, u64::MAX - 4095, 1 << 63, 4095, 4096] {
            for ph in 0..3 {
                let mut img = base.clone();
                let at = 64 + 56 * ph + field;
                img[at..at + 8].copy_from_slice(&v.to_le_bytes());
                if let Ok(h) = header::parse(&img, img.len() as u64) {
                    let _ = layout::plan(&h, BASE, &LIM);
                    let _ = layout::plan(&h, u64::MAX & !4095, &LIM);
                    let _ = layout::plan(&h, 0, &LIM);
                }
            }
        }
    }
    let mut img = base.clone();
    img[32..40].copy_from_slice(&u64::MAX.to_le_bytes()); // e_phoff
    assert!(parse(&img).is_err());
}

#[test]
fn stack_build_survives_hostile_sizes() {
    let aux = sample_aux();
    for limit in [0u64, 1, TOP - 1, TOP, TOP - 8, TOP - 24, TOP - 25] {
        let _ = stack::build(TOP, limit, &args(&["a"]), &args(&["b"]), b"c", &aux, [0; 16]);
    }
    let _ = stack::build(8, 0, &args(&["a"]), &[], b"c", &aux, [0; 16]);
    let _ = stack::build(0, 0, &[], &[], b"", &aux, [0; 16]);
}
