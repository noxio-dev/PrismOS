// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of phase 2.2 (shared memory for Xorg), the file-system
// half: the tmpfs keeps its data in 4 KiB frames, so a page of a file can be
// mapped MAP_SHARED.  What is checked here, with the real vfs.rs + tmpfs.rs:
//
//   * holes read as zeros and cost nothing, sparse writes cross pages;
//   * truncate: a shrink frees the frames and zeroes the tail, a regrowth
//     never shows stale bytes; the byte quota follows;
//   * an unlinked file lives while a descriptor holds it (the shm_open +
//     shm_unlink idiom, and what memfd_create is made of), and dies with
//     the last close;
//   * `file_page` hands out the file's own frame: the same one twice, with
//     what write() stored in it, and writes through it show up in read();
//     past the end of the file there is nothing to map.
//
//   rustc --edition 2021 --test tests/shm_host.rs -o /tmp/shm_host \
//     && /tmp/shm_host --test-threads=1

#![allow(dead_code, unused_imports)]
extern crate alloc;

mod hw {
    pub mod blk {
        #[derive(Debug)]
        pub enum BlkError {
            ReadOnly,
            Other,
        }
    }
}
#[path = "../sys/kern/src/fs/vfs.rs"]
pub mod vfs_impl;
#[path = "../sys/kern/src/fs/tmpfs.rs"]
pub mod tmpfs_impl;
mod fs {
    pub use super::tmpfs_impl as tmpfs;
    pub use super::vfs_impl as vfs;
}

use fs::vfs::*;

const PAGE: usize = 4096;

fn fresh(max_bytes: u64) -> Vfs {
    let mut v = Vfs::new();
    v.mount("/", Box::new(fs::tmpfs::TmpFs::with_limits(max_bytes, 64))).unwrap();
    v
}

/// Create /name read-write and return its descriptor slot.
fn make(v: &mut Vfs, name: &str) -> usize {
    match v.open_desc(name, O_READ | O_WRITE | O_CREATE | O_EXCL).unwrap() {
        Opened::File(fd) => fd,
        _ => panic!("not a file"),
    }
}

fn used(v: &mut Vfs) -> u64 {
    let s = v.statfs("/").unwrap();
    (s.blocks - s.free_blocks) * s.block_size as u64
}

fn read_all(v: &mut Vfs, fd: usize, len: usize) -> Vec<u8> {
    let mut buf = vec![0xAAu8; len];
    let mut got = 0;
    while got < len {
        let n = v.pread(fd, got as u64, &mut buf[got..]).unwrap();
        if n == 0 {
            break;
        }
        got += n;
    }
    buf.truncate(got);
    buf
}

#[test]
fn holes_read_zero_and_sparse_writes_cross_pages() {
    let mut v = fresh(1 << 20);
    let fd = make(&mut v, "/f");
    v.ftruncate(fd, 3 * PAGE as u64).unwrap();
    assert_eq!(v.fstat(fd).unwrap().size, 3 * PAGE as u64);
    assert!(read_all(&mut v, fd, 3 * PAGE).iter().all(|&b| b == 0), "a fresh hole reads zero");

    // A write straddling the first page boundary.
    v.seek(fd, Whence::Set(PAGE as u64 - 3)).unwrap();
    assert_eq!(v.write(fd, b"abcdefg").unwrap(), 7);
    let all = read_all(&mut v, fd, 3 * PAGE);
    assert_eq!(&all[PAGE - 3..PAGE + 4], b"abcdefg");
    assert!(all[..PAGE - 3].iter().all(|&b| b == 0));
    assert!(all[PAGE + 4..].iter().all(|&b| b == 0));
    v.close(fd).unwrap();
}

