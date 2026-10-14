// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// PS/2 keyboard on the i8042 controller, interrupt driven (IRQ1).
//
// This is a full rewrite of the polled driver, aimed at real hardware:
//
//   * The controller is properly initialized: output buffer drained,
//     self-test (0xAA), first-port interface test (0xAB), devices disabled
//     during configuration, config byte rewritten (IRQ1 on, scancode
//     translation on, mouse off), first port re-enabled, "enable scanning"
//     (0xF4) sent with ACK handling -- with a full reset (0xFF) fallback
//     for keyboards the firmware never set up.
//   * Bytes are only treated as keyboard data when the status register
//     says "keyboard, not mouse" (bit 5 clear).  On laptops with a PS/2
//     touchpad, mouse packets used to poison the keyboard stream and hang
//     the old polled loop.
//   * Both scancode sets are decoded: set 1 (translated, the classic PC
//     default) and set 2 (raw, when the controller cannot translate --
//     common after UEFI boot).  Set 2 codes are normalized to set 1 in the
//     decoder, so there is one set of tables.
//   * Modifiers include AltGr (right alt): the FR AZERTY layout maps
//     AltGr+8 to backslash, AltGr+9 to ^, AltGr+0 to @ ... which is how
//     those characters are typed on a French laptop keyboard.
//   * A controller that answers 0xFF/0x00 on the status port is detected
//     as absent (pure-USB machines): the shell stays on the serial console
//     and says so instead of spinning.
//
// Layouts are switchable at runtime (`layout us|fr` in the shell).

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use crate::io::{inb, io_wait, outb};

const KBD_DATA: u16 = 0x60;
const KBD_STATUS: u16 = 0x64;

// ------------------------------------------------------------- ring buffer

/// SPSC byte ring (power-of-two size), producer = IRQ1, consumer = shell.
struct Ring {
    buf: [u8; 512],
    head: AtomicU32, // next write index (producer)
    tail: AtomicU32, // next read index (consumer)
}

impl Ring {
    const fn new() -> Ring {
        Ring { buf: [0; 512], head: AtomicU32::new(0), tail: AtomicU32::new(0) }
    }

    fn push(&mut self, b: u8) {
        let h = self.head.load(Ordering::Relaxed);
        let t = self.tail.load(Ordering::Acquire);
        if h.wrapping_sub(t) < self.buf.len() as u32 {
            self.buf[(h & (self.buf.len() as u32 - 1)) as usize] = b;
            self.head.store(h.wrapping_add(1), Ordering::Release);
        }
        // Overrun: drop the byte rather than tear the queue.
    }

    /// Queue a character as UTF-8 (all or nothing, so a full ring never
    /// leaves half a sequence behind).
    fn push_char(&mut self, c: char) {
        let mut tmp = [0u8; 4];
        let bytes = c.encode_utf8(&mut tmp).as_bytes();
        let used = self.head.load(Ordering::Relaxed).wrapping_sub(self.tail.load(Ordering::Acquire));
        if used as usize + bytes.len() > self.buf.len() {
            return;
        }
        for &b in bytes {
            self.push(b);
        }
    }

    fn pop(&mut self) -> Option<u8> {
        let t = self.tail.load(Ordering::Relaxed);
        let h = self.head.load(Ordering::Acquire);
        if t == h {
            return None;
        }
        let b = self.buf[(t & (self.buf.len() as u32 - 1)) as usize];
        self.tail.store(t.wrapping_add(1), Ordering::Release);
        Some(b)
    }
}

// ------------------------------------------------------------------ layout

/// One keyboard layout: unshifted / shifted / AltGr tables indexed by
/// set-1 scancode, holding Unicode characters ('\0' = no character).  The
/// decoder queues them as UTF-8, so accents, the euro sign and friends
/// reach the console intact.
pub struct Layout {
    pub name: &'static str,
    pub plain: [char; 128],
    pub shift: [char; 128],
    pub altgr: [char; 128],
}

const fn fill(dst: &mut [char; 128], start: u8, s: &[char]) {
    let mut i = 0;
    while i < s.len() {
        dst[start as usize + i] = s[i];
        i += 1;
    }
}

/// Keys every layout shares: Esc, Backspace, Tab, Enter, Space, keypad.
const fn common(t: &mut [char; 128]) {
    t[0x01] = '\x1b';
    t[0x0E] = '\x7f';
    t[0x0F] = '\t';
    t[0x1C] = '\n';
    t[0x39] = ' ';
}

