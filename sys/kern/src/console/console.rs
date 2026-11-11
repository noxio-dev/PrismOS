// SPDX-License-Identifier: GPL-3.0-or-later
//
// Kernel console: always the serial port, plus VGA text or the framebuffer
// depending on what the loader tells us is available.

use core::fmt;
use core::ptr::addr_of_mut;

use crate::boot::{BootInfo, FLAG_FB, FLAG_VGATEXT};
use crate::console::ansi::{self, Action, Out};
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

/// Serializes _print across CPUs (phase 1.5 SMP): the IrqGuard only stops
/// re-entrancy on the LOCAL cpu; another cpu printing concurrently would
/// interleave on the serial line and the framebuffer.
static PRINT_LOCK: crate::lib::spin::SpinLock<()> = crate::lib::spin::SpinLock::new(());

pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    // Keep IRQ-context prints from interleaving with task-context ones, on
    // this cpu (IrqGuard) and across cpus (the spin lock).  No deadlock is
    // possible: nothing takes PRINT_LOCK twice on the same cpu, and no
    // interrupt handler prints while the holder is in IRQ context.
    let _guard = crate::arch::io::IrqGuard::new();
    let _cross = PRINT_LOCK.lock();
    // Format once into a stack buffer so the kernel log ring (dmesg) can
    // record the exact same text; no allocation, safe before heap init.
    let mut buf = LogBuf::default();
    let _ = buf.write_fmt(args);
    // SAFETY: single CPU; the guard serializes output paths.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    let _ = c.write_str(buf.as_str());
    // One copy to the screen per print call, not one per character.
    if let Some(f) = c.fb.as_mut() {
        f.flush();
    }
    // Tee into the ring while the boot log plays (before the shell starts).
    if crate::lib::klog::boot_capturing() {
        crate::lib::klog::record(crate::lib::klog::Level::Info, buf.as_str());
    }
}

// ---------------------------------------------- escape sequences

/// The ANSI interpreter both user write paths share: they write the same
/// screens, so one state machine (its SGR attribute included) is right.
static mut PARSER: ansi::Parser = ansi::Parser::new();

/// Where save-cursor (CSI s / u, ESC 7 / 8) parks the position.
static mut SAVED: (usize, usize) = (0, 0);

/// The raw character stream to the serial line, untranslated -- escapes
/// included.  A real terminal applies its own colors; the kernel's screens
/// apply theirs through the interpreter (write_bytes below).
fn serial_tee(ch: char) {
    let mut buf = [0u8; 4];
    for &b in ch.encode_utf8(&mut buf).as_bytes() {
        if b == b'\n' {
            serial::write_byte(b'\r');
        }
        serial::write_byte(b);
    }
}

/// One character on the screens only: what both write paths print a
/// ground character through (the serial copy is the caller's business).
fn screen_char(ch: char) {
    // Keep IRQ-context prints from interleaving, like _print does; the
    // flush below needs the same single-flight discipline.
    let _guard = crate::arch::io::IrqGuard::new();
    let _cross = PRINT_LOCK.lock();
    // SAFETY: single CPU; the guard serializes output paths.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    if let Some(v) = c.vga.as_mut() {
        v.put_char(ch);
    }
    if let Some(f) = c.fb.as_mut() {
        f.put_char(ch);
        f.flush();
    }
}

/// Carry out one finished escape action on every active screen.
fn apply(a: Action) {
    let _guard = crate::arch::io::IrqGuard::new();
    let _cross = PRINT_LOCK.lock();
    // SAFETY: single CPU; the guard serializes output paths.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    match a {
        Action::Sgr(s) => {
            if let Some(v) = c.vga.as_mut() {
                v.set_attr(ansi::vga_attr(s));
            }
            if let Some(f) = c.fb.as_mut() {
                f.set_sgr(s.fg, s.bg, s.reverse);
                f.flush();
            }
        }
        Action::Cup(row, col) => {
            if let Some(v) = c.vga.as_mut() {
                v.cursor_to(row, col);
            }
            if let Some(f) = c.fb.as_mut() {
                f.cursor_to(row, col);
                f.flush();
            }
        }
        Action::Move(drow, dcol) => {
            if let Some(v) = c.vga.as_mut() {
                v.move_rel(drow, dcol);
            }
            if let Some(f) = c.fb.as_mut() {
                f.move_rel(drow, dcol);
                f.flush();
            }
        }
        Action::Col(col) => {
            if let Some(v) = c.vga.as_mut() {
                v.col_set(col);
            }
            if let Some(f) = c.fb.as_mut() {
                f.col_set(col);
                f.flush();
            }
        }
        Action::Row(row) => {
            if let Some(v) = c.vga.as_mut() {
                v.row_set(row);
            }
            if let Some(f) = c.fb.as_mut() {
                f.row_set(row);
                f.flush();
            }
        }
        Action::EraseLine(mode) => {
            if let Some(v) = c.vga.as_mut() {
                v.erase_line(mode);
            }
            if let Some(f) = c.fb.as_mut() {
                f.erase_line(mode);
                f.flush();
            }
        }
        Action::EraseDisplay(mode) => {
            if let Some(v) = c.vga.as_mut() {
                v.erase_display(mode);
            }
            if let Some(f) = c.fb.as_mut() {
                f.erase_display(mode);
                f.flush();
            }
        }
        Action::SaveCursor => {
            let pos = if let Some(v) = c.vga.as_ref() {
                v.cursor_pos()
            } else if let Some(f) = c.fb.as_ref() {
                f.cursor_pos()
            } else {
                (0, 0)
            };
            // SAFETY: single CPU; the lock serializes the writers.
            unsafe { *addr_of_mut!(SAVED) = pos };
        }
        Action::RestoreCursor => {
            // SAFETY: single CPU; plain read.
            let (row, col) = unsafe { *addr_of_mut!(SAVED) };
            if let Some(v) = c.vga.as_mut() {
                v.cursor_to(row, col);
            }
            if let Some(f) = c.fb.as_mut() {
                f.cursor_to(row, col);
                f.flush();
            }
        }
        Action::HideCursor => {
            if let Some(v) = c.vga.as_mut() {
                v.hide_cursor();
            }
        }
        Action::ShowCursor => {
            if let Some(v) = c.vga.as_mut() {
                v.show_cursor();
            }
        }
        Action::Index => {
            if let Some(v) = c.vga.as_mut() {
                v.index();
            }
            if let Some(f) = c.fb.as_mut() {
                f.index();
                f.flush();
            }
        }
        Action::ReverseIndex => {
            if let Some(v) = c.vga.as_mut() {
                v.reverse_index();
            }
            if let Some(f) = c.fb.as_mut() {
                f.reverse_index();
                f.flush();
            }
        }
        Action::Nel => {
            if let Some(v) = c.vga.as_mut() {
                v.col_set(0);
                v.index();
            }
            if let Some(f) = c.fb.as_mut() {
                f.col_set(0);
                f.index();
                f.flush();
            }
        }
        Action::ScrollUp(n) => {
            if let Some(v) = c.vga.as_mut() {
                for _ in 0..n {
                    v.index();
                }
            }
            if let Some(f) = c.fb.as_mut() {
                for _ in 0..n {
                    f.index();
                }
                f.flush();
            }
        }
        Action::Reset => {
            if let Some(v) = c.vga.as_mut() {
                v.set_attr(ansi::vga_attr(ansi::SGR_RESET));
                v.erase_display(2);
                v.cursor_to(0, 0);
            }
            if let Some(f) = c.fb.as_mut() {
                f.set_sgr(7, 0, false);
                f.erase_display(2);
                f.cursor_to(0, 0);
                f.flush();
            }
        }
    }
}