#[test]
fn write_past_the_end_grows_and_zero_fills_the_gap() {
    let mut v = fresh(1 << 20);
    let fd = make(&mut v, "/f");
    v.seek(fd, Whence::Set(10_000)).unwrap();
    v.write(fd, b"tail").unwrap();
    assert_eq!(v.fstat(fd).unwrap().size, 10_004);
    let all = read_all(&mut v, fd, 10_004);
    assert!(all[..10_000].iter().all(|&b| b == 0));
    assert_eq!(&all[10_000..], b"tail");
    v.close(fd).unwrap();
}

#[test]
fn truncate_zeroes_the_tail_and_regrowth_is_clean() {
    let mut v = fresh(1 << 20);
    let fd = make(&mut v, "/f");
    v.write(fd, &vec![0x55u8; 2 * PAGE]).unwrap();
    v.ftruncate(fd, PAGE as u64 + 100).unwrap(); // mid-page cut
    assert_eq!(v.fstat(fd).unwrap().size, PAGE as u64 + 100);
    v.ftruncate(fd, 2 * PAGE as u64).unwrap(); // grow back
    let all = read_all(&mut v, fd, 2 * PAGE);
    assert!(all[..PAGE + 100].iter().all(|&b| b == 0x55));
    assert!(all[PAGE + 100..].iter().all(|&b| b == 0), "no stale bytes after regrowth");
    // Down to zero and up again: all zeros, in a page that was freed.
    v.ftruncate(fd, 0).unwrap();
    v.ftruncate(fd, PAGE as u64).unwrap();
    assert!(read_all(&mut v, fd, PAGE).iter().all(|&b| b == 0));
    v.close(fd).unwrap();
}

#[test]
fn quota_follows_truncate() {
    let mut v = fresh(64 * 1024);
    let fd = make(&mut v, "/f");
    v.ftruncate(fd, 40 * 1024).unwrap();
    assert_eq!(used(&mut v), 40 * 1024);
    assert_eq!(v.ftruncate(fd, 65 * 1024), Err(FsError::NoSpace));
    assert_eq!(v.fstat(fd).unwrap().size, 40 * 1024, "a refused growth changes nothing");
    v.ftruncate(fd, 4 * 1024).unwrap();
    assert_eq!(used(&mut v), 4 * 1024);
    // fallocate: grows like ftruncate, keep_size does not.
    v.fallocate_fd(fd, 0, 16 * 1024, false).unwrap();
    assert_eq!(v.fstat(fd).unwrap().size, 16 * 1024);
    v.fallocate_fd(fd, 0, 32 * 1024, true).unwrap();
    assert_eq!(v.fstat(fd).unwrap().size, 16 * 1024);
    v.close(fd).unwrap();
}

#[test]
fn unlinked_file_lives_until_the_last_close() {
    let mut v = fresh(1 << 20);
    let fd = make(&mut v, "/shm");
    v.write(fd, b"still here").unwrap();
    v.unlink("/shm").unwrap(); // shm_unlink right after shm_open
    assert!(v.read_file("/shm", 64).is_err(), "the name is gone");
    // The descriptor still reads, writes, resizes.
    assert_eq!(read_all(&mut v, fd, 10), b"still here");
    v.ftruncate(fd, 2 * PAGE as u64).unwrap();
    v.seek(fd, Whence::Set(PAGE as u64)).unwrap();
    v.write(fd, b"more").unwrap();
    assert_eq!(used(&mut v), 2 * PAGE as u64);
    // The last close takes it all away.
    v.close(fd).unwrap();
    assert_eq!(used(&mut v), 0, "bytes and inode go back with the last descriptor");
}

#[test]
fn two_descriptors_keep_an_unlinked_file_alive() {
    let mut v = fresh(1 << 20);
    let a = make(&mut v, "/x");
    v.write(a, b"abc").unwrap();
    let b = match v.open_desc("/x", O_READ).unwrap() {
        Opened::File(fd) => fd,
        _ => panic!(),
    };
    v.unlink("/x").unwrap();
    v.close(a).unwrap();
    assert_eq!(read_all(&mut v, b, 3), b"abc", "the second descriptor still holds it");
    assert_eq!(used(&mut v), PAGE as u64, "statfs counts whole blocks: 3 bytes are one");
    v.close(b).unwrap();
    assert_eq!(used(&mut v), 0);
}

