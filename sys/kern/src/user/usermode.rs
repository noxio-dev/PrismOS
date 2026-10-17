// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Ring 3 entry and exit (iretq / sysret path).  Needs the user selectors
// (arch/gdt.rs: USER_CS / USER_DS) and TSS.RSP0 (arch/tss.rs).
