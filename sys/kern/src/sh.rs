// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev

use crate::boot::{
    BootInfo, MEM_ACPI_NVS, MEM_ACPI_RECL, MEM_BAD, MEM_KERNEL, MEM_LOADER, MEM_RESERVED,
    MEM_USABLE,
};
use crate::io::{inb, outb};
use core::arch::asm;
use core::sync::atomic::{AtomicPtr, Ordering};

static BOOT: AtomicPtr<BootInfo> = AtomicPtr::new(core::ptr::null_mut());

/// Keep the boot info around for later commands.
pub fn set_boot_info(bi: Option<&'static BootInfo>) {
    let p = bi.map_or(core::ptr::null_mut(), |b| b as *const BootInfo as *mut BootInfo);
    BOOT.store(p, Ordering::Relaxed);
}

fn boot_info() -> Option<&'static BootInfo> {
    let p = BOOT.load(Ordering::Relaxed);
    if p.is_null() {
        None
    } else {
        // SAFETY: stored once at boot from the loader-provided pointer.
        Some(unsafe { &*p })
    }
}

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
    Cmd { name: "lsmem", usage: "", desc: "show the memory map from the loader", run: cmd_lsmem },
    Cmd { name: "cpu", usage: "", desc: "show CPU / VM information", run: cmd_cpu },
    Cmd { name: "regs", usage: "", desc: "show CRs, flags and stack pointer", run: cmd_regs },
    Cmd { name: "peek", usage: "<addr> [w]", desc: "read memory (w = 8|16|32|64 bits)", run: cmd_peek },
    Cmd { name: "poke", usage: "<addr> <val> [w]", desc: "write memory (w = 8|16|32|64 bits)", run: cmd_poke },
    Cmd { name: "dump", usage: "<addr> [len]", desc: "hexdump memory (default 64 bytes)", run: cmd_dump },
    Cmd { name: "in", usage: "<port>", desc: "read a byte from an I/O port", run: cmd_in },
    Cmd { name: "out", usage: "<port> <val>", desc: "write a byte to an I/O port", run: cmd_out },
    Cmd { name: "crash", usage: "<div|int|page>", desc: "raise an exception (IDT smoke test)", run: cmd_crash },
    Cmd { name: "clear", usage: "", desc: "clear the screen", run: cmd_clear },
    Cmd { name: "reboot", usage: "", desc: "restart the machine", run: cmd_reboot },
    Cmd { name: "halt", usage: "", desc: "stop the machine", run: cmd_halt },
];

pub fn run() -> ! {
    crate::kprintln!(
        "PrismOS {} kernel console -- type 'help' for commands.",
        env!("CARGO_PKG_VERSION")
    );
    loop {
        crate::kprint!("prism# ");
        let mut line = [0u8; 256];
        let n = crate::line::read_line(&mut line);
        let Ok(text) = core::str::from_utf8(&line[..n]) else { continue };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let (name, tail) = split_head(text);
        let mut args = Args::new(tail);
        dispatch(name, &mut args);
    }
}

/// Split off the first whitespace-delimited word (the command name).
fn split_head(line: &str) -> (&str, &str) {
    match line.find(char::is_whitespace) {
        Some(i) => (&line[..i], &line[i..]),
        None => (line, ""),
    }
}

fn dispatch(name: &str, args: &mut Args) {
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

// ---------------------------------------------------------------- parsing

/// Whitespace-separated argument scanner over the tail of a command line.
struct Args<'a> {
    rest: &'a str,
}

impl<'a> Args<'a> {
    fn new(s: &'a str) -> Self {
        Args { rest: s }
    }

    /// Next whitespace-delimited token, if any.
    fn next(&mut self) -> Option<&'a str> {
        self.rest = self.rest.trim_start();
        let end = self.rest.find(char::is_whitespace).unwrap_or(self.rest.len());
        if end == 0 {
            return None;
        }
        let (tok, tail) = self.rest.split_at(end);
        self.rest = tail;
        Some(tok)
    }

    /// Untouched remainder of the line (trimmed); for `echo` and friends.
    fn rest(&self) -> &'a str {
        self.rest.trim()
    }
}

/// Parse a number: decimal by default, hex with a `0x`/`0X` prefix.
fn parse_u64(s: &str) -> Option<u64> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<u64>().ok()
    }
}

/// Width suffix shared by peek/poke: bits, or a size letter.
fn parse_width(s: &str) -> Option<u32> {
    match s {
        "8" | "b" => Some(8),
        "16" | "w" => Some(16),
        "32" | "d" => Some(32),
        "64" | "q" => Some(64),
        _ => None,
    }
}

/// Optional trailing width argument for peek/poke; defaults to 64 bits.
fn width_arg(args: &mut Args, cmd: &str) -> Option<u32> {
    match args.next() {
        None => Some(64),
        Some(tok) => match parse_width(tok) {
            Some(w) => Some(w),
            None => {
                crate::kprintln!("{}: bad width '{}' (use 8, 16, 32 or 64)", cmd, tok);
                None
            }
        },
    }
}

