// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Kernel VFS, ext4 driver and tmpfs, built unchanged on the host.

#[path = "../../../sys/kern/src/fs/vfs.rs"]
pub mod vfs;
#[path = "../../../sys/kern/src/fs/ext4/mod.rs"]
pub mod ext4;
#[path = "../../../sys/kern/src/fs/tmpfs.rs"]
pub mod tmpfs;
