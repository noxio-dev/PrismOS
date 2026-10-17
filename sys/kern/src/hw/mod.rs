// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Hardware drivers: interrupt controllers and timers, firmware tables, serial
// and keyboard, PCI and the AHCI (SATA) block device.

pub mod acpi;
pub mod ahci;
pub mod apic;
pub mod blk;
pub mod kbd;
pub mod pci;
pub mod pic;
pub mod pit;
pub mod rtc;
pub mod serial;
