// SPDX-License-Identifier: GPL-3.0-or-later
//
// Kernel console: always the serial port, plus VGA text or the framebuffer
// depending on what the loader tells us is available.

use core::fmt;
use core::ptr::addr_of_mut;

use crate::boot::{BootInfo, FLAG_FB, FLAG_VGATEXT};
use crate::console::fb::FbConsole;
use crate::hw::serial;
use crate::console::vga::Vga;

struct Console {
    vga: Option<Vga>,
    fb: Option<FbConsole>,
}

static mut CONSOLE: Console = Console { vga: None, fb: None };

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for ch in s.chars() {
            if ch == '\n' {
                serial::write_byte(b'\r');
            }
            // The serial line carries UTF-8 as is.
            let mut buf = [0u8; 4];
            for &b in ch.encode_utf8(&mut buf).as_bytes() {
                serial::write_byte(b);
            }
            if let Some(v) = self.vga.as_mut() {
                v.put_char(ch);
            }
            if let Some(f) = self.fb.as_mut() {
                f.put_char(ch);
            }
        }
        Ok(())
    }
}

/// Bring up the console. Only the serial port works before this is called.
pub fn init(bi: Option<&BootInfo>) {
    serial::init();
    // SAFETY: single CPU, called once at boot before anything else prints.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    if let Some(bi) = bi {
        if bi.flags & FLAG_VGATEXT != 0 {
            let mut v = Vga::new();
            v.clear();
            c.vga = Some(v);
        }
        if bi.flags & FLAG_FB != 0 {
            c.fb = FbConsole::new(bi.fb);
        }
    } else {
        // No boot info: assume legacy VGA text, the safest guess on a PC.
        let mut v = Vga::new();
        v.clear();
        c.vga = Some(v);
    }
}

pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    // Keep IRQ-context prints from interleaving with task-context ones.
    let _guard = crate::arch::io::IrqGuard::new();
    // SAFETY: single CPU; the guard serializes output paths.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    let _ = c.write_fmt(args);
    // One copy to the screen per print call, not one per character.
    if let Some(f) = c.fb.as_mut() {
        f.flush();
    }
}

/// Cells `c` occupies on screen (0, 1 or 2).  VGA text mode always uses 1.
pub fn char_width(c: char) -> usize {
    // SAFETY: read-only access to the console configuration.
    let con = unsafe { &*addr_of_mut!(CONSOLE) };
    if con.fb.is_some() {
        crate::console::unifont::width(c)
    } else if (c as u32) < 0x20 || c == '\u{7F}' || crate::console::unifont::width(c) == 0 {
        0
    } else {
        1
    }
}

/// Clear every active console.
pub fn clear() {
    // SAFETY: single CPU, called from the boot console context.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    if let Some(v) = c.vga.as_mut() {
        v.clear();
    }
    if let Some(f) = c.fb.as_mut() {
        f.clear();
        f.flush();
    }
}

/// Put the text cursor back at the top-left corner without erasing the
/// screen (redraw a fixed-size text screen in place, without flicker).
pub fn home() {
    let _guard = crate::arch::io::IrqGuard::new();
    // SAFETY: single CPU, guarded against IRQ-context prints.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    if let Some(v) = c.vga.as_mut() {
        v.home();
    }
    if let Some(f) = c.fb.as_mut() {
        f.home();
    }
}

/// Switch the framebuffer console to a RAM back buffer.  Needs the PMM and
/// the live page tables, so call it right after `vmm::init`.  Returns true
/// when the back buffer is active (false: no framebuffer, or no memory --
/// drawing then keeps going straight to the screen).
pub fn enable_backbuffer() -> bool {
    let _guard = crate::arch::io::IrqGuard::new();
    // SAFETY: single CPU, guarded against IRQ-context prints.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    match c.fb.as_mut() {
        Some(f) => f.enable_backbuffer(),
        None => false,
    }
}

/// Run `f` on the framebuffer surface and present what it drew.  Returns
/// `None` when there is no framebuffer (BIOS / VGA text boot).
///
/// Interrupts stay masked while `f` runs (one frame: well under one 10 ms
/// timer tick), and `f` must not print: the console is borrowed.
pub fn with_surface<R>(f: impl FnOnce(&mut crate::console::gfx::Surface) -> R) -> Option<R> {
    let _guard = crate::arch::io::IrqGuard::new();
    // SAFETY: single CPU with IRQs masked, so no print can interleave.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    let fbc = c.fb.as_mut()?;
    let r = f(fbc.surface());
    fbc.flush();
    Some(r)
}
