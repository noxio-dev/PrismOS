// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Shell commands for the VFS: mount, umount, ls, cat, write, mkdir, rm, ...
// Paths are relative to the current directory (`cd` / `pwd`).  Arguments are
// split on whitespace, so names with spaces are not addressable from here.

use crate::fsys::{self, abs, vfs, Stamp};
use crate::sh::{hexdump, parse_u64, usage, Args, Size};
use crate::vfs::{FileType, FsError, Stat, Whence, O_CREATE, O_READ, O_TRUNC, O_WRITE};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

fn fail(cmd: &str, what: &str, e: FsError) {
    crate::kprintln!("{}: {}: {}", cmd, what, e.as_str());
}

/// Most commands need a root file system.
fn need_root(cmd: &str) -> bool {
    if vfs().is_mounted() {
        true
    } else {
        crate::kprintln!("{}: no file system mounted (see 'mount')", cmd);
        false
    }
}

fn type_char(t: FileType) -> char {
    match t {
        FileType::Regular => '-',
        FileType::Dir => 'd',
        FileType::Symlink => 'l',
        FileType::Other => '?',
    }
}

fn mode_str(st: &Stat) -> String {
    let mut s = String::new();
    s.push(type_char(st.ftype));
    for sh in [6u32, 3, 0] {
        let b = (st.mode >> sh) & 7;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

// ------------------------------------------------------------ mount / df

pub fn cmd_mount(args: &mut Args) {
    let mut ro = false;
    let mut list = false;
    let mut pos: Vec<&str> = Vec::new();
    while let Some(a) = args.next() {
        match a {
            "-r" => ro = true,
            "-l" => list = true,
            _ => pos.push(a),
        }
    }
    if list {
        let all = fsys::scan();
        if all.is_empty() {
            crate::kprintln!("no ext2/3/4 volume on any disk");
        }
        for f in all {
            match f.part {
                None => crate::kprintln!("{}        whole disk, {}", f.disk, Size(f.sectors * 512)),
                Some(p) => crate::kprintln!("{}:{}      partition, {}", f.disk, p, Size(f.sectors * 512)),
            }
        }
        return;
    }
    match pos.len() {
        0 => {
            let m = vfs().mounts();
            if m.is_empty() {
                crate::kprintln!("nothing mounted ('mount -l' lists the volumes it could mount)");
            }
            for x in m {
                crate::kprintln!("{} on {} ({}, {})", x.fs_name, x.path, x.fs_name, if x.read_only { "ro" } else { "rw" });
            }
        }
        2 => {
            let Some(f) = fsys::find(pos[0]) else {
                return crate::kprintln!("mount: no ext volume '{}' (see 'mount -l')", pos[0]);
            };
            let path = abs(pos[1]);
            match fsys::mount(&f, &path, ro) {
                Ok(()) => crate::kprintln!("mounted {} on {}", pos[0], path),
                Err(e) => fail("mount", pos[0], e),
            }
        }
        _ => usage("mount"),
    }
}

pub fn cmd_umount(args: &mut Args) {
    let Some(p) = args.next() else { return usage("umount") };
    let path = abs(p);
    match vfs().umount(&path) {
        Ok(()) => {
            if fsys::cwd().starts_with(path.as_str()) {
                fsys::set_cwd("/");
            }
            crate::kprintln!("unmounted {}", path)
        }
        Err(e) => fail("umount", &path, e),
    }
}

pub fn cmd_df(_args: &mut Args) {
    if !need_root("df") {
        return;
    }
    crate::kprintln!("{:<8} {:>10} {:>10} {:>10} {:>6}  {}", "fs", "size", "used", "avail", "inodes", "mounted on");
    for m in vfs().mounts() {
        if let Ok(s) = vfs().statfs(&m.path) {
            let bs = s.block_size as u64;
            let total = s.blocks * bs;
            let free = s.free_blocks * bs;
            crate::kprintln!(
                "{:<8} {:>10} {:>10} {:>10} {:>6}  {}{}",
                m.fs_name,
                alloc::format!("{}", Size(total)),
                alloc::format!("{}", Size(total - free)),
                alloc::format!("{}", Size(free)),
                s.files - s.free_files,
                m.path,
                if m.read_only { " (ro)" } else { "" }
            );
        }
    }
}

pub fn cmd_sync(_args: &mut Args) {
    if !need_root("sync") {
        return;
    }
    match vfs().sync_all() {
        Ok(()) => crate::kprintln!("synced"),
        Err(e) => fail("sync", "", e),
    }
}

// ------------------------------------------------------------- navigation

pub fn cmd_pwd(_args: &mut Args) {
    crate::kprintln!("{}", fsys::cwd());
}

pub fn cmd_cd(args: &mut Args) {
    if !need_root("cd") {
        return;
    }
    let target = abs(args.next().unwrap_or("/"));
    match vfs().stat(&target) {
        Ok(st) if st.ftype == FileType::Dir => fsys::set_cwd(&target),
        Ok(_) => fail("cd", &target, FsError::NotDir),
        Err(e) => fail("cd", &target, e),
    }
}

pub fn cmd_ls(args: &mut Args) {
    if !need_root("ls") {
        return;
    }
    let mut long = false;
    let mut all = false;
    let mut paths: Vec<&str> = Vec::new();
    while let Some(a) = args.next() {
        if a.starts_with('-') && a.len() > 1 {
            long |= a.contains('l');
            all |= a.contains('a');
        } else {
            paths.push(a);
        }
    }
    if paths.is_empty() {
        paths.push(".");
    }
    let many = paths.len() > 1;
    for p in paths {
        let path = abs(p);
        let st = match vfs().lstat(&path) {
            Ok(s) => s,
            Err(e) => {
                fail("ls", p, e);
                continue;
            }
        };
        if st.ftype != FileType::Dir {
            print_entry(&st, p, long, &path);
            continue;
        }
        if many {
            crate::kprintln!("{}:", path);
        }
        let mut ents = match vfs().readdir(&path) {
            Ok(e) => e,
            Err(e) => {
                fail("ls", p, e);
                continue;
            }
        };
        ents.sort_by(|a, b| a.name.cmp(&b.name));
        let mut col = 0;
        for e in ents {
            if !all && e.name.starts_with('.') {
                continue;
            }
            let full = if path == "/" { alloc::format!("/{}", e.name) } else { alloc::format!("{}/{}", path, e.name) };
            if long {
                match vfs().lstat(&full) {
                    Ok(s) => print_entry(&s, &e.name, true, &full),
                    Err(_) => crate::kprintln!("?????????? {}", e.name),
                }
            } else {
                let mut label = e.name.clone();
                if e.ftype == FileType::Dir {
                    label.push('/');
                }
                if col + label.chars().count() + 2 > 78 {
                    crate::kprintln!();
                    col = 0;
                }
                crate::kprint!("{}  ", label);
                col += label.chars().count() + 2;
            }
        }
        if !long && col > 0 {
            crate::kprintln!();
        }
    }
}

fn print_entry(st: &Stat, name: &str, long: bool, full: &str) {
    if !long {
        return crate::kprintln!("{}", name);
    }
    let mut line = alloc::format!("{} {:>3} {:>9} {} {}", mode_str(st), st.nlink, st.size, Stamp(st.mtime), name);
    if st.ftype == FileType::Symlink {
        if let Ok(t) = vfs().readlink(full) {
            line.push_str(" -> ");
            line.push_str(&t);
        }
    }
    crate::kprintln!("{}", line);
}

pub fn cmd_stat(args: &mut Args) {
    if !need_root("stat") {
        return;
    }
    let Some(p) = args.next() else { return usage("stat") };
    let path = abs(p);
    match vfs().lstat(&path) {
        Ok(st) => {
            crate::kprintln!("  file  : {}", path);
            crate::kprintln!("  type  : {:?}   mode {} ({:04o})   inode {}   links {}", st.ftype, mode_str(&st), st.mode, st.ino, st.nlink);
            crate::kprintln!("  size  : {} ({}), {} sector(s) on disk", st.size, Size(st.size), st.blocks);
            crate::kprintln!("  owner : uid {} gid {}", st.uid, st.gid);
            crate::kprintln!("  atime : {}   mtime : {}   ctime : {}", Stamp(st.atime), Stamp(st.mtime), Stamp(st.ctime));
        }
        Err(e) => fail("stat", p, e),
    }
}

// --------------------------------------------------------------- contents

/// Print file bytes as text: UTF-8 is kept, control bytes shown as '.'.
struct TextOut {
    pending: Vec<u8>,
}

impl TextOut {
    fn feed(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
        let mut buf = core::mem::take(&mut self.pending);
        let mut rest: &[u8] = &buf;
        loop {
            match core::str::from_utf8(rest) {
                Ok(s) => {
                    Self::show(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let (good, bad) = rest.split_at(e.valid_up_to());
                    Self::show(core::str::from_utf8(good).unwrap_or(""));
                    match e.error_len() {
                        Some(n) => {
                            crate::kprint!(".");
                            rest = &bad[n..];
                        }
                        None => {
                            rest = bad; // incomplete sequence: wait for more
                            break;
                        }
                    }
                }
            }
        }
        let keep = rest.to_vec();
        buf.clear();
        self.pending = keep;
    }

    fn finish(&mut self) {
        if !self.pending.is_empty() {
            crate::kprint!(".");
            self.pending.clear();
        }
    }

    fn show(s: &str) {
        let mut start = 0;
        for (i, c) in s.char_indices() {
            if c.is_control() && c != '\n' && c != '\t' && c != '\r' {
                crate::kprint!("{}.", &s[start..i]);
                start = i + c.len_utf8();
            }
        }
        crate::kprint!("{}", &s[start..]);
    }
}

pub fn cmd_cat(args: &mut Args) {
    if !need_root("cat") {
        return;
    }
    const LIMIT: u64 = 256 * 1024;
    let mut any = false;
    while let Some(p) = args.next() {
        any = true;
        let path = abs(p);
        let fd = match vfs().open(&path, O_READ) {
            Ok(fd) => fd,
            Err(e) => {
                fail("cat", p, e);
                continue;
            }
        };
        let mut out = TextOut { pending: Vec::new() };
        let mut buf = vec![0u8; 4096];
        let mut total = 0u64;
        let mut last = b'\n';
        loop {
            match vfs().read(fd, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.feed(&buf[..n]);
                    last = buf[n - 1];
                    total += n as u64;
                    if total >= LIMIT {
                        out.finish();
                        crate::kprintln!("\n[cat: output cut at {}; use 'hd {}' for a hex view]", Size(LIMIT), p);
                        last = b'\n';
                        break;
                    }
                }
                Err(e) => {
                    out.finish();
                    crate::kprintln!();
                    fail("cat", p, e);
                    break;
                }
            }
        }
        out.finish();
        if last != b'\n' {
            crate::kprintln!();
        }
        let _ = vfs().close(fd);
    }
    if !any {
        usage("cat");
    }
}

pub fn cmd_hd(args: &mut Args) {
    if !need_root("hd") {
        return;
    }
    let Some(p) = args.next() else { return usage("hd") };
    let off = args.next().and_then(parse_u64).unwrap_or(0);
    let len = args.next().and_then(parse_u64).unwrap_or(256).clamp(1, 4096) as usize;
    let fd = match vfs().open(&abs(p), O_READ) {
        Ok(fd) => fd,
        Err(e) => return fail("hd", p, e),
    };
    let mut buf = vec![0u8; len];
    let r = vfs().seek(fd, Whence::Set(off)).and_then(|_| vfs().read(fd, &mut buf));
    let _ = vfs().close(fd);
    match r {
        Ok(n) => hexdump(&buf[..n], off),
        Err(e) => fail("hd", p, e),
    }
}

/// Expand "\n", "\t" and "\\" in text typed at the prompt.
fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => out.push(b'\n'),
                Some('t') => out.push(b'\t'),
                Some('\\') => out.push(b'\\'),
                Some(o) => {
                    out.push(b'\\');
                    let mut b = [0u8; 4];
                    out.extend_from_slice(o.encode_utf8(&mut b).as_bytes());
                }
                None => out.push(b'\\'),
            }
        } else {
            let mut b = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        }
    }
    out
}