/// Run one character through the interpreter, then carry out or print
/// what it says.  The screen half of both user write paths.
fn feed_screen(ch: char) {
    // SAFETY: single CPU; output paths are serialized by the callers.
    let out = unsafe { (&mut *addr_of_mut!(PARSER)).feed(ch) };
    match out {
        Out::Print(c) => screen_char(c),
        Out::Swallow => {}
        Out::Act(a) => apply(a),
    }
}

/// Print raw bytes on the console (phase 2.6): every write to the console
/// device -- fd 1 / fd 2 of a process, `/dev/console`, -- funnels here.
/// The serial line sees the stream raw (escapes included, so a real
/// terminal keeps its colors); the screens run the ANSI interpreter --
/// ground characters print, sequences become cursor and attribute
/// operations, and stray control characters print as '?', so a user
/// program cannot garble the screens with half-formed sequences.
/// Incomplete multi-byte sequences are decoded across the chunk
/// boundaries the callers copy with.
pub fn write_bytes(mut b: &[u8]) {
    while !b.is_empty() {
        let (valid, skip, bad) = match core::str::from_utf8(b) {
            Ok(s) => (s, b.len(), false),
            Err(e) => {
                let v = e.valid_up_to();
                // SAFETY: from_utf8 vouches for the prefix.
                let s = unsafe { core::str::from_utf8_unchecked(&b[..v]) };
                (s, v + e.error_len().unwrap_or(b.len() - v), true)
            }
        };
        for ch in valid.chars() {
            serial_tee(ch);
            feed_screen(ch);
        }
        if bad {
            screen_char('?');
        }
        b = &b[skip.min(b.len())..];
    }
}

/// The console session's own output (phase 3.3.5): the text screen and
/// only it -- the interpreted write_bytes path minus its serial tee, which
/// is what keeps a console session's prompt off ttyS0 (the serial session
/// is busy answering the same keystrokes there itself).  Same rules as
/// write_bytes otherwise: escapes become operations, stray controls '?'.
pub fn write_local(mut b: &[u8]) {
    while !b.is_empty() {
        let (valid, skip, bad) = match core::str::from_utf8(b) {
            Ok(s) => (s, b.len(), false),
            Err(e) => {
                let v = e.valid_up_to();
                // SAFETY: from_utf8 vouches for the prefix.
                let s = unsafe { core::str::from_utf8_unchecked(&b[..v]) };
                (s, v + e.error_len().unwrap_or(b.len() - v), true)
            }
        };
        for ch in valid.chars() {
            feed_screen(ch);
        }
        if bad {
            screen_char('?');
        }
        b = &b[skip.min(b.len())..];
    }
}

impl Default for LogBuf {
    fn default() -> Self {
        LogBuf { buf: [0; 1024], len: 0 }
    }
}

/// Fixed-capacity formatting target for the console tee (1 KiB, no alloc).
struct LogBuf {
    buf: [u8; 1024],
    len: usize,
}

impl LogBuf {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl fmt::Write for LogBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let take = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
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

/// The text grid of the active console, (columns, rows) -- the framebuffer's
/// own geometry (phase 3.3: TIOCGWINSZ answers it), 80x25 in VGA text mode.
pub fn text_dims() -> (usize, usize) {
    // SAFETY: read-only access to the console configuration.
    let con = unsafe { &*addr_of_mut!(CONSOLE) };
    if let Some(f) = con.fb.as_ref() {
        f.text_dims()
    } else {
        (80, 25)
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
