// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The console terminal (phase 3.3): one tty sitting between the merged
// keyboard + serial input and whichever process reads the console.  This
// is the ring 3 half of lib/line.rs -- the kernel shell's reader does its
// own editing on the raw queues, a userland session gets it done here.
//
//   * `termios` state, exchanged with ring 3 through the TCGETS / TCSETS
//     pair (the Linux x86-64 struct layout, 60 bytes) -- mlibc's
//     tcgetattr / tcsetattr / isatty ride on those;
//   * a line discipline: canonical mode (ICANON) assembles a line, echoes
//     (ECHO), erases whole characters (ECHOE -- a wide CJK glyph loses
//     both of its cells, like the kernel reader), kills the line (^U) and
//     answers ^D with an end of file.  ISIG turns ^C into SIGINT for the
//     foreground process (the shell owns that number through TIOCSPGRP,
//     see fd.rs) and ^\ into SIGQUIT; without a foreground the reader
//     parked on the tty gets it.  Raw mode (ICANON clear) hands bytes
//     through as they come -- what a program like cube puts the terminal
//     into for its keyboard;
//   * the input is drained by `pump()`: from the read paths (syscall
//     context) and from the tick (so a ^C typed while nothing reads is
//     still acted on within one tick).  Every entry into the discipline
//     runs with interrupts masked: the two callers must not interleave
//     their queue pops, and the keyboard / serial rings are SPSC.
//
// Not here (README roadmap 3.3.4): sessions and process groups.  The
// foreground number is a plain pid (TIOCSPGRP), not a pgrp -- setsid /
// setpgid / kill(-pgrp) come with the busybox job control.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::user::process;
use crate::user::signal;

// --------------------------------------------------------------- termios

// c_iflag (the Linux values)
const ICRNL: u32 = 0o400;
// c_oflag
const OPOST: u32 = 0o1;
// c_cflag
const CS8: u32 = 0o60;
const CREAD: u32 = 0o200;
// c_lflag
const ISIG: u32 = 0o1;
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;
const ECHOE: u32 = 0o20;
const ECHOK: u32 = 0o40;
const IEXTEN: u32 = 0o100000;

// c_cc indices (the Linux order)
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VTIME: usize = 5;
const VMIN: usize = 6;
const VSUSP: usize = 10;
const NCCS: usize = 32;

/// The `c_iflag` / `c_oflag` / `c_cflag` / `c_lflag` bits this side of the
/// tty looks at; everything else is stored and handed back untouched.
const KNOWN_LFLAG: u32 = ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN;

/// The state of the one terminal.  Everything below is touched only under
/// masked interrupts (see the header): the discipline is not re-entrant.
struct Tty {
    iflag: u32,
    oflag: u32,
    cflag: u32,
    lflag: u32,
    cc: [u8; NCCS],
    /// Canonical line under assembly (without its terminator).
    canon: Vec<u8>,
    /// Completed lines (canonical reads pop one per read; an empty entry
    /// is the end of file a ^D produced).
    lines: VecDeque<Vec<u8>>,
    /// Raw mode's byte queue.
    raw: VecDeque<u8>,
    /// The foreground process (TIOCSPGRP): who a ^C is for.
    fg: AtomicU64,
    /// The process parked in a console read, if any (the ^C fallback when
    /// nobody claimed the foreground).
    reader: AtomicU64,
    /// Bytes of a multi-byte character being echoed: the tty sees input one
    /// byte at a time, the console wants whole characters.
    echo_utf8: Vec<u8>,
}

impl Tty {
    const fn new() -> Tty {
        let mut cc = [0u8; NCCS];
        cc[VINTR] = 0x03; // ^C
        cc[VQUIT] = 0x1C; // ^\
        cc[VERASE] = 0x7F; // backspace
        cc[VKILL] = 0x15; // ^U
        cc[VEOF] = 0x04; // ^D
        cc[VTIME] = 0;
        cc[VMIN] = 1;
        cc[VSUSP] = 0x1A; // ^Z
        Tty {
            iflag: ICRNL,
            oflag: OPOST,
            cflag: CS8 | CREAD,
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN,
            cc,
            canon: Vec::new(),
            lines: VecDeque::new(),
            raw: VecDeque::new(),
            fg: AtomicU64::new(0),
            reader: AtomicU64::new(0),
            echo_utf8: Vec::new(),
        }
    }
}

