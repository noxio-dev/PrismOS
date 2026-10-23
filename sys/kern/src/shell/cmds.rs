// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Shell command table and dispatch, plus the generic and hardware commands
// (help, cpu, regs, in/out, crash, kbd, acpi, lspci, ahci, reboot, ...).
// Memory, file system and user-space commands live in shmem.rs, shfs.rs and
// shuser.rs.

use super::{bad_number, hexdump, parse_u64, shfs, shmem, shuser, yn, Args, Size};
use crate::arch::io::{inb, outb};
use core::arch::asm;

// ------------------------------------------------------------------ shell

/// One entry of the command table.
struct Cmd {
    /// Name typed by the user.
    name: &'static str,
    /// Argument synopsis, shown by `help` and on usage errors.
    usage: &'static str,
    /// One-line description.
    desc: &'static str,
    run: fn(&mut Args),
}

const COMMANDS: &[Cmd] = &[
    Cmd { name: "help", usage: "", desc: "show this help", run: cmd_help },
    Cmd { name: "version", usage: "", desc: "kernel version", run: cmd_version },
    Cmd { name: "echo", usage: "<text>", desc: "print text", run: cmd_echo },
    Cmd { name: "lsmem", usage: "", desc: "show the memory map from the loader", run: shmem::cmd_lsmem },
    Cmd { name: "cpu", usage: "", desc: "show CPU / VM information", run: cmd_cpu },
    Cmd { name: "regs", usage: "", desc: "show CRs, flags and stack pointer", run: cmd_regs },
    Cmd { name: "peek", usage: "<addr> [w]", desc: "read memory (w = 8|16|32|64 bits)", run: shmem::cmd_peek },
    Cmd { name: "poke", usage: "<addr> <val> [w]", desc: "write memory (w = 8|16|32|64 bits)", run: shmem::cmd_poke },
    Cmd { name: "dump", usage: "<addr> [len]", desc: "hexdump memory (default 64 bytes)", run: shmem::cmd_dump },
    Cmd { name: "in", usage: "<port>", desc: "read a byte from an I/O port", run: cmd_in },
    Cmd { name: "out", usage: "<port> <val>", desc: "write a byte to an I/O port", run: cmd_out },
    Cmd { name: "crash", usage: "<div|int|page>", desc: "raise an exception (recovered by the handlers)", run: cmd_crash },
    Cmd { name: "layout", usage: "[us|fr]", desc: "show or switch the keyboard layout", run: cmd_layout },
    Cmd { name: "kbd", usage: "", desc: "keyboard / i8042 controller status", run: cmd_kbd },
    Cmd { name: "uptime", usage: "", desc: "time since boot (PIT ticks)", run: cmd_uptime },
    Cmd { name: "date", usage: "", desc: "RTC date and time", run: cmd_date },
    Cmd { name: "meminfo", usage: "", desc: "PMM / heap statistics", run: shmem::cmd_meminfo },
    Cmd { name: "acpi", usage: "", desc: "ACPI tables, CPUs, IOAPICs, HPET, APIC timer", run: cmd_acpi },
    Cmd { name: "lspci", usage: "[-v]", desc: "list PCI devices (-v: BARs)", run: cmd_lspci },
    Cmd { name: "ahci", usage: "[read <disk> <lba> [n]]", desc: "AHCI controller / disks; read sectors", run: cmd_ahci },
    Cmd { name: "heaptest", usage: "[n]", desc: "stress the heap with n objects (default 100000)", run: shmem::cmd_heaptest },
    Cmd { name: "mount", usage: "[-r] [-l] [<disk>[:<part>] <path>]", desc: "list / mount ext2/3/4 volumes (-l: volumes found)", run: shfs::cmd_mount },
    Cmd { name: "umount", usage: "<path>", desc: "flush and unmount a file system", run: shfs::cmd_umount },
    Cmd { name: "df", usage: "", desc: "file system usage", run: shfs::cmd_df },
    Cmd { name: "sync", usage: "", desc: "write pending changes to disk", run: shfs::cmd_sync },
    Cmd { name: "pwd", usage: "", desc: "print the current directory", run: shfs::cmd_pwd },
    Cmd { name: "cd", usage: "[dir]", desc: "change the current directory", run: shfs::cmd_cd },
    Cmd { name: "ls", usage: "[-la] [path...]", desc: "list a directory", run: shfs::cmd_ls },
    Cmd { name: "stat", usage: "<path>", desc: "inode details", run: shfs::cmd_stat },
    Cmd { name: "cat", usage: "<path...>", desc: "print files", run: shfs::cmd_cat },
    Cmd { name: "hd", usage: "<path> [off] [len]", desc: "hexdump a file (len <= 4096)", run: shfs::cmd_hd },
    Cmd { name: "write", usage: "<path> <text>", desc: "create/replace a file with text + newline (\\n \\t escapes)", run: shfs::cmd_write },
    Cmd { name: "append", usage: "<path> <text>", desc: "append text + newline to a file", run: shfs::cmd_append },
    Cmd { name: "mkdir", usage: "[-p] <dir...>", desc: "create directories", run: shfs::cmd_mkdir },
    Cmd { name: "rmdir", usage: "<dir...>", desc: "remove empty directories", run: shfs::cmd_rmdir },
    Cmd { name: "rm", usage: "[-r] <path...>", desc: "remove files (-r: whole trees)", run: shfs::cmd_rm },
    Cmd { name: "mv", usage: "<src> <dst>", desc: "move / rename", run: shfs::cmd_mv },
    Cmd { name: "cp", usage: "<src> <dst>", desc: "copy a file", run: shfs::cmd_cp },
    Cmd { name: "ln", usage: "[-s] <target> <name>", desc: "hard or symbolic link", run: shfs::cmd_ln },
    Cmd { name: "touch", usage: "<path...>", desc: "create empty files", run: shfs::cmd_touch },
    Cmd { name: "truncate", usage: "<path> <size>", desc: "resize a file", run: shfs::cmd_truncate },
    Cmd { name: "chmod", usage: "<octal> <path>", desc: "change permission bits", run: shfs::cmd_chmod },
    Cmd { name: "clear", usage: "", desc: "clear the screen", run: cmd_clear },
    Cmd { name: "reboot", usage: "", desc: "restart the machine", run: cmd_reboot },
    Cmd { name: "halt", usage: "", desc: "stop the machine", run: cmd_halt },
    Cmd { name: "unicode", usage: "", desc: "print a Unicode test page", run: cmd_unicode },
    Cmd { name: "snake", usage: "", desc: "play Snake (pixels on a framebuffer, text on VGA)", run: cmd_snake },
    Cmd { name: "cube", usage: "", desc: "spinning 3D cube (needs a framebuffer)", run: cmd_cube },
    Cmd { name: "user", usage: "[<program>...]", desc: "run ring 3 test programs as processes (none: list)", run: shuser::cmd_user },
    Cmd { name: "usertest", usage: "", desc: "run every ring 3 test and check the outcomes", run: shuser::cmd_usertest },
    Cmd { name: "exec", usage: "[-p prio] <program> [args...] [-- ...]", desc: "run ELF programs from the file system (bare name: /bin)", run: shuser::cmd_exec },
    Cmd { name: "ps", usage: "", desc: "list the process table", run: shuser::cmd_ps },
    Cmd { name: "kill", usage: "<pid>", desc: "terminate a process", run: shuser::cmd_kill },
    Cmd { name: "dmesg", usage: "[-l level]", desc: "kernel log ring (debug/info/warn/error)", run: cmd_dmesg },
    Cmd { name: "ktest", usage: "", desc: "run the in-kernel unit tests (phase 1.5)", run: cmd_ktest },
    Cmd { name: "sym", usage: "<addr>", desc: "resolve a kernel address through kallsyms", run: cmd_sym },
    Cmd { name: "fallocate", usage: "[-k] <path> <off> <len>", desc: "preallocate blocks (unwritten extents); -k keeps the size", run: shfs::cmd_fallocate },
    Cmd { name: "smp", usage: "", desc: "per-CPU state: online, LAPIC timer ticks, IPI round-trip (phase 1.5)", run: cmd_smp },
];

