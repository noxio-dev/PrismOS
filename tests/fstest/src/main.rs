// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Host-side tests for the VFS and the ext4 driver: the kernel sources are
// compiled unchanged against an image file.  Driven by tests/fs_test.py.
//
//   fstest <image> <script>     run a script of commands, one per line

extern crate alloc;

// The kernel files are included at the same module paths as in the kernel
// (crate::hw::blk, crate::fs::vfs, crate::fs::ext4): see hw.rs and fs.rs.
mod fs;
mod hw;

use fs::{ext4, vfs};
use hw::blk::{BlkError, BlockDev, SECTOR};
use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::rc::Rc;
use alloc::boxed::Box;

struct FileDev {
    f: RefCell<File>,
    sectors: u64,
}

struct Shared(Rc<FileDev>);

impl BlockDev for Shared {
    fn sectors(&self) -> u64 {
        self.0.sectors
    }
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        if lba + (buf.len() / SECTOR) as u64 > self.0.sectors {
            return Err(BlkError::OutOfRange);
        }
        let mut f = self.0.f.borrow_mut();
        f.seek(SeekFrom::Start(lba * SECTOR as u64)).map_err(|_| BlkError::Io)?;
        f.read_exact(buf).map_err(|_| BlkError::Io)
    }
    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlkError> {
        if lba + (buf.len() / SECTOR) as u64 > self.0.sectors {
            return Err(BlkError::OutOfRange);
        }
        let mut f = self.0.f.borrow_mut();
        f.seek(SeekFrom::Start(lba * SECTOR as u64)).map_err(|_| BlkError::Io)?;
        f.write_all(buf).map_err(|_| BlkError::Io)
    }
    fn flush(&self) -> Result<(), BlkError> {
        self.0.f.borrow_mut().flush().map_err(|_| BlkError::Io)
    }
}