static mut TTY: Tty = Tty::new();

/// Longest canonical line (like the kernel's line reader buffer).
const LINE_MAX: usize = 1024;

// ------------------------------------------------------------- discipline

/// Next input byte from the keyboard or the serial line (the same merge the
/// kernel shell's reader does), or None when idle.
fn get_input_byte() -> Option<u8> {
    if let Some(c) = crate::hw::kbd::pop_char() {
        return Some(c);
    }
    crate::hw::serial::pop_rx()
}

/// Drain whatever is pending through the discipline (echo, line assembly,
/// ISIG).  Called from the console read paths and, when a userland session
/// is up, from the tick -- a ^C typed while the foreground process renders
/// is acted on within one tick, not at its next read.
pub fn pump() {
    let flags = crate::arch::io::cli_save();
    // SAFETY: interrupts masked; the discipline is single-threaded by this
    // guard (the tick and a read path can never interleave a pop).
    unsafe {
        while let Some(b) = get_input_byte() {
            feed(b);
        }
    }
    // SAFETY: flags came from cli_save on this stack.
    unsafe { crate::arch::io::restore_flags(flags) };
}

/// One input byte through the discipline (interrupts are masked by the
/// caller: `pump`, the ioctl paths and the read paths' take).
///
/// # Safety
/// Only under the `pump` / masked-interrupt contract; touches the tty state.
unsafe fn feed(mut b: u8) {
    // SAFETY: as the contract.
    let tty = unsafe { &mut *addr_of_mut!(TTY) };
    if tty.lflag & ISIG != 0 {
        if b == tty.cc[VINTR] {
            tty.canon.clear();
            if tty.lflag & ECHO != 0 {
                echo("^C\n");
            }
            raise(signal::SIGINT);
            return;
        }
        if b == tty.cc[VQUIT] {
            tty.canon.clear();
            if tty.lflag & ECHO != 0 {
                echo("^\\\n");
            }
            raise(3); // SIGQUIT
            return;
        }
        if b == tty.cc[VSUSP] {
            // No stop / continue yet (roadmap 3.3.4): swallow it rather
            // than inject a control byte into the stream.
            return;
        }
    }
    if b == b'\r' && tty.iflag & ICRNL != 0 {
        b = b'\n';
    }
    if tty.lflag & ICANON != 0 {
        if b == tty.cc[VERASE] {
            if tty.canon.is_empty() {
                return;
            }
            // Erase the last character -- a whole character: a wide glyph
            // loses both of its cells (the same walk the kernel reader
            // does in lib/line.rs).
            let mut start = tty.canon.len() - 1;
            while start > 0 && tty.canon[start] & 0xC0 == 0x80 {
                start -= 1;
            }
            let w = core::str::from_utf8(&tty.canon[start..])
                .ok()
                .and_then(|s| s.chars().next())
                .map_or(1, crate::console::char_width);
            tty.canon.truncate(start);
            if tty.lflag & (ECHO | ECHOE) == (ECHO | ECHOE) {
                for _ in 0..w {
                    echo("\u{8} \u{8}");
                }
            }
            return;
        }
        if b == tty.cc[VKILL] {
            // Kill the line: erase it cell by cell (ECHOK, like ^U in the
            // kernel reader).
            while !tty.canon.is_empty() {
                let mut start = tty.canon.len() - 1;
                while start > 0 && tty.canon[start] & 0xC0 == 0x80 {
                    start -= 1;
                }
                let w = core::str::from_utf8(&tty.canon[start..])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .map_or(1, crate::console::char_width);
                tty.canon.truncate(start);
                if tty.lflag & (ECHO | ECHOE) == (ECHO | ECHOE) {
                    for _ in 0..w {
                        echo("\u{8} \u{8}");
                    }
                }
            }
            return;
        }
        if b == tty.cc[VEOF] {
            // The line is complete as it stands; an empty one is the end
            // of file the next read reports as a 0 count.
            tty.lines.push_back(core::mem::take(&mut tty.canon));
            return;
        }
        // A full line drops further characters (the terminator excepted)
        // instead of growing forever.
        if tty.canon.len() >= LINE_MAX && b != b'\n' {
            return;
        }
        tty.canon.push(b);
        if b == b'\n' {
            // The terminator is part of the line, like every unix.
            tty.lines.push_back(core::mem::take(&mut tty.canon));
        }
    } else {
        // Raw mode: bytes go through as they come (VMIN / VTIME are not
        // modelled -- a read answers what is there, parks when dry).
        tty.raw.push_back(b);
    }
    if tty.lflag & ECHO != 0 {
        echo_char(tty, b);
    }
}