#[test]
fn file_page_is_the_files_own_frame() {
    let mut v = fresh(1 << 20);
    let fd = make(&mut v, "/m");
    v.ftruncate(fd, 2 * PAGE as u64).unwrap();

    // A hole becomes a zeroed frame, and it is the same frame every time.
    let f0 = v.file_page(fd, 0).unwrap();
    assert_ne!(f0, 0);
    assert_eq!(f0, v.file_page(fd, 0).unwrap());
    let f1 = v.file_page(fd, 1).unwrap();
    assert_ne!(f0, f1);
    // SAFETY: a live host frame of PAGE bytes owned by the file.
    let page0 = unsafe { core::slice::from_raw_parts_mut(page_ptr(f0), PAGE) };
    assert!(page0.iter().all(|&b| b == 0));

    // What write() stores is what a mapping sees...
    v.write(fd, b"hello").unwrap();
    assert_eq!(&page0[..5], b"hello");
    // ...and what a mapping stores is what read() returns.
    page0[PAGE - 1] = 0x7F;
    let all = read_all(&mut v, fd, 2 * PAGE);
    assert_eq!(all[PAGE - 1], 0x7F);

    // Nothing to map past the end of the file.
    assert_eq!(v.file_page(fd, 2), Err(FsError::Invalid));
    v.close(fd).unwrap();
}

#[test]
fn shrinking_frees_frames_but_not_the_ones_kept() {
    let mut v = fresh(1 << 20);
    let fd = make(&mut v, "/m");
    v.ftruncate(fd, 3 * PAGE as u64).unwrap();
    let f0 = v.file_page(fd, 0).unwrap();
    let _f2 = v.file_page(fd, 2).unwrap();
    v.ftruncate(fd, PAGE as u64).unwrap();
    assert_eq!(v.file_page(fd, 0).unwrap(), f0, "the kept page is the same frame");
    assert_eq!(v.file_page(fd, 2), Err(FsError::Invalid));
    // Regrowing gives a fresh, zeroed page 2 (the old frame is gone).
    v.ftruncate(fd, 3 * PAGE as u64).unwrap();
    let f2 = v.file_page(fd, 2).unwrap();
    // SAFETY: a live host frame of PAGE bytes owned by the file.
    let page2 = unsafe { core::slice::from_raw_parts(page_ptr(f2), PAGE) };
    assert!(page2.iter().all(|&b| b == 0));
    v.close(fd).unwrap();
}

#[test]
fn only_tmpfs_has_frames_to_share() {
    // A file system that keeps the trait's default has no page_frame.
    struct Bare;
    impl FileSystem for Bare {
        fn fs_name(&self) -> &'static str {
            "bare"
        }
        fn root(&self) -> u64 {
            1
        }
        fn read_only(&self) -> bool {
            true
        }
        fn statfs(&mut self) -> FsResult<StatFs> {
            Ok(StatFs::default())
        }
        fn stat(&mut self, _ino: u64) -> FsResult<Stat> {
            Err(FsError::NotFound)
        }
        fn lookup(&mut self, _dir: u64, _name: &str) -> FsResult<u64> {
            Err(FsError::NotFound)
        }
        fn readdir(&mut self, _dir: u64) -> FsResult<Vec<DirEntry>> {
            Ok(Vec::new())
        }
        fn read(&mut self, _ino: u64, _off: u64, _buf: &mut [u8]) -> FsResult<usize> {
            Ok(0)
        }
        fn readlink(&mut self, _ino: u64) -> FsResult<String> {
            Err(FsError::Invalid)
        }
    }
    let mut b = Bare;
    assert_eq!(b.page_frame(1, 0), Err(FsError::Unsupported));
}
