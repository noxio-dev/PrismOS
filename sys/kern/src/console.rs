// SPDX-License-Identifier: GPL-3.0-or-later
//
// Kernel console: always the serial port, plus VGA text or the framebuffer
// depending on what the loader tells us is available.

use core::fmt;
use core::ptr::addr_of_mut;

use crate::boot::{BootInfo, FLAG_FB, FLAG_VGATEXT};
use crate::fb::FbConsole;
use crate::serial;
use crate::vga::Vga;

struct Console {
    vga: Option<Vga>,
    fb: Option<FbConsole>,
}

static mut CONSOLE: Console = Console { vga: None, fb: None };

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                serial::write_byte(b'\r');
            }
            serial::write_byte(b);
            if let Some(v) = self.vga.as_mut() {
                v.put(b);
            }
            if let Some(f) = self.fb.as_mut() {
                f.put(b);
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
    let _guard = crate::io::IrqGuard::new();
    // SAFETY: single CPU; the guard serializes output paths.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    let _ = c.write_fmt(args);
}

/// Clear every active console.
pub fn clear() {
    // SAFETY: single CPU, called from the boot console context.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    if let Some(v) = c.vga.as_mut() {
        v.clear();
    }
    if let Some(f) = c.fb.as_mut() {
        f.clear(); // add a clear() to FbConsole if it does not have one yet
    }
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::console::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => { $crate::kprint!("{}\n", format_args!($($arg)*)) };
}