/// Deliver `sig` to whoever owns the terminal right now: the foreground
/// process when one claimed it (TIOCSPGRP), the parked reader otherwise.
/// Nobody -- drop it.
///
/// # Safety
/// Under the `pump` masked-interrupt contract (may end the caller).
unsafe fn raise(sig: u64) {
    let fg = fg_pid();
    let target = if fg != 0 { fg } else { reader_pid() };
    if target == 0 {
        return;
    }
    if target == process::getpid() {
        // Ourselves -- the interrupted process (the tick pumped) or the
        // caller of the read that pumped: pending for a handler (delivered
        // at the syscall return), the default action applied at once
        // (which never returns, exactly like the watchdog's tick exit).
        if let Some(cur) = process::current() {
            if cur.signals.handler_of(sig) != 0 {
                cur.signals.pending |= 1 << sig;
                return;
            }
        }
        process::exit_current(process::Outcome::Signaled(sig));
    } else {
        // Another process: it cannot be on the CPU (single CPU, we are),
        // so the whole kill path is safe from here.
        let _ = signal::sys_kill(target, sig);
    }
}

/// The tty's own output: straight to the console, *not* through
/// `console::write_bytes`.  That one is the sanitiser for ring 3 programs
/// (every control character but `\n` / `\t` becomes '?'), which is exactly
/// what turned the erase sequence `\b \b` into `? ?` on screen.  What the
/// discipline echoes is the kernel's own text -- trusted.
fn echo(s: &str) {
    crate::kprint!("{}", s);
}

/// Echo one input byte the way a terminal does (ECHOCTL): newline and tab as
/// themselves, any other control character as `^X`, printable ASCII as is,
/// and multi-byte UTF-8 once the whole character has arrived.
fn echo_char(tty: &mut Tty, b: u8) {
    if b >= 0x80 || !tty.echo_utf8.is_empty() {
        if b < 0x80 {
            // A sequence cut short by an ASCII byte: show the damage once,
            // then carry on with the byte itself.
            tty.echo_utf8.clear();
            echo("?");
        } else {
            tty.echo_utf8.push(b);
            // 0 = a whole character, 1 = still incomplete, 2 = invalid.
            let state = match core::str::from_utf8(&tty.echo_utf8) {
                Ok(_) => 0,
                Err(e) if e.error_len().is_none() && tty.echo_utf8.len() < 4 => 1,
                Err(_) => 2,
            };
            match state {
                0 => {
                    if let Ok(s) = core::str::from_utf8(&tty.echo_utf8) {
                        crate::kprint!("{}", s);
                    }
                    tty.echo_utf8.clear();
                }
                1 => {}
                _ => {
                    tty.echo_utf8.clear();
                    echo("?");
                }
            }
            return;
        }
    }
    match b {
        b'\n' => echo("\n"),
        b'\t' => echo("\t"),
        0x00..=0x1F | 0x7F => crate::kprint!("^{}", (b ^ 0x40) as char),
        _ => crate::kprint!("{}", b as char),
    }
}

