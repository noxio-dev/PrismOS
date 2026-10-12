// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// PrismOS kernel.  For now it only says hello.
//
// Entered by the PrismOS loader in 64-bit long mode with the boot info
// pointer in rdi (see boot.rs and include/prism/boot.h).

#![no_std]
#![no_main]

mod boot;
mod console;
mod cpu;
mod fb;
mod font;
mod serial;
mod vga;
mod vm;
mod io;
mod kbd;
mod line;
mod sh;

use boot::BootInfo;
use core::arch::asm;
use core::panic::PanicInfo;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(boot_info: *const BootInfo) -> ! {
    // SAFETY: the pointer comes straight from the loader.
    let bi = unsafe { BootInfo::from_ptr(boot_info) };
    console::init(bi);

    kprintln!("Hello, world!");
    kprintln!();
    kprintln!("PrismOS kernel {} (Rust, x86_64)", env!("CARGO_PKG_VERSION"));

    match bi {
        Some(bi) => {
            kprintln!("firmware : {}", bi.firmware_name());
            kprintln!("memory   : {} MiB usable", bi.usable_bytes() >> 20);
            kprintln!(
                "kernel   : {} KiB at phys {:#x}, virt {:#x}",
                bi.kernel_size >> 10,
                bi.kernel_phys,
                bi.kernel_virt
            );
            if bi.flags & boot::FLAG_FB != 0 {
                kprintln!("display  : {}x{} framebuffer", bi.fb.width, bi.fb.height);
            } else {
                kprintln!("display  : VGA text 80x25");
            }
            kprintln!("modules  : {}", bi.modules().len());
            kprintln!("cmdline  : \"{}\"", bi.cmdline());
        }
        None => kprintln!("warning: no valid boot info from the loader"),
    }

        kprintln!();

    // --- Détection CPU / VM ---------------------------------------------
    let cpu = cpu::Cpu::detect();
    let brand = cpu.brand();
    kprintln!("cpu      : {}", if brand.is_empty() { "unknown" } else { brand });
    kprintln!(
        "vendor   : {}, family {} model {} stepping {}",
        cpu.vendor(),
        cpu.family(),
        cpu.model(),
        cpu.stepping()
    );
    let mut feats = [0u8; 256];
    kprintln!("features : {}", cpu.features_into(&mut feats));

    match vm::detect() {
        Some(vm) => {
            kprintln!(
                "machine  : {} (sig \"{}\", hv max leaf {:#x})",
                vm.name(),
                vm.signature_str(),
                vm.max_leaf
            );
            if let Some(pv) = vm.kvm_features() {
                kprintln!("kvm      : paravirt features {:#010x}", pv);
            }
        }
        None => kprintln!("machine  : bare metal"),
    }

    // The boot report is done: drop to the interactive shell.
    sh::set_boot_info(bi);
    sh::run()
}

pub(crate) fn halt() -> ! {
    loop {
        // SAFETY: nothing left to run, sleep forever.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("\nKERNEL PANIC: {}", info);
    halt()
}

// ---------------------------------------------------------------------------
// Memory primitives.  compiler_builtins provides weak versions on some
// toolchain configurations only, so define them here (strong symbols win).
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    asm!("rep movsb", inout("rcx") n => _, inout("rdi") dst => _, inout("rsi") src => _,
         options(nostack, preserves_flags));
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dst as usize) <= (src as usize) || (dst as usize) >= (src as usize) + n {
        return memcpy(dst, src, n);
    }
    asm!("std", "rep movsb", "cld",
         inout("rcx") n => _,
         inout("rdi") dst.add(n).wrapping_sub(1) => _,
         inout("rsi") src.add(n).wrapping_sub(1) => _,
         options(nostack));
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut u8, c: i32, n: usize) -> *mut u8 {
    asm!("rep stosb", inout("rcx") n => _, inout("rdi") dst => _, in("al") c as u8,
         options(nostack, preserves_flags));
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let (x, y) = (a.add(i).read_volatile(), b.add(i).read_volatile());
        if x != y {
            return x as i32 - y as i32;
        }
        i += 1;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    memcmp(a, b, n)
}
