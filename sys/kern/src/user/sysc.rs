// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The memory / process half of the newlib syscall set (phase 3.1); the
// file-descriptor half lives in user/fd.rs.  What is here:
//
//   21 brk(addr)                      the program break: sbrk's engine
//   22 mmap(addr,len,prot,flags,fd,off) anonymous and private file
//                                      mappings, first-fit above
//                                      USER_MMAP_BASE (vmm.rs)
//   23 munmap(addr, len)              give pages back
//   24 chdir(path)                    the per-process cwd (process.rs)
//   25 getcwd(buf, size)              ... and read it back
//   35 uname(buf)                     struct utsname, the classic six
//
// Design notes.
//
// * `brk` keeps the Unix of 1975 contract: one break per process, growing
//   up from the end of the loaded image, page-granular underneath
//   (byte-granular in the value), `-` nothing on failure -- Linux answers
//   the *old* break and so do we.  The heap lives between the image and
//   USER_MMAP_BASE; newlib's `sbrk` (phase 3.2) is a thin wrapper around
//   it.  A fork child inherits the break with the copy-on-write pages.
// * `mmap` supports MAP_ANONYMOUS (zero pages, MAP_SHARED included: it is
//   just memory) and MAP_PRIVATE file mappings (filled eagerly through
//   the VFS `pread` -- the description's offset is never touched; bytes
//   past EOF read as zero instead of the POSIX SIGBUS).  W^X is enforced
//   like everywhere (PROT_WRITE | PROT_EXEC is refused), PROT_NONE too
//   (nothing maps nothing here).  Without MAP_FIXED the address is a hint
//   we ignore: the first fit above USER_MMAP_BASE wins.  MAP_SHARED file
//   mappings are -ENOSYS -- there is no page cache to write back through
//   yet.  Free ranges are found by probing the page tables (`range_free`),
//   so there is no VMA list; munmap unmasks whatever is there (absent
//   pages are skipped, like on Linux).
// * `chdir` / `getcwd`: the cwd is a plain String in the PCB (fork copies
//   it, execve keeps it; the shell's own cwd is a different one in
//   fsys.rs).  `getcwd` returns the length including the NUL, -ERANGE
//   when the buffer cannot hold it -- the contract newlib's getcwd
//   wrapper expects.
// * `uname` fills the 6 x 65-byte utsname: "PrismOS", "prismos", "3.1",
//   a build string, "x86_64", "(none)".

use alloc::string::String;
use alloc::vec::Vec;

use crate::fs::vfs::{FileType, O_READ};
use crate::mem::vmm::{self, AddressSpace, PAGE};
use crate::user::process;
use crate::user::usermode::{EBADF, EFAULT, EINVAL, ENOSYS};

const ENOENT: i64 = 2;
const ENOMEM: i64 = 12;
const EACCES: i64 = 13;
const ENODEV: i64 = 19;
const ENOTDIR: i64 = 20;
const EISDIR: i64 = 21;
const ERANGE: i64 = 34;
const ENAMETOOLONG: i64 = 36;

// mmap bits (the Linux values, which is what callers pass).
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;
const MAP_SHARED: u64 = 1;
const MAP_PRIVATE: u64 = 2;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_KNOWN: u64 = MAP_SHARED | MAP_PRIVATE | MAP_FIXED | MAP_ANONYMOUS;

/// Largest single mapping (16 MiB: the 256 MiB machine must survive a
/// program asking for the world; -ENOMEM beyond it).
const MAX_MAP: u64 = 16 << 20;
/// Upper bound of the mmap region: stay clear of the stack at the top of
/// user space (16 KiB + guard, but a megabyte reads better).
const MMAP_LIMIT: u64 = vmm::USER_STACK_TOP - (1 << 20);

fn page_up(x: u64) -> u64 {
    (x + PAGE - 1) & !(PAGE - 1)
}

// ----------------------------------------------------------------- brk

