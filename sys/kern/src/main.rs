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
//   -> PMM -> VMM (own tables, CR3 switch) -> heap -> ring 3 MSRs
//   (syscall/sysret, per-CPU GS, SMEP/SMAP/UMIP) -> process table
//   -> ACPI -> APIC/IOAPIC (8259 off, LAPIC timer takes over from the PIT)
//   -> keyboard (i8042) -> sti -> PCI/AHCI -> VFS (ext4 on the first
//   volume found) -> shell
//
// Every step logs one line so the boot output doubles as a bring-up check.

#![no_std]
#![no_main]
// `lib/` is the kernel utility module, not a library crate target.
#![allow(special_module_name)]

extern crate alloc;

mod arch;
mod boot;
mod console;
mod fs;
mod hw;
mod lib;
mod mem;
mod shell;
mod user;

use arch::{cpu, gdt, idt, intr, vm};
use boot::BootInfo;
use fs::fsys;
use hw::{acpi, ahci, apic, blk, kbd, pci, pic, pit, serial};
use mem::{heap, pmm, vmm};
use user::process;

use core::arch::asm;
use core::panic::PanicInfo;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(boot_info: *const BootInfo) -> ! {
    // The loader enters with interrupts masked; make it explicit.
    intr::cli();

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
    gdt::init();
    kprintln!("gdt/tss  : loaded (code {:#x}, data {:#x}, tss {:#x})", gdt::KERNEL_CS, gdt::KERNEL_DS, gdt::TSS_SEL);
    idt::init();
    kprintln!("idt      : 256 gates, exceptions 0..20 + irqs 0x20..0x2f");
    pic::init();
    kprintln!("pic      : 8259 remapped to 0x20..0x2f, all lines masked");
    pit::init();
    kprintln!("pit      : channel 0 at {} Hz", pit::HZ);

    // --- Device IRQs (registered now, unmasked at the end) ------------------
    serial::init();
    intr::register_irq(pic::IRQ_COM1, |_f| serial::on_irq());
    // Nothing is unmasked yet: whether the 8259 or the IOAPIC delivers these
    // lines is decided after the ACPI/APIC probe (unmasking on the 8259 and
    // then switching leaves a stale INTR request behind).
    kprintln!(
        "serial   : COM1 115200 8n1, rx on irq{}{}",
        pic::IRQ_COM1,
        if serial::present() { "" } else { " (no UART detected)" }
    );

    intr::register_irq(pic::IRQ_TIMER, |f| pit::on_tick(f.cs & 3 == 3));
    intr::register_irq(pic::IRQ_KBD, |_f| kbd::on_irq());

    // --- Memory management ----------------------------------------------------
    let Some(bi) = bi else {
        kprintln!("fatal: cannot set up memory management without boot info");
        intr::park()
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
    if console::enable_backbuffer() {
        kprintln!("display  : back buffer active, text and graphics are double-buffered");
    }
    if heap::init() {
        kprintln!(
            "heap     : {} KiB at {:#x} (buddy 4 KiB..4 MiB + slab 32..1024)",
            heap::size() >> 10,
            vmm::HEAP_BASE
        );
    } else {
        kprintln!("heap     : init failed (no memory for the region)");
    }

    // --- Ring 3 / processes (phase 2) ---------------------------------------
    // Needs the GDT (selector layout), the heap (process stacks) and the
    // VMM (per-process page tables).
    let ui = user::usermode::init();
    kprintln!(
        "user     : syscall/sysret ready, SMEP {} SMAP {} UMIP {}",
        yn(ui.smep),
        yn(ui.smap),
        yn(ui.umip)
    );
    process::init();
    kprintln!(
        "proc     : pcb + {} KiB kernel stacks per process, preemptive priority scheduler",
        process::KSTACK_SIZE >> 10
    );

    // --- ACPI / APIC ------------------------------------------------------------
    // Needs the direct map (tables above 4 GiB) so it comes after the VMM.
    if acpi::init(bi) {
        let a = acpi::info().unwrap();
        kprintln!(
            "acpi     : rev {} ({}), {} tables, {} cpu(s), {} ioapic(s), hpet {}",
            a.rsdp_rev,
            if a.xsdt { "XSDT" } else { "RSDT" },
            a.n_tables,
            a.n_cpus,
            a.n_ioapics,
            if a.hpet { "yes" } else { "no" }
        );
    } else {
        kprintln!("acpi     : no usable tables (staying on the 8259)");
    }
    if apic::init() {
        kprintln!(
            "apic     : lapic id {} at {:#x} ({}), 8259 masked, timer {} Hz ({} counts/ms via {})",
            apic::lapic_id(),
            apic::lapic_phys(),
            if apic::is_x2apic() { "x2APIC" } else { "xAPIC" },
            pit::HZ,
            apic::timer_per_ms(),
            apic::calibration_source()
        );
        apic::unmask(pic::IRQ_COM1);
    } else {
        kprintln!("apic     : unavailable, timer stays on the PIT");
        pic::unmask(pic::IRQ_TIMER);
        pic::unmask(pic::IRQ_COM1);
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
        irq_unmask(pic::IRQ_KBD);
    } else {
        kprintln!("kbd      : no PS/2 controller detected (USB-only machine?)");
        kprintln!("           console input stays on the serial line");
    }

    // --- Lift off --------------------------------------------------------------
    intr::sti();
    kprintln!("interrupts enabled");

    // --- PCI / storage (needs the system tick for command time-outs) ----------
    kprintln!("pci      : {} function(s) on the bus", pci::init());
    let ndisks = ahci::init();
    match ahci::controller() {
        Some(c) => kprintln!(
            "ahci     : controller {:02x}:{:02x}.{} AHCI {}.{}, {} port(s), {} slot(s), {} disk(s)",
            c.pci.0,
            c.pci.1,
            c.pci.2,
            c.version >> 16,
            (c.version >> 8) & 0xFF,
            c.ports,
            c.slots,
            ndisks
        ),
        None => kprintln!("ahci     : no controller"),
    }
    for i in 0..ndisks {
        if let Some(d) = ahci::disk(i) {
            use blk::BlockDev;
            kprintln!(
                "           disk {} (port {}): {} MiB, \"{}\"",
                i,
                d.port(),
                d.sectors() >> 11,
                d.model()
            );
        }
    }

    // --- VFS / ext4 (phase 1.4) ---------------------------------------------
    fsys::init();

    kprintln!("dropping to the shell");
    kprintln!();

    shell::set_boot_info(Some(bi));
    shell::run()
}

/// Unmask an ISA IRQ on whichever controller is in charge.
fn irq_unmask(irq: u8) {
    if apic::enabled() {
        apic::unmask(irq);
    } else {
        pic::unmask(irq);
    }
}

pub(crate) fn halt() -> ! {
    loop {
        // SAFETY: nothing left to run, sleep forever.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// Yes/no helper for boot-time log lines.
fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    intr::cli();
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