fn bad_number(cmd: &str, tok: &str) {
    crate::kprintln!("{}: '{}' is not a number (decimal, or 0x-prefixed hex)", cmd, tok);
}

/// Print the usage line of a command, straight from the table.
fn usage(name: &str) {
    if let Some(c) = COMMANDS.iter().find(|c| c.name == name) {
        if c.usage.is_empty() {
            crate::kprintln!("usage: {}", name);
        } else {
            crate::kprintln!("usage: {} {}", name, c.usage);
        }
    }
}

// --------------------------------------------------------------- commands

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
    crate::kprintln!("PrismOS kernel {} (Rust, x86_64)", env!("CARGO_PKG_VERSION"));
}

fn cmd_echo(args: &mut Args) {
    crate::kprintln!("{}", args.rest());
}

fn cmd_lsmem(_args: &mut Args) {
    let Some(bi) = boot_info() else {
        crate::kprintln!("lsmem: no boot info");
        return;
    };
    let mmap = bi.memory_map();
    if mmap.is_empty() {
        crate::kprintln!("lsmem: no memory map");
        return;
    }
    crate::kprintln!("        base           end        size      type");
    let mut entries = 0usize;
    let mut usable = 0u64;
    let mut regions = 0usize;
    for e in mmap {
        entries += 1;
        let end = e.base.wrapping_add(e.length).wrapping_sub(1); // last byte
        crate::kprintln!(
            "{:#012x}  {:#012x}  {:>7} KiB  {}",
            e.base,
            end,
            e.length >> 10,
            kind_name(e.kind)
        );
        if e.kind == MEM_USABLE || e.kind == MEM_LOADER {
            usable += e.length;
            regions += 1;
        }
    }
    crate::kprintln!("{} entries, usable: {} in {} region(s)", entries, Size(usable), regions);
}

fn cmd_cpu(_args: &mut Args) {
    let cpu = crate::cpu::Cpu::detect();
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
    match crate::vm::detect() {
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
    let cr0: u64;
    let cr2: u64;
    let cr3: u64;
    let cr4: u64;
    let flags: u64;
    let rsp: u64;
    // SAFETY: plain ring-0 register reads; pushfq/pop uses the live stack.
    unsafe {
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, preserves_flags));
        asm!("mov {}, cr2", out(reg) cr2, options(nomem, preserves_flags));
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, preserves_flags));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, preserves_flags));
        asm!("pushfq\npop {}", out(reg) flags, options(preserves_flags));
        asm!("mov {}, rsp", out(reg) rsp, options(nomem, preserves_flags));
    }
    crate::kprintln!("cr0     {:#018x}", cr0);
    crate::kprintln!("cr2     {:#018x}", cr2);
    crate::kprintln!("cr3     {:#018x}", cr3);
    crate::kprintln!("cr4     {:#018x}", cr4);
    crate::kprintln!("rflags  {:#018x}", flags);
    crate::kprintln!("rsp     {:#018x}", rsp);
}

// ----------------------------------------------------------- raw memory

/// Volatile read of `w` bits at `addr`.
///
/// The address must be mapped and aligned to the access width; a bad
/// address faults (and, without a page fault handler, resets the machine).
fn read_mem(addr: u64, w: u32) -> u64 {
    // SAFETY: peek/poke validated alignment and width; whether the address
    // is mapped is the user's call -- this is a poking shell.
    match w {
        8 => unsafe { (addr as *const u8).read_volatile() as u64 },
        16 => unsafe { (addr as *const u16).read_volatile() as u64 },
        32 => unsafe { (addr as *const u32).read_volatile() as u64 },
        _ => unsafe { (addr as *const u64).read_volatile() },
    }
}

/// Volatile write of `val` to `addr` with width `w`; same contract as read_mem.
fn write_mem(addr: u64, val: u64, w: u32) {
    // SAFETY: same contract as read_mem.
    match w {
        8 => unsafe { (addr as *mut u8).write_volatile(val as u8) },
        16 => unsafe { (addr as *mut u16).write_volatile(val as u16) },
        32 => unsafe { (addr as *mut u32).write_volatile(val as u32) },
        _ => unsafe { (addr as *mut u64).write_volatile(val) },
    }
}

fn cmd_peek(args: &mut Args) {
    let Some(tok) = args.next() else { usage("peek"); return };
    let Some(addr) = parse_u64(tok) else { bad_number("peek", tok); return };
    let Some(w) = width_arg(args, "peek") else { return };
    let bytes = u64::from(w / 8);
    if addr % bytes != 0 {
        crate::kprintln!("peek: {:#x} is not {}-byte aligned", addr, bytes);
        return;
    }
    let val = read_mem(addr, w);
    match w {
        8 => crate::kprintln!("{:#018x}: {:#04x}", addr, val),
        16 => crate::kprintln!("{:#018x}: {:#06x}", addr, val),
        32 => crate::kprintln!("{:#018x}: {:#010x}", addr, val),
        _ => crate::kprintln!("{:#018x}: {:#018x}", addr, val),
    }
}

