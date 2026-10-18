// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// SMP bring-up, phase 1.5: start the application processors the way a
// bootloader does -- INIT-SIPI-SIPI through the LAPIC, with a real-mode
// trampoline page at 0xA000 (above boot0/boot1, below the EBDA) that walks
// the AP from real mode through 32-bit protected mode into the kernel's
// long mode, then lands in `ap_main` on its own stack.
//
//  * The trampoline is assembled into the `.trampoline` section with every
//    absolute address already encoded for physical 0xA000 (the section is
//    copied there verbatim at boot; SIPI vector 0xA0 -> CS:IP 0xA00:0000).
//  * Per-CPU state: the `CPUS` array, kernel cpu 0 = the BSP.  Each AP
//    reloads the kernel GDT/IDT (the trampoline GDT has a different
//    selector map), sets its GS base, runs its own LAPIC timer on vector
//    AP_TIMER (it only ticks that CPU's counters -- the scheduler stays on
//    the BSP in phase 1.5) and idles in `hlt`.
//  * IPIs: vector AP_IPI pings an AP; the `smp` shell command and the boot
//    self-test use it as a round-trip proof.
//  * Shared state: console output serializes through the console spin lock
//    (console.rs); the log ring is lock-free atomics.
//
// What the APs deliberately do NOT do in 1.5: run processes, touch the
// disks, or take the shell -- the scheduler, VFS and AHCI stay BSP-only
// until the kernel grows real per-CPU scheduling (phase 2+).

use core::arch::global_asm;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::hw::acpi;

/// Physical (== virtual, identity mapped) address of the trampoline page.
const TRAMPOLINE: u64 = 0xA000;
/// SIPI start vector: the AP enters at linear vector * 4096 (CS selector
/// = vector << 8, real-mode base = selector << 4 -- same convention Linux
/// uses: trampoline address >> 12).  0x0A * 0x1000 = 0xA000.
const SIPI_VECTOR: u8 = 0x0A;

pub const AP_TIMER: u8 = 0x32;
pub const AP_IPI: u8 = 0x33;

/// Kernel stack per AP (like the process kernel stacks).
const AP_STACK: usize = 16 * 1024;
pub const MAX_APS: usize = acpi::MAX_CPUS - 1;

/// Per-CPU block (slot 0 = the BSP, which mostly ignores it).
pub struct Cpu {
    pub lapic: u32,
    pub present: bool,
    online: AtomicBool,
    ticks: AtomicU64,
    ipis: AtomicU64,
    idles: AtomicU64,
}

const CPU_OFF: Cpu = Cpu {
    lapic: 0,
    present: false,
    online: AtomicBool::new(false),
    ticks: AtomicU64::new(0),
    ipis: AtomicU64::new(0),
    idles: AtomicU64::new(0),
};

static mut CPUS: [Cpu; acpi::MAX_CPUS] = [CPU_OFF; acpi::MAX_CPUS];
static N_ONLINE: AtomicU32 = AtomicU32::new(0);

/// Shared view of the per-CPU array.
fn cpus() -> &'static [Cpu] {
    // SAFETY: the array is a static; we only read through this view.
    unsafe { &*addr_of_mut!(CPUS) }
}

/// Mutable view (bring-up only: the BSP fills the slots before the APs run,
/// each AP touches only its own slot afterwards).
///
/// SAFETY: callers must not alias two mutable refs to the same slot.
unsafe fn cpu_at_mut(i: usize) -> &'static mut Cpu {
    unsafe { &mut *addr_of_mut!(CPUS).cast::<Cpu>().add(i) }
}

fn cpu_at(i: usize) -> &'static Cpu {
    &cpus()[i]
}

/// AP kernel stacks, 16-byte aligned in .bss.
#[repr(align(16))]
struct ApStack([u8; AP_STACK]);
const AP_STACK_ZERO: ApStack = ApStack([0; AP_STACK]);
static mut AP_STACKS: [ApStack; MAX_APS] = [AP_STACK_ZERO; MAX_APS];

// ------------------------------------------------------------- trampoline