pub(super) fn dispatch(name: &str, args: &mut Args) {
    // Exact match first, then a unique prefix ("l" -> lsmem).
    let mut prefix: Option<&Cmd> = None;
    let mut hits = 0;
    for c in COMMANDS {
        if c.name == name {
            (c.run)(args);
            return;
        }
        if c.name.starts_with(name) {
            prefix = Some(c);
            hits += 1;
        }
    }
    if hits == 1 {
        if let Some(c) = prefix {
            (c.run)(args);
        }
        return;
    }
    if hits > 1 {
        crate::kprintln!("{}: ambiguous command, matches:", name);
        for c in COMMANDS {
            if c.name.starts_with(name) {
                crate::kprintln!("  {}", c.name);
            }
        }
        return;
    }
    crate::kprintln!("{}: command not found", name);
    if let Some(s) = suggest(name) {
        crate::kprintln!("did you mean '{}'?", s);
    }
}

// --------------------------------------------------------------- commands

/// Print the usage line of a command, straight from the table.
pub(crate) fn usage(name: &str) {
    if let Some(c) = COMMANDS.iter().find(|c| c.name == name) {
        if c.usage.is_empty() {
            crate::kprintln!("usage: {}", name);
        } else {
            crate::kprintln!("usage: {} {}", name, c.usage);
        }
    }
}