/// `brk(addr)` (syscall 21): set the program break when `addr` is a valid
/// one (between the image end and USER_MMAP_BASE), answer the current
/// break otherwise (Linux: brk(0) is the portable query).  Growth maps
/// zero pages, shrink unmaps whole pages; the value itself may be
/// byte-granular -- the pages are rounded up, like everywhere.
pub fn sys_brk(addr: u64) -> i64 {
    let Some(cur) = process::current() else { return -EFAULT };
    let (base, old) = (cur.brk_base, cur.brk);
    let limit = vmm::USER_MMAP_BASE;
    let new = if addr >= base && addr <= limit { addr } else { old };
    if new == old {
        return old as i64;
    }
    let mut ok = true;
    if new > old {
        // Grow: map the pages the rounded-up span adds.  The PMM is
        // checked up front (with the margin map_region keeps), so a
        // half-mapped heap is a corner case, not the plan.
        let from = page_up(old);
        let to = page_up(new);
        if to > from {
            let pages = (to - from) / PAGE;
            let free = crate::mem::pmm::stats_snapshot().free_pages.load(core::sync::atomic::Ordering::Relaxed);
            if free < pages + 256 {
                ok = false;
            } else {
                let Some(a) = cur.aspace.as_mut() else { return -EFAULT };
                if !a.alloc_map(from, to - from, true, false) {
                    // alloc_map leaves the pages it already mapped: undo.
                    // SAFETY: syscall context, our CR3 is live.
                    unsafe { a.unmap(from, to - from) };
                    ok = false;
                }
            }
        }
    } else {
        // Shrink: unmap the pages the rounded spans stop covering.
        let from = page_up(new);
        let to = page_up(old);
        if to > from {
            let Some(a) = cur.aspace.as_mut() else { return -EFAULT };
            // SAFETY: syscall context, our CR3 is live.
            unsafe { a.unmap(from, to - from) };
        }
    }
    if ok {
        let Some(cur) = process::current() else { return -EFAULT };
        cur.brk = new;
        new as i64
    } else {
        old as i64 // the Unix answer to a brk that cannot happen
    }
}

// ----------------------------------------------------------------- mmap