fn unescape(s: &str) -> Vec<u8> {
    // \n and \\ only
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            match b[i + 1] {
                b'n' => out.push(b'\n'),
                b'\\' => out.push(b'\\'),
                c => {
                    out.push(b'\\');
                    out.push(c)
                }
            }
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Deterministic pattern so contents can be compared without storing them.
fn pattern(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: fstest <image> <script> [ro]");
        std::process::exit(2);
    }
    let f = OpenOptions::new().read(true).write(true).open(&args[1]).expect("open image");
    let sectors = f.metadata().unwrap().len() / SECTOR as u64;
    let dev = Rc::new(FileDev { f: RefCell::new(f), sectors });
    let script = std::fs::read_to_string(&args[2]).expect("read script");
    let force_ro = args.get(3).map(|s| s == "ro").unwrap_or(false);

    vfs::set_clock(|| 1_760_000_000);
    let mut vfs = vfs::Vfs::new();
    let mounted = if force_ro { ext4::Ext4::mount_ro(Box::new(Shared(dev.clone()))) } else { ext4::Ext4::mount(Box::new(Shared(dev.clone()))) };
    let fs = match mounted {
        Ok(fs) => fs,
        Err(e) => {
            println!("MOUNT-FAIL {:?}", e);
            std::process::exit(3);
        }
    };
    println!("MOUNT ok ro={}", fs_ro(&fs));
    vfs.mount("/", Box::new(fs)).unwrap();

    let mut fails = 0;
    for (ln, line) in script.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(3, ' ');
        let cmd = it.next().unwrap();
        let a = it.next().unwrap_or("");
        let rest = it.next().unwrap_or("");
        let expect_err = cmd.starts_with('!');
        let cmd = cmd.trim_start_matches('!');
        let res: Result<String, vfs::FsError> = (|| -> Result<String, vfs::FsError> {
            Ok(match cmd {
                "mkdir" => { vfs.mkdir(a, 0o755)?; String::new() }
                "mkdirp" => { vfs.mkdir_p(a, 0o755)?; String::new() }
                "write" => { vfs.write_file(a, &unescape(rest))?; String::new() }
                "append" => { vfs.append_file(a, &unescape(rest))?; String::new() }
                // gen <path> <seed>:<len>
                "gen" => {
                    let (s, l) = rest.split_once(':').unwrap();
                    vfs.write_file(a, &pattern(s.parse().unwrap(), l.parse().unwrap()))?;
                    String::new()
                }
                // check <path> <seed>:<len>
                "check" => {
                    let (s, l) = rest.split_once(':').unwrap();
                    let want = pattern(s.parse().unwrap(), l.parse().unwrap());
                    let got = vfs.read_file(a, usize::MAX)?;
                    if got != want {
                        let at = got.iter().zip(want.iter()).position(|(x, y)| x != y);
                        return Err(if got.len() != want.len() { eprintln!("len {} != {}", got.len(), want.len()); vfs::FsError::Corrupt } else { eprintln!("first diff at {:?}", at); vfs::FsError::Corrupt });
                    }
                    String::new()
                }
                // patch <path> <off> <seed>:<len>  (overwrite in the middle)
                "patch" => {
                    let (off, sl) = rest.split_once(' ').unwrap();
                    let (s, l) = sl.split_once(':').unwrap();
                    let fd = vfs.open(a, vfs::O_WRITE)?;
                    vfs.seek(fd, vfs::Whence::Set(off.parse().unwrap()))?;
                    let d = pattern(s.parse().unwrap(), l.parse().unwrap());
                    let mut done = 0;
                    while done < d.len() { done += vfs.write(fd, &d[done..])?; }
                    vfs.close(fd)?;
                    String::new()
                }
                "cat" => String::from_utf8_lossy(&vfs.read_file(a, usize::MAX)?).into_owned(),
                "size" => format!("{}", vfs.stat(a)?.size),
                "nlink" => format!("{}", vfs.stat(a)?.nlink),
                "mode" => format!("{:o}", vfs.stat(a)?.mode),
                "ls" => {
                    let mut v: Vec<String> = vfs.readdir(a)?.into_iter().map(|e| e.name).filter(|n| n != "." && n != "..").collect();
                    v.sort();
                    v.join(" ")
                }
                "rm" => { vfs.unlink(a)?; String::new() }
                "rmdir" => { vfs.rmdir(a)?; String::new() }
                "mv" => { vfs.rename(a, rest)?; String::new() }
                "ln" => { vfs.link(a, rest)?; String::new() }
                "symlink" => { vfs.symlink(a, rest)?; String::new() }
                "readlink" => vfs.readlink(a)?,
                "truncate" => { vfs.truncate(a, rest.parse().unwrap())?; String::new() }
                "chmod" => { vfs.chmod(a, u16::from_str_radix(rest, 8).unwrap())?; String::new() }
                "touch" => { vfs.create(a, 0o644)?; String::new() }
                "sync" => { vfs.sync_all()?; String::new() }
                "df" => { let s = vfs.statfs(if a.is_empty() {"/"} else {a})?; format!("{} {} {} {}", s.blocks, s.free_blocks, s.files, s.free_files) }
                "umount" => { vfs.umount(a)?; String::new() }
                // tmpfs <path> [max_bytes] [max_files] (phase 3.3): mount the
                // RAM file system the kernel mounts on /tmp at boot, with
                // test-sized limits for the quota scenarios.
                "tmpfs" => {
                    let mut it = rest.split_whitespace();
                    let bytes: u64 = it.next().map_or(fs::tmpfs::MAX_BYTES, |s| s.parse().unwrap());
                    let files: u64 = it.next().map_or(fs::tmpfs::MAX_FILES, |s| s.parse().unwrap());
                    vfs.mount(a, Box::new(fs::tmpfs::TmpFs::with_limits(bytes, files)))?;
                    String::new()
                }
                // phase 1.5: preallocation (two flavours), block count,
                // raw bytes, and the crash-right-after-journal-commit exit.
                "prealloc" => { let (o, l) = rest.split_once(' ').unwrap(); vfs.fallocate(a, o.parse().unwrap(), l.parse().unwrap(), false)?; String::new() }
                "preallok" => { let (o, l) = rest.split_once(' ').unwrap(); vfs.fallocate(a, o.parse().unwrap(), l.parse().unwrap(), true)?; String::new() }
                "blk" => format!("{}", vfs.stat(a)?.blocks),
                "peek" => {
                    let (o, l) = rest.split_once(' ').unwrap();
                    let (o, l): (u64, usize) = (o.parse().unwrap(), l.parse().unwrap());
                    let mut b = vec![0u8; l];
                    let fd = vfs.open(a, vfs::O_READ)?;
                    vfs.seek(fd, vfs::Whence::Set(o))?;
                    vfs.read(fd, &mut b)?;
                    vfs.close(fd)?;
                    b.iter().map(|x| format!("{:02x}", x)).collect::<Vec<_>>().join("")
                }
                "crash" => {
                    // Simulate power loss after the last commit: the journal
                    // holds un-checkpointed transactions, nothing is synced.
                    println!("crash: exiting without sync");
                    std::process::exit(0);
                }
                _ => panic!("line {}: unknown command {}", ln + 1, cmd),
            })
        })();
        match (res, expect_err) {
            (Ok(out), false) => println!("{}: {}", line.split(' ').take(2).collect::<Vec<_>>().join(" "), out),
            (Err(e), true) => println!("{}: ERR {:?}", line.split(' ').take(2).collect::<Vec<_>>().join(" "), e),
            (Ok(_), true) => { println!("UNEXPECTED-OK line {}: {}", ln + 1, line); fails += 1 }
            (Err(e), false) => { println!("FAIL line {}: {} -> {:?}", ln + 1, line, e); fails += 1 }
        }
    }
    let _ = vfs.sync_all();
    println!("DONE fails={}", fails);
    std::process::exit(if fails > 0 { 1 } else { 0 });
}

fn fs_ro(fs: &ext4::Ext4) -> bool {
    use vfs::FileSystem;
    fs.read_only()
}
