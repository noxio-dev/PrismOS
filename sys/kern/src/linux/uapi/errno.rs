// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Linux x86-64 errno values (include/uapi/asm-generic/errno*.h), the ones
// the layer uses so far.  Errors travel as these positive numbers; a system
// call returns them negated.

pub const ENOENT: i32 = 2;
pub const EIO: i32 = 5;
pub const E2BIG: i32 = 7;
pub const ENOEXEC: i32 = 8;
pub const ENOMEM: i32 = 12;
pub const EACCES: i32 = 13;
pub const EINVAL: i32 = 22;
/// "Accessing a corrupted shared library": what `execve` reports for a bad
/// program interpreter.
pub const ELIBBAD: i32 = 80;