fn put(cmd: &str, args: &mut Args, append: bool) {
    if !need_root(cmd) {
        return;
    }
    let Some(p) = args.next() else { return usage(cmd) };
    let mut data = unescape(args.rest());
    data.push(b'\n');
    let path = abs(p);
    let r = if append { vfs().append_file(&path, &data) } else { vfs().write_file(&path, &data) };
    if let Err(e) = r {
        fail(cmd, p, e);
    }
}

pub fn cmd_write(args: &mut Args) {
    put("write", args, false);
}

pub fn cmd_append(args: &mut Args) {
    put("append", args, true);
}

// -------------------------------------------------------------- namespace

pub fn cmd_mkdir(args: &mut Args) {
    if !need_root("mkdir") {
        return;
    }
    let mut parents = false;
    let mut n = 0;
    while let Some(a) = args.next() {
        if a == "-p" {
            parents = true;
            continue;
        }
        n += 1;
        let r = if parents { vfs().mkdir_p(&abs(a), 0o755) } else { vfs().mkdir(&abs(a), 0o755) };
        if let Err(e) = r {
            fail("mkdir", a, e);
        }
    }
    if n == 0 {
        usage("mkdir");
    }
}

pub fn cmd_rmdir(args: &mut Args) {
    if !need_root("rmdir") {
        return;
    }
    let mut n = 0;
    while let Some(a) = args.next() {
        n += 1;
        if let Err(e) = vfs().rmdir(&abs(a)) {
            fail("rmdir", a, e);
        }
    }
    if n == 0 {
        usage("rmdir");
    }
}

