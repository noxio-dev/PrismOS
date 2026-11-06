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

// ---------------------------------------------- escape sequences / column

/// 0 = normal, 1 = just after ESC, 2 = inside a CSI sequence.
static mut ESC_STATE: u8 = 0;
/// First numeric parameter of the CSI sequence under way.
static mut ESC_PARAM: u32 = 0;
/// Approximate cursor column, tracked from what ring 3 writes (the
/// Vga / FbConsole types expose no cursor query, so we follow it here).
static mut COL: usize = 0;

/// Update the tracked column for one character that reaches the screen.
fn track_col(ch: char) {
    // SAFETY: single CPU; output paths are serialized by the callers.
    let col = unsafe { &mut *addr_of_mut!(COL) };
    let (cols, _) = text_dims();
    match ch {
        '\n' | '\r' => *col = 0,
        '\x08' => *col = col.saturating_sub(1),
        '\t' => *col = ((*col + 8) & !7).min(cols.saturating_sub(1)),
        c => {
            *col += char_width(c).max(1);
            if *col >= cols {
                *col = 0;
            }
        }
    }
}

fn current_col() -> usize {
    // SAFETY: single CPU; plain read.
    unsafe { *addr_of_mut!(COL) }
}

/// Erase from the cursor to the end of the line, cursor stays put: spaces
/// over the rest of the line, then the same number of backspaces.  The very
/// last cell of the line is left alone (writing it would wrap the cursor).
fn erase_eol(emit: &mut dyn FnMut(char)) {
    let (cols, _) = text_dims();
    let n = cols.saturating_sub(current_col() + 1);
    for _ in 0..n {
        emit(' ');
    }
    for _ in 0..n {
        emit('\x08');
    }
}

/// Interprets the few CSI sequences we can honour: `ESC[nD` (cursor left),
/// `ESC[K` / `ESC[J` (erase to end of line).  Everything else is swallowed.
/// `emit` prints one character the way the caller does.  Returns true when
/// `ch` was consumed by the sequence.
fn eat_escape(ch: char, emit: &mut dyn FnMut(char)) -> bool {
    // SAFETY: single CPU; output paths are serialized by the callers.
    let (st, param) = unsafe { (&mut *addr_of_mut!(ESC_STATE), &mut *addr_of_mut!(ESC_PARAM)) };
    match *st {
        0 => {
            if ch == '\x1b' {
                *st = 1;
                true
            } else {
                false
            }
        }
        1 => {
            if ch == '[' {
                *st = 2;
                *param = 0;
                true
            } else {
                *st = 0; // lone ESC: drop it, the next char prints normally
                false
            }
        }
        _ => {
            if let Some(d) = ch.to_digit(10) {
                *param = param.saturating_mul(10).saturating_add(d).min(500);
            } else if ('\u{40}'..='\u{7e}').contains(&ch) {
                *st = 0;
                let n = (*param).max(1);
                match ch {
                    'D' => {
                        for _ in 0..n.min(current_col() as u32) {
                            emit('\x08');
                        }
                    }
                    'K' | 'J' => erase_eol(emit),
                    _ => {}
                }
            }
            true
        }
    }
}

/// Print one char on screen *and* serial (the /dev/console path).
fn emit_tee(c: char) {
    track_col(c);
    crate::kprint!("{}", c);
}

/// Fixed-capacity formatting target for the console tee (1 KiB, no alloc).
struct LogBuf {
    buf: [u8; 1024],
    len: usize,
}

/// Print raw bytes on the console (phase 2.6): every write to the console
/// device -- fd 1 / fd 2 of a process, `/dev/console`, -- funnels here.
/// Valid UTF-8 goes through as is; backspace, newline and tab are kept,
/// `\r` returns to the start of the line, a few CSI sequences are honoured
/// (see eat_escape); other control characters and invalid sequences print
/// as '?', so a user program cannot inject terminal escapes into the
/// console.  Incomplete multi-byte sequences are decoded across the chunk
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
            if eat_escape(ch, &mut emit_tee) {
                continue;
            }
            match ch {
                '\r' => {
                    for _ in 0..current_col() {
                        emit_tee('\x08');
                    }
                }
                c if c.is_control() && c != '\n' && c != '\t' && c != '\x08' => emit_tee('?'),
                c => emit_tee(c),
            }
        }
        if bad {
            emit_tee('?');
        }
        b = &b[skip.min(b.len())..];
    }
}

/// The console session's own output (phase 3.3.5): the text screen and
/// only it -- no serial copy.  Kernel messages and /dev/console writes
/// keep the serial tee (write_bytes above); what the console terminal
/// (/dev/tty0) and its line discipline echo would land on ttyS0 twice,
/// because the serial session echoes the same keystrokes there itself.
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
            if eat_escape(ch, &mut local_char) {
                continue;
            }
            if ch == '\r' {
                for _ in 0..current_col() {
                    local_char('\x08');
                }
                continue;
            }
            local_char(ch);
        }
        if bad {
            local_char('?');
        }
        b = &b[skip.min(b.len())..];
    }
}

/// One character on the text screens only (the serial tee skipped).
fn local_char(mut ch: char) {
    // Keep IRQ-context prints from interleaving, like _print does; the
    // ring buffer flush below needs the same single-flight discipline.
    let _guard = crate::arch::io::IrqGuard::new();
    let _cross = PRINT_LOCK.lock();
    // SAFETY: single CPU; the guard serializes output paths.
    let c = unsafe { &mut *addr_of_mut!(CONSOLE) };
    if ch.is_control() && ch != '\n' && ch != '\t' && ch != '\x08' {
        ch = '?';
    }
    track_col(ch);
    if let Some(v) = c.vga.as_mut() {
        v.put_char(ch);
    }
    if let Some(f) = c.fb.as_mut() {
        f.put_char(ch);
    }
    if let Some(f) = c.fb.as_mut() {
        f.flush();
    }
}

impl Default for LogBuf {
    fn default() -> Self {
        LogBuf { buf: [0; 1024], len: 0 }
    }
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
    // SAFETY: single CPU; the cursor is back at the origin.
    unsafe { *addr_of_mut!(COL) = 0 };
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
    // SAFETY: single CPU; the cursor is back at the origin.
    unsafe { *addr_of_mut!(COL) = 0 };
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