/// `mmap(addr, len, prot, flags, fd, off)` (syscall 22).  Six arguments
/// (r10 / r8 / r9 carry 4..6).  Returns the mapping address or -errno;
/// see the header for the supported subset.
pub fn sys_mmap(a: [u64; 6]) -> i64 {
    let (addr, len, prot, flags, fd, off) = (a[0], a[1], a[2], a[3], a[4], a[5]);

    if len == 0 {
        return -EINVAL;
    }
    if len > MAX_MAP {
        return -ENOMEM;
    }
    if flags & !MAP_KNOWN != 0 {
        return -EINVAL; // unknown bits: refuse rather than guess
    }
    if flags & (MAP_SHARED | MAP_PRIVATE) == 0
        || flags & (MAP_SHARED | MAP_PRIVATE) == (MAP_SHARED | MAP_PRIVATE)
    {
        return -EINVAL; // exactly one of the two, POSIX says
    }
    if prot == 0 {
        return -EINVAL; // PROT_NONE: nothing maps nothing here
    }
    if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return -EINVAL; // unknown protection bits: refuse rather than guess
    }
    if prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 {
        return -EACCES; // W^X, the same rule the ELF loader enforces
    }
    let w = prot & PROT_WRITE != 0;
    let x = prot & PROT_EXEC != 0;
    let len = page_up(len);

    // ---- the fd side first (mmap of a file): resolve it before any
    // address-space borrowing, so the two never overlap.
    let mut file: Option<usize> = None; // the VFS slot of a file mapping
    if flags & MAP_ANONYMOUS == 0 {
        if flags & MAP_SHARED != 0 {
            return -ENOSYS; // no page cache to write back through yet
        }
        if off & (PAGE - 1) != 0 {
            return -EINVAL;
        }
        let desc = match crate::user::fd::desc_of(fd as usize) {
            Some(d) => d,
            None => return -EBADF,
        };
        if desc.flags() & O_READ == 0 {
            return -EACCES; // not opened for reading
        }
        match crate::user::fd::vfs_slot_of(&desc) {
            Some(slot) => file = Some(slot),
            None => return -ENODEV, // a device / pipe: nothing to map
        }
        let st = match crate::fs::fsys::vfs().fstat(file.unwrap()) {
            Ok(s) => s,
            Err(_) => return -ENODEV, // the description went stale mid-call
        };
        if st.ftype == FileType::Dir {
            return -EISDIR;
        }
    }

    // ---- where: MAP_FIXED pins, otherwise first-fit above the base.
    let Some(cur) = process::current() else { return -EFAULT };
    let Some(aspace) = cur.aspace.as_mut() else { return -EFAULT };
    let start = if flags & MAP_FIXED != 0 {
        if addr & (PAGE - 1) != 0 {
            return -EINVAL;
        }
        // Whatever was there goes first (Linux semantics).
        // SAFETY: syscall context, our CR3 is live.
        unsafe { aspace.unmap(addr, len) };
        addr
    } else {
        let mut v = vmm::USER_MMAP_BASE;
        loop {
            if v + len > MMAP_LIMIT {
                return -ENOMEM; // no gap big enough
            }
            if aspace.range_free(v, len) {
                break;
            }
            v += PAGE; // a first-fit probe: fine for pages of small maps
        }
        v
    };
    if start & (PAGE - 1) != 0 || start < vmm::USER_BASE || start + len > MMAP_LIMIT {
        return -ENOMEM;
    }
    // The signal-return trampoline page is kernel-promise territory: a
    // mapping over it would break every future handler return.
    let sig_lo = vmm::USER_SIGCODE & !(PAGE - 1);
    if start < sig_lo + PAGE && start + len > sig_lo {
        return -ENOMEM;
    }

    // ---- the pages.  The PMM margin covers the fill's page tables too.
    let pages = len / PAGE;
    let free = crate::mem::pmm::stats_snapshot().free_pages.load(core::sync::atomic::Ordering::Relaxed);
    if free < pages + 256 + pages / 512 {
        return -ENOMEM;
    }
    // A file mapping is filled through the kernel alias (write with
    // force), so the pages can carry the *asked* protection from the
    // start -- no reprotect pass, no window of writable rodata.
    if !aspace.alloc_map(start, len, w, x) {
        // SAFETY: syscall context, our CR3 is live.
        unsafe { aspace.unmap(start, len) }; // partial map: undo
        return -ENOMEM;
    }

    // ---- the file fill: chunked pread (never touching the fd offset),
    // zeros beyond EOF.
    if let Some(slot) = file {
        let vfs = crate::fs::fsys::vfs();
        let mut chunk = [0u8; 512];
        let mut pos = 0u64;
        while pos < len {
            let n = (len - pos).min(chunk.len() as u64) as usize;
            let got = match vfs.pread(slot, off + pos, &mut chunk[..n]) {
                Ok(0) => break, // EOF: the rest of the mapping stays zero
                Ok(k) => k,
                Err(_) => {
                    // SAFETY: syscall context, our CR3 is live.
                    unsafe { aspace.unmap(start, len) };
                    return -ENOMEM; // the fill failed: no half-mapped file
                }
            };
            if !aspace.write(start + pos, &chunk[..got], true) {
                // SAFETY: syscall context, our CR3 is live.
                unsafe { aspace.unmap(start, len) };
                return -EFAULT;
            }
            pos += got as u64;
        }
    }
    start as i64
}

// ---------------------------------------------------------------- munmap

