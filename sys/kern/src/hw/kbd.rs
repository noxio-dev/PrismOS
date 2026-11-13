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
//   * Every access to ports 0x60/0x64 is paced (~50 us between status
//     polls) and every wait is bounded in time, like Linux's i8042 driver
//     (udelay(50) + I8042_CTL_TIMEOUT).  On machines where the firmware
//     keeps "USB legacy support" on, those ports are trapped by an SMM
//     handler, and a tight polling loop fires one SMI per microsecond --
//     which is how a stuck init used to freeze the whole box.  Paced and
//     bounded, the driver survives behind the trap; a future USB driver
//     should also take the legacy handoff over, the way Linux's PCI
//     quirks do.
//   * The reset fallback (0xFF) waits for the keyboard's BAT completion
//     (0xAA, 500-750 ms per the PS/2 spec, up to ~4 s behind SMM -- the
//     same budget Linux's libps2 gives PS2_CMD_RESET_BAT) before
//     re-sending "enable scanning", so the command is no longer
//     swallowed in the middle of the keyboard's self-test.
//   * A controller that answers 0xFF/0x00 on the status port is detected
//     as absent (pure-USB machines): the shell stays on the serial console
//     and says so instead of spinning.
//
// Decoding is in keymap.rs: a Linux-style keymap engine (Caps / Num / Scroll
// Lock with their LEDs, navigation and function keys, dead keys, keymaps
// loadable at run time with BusyBox `loadkmap`).  This file is the
// hardware side: i8042 bring-up, scancode normalisation, the LED handshake.
// Layouts are switchable at runtime (`layout us|fr` in the shell).

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use super::keymap::{self, Engine, Keymap, LED_CAP};
use crate::arch::io::{inb, io_wait, outb, IrqGuard};

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

    /// Queue a whole key's bytes (all or nothing, so a full ring never
    /// leaves half an escape sequence behind).
    fn push_bytes(&mut self, bytes: &[u8]) {
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
//
// The keys live in keymap.rs (Linux console keymaps, loadable at run time
// with BusyBox `loadkmap`).  `layout us|fr` regenerates the built-in ones.

const LAYOUT_US_ID: u8 = 0;
const LAYOUT_FR_ID: u8 = 1;
/// A keymap modified through KDSKBENT (loadkmap): no longer a built-in one.
const LAYOUT_CUSTOM: u8 = 2;

static LAYOUT: AtomicU8 = AtomicU8::new(LAYOUT_US_ID);

/// Install a built-in layout ("us" or "fr").
pub fn set_layout(name: &str) -> bool {
    let (layout, id) = match name {
        "us" => (&keymap::LAYOUT_US, LAYOUT_US_ID),
        "fr" => (&keymap::LAYOUT_FR, LAYOUT_FR_ID),
        _ => return false,
    };
    let _g = IrqGuard::new();
    // SAFETY: interrupts off; the decoder only reads the keymap from IRQ1.
    unsafe {
        keymap::build_default(&mut *addr_of_mut!(KEYMAP), layout);
        (*addr_of_mut!(KBD)).engine.release_all();
    }
    LAYOUT.store(id, Ordering::Release);
    true
}

pub fn layout_name() -> &'static str {
    match LAYOUT.load(Ordering::Acquire) {
        LAYOUT_FR_ID => "fr",
        LAYOUT_US_ID => "us",
        _ => "custom",
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
    // function keys (F7 is 0x83: handled before the table), locks, windows keys
    t[0x05] = 0x3B; // F1
    t[0x06] = 0x3C; // F2
    t[0x04] = 0x3D; // F3
    t[0x0C] = 0x3E; // F4
    t[0x03] = 0x3F; // F5
    t[0x0B] = 0x40; // F6
    t[0x0A] = 0x42; // F8
    t[0x01] = 0x43; // F9
    t[0x09] = 0x44; // F10
    t[0x78] = 0x57; // F11
    t[0x07] = 0x58; // F12
    t[0x77] = 0x45; // num lock
    t[0x7E] = 0x46; // scroll lock
    t[0x1F] = 0x5B; // E0: left windows
    t[0x27] = 0x5C; // E0: right windows
    t[0x2F] = 0x5D; // E0: menu
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
    t
};

// ------------------------------------------------------------------- state

const LED_IDLE: u8 = 0;
const LED_AWAIT_CMD: u8 = 1; // 0xED sent, waiting for its ACK
const LED_AWAIT_MASK: u8 = 2; // mask sent, waiting for its ACK
const LED_NO_OVERRIDE: u8 = 0xFF;

pub struct Kbd {
    ext: bool,  // 0xE0 prefix pending
    brk: bool,  // set 2: 0xF0 break prefix pending
    set2: bool, // decoding raw set 2 codes
    skip: u8,   // bytes of an 0xE1 (Pause) sequence still to swallow
    engine: Engine,
    // --- LED update state machine (0xED + mask, each acknowledged) ---
    led_state: u8,
    led_applied: u8, // mask the keyboard last confirmed (0xFF = unknown)
    led_last: u8,    // mask in flight
    led_retry: u8,
    led_noise: u8,   // key bytes seen while waiting for an ACK
    led_override: u8, // KDSETLED, LED_NO_OVERRIDE = follow the locks
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
/// The keyboard answered at init: worth sending it LED commands.
static LED_CAPABLE: AtomicBool = AtomicBool::new(false);

static mut KBD: Kbd = Kbd::new();
static mut QUEUE: Ring = Ring::new();
static mut KEYMAP: Keymap = Keymap::new();

impl Kbd {
    const fn new() -> Kbd {
        Kbd {
            ext: false,
            brk: false,
            set2: false,
            skip: 0,
            engine: Engine::new(),
            led_state: LED_IDLE,
            led_applied: 0xFF,
            led_last: 0,
            led_retry: 0,
            led_noise: 0,
            led_override: LED_NO_OVERRIDE,
        }
    }

    /// The LED mask the keyboard should show.
    fn led_want(&self) -> u8 {
        if self.led_override != LED_NO_OVERRIDE {
            self.led_override
        } else {
            self.engine.leds & 7
        }
    }

    /// Start a LED update if the keyboard is out of date.  Never waits for
    /// the keyboard: the ACKs come back through IRQ1 (`led_reply`).
    fn kick_leds(&mut self) {
        if self.led_state != LED_IDLE
            || !LED_CAPABLE.load(Ordering::Relaxed)
            || self.led_want() == self.led_applied
        {
            return;
        }
        if !wait_clear(0x02, 20) {
            return; // controller busy: the next key press retries
        }
        // SAFETY: keyboard command on fixed ports.
        unsafe { outb(KBD_DATA, 0xED) };
        self.led_state = LED_AWAIT_CMD;
        self.led_retry = 0;
        self.led_noise = 0;
    }

    /// The keyboard answered a LED command: 0xFA = ACK, 0xFE = resend.
    fn led_reply(&mut self, reply: u8) {
        let write = |b: u8| -> bool {
            if !wait_clear(0x02, 20) {
                return false;
            }
            // SAFETY: as in kick_leds.
            unsafe { outb(KBD_DATA, b) };
            true
        };
        match (self.led_state, reply) {
            (LED_AWAIT_CMD, 0xFA) => {
                self.led_last = self.led_want();
                self.led_state = if write(self.led_last) { LED_AWAIT_MASK } else { LED_IDLE };
            }
            (LED_AWAIT_MASK, 0xFA) => {
                self.led_applied = self.led_last;
                self.led_state = LED_IDLE;
                self.kick_leds(); // the locks may have changed meanwhile
            }
            (LED_AWAIT_CMD, 0xFE) | (LED_AWAIT_MASK, 0xFE) => {
                self.led_retry += 1;
                let byte = if self.led_state == LED_AWAIT_CMD { 0xED } else { self.led_last };
                if self.led_retry > 3 || !write(byte) {
                    self.led_state = LED_IDLE;
                }
            }
            _ => {} // stray ACK: nothing was waiting for it
        }
    }

    /// One raw controller byte in: scancode decoding, keymap, queue out.
    fn handle_byte(&mut self, raw: u8, km: &Keymap, queue: &mut Ring) {
        // ACK / resend belong to the LED handshake, never to a key.
        if raw == 0xFA || raw == 0xFE {
            self.led_reply(raw);
            return;
        }
        // Key bytes may legitimately overtake the ACK; but if a lot of them
        // pass without one, the answer got lost: stop waiting.
        if self.led_state != LED_IDLE {
            self.led_noise += 1;
            if self.led_noise > 16 {
                self.led_state = LED_IDLE;
            }
        }

        // showkey / kbd_mode -s: hand the bytes over untouched.
        if self.engine.mode == keymap::K_RAW {
            queue.push_bytes(&[raw]);
            return;
        }

        // Pause/Break is one long prefix-1 sequence without a release.
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        if raw == 0xE1 {
            self.skip = if self.set2 { 7 } else { 5 };
            return;
        }
        // The extended-key prefix is the same in both sets.
        if raw == 0xE0 {
            self.ext = true;
            return;
        }
        if self.set2 && raw == 0xF0 {
            self.brk = true;
            return;
        }

        // Normalize to (release, set-1 scancode).
        let (release, sc): (bool, u8) = if self.set2 {
            let r = self.brk;
            self.brk = false;
            let sc = match raw {
                0x83 => 0x41, // F7 does not fit in 7 bits
                0..=0x7F => SET2_TO_SET1[raw as usize],
                _ => 0,
            };
            (r, sc)
        } else {
            (raw & 0x80 != 0, raw & 0x7F)
        };
        let ext = self.ext;
        self.ext = false;
        if sc == 0 {
            return; // a key we do not know
        }
        let kc = keymap::keycode_of(sc, ext);
        if kc == 0 {
            return; // fake shift, multimedia key...
        }

        let leds_before = self.engine.leds;
        let mut out = self.engine.new_out();
        self.engine.key(km, kc, !release, &mut out);
        queue.push_bytes(out.bytes());
        if self.engine.leds != leds_before {
            self.kick_leds();
        }
    }
}


// ------------------------------------------------------------ i8042 driver
//
// All waiting on the controller goes through paced, bounded helpers, the
// way Linux's i8042 driver does it (drivers/input/serio/i8042.c):
//
//   * i8042_wait_read/i8042_wait_write poll the status register with
//     udelay(50) between polls, bounded by I8042_CTL_TIMEOUT = 10000
//     polls of 50 us = 500 ms -- "long enough for the i8042 itself to
//     timeout on sending a byte to a non-existent mouse" (i8042.h).
//   * i8042_flush() reads at most I8042_BUFFER_SIZE bytes, pausing
//     udelay(50) before each one.
//   * __i8042_command() waits for the input buffer to be free before the
//     command byte AND before every parameter byte.
//   * libps2.c gives a keyboard 200 ms to ACK an ordinary command, 1 s to
//     clock a reset (0xFF), and 4 s to post the BAT completion after it.
//
// Pacing matters as much as the bounds: with USB legacy support enabled,
// the firmware's SMM handler traps 0x60/0x64, and an un-paced polling
// loop fires an SMI per microsecond -- the machine then spends all its
// time entering and leaving SMM and the kernel appears hung.  With one
// poll per ~50 us the trap costs at most ~20k SMI/s during bring-up
// (init-time only; the IRQ path never polls on a clock).

fn status() -> u8 {
    // SAFETY: fixed legacy port, no memory involved.
    unsafe { inb(KBD_STATUS) }
}

fn data() -> u8 {
    // SAFETY: as above.
    unsafe { inb(KBD_DATA) }
}

/// ~50 us of pacing between two status polls.  io_wait() is a single
/// write to the POST debug port (~1 us each), looped 50 times -- the
/// no-timer equivalent of Linux's udelay(50).
fn pace() {
    let mut i = 0;
    while i < 50 {
        io_wait();
        i += 1;
    }
}

/// Busy-wait `polls` paced polls (~50 us each).  Used where Linux sleeps
/// between retries (msleep(50) in i8042_controller_selftest), without
/// requiring a timer this early in bring-up.
fn delay_polls(polls: u32) {
    let mut i = 0;
    while i < polls {
        pace();
        i += 1;
    }
}

/// Wait until the `mask` bits are SET in the status register, one paced
/// poll at a time, at most `budget` polls.  False = timeout.
fn wait_set(mask: u8, budget: u32) -> bool {
    let mut n = 0u32;
    while n < budget {
        if status() & mask != 0 {
            return true;
        }
        pace();
        n += 1;
    }
    false
}

/// Wait until the `mask` bits are CLEAR in the status register (same
/// shape as wait_set).
fn wait_clear(mask: u8, budget: u32) -> bool {
    let mut n = 0u32;
    while n < budget {
        if status() & mask == 0 {
            return true;
        }
        pace();
        n += 1;
    }
    false
}

// Wait budgets, in polls of ~50 us (see pace()):
//   10_000 polls ~= 500 ms  -- controller replies: I8042_CTL_TIMEOUT.
//                          Self-test and port-test answers are slow, and
//                          SMM emulation only makes them slower.
//    4_000 polls ~= 200 ms  -- keyboard ACK, libps2.c's default timeout.
//   20_000 polls ~=   1 s   -- reset (0xFF) ACK: libps2 gives the reset
//                          command itself a full second.
//   80_000 polls ~=   4 s   -- BAT completion after a reset: libps2's
//                          PS2_CMD_RESET_BAT budget.
const CTL_TIMEOUT: u32 = 10_000;
const KBD_ACK_TIMEOUT: u32 = 4_000;
const RESET_ACK_TIMEOUT: u32 = 20_000;
const BAT_TIMEOUT: u32 = 80_000;

/// Drain the output buffer, throwing away firmware leftovers.  Mirrors
/// Linux's i8042_flush(): bounded, with a ~50 us pause before each read
/// so an SMM trap is not hammered 64 times in a row.
fn drain() {
    let mut n = 0;
    while n < 32 {
        if status() & 0x01 == 0 {
            break; // output buffer empty
        }
        pace();
        data();
        n += 1;
    }
}

/// Write a no-parameter, no-reply controller command (0xAD, 0xA7, 0xAE).
/// Waits for the input buffer to be free first, like Linux's
/// __i8042_command() does before every byte it writes to the chip.
fn ctl_outb(cmd: u8) {
    if !wait_clear(0x02, CTL_TIMEOUT) {
        return; // controller wedged: nothing else we can do from here
    }
    // SAFETY: controller command on fixed legacy ports.
    unsafe { outb(KBD_STATUS, cmd) };
}

/// Send a controller command and return its reply byte, or None on
/// timeout.  The reply wait gets the full CTL_TIMEOUT budget: the
/// self-test (0xAA) legitimately takes tens to hundreds of milliseconds
/// on real controllers, and longer still behind an SMM trap.
fn ctrl_cmd(cmd: u8) -> Option<u8> {
    if !wait_clear(0x02, CTL_TIMEOUT) {
        return None; // input buffer stuck full: controller wedged
    }
    // SAFETY: controller command protocol on fixed ports.
    unsafe { outb(KBD_STATUS, cmd) };
    if !wait_set(0x01, CTL_TIMEOUT) {
        return None; // controller never answered
    }
    Some(data())
}

/// Read the controller config byte (command 0x20), re-reading until two
/// consecutive reads agree (bounded).  A single read can come back
/// corrupted -- especially behind SMM emulation -- which is why Linux's
/// i8042_controller_init() double-reads the CTR the same way before
/// trusting it.
fn read_config() -> Option<u8> {
    let mut prev = ctrl_cmd(0x20)?;
    let mut tries = 0;
    while tries < 10 {
        match ctrl_cmd(0x20) {
            Some(cur) if cur == prev => return Some(cur),
            Some(cur) => {
                pace(); // udelay(50), like Linux between CTR re-reads
                prev = cur;
            }
            None => return Some(prev), // re-read timed out: trust the first
        }
        tries += 1;
    }
    Some(prev) // never stabilized: best effort
}

/// Write the controller config byte (command 0x60).  Waits for the input
/// buffer before the command byte *and* before the parameter, exactly
/// like Linux's __i8042_command().
fn write_config(cfg: u8) -> bool {
    if !wait_clear(0x02, CTL_TIMEOUT) {
        return false;
    }
    // SAFETY: controller command protocol on fixed ports.
    unsafe { outb(KBD_STATUS, 0x60) };
    if !wait_clear(0x02, CTL_TIMEOUT) {
        return false;
    }
    // SAFETY: as above.
    unsafe { outb(KBD_DATA, cfg) };
    true
}

/// Send a command TO THE KEYBOARD (through 0x60) and wait for its ACK
/// (0xFA), handling one resend request (0xFE) per attempt, two attempts
/// total.  `budget` bounds each ACK wait in polls of ~50 us.
///
/// This is the function that used to wedge the box: the old code spun up
/// to 2 x 20 million un-paced status reads, so behind an SMM trap
/// (USB legacy support) it fired an SMI per microsecond, and even on sane
/// hardware a silent keyboard blocked init for tens of seconds.  Paced
/// and bounded, a missing keyboard costs at most `budget` polls per
/// attempt -- and the controller itself reports a transmit timeout long
/// before that anyway (it gives up on a device after ~15 ms, which is
/// exactly why Linux sized I8042_CTL_TIMEOUT at 500 ms).
fn kbd_cmd(cmd: u8, budget: u32) -> bool {
    for _attempt in 0..2 {
        if !wait_clear(0x02, CTL_TIMEOUT) {
            return false; // controller input buffer wedged
        }
        // SAFETY: keyboard command protocol on fixed ports.
        unsafe { outb(KBD_DATA, cmd) };
        let mut noise = 0u32;
        loop {
            if !wait_set(0x01, budget) {
                return false; // keyboard silent: absent or wedged
            }
            // SAFETY: as above.
            match data() {
                0xFA => return true, // ACK
                0xFE => break,       // resend -> one more attempt
                0xAA | 0xEE => {}    // BAT ok / echo: keep waiting
                _ => {
                    noise += 1;
                    if noise > 64 {
                        return false; // stream of junk: give up
                    }
                }
            }
        }
    }
    false
}

/// After a keyboard reset (0xFF), wait for the BAT completion the
/// keyboard posts when its self-test finishes: 0xAA ok, 0xFC/0xFD error.
/// The PS/2 spec gives the keyboard 500-750 ms; SMM emulation adds a
/// lot, so the budget is ~4 s -- the same figure Linux's libps2 waits
/// for PS2_CMD_RESET_BAT.  Mouse bytes in the shared buffer are skipped.
fn wait_bat() -> bool {
    let mut n = 0u32;
    while n < BAT_TIMEOUT {
        // SAFETY: status/data reads on fixed legacy ports.
        let st = status();
        if st & 0x01 != 0 {
            let b = data();
            if st & 0x20 == 0 {
                match b {
                    0xAA => return true,       // BAT ok
                    0xFC | 0xFD => return false, // BAT error
                    _ => {}                    // late ACK echo / stale byte
                }
            }
        }
        pace();
        n += 1;
    }
    false
}

/// Bring up the i8042 + keyboard for real hardware (see module comment).
pub fn init() -> Status {
    // 0. The default keymap, before any key can arrive (also on machines
    //    without a PS/2 controller: the shell `layout` command needs it).
    set_layout("us");

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
    //    Like Linux's i8042_controller_selftest(), the self-test is
    //    retried a few times: on some fragile controllers the first
    //    answer is garbage.
    let mut self_test = false;
    let mut tries = 0;
    while tries < 5 {
        match ctrl_cmd(0xAA) {
            Some(0x55) => {
                self_test = true;
                break;
            }
            Some(_) => {}   // wrong reply: pause and retry
            None => break,  // controller silent: retrying is pointless
        }
        delay_polls(1000); // ~50 ms, Linux's msleep(50) between tries
        tries += 1;
    }
    let port_test = ctrl_cmd(0xAB) == Some(0x00);
    STATUS_SELFTEST.store(self_test, Ordering::Release);
    STATUS_PORTTEST.store(port_test, Ordering::Release);

    // 4. Disable both ports while reconfiguring, then rewrite the config
    //    byte: first-port IRQ on, translation on, mouse off.
    ctl_outb(0xAD); // disable first port
    ctl_outb(0xA7); // disable second port (kills AUX bytes)
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
    ctl_outb(0xAE);
    drain();
    let mut acked = kbd_cmd(0xF4, KBD_ACK_TIMEOUT); // enable scanning
    if !acked {
        // Keyboards the firmware never configured (common on UEFI-only
        // laptops) may need a full reset before they talk to us.  After
        // the reset the keyboard is busy running its BAT for 500-750 ms
        // (PS/2 spec, slower behind SMM): wait for the 0xAA completion
        // *before* re-sending 0xF4, or the command is swallowed in the
        // middle of the self-test and the keyboard never answers it.
        if kbd_cmd(0xFF, RESET_ACK_TIMEOUT) && wait_bat() {
            drain(); // the BAT byte is behind us; drop anything stale
            acked = kbd_cmd(0xF4, KBD_ACK_TIMEOUT);
        }
    }
    STATUS_ACKED.store(acked, Ordering::Release);
    drain();

    // 6. Show the lock state (Num Lock starts on) on the LEDs.  Synchronous
    //    here -- IRQ1 is still masked; later updates go through IRQ1.
    if acked {
        LED_CAPABLE.store(true, Ordering::Release);
        // SAFETY: IRQs masked, single CPU.
        let kbd = unsafe { &mut *addr_of_mut!(KBD) };
        let mask = kbd.led_want();
        if kbd_cmd(0xED, KBD_ACK_TIMEOUT) && kbd_cmd(mask, KBD_ACK_TIMEOUT) {
            kbd.led_applied = mask;
        }
        drain();
    }

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
        let km = unsafe { &*addr_of_mut!(KEYMAP) };
        kbd.handle_byte(b, km, queue);
    }
}

/// Next decoded character, or None (for the line discipline).
pub fn pop_char() -> Option<u8> {
    // SAFETY: consumer side of the SPSC ring.
    let queue = unsafe { &mut *addr_of_mut!(QUEUE) };
    queue.pop()
}

/// Whether a decoded character is queued (poll / select's console probe,
/// phase 3.2) -- the non-destructive side of `pop_char`.
pub fn has_char() -> bool {
    // SAFETY: read-only peek at the SPSC ring counters.
    let queue = unsafe { &*addr_of_mut!(QUEUE) };
    queue.head.load(Ordering::Relaxed) != queue.tail.load(Ordering::Acquire)
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
    let km = unsafe { &*addr_of_mut!(KEYMAP) };
    kbd.handle_byte(b, km, queue);
    true
}

/// Inject one key event from a keyboard that is not on the i8042 (USB HID),
/// already normalised the way `handle_byte` normalises PS/2 bytes: a set-1
/// scancode (7 bits) plus the extended (E0) flag.  It goes through the same
/// keymap / modifier engine as a PS/2 key, so layouts, Shift/Ctrl/AltGr,
/// Caps Lock and `kbd_mode -s` raw mode behave identically.
///
/// Runs with local interrupts masked, so it cannot interleave with IRQ1 on
/// the same CPU.  (A PS/2 and a USB keyboard typed on at the very same
/// instant from two CPUs could still interleave; the ring is SPSC.)
pub fn inject_key(sc: u8, ext: bool, release: bool) {
    let _irq = crate::arch::io::IrqGuard::new();
    // SAFETY: interrupts are masked; see the note above for the SMP case.
    let kbd = unsafe { &mut *addr_of_mut!(KBD) };
    let queue = unsafe { &mut *addr_of_mut!(QUEUE) };
    let km = unsafe { &*addr_of_mut!(KEYMAP) };
    if kbd.engine.mode == keymap::K_RAW {
        // showkey: hand over what a PS/2 keyboard would have sent (set 1).
        if ext {
            queue.push_bytes(&[0xE0]);
        }
        queue.push_bytes(&[(sc & 0x7F) | if release { 0x80 } else { 0 }]);
        return;
    }
    let kc = keymap::keycode_of(sc & 0x7F, ext);
    if kc == 0 {
        return;
    }
    let leds_before = kbd.engine.leds;
    let mut out = kbd.engine.new_out();
    kbd.engine.key(km, kc, !release, &mut out);
    queue.push_bytes(out.bytes());
    if kbd.engine.leds != leds_before {
        kbd.kick_leds(); // no-op without a PS/2 keyboard (LED_CAPABLE unset)
    }
}

/// Modifier snapshot for the `kbd` command: (shift, ctrl, alt, altgr, caps).
pub fn modifiers() -> (bool, bool, bool, bool, bool) {
    // SAFETY: read-only snapshot of the decoder state.
    let e = unsafe { &(*addr_of_mut!(KBD)).engine };
    let s = e.shift_state;
    (
        s & 0b0011_0001 != 0,
        s & 0b1100_0100 != 0,
        s & 0b0000_1010 != 0,
        s & 0b0000_0010 != 0,
        e.leds & LED_CAP != 0,
    )
}

/// Lock state, the LED bits: bit 0 Scroll Lock, bit 1 Num Lock, bit 2 Caps Lock.
pub fn leds() -> u8 {
    // SAFETY: read-only snapshot.
    unsafe { (*addr_of_mut!(KBD)).engine.leds }
}

// ------------------------------------------------- console keyboard ioctls
//
// What `sys_ioctl` needs for KDGKBENT / KDSKBENT (BusyBox loadkmap and
// dumpkmap), KDGKBMODE / KDSKBMODE (kbd_mode, showkey) and the LED ioctls.
// Callers run in process context: the decoder state is shared with IRQ1, so
// every mutation happens with interrupts off.

/// KDGKBENT: K_NOSUCHMAP for a table that does not exist.
pub fn kd_get_entry(table: u8, index: u8) -> u16 {
    let _g = IrqGuard::new();
    // SAFETY: interrupts are off, single CPU takes IRQ1.
    unsafe { (*addr_of_mut!(KEYMAP)).get(table as usize, index as usize) }
}

/// KDSKBENT.  False = invalid table or index.
pub fn kd_set_entry(table: u8, index: u8, value: u16) -> bool {
    let _g = IrqGuard::new();
    // SAFETY: as above.
    let ok = unsafe { (*addr_of_mut!(KEYMAP)).set(table as usize, index as usize, value) };
    if ok {
        LAYOUT.store(LAYOUT_CUSTOM, Ordering::Release);
    }
    ok
}

/// KDGKBMODE.
pub fn kd_get_mode() -> u8 {
    // SAFETY: read-only snapshot.
    unsafe { (*addr_of_mut!(KBD)).engine.mode }
}

/// KDSKBMODE.  False = unknown mode.
pub fn kd_set_mode(mode: u8) -> bool {
    if mode > keymap::K_OFF {
        return false;
    }
    let _g = IrqGuard::new();
    // SAFETY: interrupts off.
    let kbd = unsafe { &mut *addr_of_mut!(KBD) };
    if kbd.engine.mode != mode {
        kbd.engine.mode = mode;
        // Release events were not tracked in the previous mode.
        kbd.engine.release_all();
    }
    true
}

/// KDGKBLED: lock state in the low 3 bits, the "default" state above.
pub fn kd_get_kbled() -> u8 {
    // SAFETY: read-only snapshot.
    let e = unsafe { &(*addr_of_mut!(KBD)).engine };
    (e.leds & 7) | (e.default_leds & 7) << 4
}

/// KDSKBLED: set the lock state (arg 0..7) or the default state (arg
/// 0x10..0x70, the state shifted left by 4).  False = invalid argument.
pub fn kd_set_kbled(arg: u64) -> bool {
    if arg & !0x77 != 0 {
        return false;
    }
    let _g = IrqGuard::new();
    // SAFETY: interrupts off.
    let kbd = unsafe { &mut *addr_of_mut!(KBD) };
    if arg & 0x70 != 0 {
        kbd.engine.default_leds = (arg >> 4) as u8 & 7;
    } else {
        kbd.engine.leds = arg as u8 & 7;
    }
    kbd.kick_leds();
    true
}

/// KDGETLED: what the keyboard LEDs show right now.
pub fn kd_get_led() -> u8 {
    // SAFETY: read-only snapshot.
    unsafe { (*addr_of_mut!(KBD)).led_want() }
}

/// KDSETLED: show `arg` (0..7) on the LEDs, or, for anything else, go back
/// to showing the lock state.
pub fn kd_set_led(arg: u64) {
    let _g = IrqGuard::new();
    // SAFETY: interrupts off.
    let kbd = unsafe { &mut *addr_of_mut!(KBD) };
    kbd.led_override = if arg <= 7 { arg as u8 } else { LED_NO_OVERRIDE };
    kbd.kick_leds();
}
