// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// File systems.
//
//   vfs.rs    the `FileSystem` trait, mount table, path walk and the open-file
//             table.  It only depends on `alloc` / `core` (and hw/blk.rs), so
//             tests/fstest builds it on the host together with ext4/.
//   ext4/     the ext2/3/4 driver
//   fsys.rs   kernel glue: global VFS, cwd, GPT scan, boot-time mount, clock

pub mod ext4;
pub mod fsys;
pub mod vfs;
