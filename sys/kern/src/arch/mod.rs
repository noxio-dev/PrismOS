// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Everything that is specific to x86-64: CPU identification, descriptor
// tables, interrupt entry, port I/O and control registers.

pub mod cpu;
pub mod gdt;
pub mod idt;
pub mod intr;
pub mod io;
pub mod regs;
pub mod tss;
pub mod vm;