/// `munmap(addr, len)` (syscall 23): give the pages back.  A page-aligned
/// `addr` is required, the length is rounded up, absent pages inside the
/// range are skipped -- like on Linux, unmapping a hole is not an error.
pub fn sys_munmap(addr: u64, len: u64) -> i64 {
    if len == 0 {
        return 0;
    }
    if addr & (PAGE - 1) != 0 {
        return -EINVAL;
    }
    let Some(cur) = process::current() else { return -EFAULT };
    let Some(aspace) = cur.aspace.as_mut() else { return -EFAULT };
    // SAFETY: syscall context, our CR3 is live.
    if unsafe { aspace.unmap(addr, len) } {
        0
    } else {
        -EINVAL
    }
}

// ------------------------------------------------------------ chdir/getcwd

/// `chdir(path)` (syscall 24): the process's own cwd (process.rs) --
/// verified to be a directory first, like everywhere.
pub fn sys_chdir(path_p: u64) -> i64 {
    let got: Result<String, i64> = {
        let Some(cur) = process::current() else { return -EFAULT };
        let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return -EFAULT };
        match crate::user::syscall::read_user_cstr(aspace, path_p, 255) {
            Ok(v) => match String::from_utf8(v) {
                Ok(s) => Ok(s),
                Err(_) => Err(-ENOENT), // like execve: a non-utf8 path is a no-such-file
            },
            Err(-7) => Err(-ENAMETOOLONG),
            Err(e) => Err(e),
        }
    };
    let path = match got {
        Ok(p) => p,
        Err(e) => return e,
    };
    if path.is_empty() {
        return -ENOENT;
    }
    let abs = process::abs_current(&path);
    match crate::fs::fsys::vfs().stat(&abs) {
        Ok(st) if st.ftype == FileType::Dir => {
            let Some(cur) = process::current() else { return -EFAULT };
            cur.cwd = abs;
            0
        }
        Ok(_) => -ENOTDIR,
        Err(e) => -crate::user::fd::fs_errno(e),
    }
}

/// `getcwd(buf, size)` (syscall 25): the absolute cwd, NUL-terminated;
/// returns its length including the NUL (the raw-Linux contract), or
/// -ERANGE / -EFAULT.
pub fn sys_getcwd(buf: u64, size: u64) -> i64 {
    let cwd = {
        let Some(cur) = process::current() else { return -EFAULT };
        if cur.cwd.is_empty() { String::from("/") } else { cur.cwd.clone() }
    };
    let need = cwd.len() + 1;
    if (size as usize) < need {
        return -ERANGE;
    }
    let mut bytes = Vec::with_capacity(need);
    bytes.extend_from_slice(cwd.as_bytes());
    bytes.push(0);
    let Some(cur) = process::current() else { return -EFAULT };
    let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return -EFAULT };
    if !aspace.ensure_writable(buf) || !aspace.write(buf, &bytes, false) {
        return -EFAULT;
    }
    need as i64
}

// ----------------------------------------------------------------- uname

/// `uname(buf)` (syscall 35): the 6 x 65-byte utsname (sysname, nodename,
/// release, version, machine, domainname) -- "PrismOS 3.1" in the classic
/// shape, what uname(1)-style tools print.
pub fn sys_uname(buf: u64) -> i64 {
    const FIELDS: [&str; 6] = [
        "PrismOS",
        "prismos",
        "3.1",
        "PrismOS 3.1 (phase 3.1: the newlib syscall set)",
        "x86_64",
        "(none)",
    ];
    let mut out = [0u8; 6 * 65];
    for (i, f) in FIELDS.iter().enumerate() {
        let b = f.as_bytes();
        let n = b.len().min(64);
        out[i * 65..i * 65 + n].copy_from_slice(&b[..n]);
    }
    let Some(cur) = process::current() else { return -EFAULT };
    let Some(aspace): Option<&AddressSpace> = cur.aspace.as_ref() else { return -EFAULT };
    if !aspace.ensure_writable(buf) || !aspace.write(buf, &out, false) {
        return -EFAULT;
    }
    0
}
