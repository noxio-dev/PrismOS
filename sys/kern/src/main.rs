// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// PrismOS kernel.
//
// Entered by the PrismOS loader in 64-bit long mode with the boot info
// pointer in rdi (see boot.rs and include/prism/boot.h): interrupts off,
// CS=0x08 / DS=0x10, first 4 GiB identity mapped, kernel at KERNBASE.
//
// Bring-up order:
//   console -> GDT/TSS -> IDT -> PIC (masked) -> PIT -> serial IRQ
//   -> PMM -> VMM (own tables, CR3 switch) -> heap -> keyboard (i8042)
//   -> sti -> shell
//
// Every step logs one line so the boot output doubles as a bring-up check.

#![no_std]
#![no_main]

mod boot;
mod console;
mod cpu;
mod fb;
mod font;
mod heap;
mod int;
mod io;
mod kbd;
mod line;
mod pic;
mod pit;
mod pmm;
mod rtc;
mod serial;
mod sh;
mod vga;
mod vm;
mod vmm;
mod snake;

use boot::BootInfo;
use core::arch::asm;
use core::panic::PanicInfo;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(boot_info: *const BootInfo) -> ! {
    // The loader enters with interrupts masked; make it explicit.
    int::cli();

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

    // --- CPU / VM detection -----------------------------------------------
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
        Some(v) => {
            kprintln!(
                "machine  : {} (sig \"{}\", hv max leaf {:#x})",
                v.name(),
                v.signature_str(),
                v.max_leaf
            );
            if let Some(pv) = v.kvm_features() {
                kprintln!("kvm      : paravirt features {:#010x}", pv);
            }
        }
        None => kprintln!("machine  : bare metal"),
    }

    // --- Interrupt infrastructure -------------------------------------------
    int::gdt_init();
    kprintln!("gdt/tss  : loaded (code {:#x}, data {:#x}, tss {:#x})", int::KERNEL_CS, int::KERNEL_DS, int::TSS_SEL);
    int::idt_init();
    kprintln!("idt      : 256 gates, exceptions 0..20 + irqs 0x20..0x2f");
    pic::init();
    kprintln!("pic      : 8259 remapped to 0x20..0x2f, all lines masked");
    pit::init();
    kprintln!("pit      : channel 0 at {} Hz", pit::HZ);

    // --- Device IRQs (registered now, unmasked at the end) ------------------
    serial::init();
    int::register_irq(pic::IRQ_COM1, |_f| serial::on_irq());
    pic::unmask(pic::IRQ_COM1);
    kprintln!(
        "serial   : COM1 115200 8n1, rx on irq{}{}",
        pic::IRQ_COM1,
        if serial::present() { "" } else { " (no UART detected)" }
    );

    int::register_irq(pic::IRQ_TIMER, |_f| pit::on_tick());
    pic::unmask(pic::IRQ_TIMER);
    int::register_irq(pic::IRQ_KBD, |_f| kbd::on_irq());

    // --- Memory management ----------------------------------------------------
    let Some(bi) = bi else {
        kprintln!("fatal: cannot set up memory management without boot info");
        int::park()
    };
    let ps = pmm::init(bi);
    kprintln!(
        "pmm      : {} MiB in {} pages ({} free), bitmap ok",
        (ps.top - ps.base) >> 20,
        ps.total_pages,
        ps.free_pages.load(core::sync::atomic::Ordering::Relaxed)
    );

    if vmm::init(bi) {
        kprintln!(
            "vmm      : own pml4 active, direct map at {:#x}, kernel W^X",
            vmm::DIRECT_MAP_BASE
        );
    } else {
        kprintln!("vmm      : table build failed -- running on loader tables");
    }
    if heap::init() {
        kprintln!(
            "heap     : {} KiB at {:#x} (16-byte blocks, free-list)",
            vmm::HEAP_SIZE >> 10,
            vmm::HEAP_BASE
        );
    } else {
        kprintln!("heap     : init failed (no memory for the region)");
    }

    // --- Keyboard ------------------------------------------------------------
    let ks = kbd::init();
    if ks.present {
        kprintln!(
            "kbd      : i8042 {}{}, scancode set {}, layout {}",
            if ks.self_test { "self-test ok" } else { "self-test FAILED" },
            if ks.port_test { ", port ok" } else { ", port test FAILED" },
            if ks.set2 { 2 } else { 1 },
            kbd::layout_name()
        );
        if ks.set2 {
            kprintln!("           translation unavailable: decoding raw set 2");
        }
        if !ks.acked {
            kprintln!("           keyboard did not ACK -- it may stay silent");
        }
        pic::unmask(pic::IRQ_KBD);
    } else {
        kprintln!("kbd      : no PS/2 controller detected (USB-only machine?)");
        kprintln!("           console input stays on the serial line");
    }

    // --- Lift off --------------------------------------------------------------
    int::sti();
    kprintln!("interrupts enabled -- dropping to the shell");
    kprintln!();

    sh::set_boot_info(Some(bi));
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
    int::cli();
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