/// Remove a directory tree.  Symlinks are removed, never followed.
fn remove_tree(path: &str) -> Result<(), FsError> {
    let st = vfs().lstat(path)?;
    if st.ftype == FileType::Dir {
        for e in vfs().readdir(path)? {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let child = if path == "/" { alloc::format!("/{}", e.name) } else { alloc::format!("{}/{}", path, e.name) };
            remove_tree(&child)?;
        }
        vfs().rmdir(path)
    } else {
        vfs().unlink(path)
    }
}

pub fn cmd_rm(args: &mut Args) {
    if !need_root("rm") {
        return;
    }
    let mut rec = false;
    let mut n = 0;
    while let Some(a) = args.next() {
        if a == "-r" || a == "-rf" || a == "-R" {
            rec = true;
            continue;
        }
        n += 1;
        let path = abs(a);
        if path == "/" {
            crate::kprintln!("rm: refusing to remove '/'");
            continue;
        }
        let r = if rec { remove_tree(&path) } else { vfs().unlink(&path) };
        if let Err(e) = r {
            fail("rm", a, e);
        }
    }
    if n == 0 {
        usage("rm");
    }
}

pub fn cmd_mv(args: &mut Args) {
    if !need_root("mv") {
        return;
    }
    let (Some(a), Some(b)) = (args.next(), args.next()) else { return usage("mv") };
    let dst = abs(b);
    // Moving into an existing directory keeps the name.
    let dst = match vfs().stat(&dst) {
        Ok(st) if st.ftype == FileType::Dir => {
            let leaf = a.trim_end_matches('/').rsplit('/').next().unwrap_or(a);
            if dst == "/" { alloc::format!("/{}", leaf) } else { alloc::format!("{}/{}", dst, leaf) }
        }
        _ => dst,
    };
    if let Err(e) = vfs().rename(&abs(a), &dst) {
        fail("mv", a, e);
    }
}