// ------------------------------------------------------------ the read side

/// Take up to `out.len()` bytes of completed input.  A canonical read
/// answers at most one line (an empty one is the end of file); a raw read
/// takes whatever is queued.  None = nothing complete yet.
pub fn take(out: &mut [u8]) -> Option<usize> {
    let flags = crate::arch::io::cli_save();
    // SAFETY: interrupts masked; the queues are ours for the duration.
    let r = unsafe {
        let tty = &mut *addr_of_mut!(TTY);
        if tty.lflag & ICANON != 0 {
            let mut line = tty.lines.pop_front()?;
            let n = out.len().min(line.len());
            out[..n].copy_from_slice(&line[..n]);
            if n < line.len() {
                line.drain(..n);
                tty.lines.push_front(line); // the remainder waits for the next read
            }
            Some(n)
        } else {
            let mut n = 0;
            while n < out.len() {
                match tty.raw.pop_front() {
                    Some(b) => out[n] = b,
                    None => break,
                }
                n += 1;
            }
            (n > 0).then_some(n)
        }
    };
    // SAFETY: flags came from cli_save on this stack.
    unsafe { crate::arch::io::restore_flags(flags) };
    r
}

/// Whether a console read has something to answer right now (poll /
/// select's probe).  Raw mode also counts undrained hardware bytes: the
/// read path pumps before it takes.
pub fn readable() -> bool {
    let flags = crate::arch::io::cli_save();
    // SAFETY: read-only look under masked interrupts.
    let r = unsafe {
        let tty = &*addr_of_mut!(TTY);
        if tty.lflag & ICANON != 0 {
            !tty.lines.is_empty()
        } else {
            !tty.raw.is_empty() || crate::hw::kbd::has_char() || crate::hw::serial::has_byte()
        }
    };
    // SAFETY: as cli_save.
    unsafe { crate::arch::io::restore_flags(flags) };
    r
}

/// Record who is parked in a console read (the ^C fallback target).  The
/// read paths call this on entry and clear it (`0`) on the way out.
pub fn set_reader(pid: u64) {
    // SAFETY: the tty is only touched under masked interrupts; this is an
    // atomic store on a static.
    unsafe { (*addr_of_mut!(TTY)).reader.store(pid, Ordering::Relaxed) };
}

/// Drop everything the discipline holds (phase 3.3's rescue hand-over:
/// what was typed during a dying session belongs to nobody).  The
/// foreground claim goes with it -- the kernel shell owns the console.
pub fn reset_input() {
    let flags = crate::arch::io::cli_save();
    // SAFETY: the discipline is ours under masked interrupts.
    unsafe {
        let t = &mut *addr_of_mut!(TTY);
        t.canon.clear();
        t.lines.clear();
        t.raw.clear();
        t.reader.store(0, Ordering::Relaxed);
        t.fg.store(0, Ordering::Relaxed);
    }
    // SAFETY: flags came from cli_save on this stack.
    unsafe { crate::arch::io::restore_flags(flags) };
}

/// The parked reader, if any.
pub fn reader_pid() -> u64 {
    // SAFETY: atomic load of a static.
    unsafe { (*addr_of_mut!(TTY)).reader.load(Ordering::Relaxed) }
}

/// The foreground process (TIOCSPGRP), 0 when unclaimed.
pub fn fg_pid() -> u64 {
    // SAFETY: atomic load of a static.
    unsafe { (*addr_of_mut!(TTY)).fg.load(Ordering::Relaxed) }
}

/// Claim the foreground (TIOCSPGRP): the shell parks its own pid at the
/// prompt and the pid of the command it runs while it waits.
pub fn set_fg(pid: u64) {
    // SAFETY: atomic store on a static.
    unsafe { (*addr_of_mut!(TTY)).fg.store(pid, Ordering::Relaxed) };
}

// -------------------------------------------------------------- the ioctls

