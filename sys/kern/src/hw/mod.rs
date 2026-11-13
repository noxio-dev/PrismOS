// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Hardware drivers: interrupt controllers and timers (8259, PIT, APIC, HPET),
// the system clock built on them, firmware tables, serial and keyboard, PCI
// and the AHCI (SATA) block device.

pub mod acpi;
pub mod ahci;
pub mod apic;
pub mod blk;
pub mod clock;
pub mod disk;
pub mod hal;
pub mod hpet;
pub mod kbd;
pub mod keymap;
pub mod nvme;
pub mod pci;
pub mod pic;
pub mod pit;
pub mod rtc;
pub mod serial;
pub mod smp;
pub mod usbhid;
pub mod usbmsc;
pub mod xhci;