pub fn cmd_cp(args: &mut Args) {
    if !need_root("cp") {
        return;
    }
    let (Some(a), Some(b)) = (args.next(), args.next()) else { return usage("cp") };
    let src = abs(a);
    let mut dst = abs(b);
    if let Ok(st) = vfs().stat(&dst) {
        if st.ftype == FileType::Dir {
            let leaf = src.rsplit('/').next().unwrap_or("");
            dst = if dst == "/" { alloc::format!("/{}", leaf) } else { alloc::format!("{}/{}", dst, leaf) };
        }
    }
    if src == dst {
        return crate::kprintln!("cp: '{}' and '{}' are the same file", a, b);
    }
    let sfd = match vfs().open(&src, O_READ) {
        Ok(f) => f,
        Err(e) => return fail("cp", a, e),
    };
    let dfd = match vfs().open(&dst, O_WRITE | O_CREATE | O_TRUNC) {
        Ok(f) => f,
        Err(e) => {
            let _ = vfs().close(sfd);
            return fail("cp", &dst, e);
        }
    };
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    let res: Result<(), FsError> = (|| loop {
        let n = vfs().read(sfd, &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let mut done = 0;
        while done < n {
            let w = vfs().write(dfd, &buf[done..n])?;
            if w == 0 {
                return Err(FsError::NoSpace);
            }
            done += w;
        }
        total += n as u64;
    })();
    let _ = vfs().close(sfd);
    let _ = vfs().close(dfd);
    match res {
        Ok(()) => crate::kprintln!("{} -> {} ({})", a, dst, Size(total)),
        Err(e) => fail("cp", a, e),
    }
}

pub fn cmd_ln(args: &mut Args) {
    if !need_root("ln") {
        return;
    }
    let mut sym = false;
    let mut pos: Vec<&str> = Vec::new();
    while let Some(a) = args.next() {
        if a == "-s" {
            sym = true;
        } else {
            pos.push(a);
        }
    }
    if pos.len() != 2 {
        return usage("ln");
    }
    let r = if sym { vfs().symlink(pos[0], &abs(pos[1])) } else { vfs().link(&abs(pos[0]), &abs(pos[1])) };
    if let Err(e) = r {
        fail("ln", pos[1], e);
    }
}

pub fn cmd_touch(args: &mut Args) {
    if !need_root("touch") {
        return;
    }
    let mut n = 0;
    while let Some(a) = args.next() {
        n += 1;
        let path = abs(a);
        if vfs().exists(&path) {
            continue; // no utimes yet: an existing file is left alone
        }
        if let Err(e) = vfs().create(&path, 0o644) {
            fail("touch", a, e);
        }
    }
    if n == 0 {
        usage("touch");
    }
}

pub fn cmd_truncate(args: &mut Args) {
    if !need_root("truncate") {
        return;
    }
    let (Some(p), Some(n)) = (args.next(), args.next()) else { return usage("truncate") };
    let Some(n) = parse_u64(n) else { return crate::kprintln!("truncate: '{}' is not a number", n) };
    if let Err(e) = vfs().truncate(&abs(p), n) {
        fail("truncate", p, e);
    }
}

pub fn cmd_chmod(args: &mut Args) {
    if !need_root("chmod") {
        return;
    }
    let (Some(m), Some(p)) = (args.next(), args.next()) else { return usage("chmod") };
    let Ok(mode) = u16::from_str_radix(m, 8) else {
        return crate::kprintln!("chmod: '{}' is not an octal mode", m);
    };
    if let Err(e) = vfs().chmod(&abs(p), mode) {
        fail("chmod", p, e);
    }
}