// The whole page is written for physical address 0xA000: every absolute
// reference below is a literal.  Section layout (offsets within the page):
//   0x000  16-bit entry (real mode, CS:IP = 0xA00:0000)
//   0x040  32-bit stage: segments, PAE, CR3, EFER.LME, CR0.PG
//   0x0A0  64-bit stage: stack + cpu/lapic in rdi/rsi, jmp to the kernel
//   0x140  GDT: null, code32 (0x08), data (0x10), code64 (0x18)
//   0x1F8  GDTR: limit 31, base 0x0000A140 (m16:32 for the 16-bit lgdt)
//   0xF00  patch block, filled in per AP by the BSP (page offset 0xF00 =
//          ABSOLUTE 0xAF00 -- note the page base is 0xA000, four digits):
//          +0x00 cr3, +0x08 entry rip, +0x10 stack top,
//          +0x18 cpu index, +0x20 lapic id
global_asm!(
    ".section .trampoline,\"a\",@progbits",
    ".globl __trampoline_start",
    "__trampoline_start:",
    ".code16",
    "cli",
    "xor ax, ax",
    "mov ds, ax",
    "mov es, ax",
    "mov ss, ax",
    "lgdt [0xa1f8]",
    "mov eax, cr0",
    "or eax, 1",
    "mov cr0, eax",
    // far jmp ptr16:16 -> 0x0008:0xa040 (encoded by hand: the assembler
    // does not accept immediate far jumps in Intel syntax)
    ".byte 0xea",
    ".word 0xa040",
    ".word 0x0008",
    ".org 0x40",
    ".code32",
    "mov ax, 0x10",
    "mov ds, ax",
    "mov es, ax",
    "mov ss, ax",
    "mov eax, cr4",
    "or eax, 0x20",              // PAE
    "mov cr4, eax",
    "mov eax, [0xaf00]",         // CR3: the BSP's page tables
    "mov cr3, eax",
    "mov ecx, 0xc0000080",       // EFER
    "rdmsr",
    "or eax, 0x900",             // LME | NXE: the kernel maps .data/.bss
    "wrmsr",                     // no-execute, and NX is reserved unless
                                 // EFER.NXE is on (a write to such a page
                                 // would #PF with error 0xa)
    "mov eax, cr0",
    "or eax, 0x80000000",        // PG
    "mov cr0, eax",
    // far jmp ptr16:32 -> 0x0018:0xa0a0 (hand-encoded, see above)
    ".byte 0xea",
    ".long 0xa0a0",
    ".word 0x0018",
    ".org 0xa0",
    ".code64",
    "mov rsp, [0xaf10]",         // this AP's kernel stack
    "mov rdi, [0xaf18]",         // kernel cpu index
    "mov rsi, [0xaf20]",         // LAPIC id
    "mov rax, [0xaf08]",         // 64-bit kernel entry
    "jmp rax",
    ".org 0x140",
    ".quad 0",                    // null
    ".quad 0x00cf9a000000ffff",   // 0x08 code32
    ".quad 0x00cf92000000ffff",   // 0x10 data
    ".quad 0x00af9a000000ffff",   // 0x18 code64 (L=1)
    ".org 0x1f8",
    ".word 31",                   // GDT limit
    ".quad 0xa140",               // GDT base (16-bit lgdt reads m16:32)
    ".org 0x210",
    "__trampoline_end:",
    ".section .text",
    ".code64",
);

extern "C" {
    #[allow(dead_code)]
    pub static __trampoline_start: u8;
    #[allow(dead_code)]
    pub static __trampoline_end: u8;
}

/// Copy the trampoline section to 0xA000 (identity mapped) and fill in the
/// per-AP patch block.
///
/// SAFETY: 0xA000 must be free and identity mapped; call on the BSP only.
unsafe fn setup_trampoline(cr3: u64, entry: u64, stack: u64, cpu: u64, lapic: u64) {
    let src = &__trampoline_start as *const u8;
    let dst = TRAMPOLINE as *mut u8;
    let len = (&__trampoline_end as *const u8 as usize) - (src as usize);
    // SAFETY: both ranges are ours (a .rodata symbol and the trampoline page).
    unsafe {
        for i in 0..len {
            dst.add(i).write_volatile(src.add(i).read_volatile());
        }
        let wr = |off: u64, v: u64| {
            ((TRAMPOLINE + off) as *mut u64).write_volatile(v);
        };
        wr(0xF00, cr3);
        wr(0xF08, entry);
        wr(0xF10, stack);
        wr(0xF18, cpu);
        wr(0xF20, lapic);
    }
}

// ------------------------------------------------------------- AP side

