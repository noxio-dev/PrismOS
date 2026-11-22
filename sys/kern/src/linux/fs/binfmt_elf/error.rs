// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Why the loader refused a program: a Linux errno (what `execve` would
// return) and a few words for the kernel log.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadError {
    /// A positive Linux errno (uapi/errno.rs).
    pub errno: i32,
    pub what: &'static str,
}

impl LoadError {
    pub const fn new(errno: i32, what: &'static str) -> LoadError {
        LoadError { errno, what }
    }
}