const fn keypad(t: &mut [char; 128]) {
    fill(t, 0x47, &['7', '8', '9', '-', '4', '5', '6', '+', '1', '2', '3', '0', '.']);
}

const US_PLAIN: [char; 128] = {
    let mut t = ['\0'; 128];
    common(&mut t);
    fill(&mut t, 0x02, &['1', '2', '3', '4', '5', '6', '7', '8', '9', '0', '-', '=']);
    fill(&mut t, 0x10, &['q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p', '[', ']']);
    fill(&mut t, 0x1E, &['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', ';', '\'']);
    t[0x29] = '`';
    t[0x2B] = '\\';
    fill(&mut t, 0x2C, &['z', 'x', 'c', 'v', 'b', 'n', 'm', ',', '.', '/']);
    t[0x56] = '\\'; // ISO key next to left shift
    keypad(&mut t);
    t
};

const US_SHIFT: [char; 128] = {
    let mut t = ['\0'; 128];
    common(&mut t);
    fill(&mut t, 0x02, &['!', '@', '#', '$', '%', '^', '&', '*', '(', ')', '_', '+']);
    fill(&mut t, 0x10, &['Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', 'O', 'P', '{', '}']);
    fill(&mut t, 0x1E, &['A', 'S', 'D', 'F', 'G', 'H', 'J', 'K', 'L', ':', '"']);
    t[0x29] = '~';
    t[0x2B] = '|';
    fill(&mut t, 0x2C, &['Z', 'X', 'C', 'V', 'B', 'N', 'M', '<', '>', '?']);
    t[0x56] = '|';
    t
};

const US_ALTGR: [char; 128] = ['\0'; 128];