/// 64-bit kernel entry of every AP: the trampoline jumps here with
/// rdi = cpu index, rsi = lapic id, rsp = the per-AP stack.  The kernel GDT
/// and IDT are reloaded FIRST: the trampoline GDT puts code32 at 0x08 and
/// code64 at 0x18, the kernel has code64 at 0x08 and user data at 0x18 --
/// the next interrupt would iretq into a data descriptor if we kept it.
unsafe extern "C" fn ap_entry(cpu: u64, lapic: u64) -> ! {
    use core::arch::asm;
    // SAFETY: AP boot context, IF=0, running once on its own stack.
    unsafe {
        let gdtr = crate::arch::gdt::gdtr_va();
        asm!("lgdt [{}]", in(reg) gdtr, options(nostack, preserves_flags));
        let idtr = crate::arch::idt::idtr_va();
        asm!("lidt [{}]", in(reg) idtr, options(nostack, preserves_flags));
        asm!(
            "mov ax, {ds}\nmov ds, ax\nmov es, ax\nmov ss, ax\nxor eax, eax\nmov fs, ax\nmov gs, ax",
            ds = const crate::arch::gdt::KERNEL_DS,
            out("ax") _,
            options(nostack, preserves_flags),
        );
        // CS := kernel code64 through a far return.
        asm!(
            "push {cs}\nlea rax, [rip+2f]\npush rax\nretfq\n2:",
            cs = const crate::arch::gdt::KERNEL_CS,
            out("rax") _,
            options(preserves_flags),
        );
    }
    ap_main(cpu, lapic)
}

/// C-level AP entry: per-CPU LAPIC + timer, GS base, then idle forever.
///
/// SAFETY: called exactly once per AP by the trampoline.
unsafe fn ap_main(cpu: u64, _lapic: u64) -> ! {
    let cpu = cpu as usize;
    // The kernel does no FP on the idle path, but make SSE legal anyway.
    // SAFETY: privileged register writes on this CPU.
    unsafe {
        let cr0 = crate::arch::regs::read_cr0();
        crate::arch::regs::write_cr0(cr0 & !0xC); // clear EM and TS
        let cr4 = crate::arch::regs::read_cr4();
        crate::arch::regs::write_cr4(cr4 | (1 << 9) | (1 << 10)); // OSFXSR, OSXMMEXCPT
    }

    // This CPU's GS base: its own usermode per-CPU slot.
    crate::user::usermode::set_gs_for_cpu(cpu);

    // Per-CPU LAPIC: spurious vector, error LVT and a periodic timer on
    // AP_TIMER at the BSP's calibrated rate.
    // SAFETY: touches only this CPU's LAPIC.
    unsafe { crate::hw::apic::init_lapic_secondary(AP_TIMER) };

    let c = cpu_at(cpu);
    c.online.store(true, Ordering::Release);
    N_ONLINE.fetch_add(1, Ordering::AcqRel);

    // Idle: this CPU's interrupts are only its own timer and IPIs.
    // SAFETY: deliberate sti;hlt idle loop.
    unsafe {
        core::arch::asm!("sti", "hlt", options(nomem, nostack));
        loop {
            c.idles.fetch_add(1, Ordering::Relaxed);
            core::arch::asm!("hlt", options(nomem, nostack));
        }
    }
}

/// AP_TIMER (0x32) handler, running on the AP it belongs to.
pub fn on_ap_timer() {
    let c = cpu_at(current_cpu());
    c.ticks.fetch_add(1, Ordering::Relaxed);
}

/// AP_IPI (0x33) handler: the BSP (or another AP) pinged us.
pub fn on_ipi() {
    let c = cpu_at(current_cpu());
    c.ipis.fetch_add(1, Ordering::Relaxed);
}

/// Kernel cpu index of the running CPU: slot 0 is the BSP, every AP is
/// found through its LAPIC id (the trampoline passed the index, but
/// interrupt context has to re-derive it).
pub fn current_cpu() -> usize {
    let id = crate::hw::apic::lapic_id();
    for (i, c) in cpus().iter().enumerate() {
        if c.present && c.lapic == id {
            return i;
        }
    }
    0
}

// ------------------------------------------------------------- BSP side