/// `struct termios`, the x86-64 Linux layout mlibc's tcgetattr reads:
/// four u32 flags, c_line, c_cc[32], then the (padded) speed pair -- 60
/// bytes exactly, the size the pre-3.3 TCGETS used to zero out.
pub const TERMIOS_LEN: usize = 60;

/// Serialize the termios state into `out` (exactly TERMIOS_LEN bytes).
pub fn get_termios(out: &mut [u8; TERMIOS_LEN]) {
    let flags = crate::arch::io::cli_save();
    // SAFETY: read-only copy under masked interrupts.
    let (iflag, oflag, cflag, lflag, cc) = unsafe {
        let t = &*addr_of_mut!(TTY);
        (t.iflag, t.oflag, t.cflag, t.lflag, t.cc)
    };
    // SAFETY: as cli_save.
    unsafe { crate::arch::io::restore_flags(flags) };
    out[0..4].copy_from_slice(&iflag.to_le_bytes());
    out[4..8].copy_from_slice(&oflag.to_le_bytes());
    out[8..12].copy_from_slice(&cflag.to_le_bytes());
    out[12..16].copy_from_slice(&lflag.to_le_bytes());
    out[16] = 0; // c_line: the only line there is
    out[17..49].copy_from_slice(&cc);
    out[49..52].fill(0);
    out[52..56].copy_from_slice(&38400u32.to_le_bytes()); // ispeed (B38400)
    out[56..60].copy_from_slice(&38400u32.to_le_bytes()); // ospeed
}

/// Install a termios state (TCSETS / TCSETSW / TCSETSF -- the three differ
/// only in flushing rules we cannot wait for; the input queue is flushed
/// when ICANON is toggled, the honest part of TCSAFLUSH).
pub fn set_termios(bytes: &[u8; TERMIOS_LEN]) {
    let read_u32 = |off: usize| u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
    let oflag = read_u32(4) & OPOST;
    let cflag = read_u32(8) & (CS8 | CREAD);
    let lflag = read_u32(12) & KNOWN_LFLAG;
    let mut cc = [0u8; NCCS];
    cc.copy_from_slice(&bytes[17..49]);
    let flags = crate::arch::io::cli_save();
    // SAFETY: the discipline is ours under masked interrupts.
    unsafe {
        let t = &mut *addr_of_mut!(TTY);
        let was_canon = t.lflag & ICANON != 0;
        // Input translations: CR -> NL stays on whatever a program asks
        // for -- the serial terminals send CR, and the other directions
        // are not modelled here.
        t.iflag = ICRNL;
        t.oflag = oflag;
        t.cflag = cflag;
        t.lflag = lflag;
        // A control character of 0 _disable_s its function (POSIX): only
        // copy the ones that stay alive so the defaults survive a raw
        // program restoring a zeroed struct.
        if cc[VINTR] != 0 {
            t.cc[VINTR] = cc[VINTR];
        }
        if cc[VQUIT] != 0 {
            t.cc[VQUIT] = cc[VQUIT];
        }
        if cc[VERASE] != 0 {
            t.cc[VERASE] = cc[VERASE];
        }
        if cc[VKILL] != 0 {
            t.cc[VKILL] = cc[VKILL];
        }
        if cc[VEOF] != 0 {
            t.cc[VEOF] = cc[VEOF];
        }
        t.cc[VTIME] = cc[VTIME];
        t.cc[VMIN] = cc[VMIN];
        if cc[VSUSP] != 0 {
            t.cc[VSUSP] = cc[VSUSP];
        }
        if was_canon != (lflag & ICANON != 0) {
            // Mode change: what the old mode had assembled cannot mean
            // anything to the new one.
            t.canon.clear();
            t.lines.clear();
            t.raw.clear();
        }
    }
    // SAFETY: as cli_save.
    unsafe { crate::arch::io::restore_flags(flags) };
}

/// Text grid of the active console (TIOCGWINSZ): the framebuffer's own
/// geometry, 80x25 in VGA text mode.
pub fn text_dims() -> (usize, usize) {
    crate::console::text_dims()
}