/// French AZERTY (the standard Windows FR layout).  AltGr layer:
///   é~ " # ' { ( [ - | è` _ \ ç^ à@ ) ] = }
/// so **AltGr+8 is the backslash** (and AltGr+9 = ^, AltGr+0 = @), AltGr+E
/// is the euro sign.  ^ and ¨ are typed as plain characters (no dead keys).
const FR_PLAIN: [char; 128] = {
    let mut t = ['\0'; 128];
    common(&mut t);
    t[0x29] = '²';
    fill(&mut t, 0x02, &['&', 'é', '"', '\'', '(', '-', 'è', '_', 'ç', 'à', ')', '=']);
    fill(&mut t, 0x10, &['a', 'z', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p', '^', '$']);
    fill(&mut t, 0x1E, &['q', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', 'm', 'ù']);
    t[0x2B] = '*';
    fill(&mut t, 0x2C, &['w', 'x', 'c', 'v', 'b', 'n', ',', ';', ':', '!']);
    t[0x56] = '<';
    keypad(&mut t);
    t
};

const FR_SHIFT: [char; 128] = {
    let mut t = ['\0'; 128];
    common(&mut t);
    fill(&mut t, 0x02, &['1', '2', '3', '4', '5', '6', '7', '8', '9', '0', '°', '+']);
    fill(&mut t, 0x10, &['A', 'Z', 'E', 'R', 'T', 'Y', 'U', 'I', 'O', 'P', '¨', '£']);
    fill(&mut t, 0x1E, &['Q', 'S', 'D', 'F', 'G', 'H', 'J', 'K', 'L', 'M', '%']);
    t[0x2B] = 'µ';
    fill(&mut t, 0x2C, &['W', 'X', 'C', 'V', 'B', 'N', '?', '.', '/', '§']);
    t[0x56] = '>';
    t
};

const FR_ALTGR: [char; 128] = {
    let mut t = ['\0'; 128];
    // 0x03..0x0D: the AltGr layer of the digit row.
    fill(&mut t, 0x03, &['~', '#', '{', '[', '|', '`', '\\', '^', '@', ']', '}']);
    t[0x12] = '€';
    t[0x1B] = '¤';
    t
};

pub static LAYOUT_US: Layout = Layout { name: "us", plain: US_PLAIN, shift: US_SHIFT, altgr: US_ALTGR };
pub static LAYOUT_FR: Layout = Layout { name: "fr", plain: FR_PLAIN, shift: FR_SHIFT, altgr: FR_ALTGR };

/// 0 = US, 1 = FR.
static LAYOUT: AtomicU8 = AtomicU8::new(0);

pub fn set_layout(name: &str) -> bool {
    match name {
        "us" => LAYOUT.store(0, Ordering::Release),
        "fr" => LAYOUT.store(1, Ordering::Release),
        _ => return false,
    }
    true
}

pub fn layout_name() -> &'static str {
    if LAYOUT.load(Ordering::Acquire) == 1 {
        "fr"
    } else {
        "us"
    }
}

fn current_layout() -> &'static Layout {
    if LAYOUT.load(Ordering::Acquire) == 1 {
        &LAYOUT_FR
    } else {
        &LAYOUT_US
    }
}

// ---------------------------------------------- set 2 -> set 1 translation
// When the i8042 cannot translate (config bit 6 refuses to stick -- seen on
// some UEFI machines), the keyboard delivers raw set 2 codes: make codes
// with a 0xF0 break prefix.  Normalizing them to set 1 lets the decoder and
// the layout tables stay unique.  Extended set 2 codes (E0 14 = right ctrl,
// E0 11 = right alt, E0 5A = keypad enter, E0 4A = keypad /) land exactly
// on their set 1 equivalents after normalization.

const SET2_TO_SET1: [u8; 128] = {
    let mut t = [0u8; 128];
    t[0x0E] = 0x29; // grave
    t[0x0D] = 0x0F; // tab
    t[0x11] = 0x38; // left alt
    t[0x12] = 0x2A; // left shift
    t[0x14] = 0x1D; // left ctrl
    t[0x15] = 0x10; // q
    t[0x16] = 0x02; // 1
    t[0x1A] = 0x2C; // z
    t[0x1B] = 0x1F; // s
    t[0x1C] = 0x1E; // a
    t[0x1D] = 0x11; // w
    t[0x1E] = 0x03; // 2
    t[0x21] = 0x2E; // c
    t[0x22] = 0x2D; // x
    t[0x23] = 0x20; // d
    t[0x24] = 0x12; // e
    t[0x25] = 0x05; // 4
    t[0x26] = 0x04; // 3
    t[0x29] = 0x39; // space
    t[0x2A] = 0x2F; // v
    t[0x2B] = 0x21; // f
    t[0x2C] = 0x14; // t
    t[0x2D] = 0x13; // r
    t[0x2E] = 0x06; // 5
    t[0x31] = 0x31; // n
    t[0x32] = 0x30; // b
    t[0x33] = 0x23; // h
    t[0x34] = 0x22; // g
    t[0x35] = 0x15; // y
    t[0x36] = 0x07; // 6
    t[0x3A] = 0x32; // m
    t[0x3B] = 0x24; // j
    t[0x3C] = 0x17; // u
    t[0x3D] = 0x08; // 7
    t[0x3E] = 0x09; // 8
    t[0x41] = 0x33; // comma
    t[0x42] = 0x25; // k
    t[0x43] = 0x18; // i
    t[0x44] = 0x19; // o
    t[0x45] = 0x0B; // 0
    t[0x46] = 0x0A; // 9
    t[0x49] = 0x34; // period
    t[0x4A] = 0x35; // slash
    t[0x4B] = 0x26; // l
    t[0x4C] = 0x27; // semicolon
    t[0x4D] = 0x19; // p
    t[0x4E] = 0x0C; // minus
    t[0x61] = 0x56; // ISO key next to left shift (<> / backslash)
    t[0x52] = 0x28; // apostrophe
    t[0x54] = 0x1A; // [
    t[0x55] = 0x0D; // =
    t[0x58] = 0x3A; // caps
    t[0x59] = 0x36; // right shift
    t[0x5A] = 0x1C; // enter
    t[0x5B] = 0x1B; // ]
    t[0x5D] = 0x2B; // backslash
    t[0x66] = 0x0E; // backspace
    t[0x76] = 0x01; // esc
    // keypad
    t[0x69] = 0x4F; // 1
    t[0x6B] = 0x4B; // 4
    t[0x6C] = 0x47; // 7
    t[0x70] = 0x52; // 0
    t[0x71] = 0x53; // .
    t[0x72] = 0x50; // 2
    t[0x73] = 0x4C; // 5
    t[0x74] = 0x4D; // 6
    t[0x75] = 0x48; // 8
    t[0x79] = 0x4E; // +
    t[0x7A] = 0x51; // 3
    t[0x7B] = 0x4A; // -
    t[0x7C] = 0x37; // *
    t[0x7D] = 0x49; // 9
    t[0x4D] = 0x19; // p (correct entry)
    t
};

// ------------------------------------------------------------------- state

pub struct Kbd {
    shift_l: bool,
    shift_r: bool,
    ctrl_l: bool,
    ctrl_r: bool,
    alt_l: bool,
    altgr: bool, // right alt
    caps: bool,
    ext: bool, // 0xE0 prefix pending
    brk: bool, // set 2: 0xF0 break prefix pending
    set2: bool, // decoding raw set 2 codes
}

/// Controller probe results, exposed to the boot log and `kbd` command.
#[allow(dead_code)]
pub struct Status {
    pub present: bool,
    pub self_test: bool,
    pub port_test: bool,
    pub config: u8,
    pub set2: bool,
    pub acked: bool,
    pub irq_count: u32,
}

static STATUS_PRESENT: AtomicBool = AtomicBool::new(false);
static STATUS_SELFTEST: AtomicBool = AtomicBool::new(false);
static STATUS_PORTTEST: AtomicBool = AtomicBool::new(false);
static STATUS_CONFIG: AtomicU8 = AtomicU8::new(0);
static STATUS_SET2: AtomicBool = AtomicBool::new(false);
static STATUS_ACKED: AtomicBool = AtomicBool::new(false);
static IRQ_COUNT: AtomicU32 = AtomicU32::new(0);

static mut KBD: Kbd = Kbd::new();
static mut QUEUE: Ring = Ring::new();

impl Kbd {
    const fn new() -> Kbd {
        Kbd {
            shift_l: false,
            shift_r: false,
            ctrl_l: false,
            ctrl_r: false,
            alt_l: false,
            altgr: false,
            caps: false,
            ext: false,
            brk: false,
            set2: false,
        }
    }

    /// Translate one raw controller byte into an optional character.
    fn feed(&mut self, raw: u8) -> Option<char> {
        // The extended-key prefix is the same in both sets.
        if raw == 0xE0 {
            self.ext = true;
            return None;
        }
        if self.set2 && raw == 0xF0 {
            self.brk = true;
            return None;
        }

        // Normalize to (release, set-1 scancode).
        let (release, sc): (bool, u8) = if self.set2 {
            let r = self.brk;
            self.brk = false;
            (r, SET2_TO_SET1[(raw & 0x7F) as usize])
        } else {
            (raw & 0x80 != 0, raw & 0x7F)
        };
        if sc == 0 {
            return None; // F-keys and friends: nothing we decode
        }

        // --- extended (E0-prefixed) keys ---------------------------------
        if self.ext {
            self.ext = false;
            match sc {
                0x1D => self.ctrl_r = !release, // right ctrl
                0x38 => self.altgr = !release, // right alt = AltGr
                0x1C if !release => return Some('\n'), // keypad enter
                0x35 if !release => return Some('/'),  // keypad slash
                _ => {}
            }
            return None; // arrows, insert, delete, releases: not needed yet
        }

        // --- modifiers ----------------------------------------------------
        match sc {
            0x2A => {
                self.shift_l = !release;
                return None;
            }
            0x36 => {
                self.shift_r = !release;
                return None;
            }
            0x1D => {
                self.ctrl_l = !release;
                return None;
            }
            0x38 => {
                self.alt_l = !release;
                return None;
            }
            0x3A if !release => {
                self.caps = !self.caps;
                return None;
            }
            _ => {}
        }
        if release {
            return None;
        }

        // --- printable translation -----------------------------------------
        let layout = current_layout();
        let shifted = self.shift_l || self.shift_r;
        let mut c = if self.altgr {
            layout.altgr[sc as usize]
        } else if shifted {
            layout.shift[sc as usize]
        } else {
            layout.plain[sc as usize]
        };
        if c == '\0' {
            return None;
        }
        if self.caps && c.is_alphabetic() && !self.altgr {
            // Caps Lock flips the case of letters (é -> É as well).
            let flipped = if shifted { c.to_lowercase().next() } else { c.to_uppercase().next() };
            c = flipped.unwrap_or(c);
        }
        // Ctrl+letter -> matching control character (Ctrl-C, Ctrl-U...).
        if (self.ctrl_l || self.ctrl_r) && c.is_ascii_alphabetic() {
            return Some((c.to_ascii_lowercase() as u8 & 0x1F) as char);
        }
        Some(c)
    }
}

// ------------------------------------------------------------ i8042 driver

fn status() -> u8 {
    // SAFETY: fixed legacy port, no memory involved.
    unsafe { inb(KBD_STATUS) }
}

fn data() -> u8 {
    // SAFETY: as above.
    unsafe { inb(KBD_DATA) }
}

/// Drain the output buffer (bounded), throwing away firmware leftovers.
fn drain() {
    let mut n = 0;
    while status() & 0x01 != 0 && n < 32 {
        data();
        io_wait();
        n += 1;
    }
}

/// Send a controller command and return the (bounded-wait) reply byte.
fn ctrl_cmd(cmd: u8) -> Option<u8> {
    // SAFETY: controller command protocol on fixed ports.
    unsafe {
        let mut spin = 0u32;
        while status() & 0x02 != 0 && spin < 100_000 {
            spin += 1;
        }
        if spin >= 100_000 {
            return None;
        }
        outb(KBD_STATUS, cmd);
        spin = 0;
        while status() & 0x01 == 0 && spin < 10_000_000 {
            spin += 1;
        }
        if spin >= 10_000_000 {
            return None;
        }
        Some(inb(KBD_DATA))
    }
}

/// Read the controller config byte (command 0x20).
fn read_config() -> Option<u8> {
    ctrl_cmd(0x20)
}

/// Write the controller config byte (command 0x60).
fn write_config(cfg: u8) -> bool {
    // SAFETY: as above.
    unsafe {
        let mut spin = 0u32;
        while status() & 0x02 != 0 && spin < 100_000 {
            spin += 1;
        }
        if spin >= 100_000 {
            return false;
        }
        outb(KBD_STATUS, 0x60);
        outb(KBD_DATA, cfg);
        true
    }
}

/// Send a command TO THE KEYBOARD (through 0x60) and wait for its ACK
/// (0xFA), handling one resend request (0xFE).  Returns true on ACK.
fn kbd_cmd(cmd: u8) -> bool {
    // SAFETY: keyboard command protocol on fixed ports.
    unsafe {
        for _attempt in 0..2 {
            let mut spin = 0u32;
            while status() & 0x02 != 0 && spin < 100_000 {
                spin += 1;
            }
            if spin >= 100_000 {
                return false;
            }
            outb(KBD_DATA, cmd);
            let mut noise = 0u32;
            loop {
                let mut spin2 = 0u32;
                while status() & 0x01 == 0 && spin2 < 20_000_000 {
                    spin2 += 1;
                }
                if spin2 >= 20_000_000 {
                    return false; // keyboard silent: absent or wedged
                }
                match inb(KBD_DATA) {
                    0xFA => return true, // ACK
                    0xFE => break,       // resend -> one more attempt
                    0xAA | 0xEE => {}    // BAT ok / echo: keep waiting
                    _ => {
                        noise += 1;
                        if noise > 64 {
                            return false;
                        }
                    }
                }
            }
        }
        false
    }
}

/// Bring up the i8042 + keyboard for real hardware (see module comment).
pub fn init() -> Status {
    // 1. Presence probe: repeated 0xFF (floating bus) or 0x00 with no
    //    system flag means there is no controller to talk to.
    let mut all_ff = true;
    let mut all_zero = true;
    for _ in 0..3 {
        let s = status();
        if s != 0xFF {
            all_ff = false;
        }
        if s != 0x00 {
            all_zero = false;
        }
    }
    if all_ff || all_zero {
        STATUS_PRESENT.store(false, Ordering::Release);
        return status_snapshot();
    }

    // 2. Throw away whatever the firmware left in the output buffer.
    drain();

    // 3. Self-test (0xAA -> 0x55) and first-port test (0xAB -> 0x00).
    let self_test = ctrl_cmd(0xAA) == Some(0x55);
    let port_test = ctrl_cmd(0xAB) == Some(0x00);
    STATUS_SELFTEST.store(self_test, Ordering::Release);
    STATUS_PORTTEST.store(port_test, Ordering::Release);

    // 4. Disable both ports while reconfiguring, then rewrite the config
    //    byte: first-port IRQ on, translation on, mouse off.
    // SAFETY: controller command sequence on fixed ports.
    unsafe {
        outb(KBD_STATUS, 0xAD); // disable first port
        outb(KBD_STATUS, 0xA7); // disable second port (kills AUX bytes)
    }
    drain();

    let mut cfg = read_config().unwrap_or(0);
    cfg |= 0x01; // first-port IRQ
    cfg |= 0x40; // scancode translation -> set 1 on the bus
    cfg &= !0x02; // no second-port IRQ
    cfg |= 0x20; // second-port clock held (mouse stays off)
    cfg &= !0x10; // first-port clock enabled
    let wrote = write_config(cfg);
    let back = read_config().unwrap_or(0);
    let translated = wrote && back & 0x40 != 0;
    STATUS_CONFIG.store(back, Ordering::Release);
    STATUS_SET2.store(!translated, Ordering::Release);
    // SAFETY: state machine in .bss, single CPU, IRQs still masked.
    unsafe { (*addr_of_mut!(KBD)).set2 = !translated };

    // 5. Enable the first port and (re)start scanning.
    // SAFETY: controller command.
    unsafe { outb(KBD_STATUS, 0xAE) };
    drain();
    let mut acked = kbd_cmd(0xF4); // enable scanning
    if !acked {
        // Keyboards the firmware never configured (common on UEFI-only
        // laptops) may need a full reset before they talk to us.
        if kbd_cmd(0xFF) {
            drain(); // reset echoes BAT status
            acked = kbd_cmd(0xF4);
        }
    }
    STATUS_ACKED.store(acked, Ordering::Release);
    drain();

    STATUS_PRESENT.store(self_test || port_test || acked, Ordering::Release);
    status_snapshot()
}

fn status_snapshot() -> Status {
    Status {
        present: STATUS_PRESENT.load(Ordering::Acquire),
        self_test: STATUS_SELFTEST.load(Ordering::Acquire),
        port_test: STATUS_PORTTEST.load(Ordering::Acquire),
        config: STATUS_CONFIG.load(Ordering::Acquire),
        set2: STATUS_SET2.load(Ordering::Acquire),
        acked: STATUS_ACKED.load(Ordering::Acquire),
        irq_count: IRQ_COUNT.load(Ordering::Relaxed),
    }
}

/// IRQ1 entry: drain every pending keyboard byte (mouse bytes are skipped
/// thanks to the AUX bit) and queue the decoded characters.
pub fn on_irq() {
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    // Bounded: broken hardware cannot wedge us inside one interrupt.
    for _ in 0..64 {
        let st = status();
        if st & 0x01 == 0 {
            break; // output buffer empty
        }
        let b = data();
        if st & 0x20 != 0 {
            continue; // mouse byte in the shared buffer: not ours
        }
        // SAFETY: single IRQ context; the ring is SPSC.
        let kbd = unsafe { &mut *addr_of_mut!(KBD) };
        let queue = unsafe { &mut *addr_of_mut!(QUEUE) };
        if let Some(c) = kbd.feed(b) {
            queue.push_char(c);
        }
    }
}

/// Next decoded character, or None (for the line discipline).
pub fn pop_char() -> Option<u8> {
    // SAFETY: consumer side of the SPSC ring.
    let queue = unsafe { &mut *addr_of_mut!(QUEUE) };
    queue.pop()
}

/// Polling fallback used before IRQs are enabled.  Returns true when a
/// keyboard byte was consumed.
#[allow(dead_code)] // polling fallback
pub fn poll() -> bool {
    let st = status();
    if st & 0x01 == 0 || st & 0x20 != 0 {
        return false;
    }
    let b = data();
    // SAFETY: as above.
    let kbd = unsafe { &mut *addr_of_mut!(KBD) };
    let queue = unsafe { &mut *addr_of_mut!(QUEUE) };
    if let Some(c) = kbd.feed(b) {
        queue.push_char(c);
    }
    true
}

/// Modifier snapshot for the `kbd` command: (shift, ctrl, alt, altgr, caps).
pub fn modifiers() -> (bool, bool, bool, bool, bool) {
    // SAFETY: read-only snapshot of the decoder state.
    let k = unsafe { &*addr_of_mut!(KBD) };
    (k.shift_l || k.shift_r, k.ctrl_l || k.ctrl_r, k.alt_l || k.altgr, k.altgr, k.caps)
}