/// Bring up every enabled AP from the MADT.  Returns the number of CPUs
/// online afterwards (>= 1: the BSP counts).
pub fn init() -> usize {
    let Some(a) = acpi::info() else { return 1 };
    if !crate::hw::apic::enabled() {
        return 1;
    }
    let bsp_lapic = crate::hw::apic::lapic_id();
    let cr3 = crate::arch::regs::read_cr3();

    {
        // SAFETY: boot context, no AP runs yet.
        let bsp = unsafe { cpu_at_mut(0) };
        bsp.present = true;
        bsp.lapic = bsp_lapic;
        bsp.online.store(true, Ordering::Release);
    }
    N_ONLINE.store(1, Ordering::Relaxed);

    let mut next = 1usize; // next kernel cpu index to hand out
    for e in &a.cpus[..a.n_cpus] {
        if !e.enabled || e.apic_id == bsp_lapic || next > MAX_APS {
            continue;
        }
        let stack_top = unsafe {
            let stacks = &mut *addr_of_mut!(AP_STACKS);
            stacks[next - 1].0.as_ptr() as u64 + AP_STACK as u64
        };
        // SAFETY: bring-up context; the slot is not shared yet.
        let slot = unsafe { cpu_at_mut(next) };
        slot.present = true;
        slot.lapic = e.apic_id;
        // SAFETY: the trampoline page (below 1 MiB, never handed out by the
        // PMM), the running page tables, the kernel entry and a dedicated
        // .bss stack are all valid and exclusive to this AP.
        unsafe {
            setup_trampoline(
                cr3,
                ap_entry as usize as u64,
                stack_top,
                next as u64,
                e.apic_id as u64,
            )
        };

        // INIT-SIPI-SIPI (Intel SDM Vol 3A, chapters 8.4 and 10.4).
        // SAFETY: privileged LAPIC IPI writes from the BSP.
        unsafe {
            crate::hw::apic::send_ipi(e.apic_id, 0x500); // INIT, edge, assert
            crate::hw::pit::spin_ms(10);
            crate::hw::apic::send_ipi(e.apic_id, 0x600 | SIPI_VECTOR as u64); // SIPI
            crate::hw::pit::spin_ms(1);
            crate::hw::apic::send_ipi(e.apic_id, 0x600 | SIPI_VECTOR as u64); // SIPI again
        }
        // Wait for the AP to come online (a few hundred ms on real hardware,
        // instant under TCG).
        let mut waited = 0u64;
        while !slot.online.load(Ordering::Acquire) && waited < 3000 {
            crate::hw::pit::spin_ms(1);
            waited += 1;
        }
        if slot.online.load(Ordering::Acquire) {
            next += 1;
        } else {
            // Did not come up: free the slot for the next candidate.
            slot.present = false;
        }
    }
    N_ONLINE.load(Ordering::Acquire) as usize
}

// ------------------------------------------------------------- reporting

pub struct CpuInfo {
    pub index: usize,
    pub lapic: u32,
    pub online: bool,
    pub ticks: u64,
    pub ipis: u64,
    pub idles: u64,
}

/// Snapshot every handed-out CPU slot into `out`; returns how many were
/// filled (the BSP first, then APs in bring-up order).
pub fn snapshot(out: &mut [CpuInfo]) -> usize {
    let mut n = 0;
    for (i, c) in cpus().iter().enumerate() {
        if n == out.len() {
            break;
        }
        if !c.present {
            continue;
        }
        out[n] = CpuInfo {
            index: i,
            lapic: c.lapic,
            online: c.online.load(Ordering::Acquire),
            ticks: c.ticks.load(Ordering::Relaxed),
            ipis: c.ipis.load(Ordering::Relaxed),
            idles: c.idles.load(Ordering::Relaxed),
        };
        n += 1;
    }
    n
}

pub fn n_online() -> usize {
    N_ONLINE.load(Ordering::Acquire) as usize
}

/// Ping every online AP; returns (answered, aps).  The caller waits, then
/// compares per-CPU IPI counters.
pub fn ipi_ping_all() -> (usize, usize) {
    let mut before = [0u64; acpi::MAX_CPUS];
    let mut after = [0u64; acpi::MAX_CPUS];
    let mut idx = [0usize; acpi::MAX_CPUS];
    let mut n = 0;
    for (i, c) in cpus().iter().enumerate() {
        if c.present {
            before[n] = c.ipis.load(Ordering::Relaxed);
            idx[n] = i;
            n += 1;
        }
    }
    for k in 0..n {
        let c = cpu_at(idx[k]);
        if c.online.load(Ordering::Acquire) && idx[k] != 0 {
            // SAFETY: privileged LAPIC IPI write.
            unsafe { crate::hw::apic::send_ipi(c.lapic, AP_IPI as u64) };
        }
    }
    crate::hw::pit::spin_ms(50);
    let mut answered = 0;
    let mut aps = 0;
    for k in 0..n {
        let c = cpu_at(idx[k]);
        if idx[k] == 0 || !c.online.load(Ordering::Acquire) {
            continue;
        }
        aps += 1;
        after[k] = c.ipis.load(Ordering::Relaxed);
        if after[k] > before[k] {
            answered += 1;
        }
    }
    (answered, aps)
}