fn cmd_help(_args: &mut Args) {
    crate::kprintln!("commands:");
    // Width of the "name usage" column so descriptions line up.
    let mut width = 0;
    for c in COMMANDS {
        let l = c.name.len() + if c.usage.is_empty() { 0 } else { 1 + c.usage.len() };
        width = width.max(l);
    }
    let target = width + 4; // 2 spaces indent + label + 2 spaces gutter
    for c in COMMANDS {
        crate::kprint!("  {}", c.name);
        let mut col = 2 + c.name.len();
        if !c.usage.is_empty() {
            crate::kprint!(" {}", c.usage);
            col += 1 + c.usage.len();
        }
        while col < target {
            crate::kprint!(" ");
            col += 1;
        }
        crate::kprintln!("{}", c.desc);
    }
    crate::kprintln!("unambiguous prefixes work ('l' -> lsmem); numbers: decimal or 0x-hex");
    crate::kprintln!("peek/poke/dump/in/out/crash hit raw hardware; bad values can reset the box");
}

fn cmd_version(_args: &mut Args) {
    crate::kprintln!("PrismOS kernel {} (build {}) (Rust, x86_64)",
                 env!("CARGO_PKG_VERSION"), env!("BUILD_DATE"));
}

fn cmd_echo(args: &mut Args) {
    crate::kprintln!("{}", args.rest());
}

fn cmd_cpu(_args: &mut Args) {
    let cpu = crate::arch::cpu::Cpu::detect();
    let brand = cpu.brand();
    crate::kprintln!("cpu      : {}", if brand.is_empty() { "unknown" } else { brand });
    crate::kprintln!(
        "vendor   : {}, family {} model {} stepping {}",
        cpu.vendor(),
        cpu.family(),
        cpu.model(),
        cpu.stepping()
    );
    let mut feats = [0u8; 256];
    crate::kprintln!("features : {}", cpu.features_into(&mut feats));
    match crate::arch::vm::detect() {
        Some(v) => crate::kprintln!(
            "machine  : {} (sig \"{}\", hv max leaf {:#x})",
            v.name(),
            v.signature_str(),
            v.max_leaf
        ),
        None => crate::kprintln!("machine  : bare metal"),
    }
}

fn cmd_regs(_args: &mut Args) {
    use crate::arch::regs;
    let (cr0, cr2, cr3, cr4) = (regs::read_cr0(), regs::read_cr2(), regs::read_cr3(), regs::read_cr4());
    let (flags, rsp) = (regs::read_rflags(), regs::read_rsp());
    crate::kprintln!("cr0     {:#018x}", cr0);
    crate::kprintln!("cr2     {:#018x}", cr2);
    crate::kprintln!("cr3     {:#018x}", cr3);
    crate::kprintln!("cr4     {:#018x}", cr4);
    crate::kprintln!("rflags  {:#018x}", flags);
    crate::kprintln!("rsp     {:#018x}", rsp);
    if crate::hw::apic::enabled() {
        crate::kprintln!("lapic id {} (version {:#x}, {})", crate::hw::apic::lapic_id(),
            crate::hw::apic::lapic_version(), if crate::hw::apic::is_x2apic() { "x2APIC" } else { "xAPIC" });
    } else {
        crate::kprintln!("lapic    : not in use (8259 mode)");
    }
}

fn cmd_in(args: &mut Args) {
    let Some(tok) = args.next() else { usage("in"); return };
    let Some(port) = parse_u64(tok) else { bad_number("in", tok); return };
    if port > 0xffff {
        crate::kprintln!("in: port {:#x} out of range (0..=0xffff)", port);
        return;
    }
    // SAFETY: raw port read, asked for explicitly by the user.
    let v = unsafe { inb(port as u16) };
    crate::kprintln!("in  {:#x} -> {:#04x} ({})", port, v, v);
}

