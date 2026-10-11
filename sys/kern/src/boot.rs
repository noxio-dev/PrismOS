// SPDX-License-Identifier: GPL-3.0-or-later
//
// Rust mirror of include/prism/boot.h: the boot protocol between the PrismOS
// loader (BIOS and UEFI) and the kernel.  Keep both files in sync.

#![allow(dead_code)]

use core::ffi::CStr;

pub const BOOT_MAGIC: u64 = 0x4F4F_424D_5349_5250; // "PRISMBOO"
pub const BOOT_VERSION: u32 = 1;

pub const FW_BIOS: u32 = 1;
pub const FW_UEFI: u32 = 2;

pub const FLAG_FB: u32 = 1 << 0;
pub const FLAG_VGATEXT: u32 = 1 << 1;

pub const MEM_USABLE: u32 = 1;
pub const MEM_RESERVED: u32 = 2;
pub const MEM_ACPI_RECL: u32 = 3;
pub const MEM_ACPI_NVS: u32 = 4;
pub const MEM_BAD: u32 = 5;
pub const MEM_LOADER: u32 = 6;
pub const MEM_KERNEL: u32 = 7;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MmapEntry {
    pub base: u64,
    pub length: u64,
    pub kind: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Module {
    pub phys: u64,
    pub size: u64,
    pub name: [u8; 32],
    pub kind: [u8; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Framebuffer {
    pub addr: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub red_shift: u8,
    pub red_bits: u8,
    pub green_shift: u8,
    pub green_bits: u8,
    pub blue_shift: u8,
    pub blue_bits: u8,
    pub reserved: u16,
}

#[repr(C)]
pub struct BootInfo {
    pub magic: u64,
    pub version: u32,
    pub size: u32,
    pub firmware: u32,
    pub flags: u32,
    pub kernel_phys: u64,
    pub kernel_virt: u64,
    pub kernel_size: u64,
    pub mmap_phys: u64,
    pub mmap_count: u32,
    pub mmap_entry_size: u32,
    pub modules_phys: u64,
    pub module_count: u32,
    pub module_entry_size: u32,
    pub cmdline_phys: u64,
    pub env_phys: u64,
    pub rsdp_phys: u64,
    pub smbios_phys: u64,
    pub efi_system_table: u64,
    pub pml4_phys: u64,
    pub fb: Framebuffer,
}

impl BootInfo {
    /// Validate the structure handed over by the loader.
    ///
    /// # Safety
    /// `ptr` must be the pointer received from the loader.
    pub unsafe fn from_ptr<'a>(ptr: *const BootInfo) -> Option<&'a BootInfo> {
        if ptr.is_null() {
            return None;
        }
        let bi = &*ptr;
        if bi.magic != BOOT_MAGIC || bi.version != BOOT_VERSION {
            return None;
        }
        if (bi.size as usize) < core::mem::size_of::<BootInfo>() {
            return None;
        }
        Some(bi)
    }

    pub fn firmware_name(&self) -> &'static str {
        match self.firmware {
            FW_BIOS => "BIOS",
            FW_UEFI => "UEFI",
            _ => "unknown",
        }
    }

    pub fn memory_map(&self) -> &[MmapEntry] {
        if self.mmap_phys == 0 || self.mmap_entry_size as usize != core::mem::size_of::<MmapEntry>() {
            return &[];
        }
        // SAFETY: the loader maps the table (identity) and keeps it intact.
        unsafe { core::slice::from_raw_parts(self.mmap_phys as *const MmapEntry, self.mmap_count as usize) }
    }

    pub fn modules(&self) -> &[Module] {
        if self.modules_phys == 0 || self.module_entry_size as usize != core::mem::size_of::<Module>() {
            return &[];
        }
        // SAFETY: as above.
        unsafe { core::slice::from_raw_parts(self.modules_phys as *const Module, self.module_count as usize) }
    }

    pub fn cmdline(&self) -> &str {
        if self.cmdline_phys == 0 {
            return "";
        }
        // SAFETY: the loader stores a NUL terminated string.
        unsafe { CStr::from_ptr(self.cmdline_phys as *const core::ffi::c_char) }
            .to_str()
            .unwrap_or("")
    }

    /// Total usable RAM in bytes (usable + reclaimable loader memory).
    pub fn usable_bytes(&self) -> u64 {
        self.memory_map()
            .iter()
            .filter(|e| e.kind == MEM_USABLE || e.kind == MEM_LOADER)
            .map(|e| e.length)
            .sum()
    }
}
