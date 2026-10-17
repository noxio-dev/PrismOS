// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Kernel console output: serial + VGA text or framebuffer, the bitmap fonts
// and the `Console` abstraction that fans text out to all of them.  The
// console API (`init`, `clear`, `char_width`, `_print`) is re-exported here.

pub mod fb;
#[allow(dead_code)] // legacy 8x8 font, superseded by unifont
pub mod font;
pub mod unifont;
pub mod vga;

#[allow(clippy::module_inception)]
mod console;
pub use self::console::*;