fn cmd_out(args: &mut Args) {
    let Some(ptok) = args.next() else { usage("out"); return };
    let Some(port) = parse_u64(ptok) else { bad_number("out", ptok); return };
    let Some(vtok) = args.next() else { usage("out"); return };
    let Some(val) = parse_u64(vtok) else { bad_number("out", vtok); return };
    if port > 0xffff {
        crate::kprintln!("out: port {:#x} out of range (0..=0xffff)", port);
        return;
    }
    if val > 0xff {
        crate::kprintln!("out: {:#x} truncated to 8 bits", val);
    }
    // SAFETY: raw port write, asked for explicitly by the user.
    unsafe { outb(port as u16, val as u8) };
    crate::kprintln!("out {:#x} <- {:#04x}", port, val as u8);
}

fn cmd_crash(args: &mut Args) {
    let Some(mode) = args.next() else { usage("crash"); return };
    match mode {
        "div" => {
            crate::kprintln!("crash: divide by zero (recovered by the #DE handler)");
            // SAFETY: deliberate divide-by-zero; the handler zeroes rax:rdx
            // and skips the 3-byte `div rax` (48 f7 f0).
            unsafe { asm!("xor rax, rax\ndiv rax", out("rax") _, out("rdx") _) }
        }
        "int" => {
            crate::kprintln!("crash: int3 (recovered by the #BP handler)");
            // SAFETY: deliberate breakpoint trap; the handler advances RIP.
            unsafe { asm!("int3") }
        }
        "page" => {
            crate::kprintln!("crash: page fault (recovered by the #PF handler)");
            // SAFETY: deliberate read of the magic test page; the handler
            // maps it and the instruction re-executes.
            let v = unsafe { (crate::mem::vmm::CRASH_TEST_ADDR as *const u64).read_volatile() };
            crate::kprintln!("crash: survived, the page reads {:#x}", v);
        }
        _ => {
            crate::kprintln!("crash: unknown mode '{}' (use div, int or page)", mode);
            return;
        }
    }
    crate::kprintln!("crash: survived -- the handler returned cleanly");
}

fn cmd_layout(args: &mut Args) {
    match args.next() {
        None => {
            crate::kprintln!("layout: {} (layouts: us, fr)", crate::hw::kbd::layout_name());
            crate::kprintln!("  fr: AltGr+8 = backslash, AltGr+9 = ^, AltGr+0 = @");
            crate::kprintln!("  us: the key next to ] prints the backslash");
        }
        Some(l) => {
            if crate::hw::kbd::set_layout(l) {
                crate::kprintln!("layout: switched to {}", l);
            } else {
                crate::kprintln!("layout: unknown layout '{}' (use us or fr)", l);
            }
        }
    }
}

fn cmd_kbd(_args: &mut Args) {
    let (shift, ctrl, alt, altgr, caps) = crate::hw::kbd::modifiers();
    let st = crate::arch::io::IrqGuard::new();
    let _ = st;
    crate::kprintln!("keyboard : {}", crate::hw::kbd::layout_name());
    crate::kprintln!("modifiers: shift={} ctrl={} alt={} altgr={} caps={}",
        yn(shift), yn(ctrl), yn(alt), yn(altgr), yn(caps));
    // SAFETY: raw status port read.
    let status = unsafe { crate::arch::io::inb(0x64) };
    crate::kprintln!("i8042    : status {:#04x} (obf={} ibf={} aux={} sys={})",
        status, yn(status & 1 != 0), yn(status & 2 != 0), yn(status & 0x20 != 0), yn(status & 4 != 0));
    let counts = [
        crate::arch::intr::IRQ_COUNTS[0].load(core::sync::atomic::Ordering::Relaxed),
        crate::arch::intr::IRQ_COUNTS[1].load(core::sync::atomic::Ordering::Relaxed),
        crate::arch::intr::IRQ_COUNTS[4].load(core::sync::atomic::Ordering::Relaxed),
    ];
    crate::kprintln!("irqs     : timer={} kbd={} com1={} (masked: {:#04x})",
        counts[0], counts[1], counts[2], crate::hw::pic::mask_state());
    crate::kprintln!("timer    : {} ticks, source {}", crate::hw::pit::ticks(),
        if crate::hw::apic::enabled() { "LAPIC" } else { "PIT" });
    crate::kprintln!("tip      : 'layout fr' if your keyboard is AZERTY");
}