fn cmd_poke(args: &mut Args) {
    let Some(atok) = args.next() else { usage("poke"); return };
    let Some(addr) = parse_u64(atok) else { bad_number("poke", atok); return };
    let Some(vtok) = args.next() else { usage("poke"); return };
    let Some(val) = parse_u64(vtok) else { bad_number("poke", vtok); return };
    let Some(w) = width_arg(args, "poke") else { return };
    if w < 64 && val >> w != 0 {
        crate::kprintln!("poke: {:#x} does not fit in {} bits", val, w);
        return;
    }
    let bytes = u64::from(w / 8);
    if addr % bytes != 0 {
        crate::kprintln!("poke: {:#x} is not {}-byte aligned", addr, bytes);
        return;
    }
    let old = read_mem(addr, w);
    write_mem(addr, val, w);
    crate::kprintln!("{:#018x}: {:#x} -> {:#x}", addr, old, val);
}

fn cmd_dump(args: &mut Args) {
    const MAX_LEN: u64 = 4096;
    let Some(atok) = args.next() else { usage("dump"); return };
    let Some(addr) = parse_u64(atok) else { bad_number("dump", atok); return };
    let len = match args.next() {
        None => 64,
        Some(tok) => match parse_u64(tok) {
            Some(n) if n >= 1 && n <= MAX_LEN => n,
            _ => {
                crate::kprintln!("dump: bad length '{}' (1..={})", tok, MAX_LEN);
                return;
            }
        },
    };
    if len > u64::MAX - addr {
        crate::kprintln!("dump: range wraps the address space");
        return;
    }
    let base = addr as *const u8;
    let mut off = 0u64;
    while off < len {
        let n = (len - off).min(16) as usize;
        // Read the row first so every byte is touched exactly once; this
        // keeps MMIO reads sane and the two columns consistent.
        let mut row = [0u8; 16];
        // SAFETY: raw memory access asked for by the user; may fault.
        for i in 0..n {
            unsafe { row[i] = base.add(off as usize + i).read_volatile() };
        }
        crate::kprint!("{:#010x}  ", addr + off);
        for i in 0..16 {
            if i < n {
                crate::kprint!("{:02x} ", row[i]);
            } else {
                crate::kprint!("   ");
            }
            if i == 7 {
                crate::kprint!(" ");
            }
        }
        crate::kprint!("|");
        for i in 0..n {
            let b = row[i];
            let c = if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' };
            crate::kprint!("{}", c);
        }
        for _ in n..16 {
            crate::kprint!(" ");
        }
        crate::kprintln!("|");
        off += n as u64;
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
    crate::kprintln!("crash: triggering a {} exception...", mode);
    match mode {
        "div" => {
            // SAFETY: deliberate divide-by-zero (#DE).
            unsafe { asm!("xor rax, rax\ndiv rax", out("rax") _, out("rdx") _) }
        }
        "int" => {
            // SAFETY: deliberate breakpoint trap (#BP) through the current IDT.
            unsafe { asm!("int3") }
        }
        "page" => {
            // SAFETY: deliberate read of a page nothing should map (#PF).
            let _ = unsafe { (0x0000_5000_0000_0000u64 as *const u64).read_volatile() };
        }
        _ => {
            crate::kprintln!("crash: unknown mode '{}'", mode);
            return;
        }
    }
    crate::kprintln!("crash: survived -- the handler returned cleanly");
}

fn cmd_clear(_args: &mut Args) {
    crate::console::clear();
}

fn cmd_reboot(_args: &mut Args) {
    reboot_now();
}

fn cmd_halt(_args: &mut Args) {
    crate::halt();
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

fn kind_name(kind: u32) -> &'static str {
    match kind {
        MEM_USABLE => "usable",
        MEM_RESERVED => "reserved",
        MEM_ACPI_RECL => "ACPI reclaimable",
        MEM_ACPI_NVS => "ACPI NVS",
        MEM_BAD => "bad memory",
        MEM_LOADER => "loader",
        MEM_KERNEL => "kernel",
        _ => "unknown",
    }
}

/// Byte count rendered as B / KiB / MiB / GiB with at most one decimal.
struct Size(u64);

impl core::fmt::Display for Size {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let n = self.0;
        let (div, unit) = if n >= 1 << 30 {
            (1u64 << 30, "GiB")
        } else if n >= 1 << 20 {
            (1u64 << 20, "MiB")
        } else if n >= 1 << 10 {
            (1u64 << 10, "KiB")
        } else {
            return write!(f, "{} B", n);
        };
        let whole = n / div;
        let tenth = ((n % div) * 10 + div / 2) / div; // rounded
        if tenth >= 10 {
            write!(f, "{} {}", whole + 1, unit)
        } else if tenth == 0 {
            write!(f, "{} {}", whole, unit)
        } else {
            write!(f, "{}.{} {}", whole, tenth, unit)
        }
    }
}