fn cmd_uptime(_args: &mut Args) {
    let ms = crate::hw::pit::uptime_ms();
    crate::kprintln!("uptime   : {}.{:03} s ({} ticks at {} Hz)",
        ms / 1000, ms % 1000, crate::hw::pit::ticks(), crate::hw::pit::HZ);
}

fn cmd_date(_args: &mut Args) {
    match crate::hw::rtc::read() {
        Some(dt) => crate::kprintln!(
            "rtc      : {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            dt.year, dt.month, dt.day, dt.hour, dt.min, dt.sec
        ),
        None => crate::kprintln!("rtc      : not available"),
    }
}

fn cmd_acpi(_args: &mut Args) {
    let Some(a) = crate::hw::acpi::info() else {
        crate::kprintln!("acpi     : no tables");
        return;
    };
    crate::kprintln!("rsdp     : revision {}, oem \"{}\", root {}", a.rsdp_rev,
        core::str::from_utf8(&a.oem).unwrap_or("?").trim(), if a.xsdt { "XSDT" } else { "RSDT" });
    crate::kprint!("tables   :");
    for t in &a.tables[..a.n_tables] {
        crate::kprint!(" {}", core::str::from_utf8(t).unwrap_or("????"));
    }
    crate::kprintln!();
    crate::kprintln!("madt     : lapic at {:#x}, pcat_compat={}", a.lapic_addr, yn(a.pcat_compat));
    for c in &a.cpus[..a.n_cpus] {
        crate::kprintln!("  cpu    : acpi id {} apic id {} {}", c.acpi_id, c.apic_id,
            if c.enabled { "enabled" } else { "disabled" });
    }
    for io in &a.ioapics[..a.n_ioapics] {
        crate::kprintln!("  ioapic : id {} at {:#x}, gsi base {}", io.id, io.addr, io.gsi_base);
    }
    for o in &a.overrides[..a.n_overrides] {
        crate::kprintln!("  irq    : ISA {} -> GSI {} (flags {:#x})", o.irq, o.gsi, o.flags);
    }
    if a.fadt {
        crate::kprintln!("fadt     : sci irq {}, pm timer port {:#x}, flags {:#x}", a.sci_irq, a.pm_timer_port, a.fadt_flags);
    }
    if a.hpet {
        crate::kprintln!("hpet     : at {:#x}, min tick {}", a.hpet_addr, a.hpet_min_tick);
    }
    if crate::hw::apic::enabled() {
        crate::kprintln!("apic     : id {}, {} counts/ms (calibrated on {}), tick {} Hz",
            crate::hw::apic::lapic_id(), crate::hw::apic::timer_per_ms(), crate::hw::apic::calibration_source(), crate::hw::pit::HZ);
    }
}

fn cmd_lspci(args: &mut Args) {
    let verbose = args.next() == Some("-v");
    for d in crate::hw::pci::devices() {
        crate::kprintln!("{:02x}:{:02x}.{} {:04x}:{:04x}  {} [{:02x}{:02x}] ({}) irq {}",
            d.bus, d.dev, d.func, d.vendor, d.device,
            crate::hw::pci::class_name(d.class, d.subclass), d.class, d.subclass,
            crate::hw::pci::vendor_name(d.vendor), d.irq_line);
        if verbose {
            crate::kprintln!("         progif {:#04x} rev {:#04x} cmd {:#06x}", d.progif, d.revision, d.read_command());
            for i in 0..6 {
                if let Some(b) = d.bar(i) {
                    crate::kprintln!("         bar{} {} {:#x} size {:#x}{}", i, if b.io { "io " } else { "mem" },
                        b.addr, b.size, if b.is64 { " (64-bit)" } else { "" });
                }
            }
        }
    }
}

fn cmd_ahci(args: &mut Args) {
    use crate::hw::blk::BlockDev;
    match args.next() {
        None => {
            let Some(c) = crate::hw::ahci::controller() else {
                crate::kprintln!("ahci     : no controller");
                return;
            };
            crate::kprintln!("ahci     : {:02x}:{:02x}.{}, AHCI {}.{}, {} port(s), {} slot(s), 64-bit DMA {}, msi {}, {} completion irq(s)",
                c.pci.0, c.pci.1, c.pci.2, c.version >> 16, (c.version >> 8) & 0xFF, c.ports, c.slots,
                yn(c.s64a), yn(c.msi), c.irqs);
            for i in 0..crate::hw::ahci::MAX_DISKS {
                if let Some(d) = crate::hw::ahci::disk(i) {
                    crate::kprintln!("disk {}   : port {}, {} sectors ({}), \"{}\" serial {}, ncq {}",
                        i, d.port(), d.sectors(), Size(d.sectors() * 512), d.model(), d.serial(), yn(d.ncq()));
                }
            }
        }
        Some("read") => {
            let (Some(di), Some(lba)) = (args.next(), args.next()) else { return usage("ahci") };
            let (Some(di), Some(lba)) = (parse_u64(di), parse_u64(lba)) else {
                return crate::kprintln!("ahci: bad number");
            };
            let count = args.next().and_then(parse_u64).unwrap_or(1).clamp(1, 8) as usize;
            let Some(d) = crate::hw::ahci::disk(di as usize) else {
                return crate::kprintln!("ahci: no disk {}", di);
            };
            let mut buf = [0u8; 8 * 512];
            let buf = &mut buf[..count * 512];
            match d.read(lba, buf) {
                Ok(()) => hexdump(buf, lba * 512),
                Err(e) => crate::kprintln!("ahci: read of sector {} failed: {:?}", lba, e),
            }
        }
        Some(_) => usage("ahci"),
    }
}

fn cmd_clear(_args: &mut Args) {
    crate::console::clear();
}

fn cmd_reboot(_args: &mut Args) {
    crate::fs::fsys::sync_all();
    reboot_now();
}

fn cmd_halt(_args: &mut Args) {
    crate::fs::fsys::sync_all();
    crate::halt();
}

fn cmd_unicode(_args: &mut Args) {
    crate::kprintln!("ASCII    : The quick brown fox \\ | / ~ ` ^ jumps over 0123456789");
    crate::kprintln!("Latin    : Ça va ? Très bien, merci — œuvre, naïve, Straße, ÿ, Ł, ß, €, £, ¥");
    crate::kprintln!("Greek    : Α Β Γ Δ α β γ δ ω   ελληνικά");
    crate::kprintln!("Cyrillic : Привет, мир!  Ж ж Щ щ");
    crate::kprintln!("Hebrew   : שלום   Arabic: مرحبا");
    crate::kprintln!("Devanagari: नमस्ते   Thai: สวัสดี");
    crate::kprintln!("CJK      : 你好，世界  こんにちは  안녕하세요");
    crate::kprintln!("Symbols  : ← ↑ → ↓ ∑ ∞ ≠ ≤ ≥ √ π ∂ ∫ ★ ☺ ♥ ♪ ✓ ✗");
    crate::kprintln!("Boxes    : ┌─┬─┐ │ ╔═╗ ░▒▓█ ▀▄ ┘");
    crate::kprintln!("Emoji    : 😀 🚀 🐧 🦀");
    crate::kprintln!("Combining: e\u{301} a\u{300} o\u{302} n\u{303}  (e + accent marks)");
}

fn cmd_snake(_args: &mut Args) {
    crate::lib::snake::run();
}

fn cmd_cube(_args: &mut Args) {
    crate::lib::cube::run();
}

// ---------------------------------------------------------------- reboot

fn reboot_now() -> ! {
    crate::kprintln!("rebooting...");
    const CTRL: u16 = 0x64;
    // SAFETY: pulse the reset line of the 8042 keyboard controller,
    // after a bounded wait for an empty input buffer.
    unsafe {
        let mut spins = 0u32;
        while inb(CTRL) & 0x02 != 0 && spins < 100_000 {
            spins += 1;
        }
        outb(CTRL, 0xFE);
    }
    // Give the controller a moment to act...
    for _ in 0..10_000_000 {
        core::hint::spin_loop();
    }
    // ...and if it never does, triple fault: with a zero-length IDT any
    // interrupt escalates to a triple fault, which resets the CPU.  No
    // ACPI needed.
    triple_fault()
}

/// Load a null IDT descriptor and raise an interrupt.  Never returns.
#[cold]
fn triple_fault() -> ! {
    // limit = 0, base = 0: every interrupt vectors into nothing.
    let idtr: [u8; 10] = [0; 10];
    // SAFETY: that is the point -- the triple fault resets the machine.
    unsafe { asm!("cli\nlidt [{}]\nint3", in(reg) idtr.as_ptr(), options(noreturn)) }
}

// ---------------------------------------------------------------- helpers

/// Closest command name within edit distance 2, for typo hints.
fn suggest(cmd: &str) -> Option<&'static str> {
    let mut best = ("", usize::MAX);
    for c in COMMANDS {
        let d = edit_distance(cmd, c.name);
        if d < best.1 {
            best = (c.name, d);
        }
    }
    (best.1 <= 2).then_some(best.0)
}

/// Levenshtein distance over bytes; anything longer than MAX is treated as
/// infinitely far, which keeps the DP rows in fixed stack arrays.
fn edit_distance(a: &str, b: &str) -> usize {
    const MAX: usize = 24;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() > MAX || b.len() > MAX {
        return usize::MAX;
    }
    let mut prev = [0usize; MAX + 1];
    let mut cur = [0usize; MAX + 1];
    for (i, slot) in prev.iter_mut().enumerate() {
        *slot = i;
    }
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        core::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

// ----------------------------------------------------- phase 1.5 commands

/// `dmesg [-l level]`: dump the kernel log ring.
fn cmd_dmesg(args: &mut Args) {
    use crate::lib::klog::{self, Level};
    let mut level = Level::Debug;
    while let Some(tok) = args.next() {
        if tok == "-l" {
            match args.next().and_then(|l| Level::parse(l)) {
                Some(l) => level = l,
                None => return usage("dmesg"),
            }
        } else {
            return usage("dmesg");
        }
    }
    let (used, retired) = klog::usage();
    let recs = klog::read(level);
    for r in &recs {
        let whole = r.ts_ms / 1000;
        let tenth = (r.ts_ms % 1000) / 100;
        crate::kprintln!("[{:>5}.{}] {:<5} {}", whole, tenth, r.level.as_str(), r.text);
    }
    crate::kprintln!("-- {} record(s), {} bytes live, {} retired by wrapping --",
        recs.len(), used, retired);
}

/// `ktest`: run the in-kernel unit test table.
fn cmd_ktest(_args: &mut Args) {
    crate::kprintln!("running {} in-kernel tests", crate::lib::ktest::TESTS.len());
    let (pass, fail) = crate::lib::ktest::run_all();
    if fail == 0 {
        crate::kprintln!("ktest: all {} passed", pass);
    } else {
        crate::kprintln!("ktest: {} passed, {} FAILED", pass, fail);
    }
}

/// `sym <addr>`: resolve a kernel address.
fn cmd_sym(args: &mut Args) {
    let Some(tok) = args.next() else { return usage("sym") };
    let Some(addr) = super::parse_u64(tok) else { return bad_number("sym", tok) };
    match crate::lib::kallsyms::lookup(addr) {
        Some((name, 0)) => crate::kprintln!("{:#018x} {}", addr, name),
        Some((name, off)) => crate::kprintln!("{:#018x} {}+{:#x}", addr, name, off),
        None => crate::kprintln!("{:#018x} ? (no symbol covers this address)", addr),
    }
}

/// `smp`: per-CPU snapshot, then an IPI round-trip to prove the APs are
/// alive and taking interrupts (phase 1.5).
fn cmd_smp(_args: &mut Args) {
    use crate::hw::smp;
    const ZERO: smp::CpuInfo = smp::CpuInfo {
        index: 0,
        lapic: 0,
        online: false,
        ticks: 0,
        ipis: 0,
        idles: 0,
    };
    let mut info = [ZERO; crate::hw::acpi::MAX_CPUS];
    let n = smp::snapshot(&mut info);
    if n == 0 {
        crate::kprintln!("smp      : not initialized");
        return;
    }
    crate::kprintln!("cpu  lapic  state   ticks      ipis   idle-halts");
    for c in &info[..n] {
        crate::kprintln!(
            "{:>3}  {:>5}  {:<6}  {:>9}  {:>6}  {:>10}",
            c.index,
            c.lapic,
            if c.online { "online" } else { "down" },
            c.ticks,
            c.ipis,
            c.idles
        );
    }
    let (answered, aps) = smp::ipi_ping_all();
    if aps == 0 {
        crate::kprintln!("ipi      : no AP to ping (single cpu system)");
    } else {
        crate::kprintln!("ipi      : {}/{} AP(s) answered the round-trip", answered, aps);
        if answered == aps {
            crate::kprintln!("ipi      : ok");
        }
    }
}
