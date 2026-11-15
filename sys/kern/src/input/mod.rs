// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The Linux input layer's user interface (evdev), enough for Xorg's
// xf86-input-evdev / libinput: /dev/input/event0 is the keyboard, event1 the
// PS/2 mouse.  The wire format and every ioctl number are Linux's
// (<linux/input.h>, x86-64):
//
//   struct input_event { i64 sec; i64 usec; u16 type; u16 code; i32 value; }  // 24 bytes
//
// Producers (the i8042 keyboard decoder, the USB HID keyboard path, the PS/2
// mouse decoder) call `key()` / `mouse()` from interrupt context; every open
// file has a ring of its own that they fan out into, so two readers (Xorg and
// `evtest`) each see the whole stream.  A full ring is dropped whole and a
// SYN_DROPPED marker queued -- what the kernel does -- so a client resyncs
// with EVIOCGKEY.
//
// Key *semantics* are Linux's too: a press of a key that is already down
// becomes value 2 (autorepeat, which also covers the USB repeat timer that
// re-injects presses), and a release of a key that is not down is dropped.
//
// Locking: all state lives in one `static mut`; process context takes an
// IrqGuard, interrupt context already has interrupts masked, and the kernel's
// one-CPU input path (see hw/kbd.rs) is the only producer.

use alloc::rc::Rc;

use crate::arch::io::IrqGuard;
use crate::user::fd::{copy_in, copy_out};

pub mod ps2;

// ---------------------------------------------------------------- uapi

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;
pub const EV_SW: u16 = 0x05;
pub const EV_LED: u16 = 0x11;
pub const EV_SND: u16 = 0x12;
pub const EV_REP: u16 = 0x14;
pub const EV_FF: u16 = 0x15;
pub const EV_MAX: u16 = 0x1f;

pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;

pub const KEY_MAX: usize = 0x2ff;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_HWHEEL: u16 = 0x06;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_MAX: usize = 0x0f;
pub const ABS_MAX: usize = 0x3f;
pub const MSC_MAX: usize = 0x07;
pub const LED_MAX: usize = 0x0f;
pub const SND_MAX: usize = 0x07;
pub const SW_MAX: usize = 0x10;
pub const FF_MAX: usize = 0x7f;
pub const INPUT_PROP_MAX: usize = 0x1f;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;

pub const LED_NUML: u16 = 0;
pub const LED_CAPSL: u16 = 1;
pub const LED_SCROLLL: u16 = 2;

pub const BUS_I8042: u16 = 0x11;
pub const EV_VERSION: u32 = 0x01_00_01;

pub const EVENT_SIZE: usize = 24;

/// `struct input_event`.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Event {
    pub sec: i64,
    pub usec: i64,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

impl Event {
    pub fn to_bytes(&self) -> [u8; EVENT_SIZE] {
        let mut b = [0u8; EVENT_SIZE];
        b[0..8].copy_from_slice(&self.sec.to_le_bytes());
        b[8..16].copy_from_slice(&self.usec.to_le_bytes());
        b[16..18].copy_from_slice(&self.type_.to_le_bytes());
        b[18..20].copy_from_slice(&self.code.to_le_bytes());
        b[20..24].copy_from_slice(&self.value.to_le_bytes());
        b
    }
    pub fn from_bytes(b: &[u8; EVENT_SIZE]) -> Event {
        Event {
            sec: i64::from_le_bytes(b[0..8].try_into().unwrap()),
            usec: i64::from_le_bytes(b[8..16].try_into().unwrap()),
            type_: u16::from_le_bytes([b[16], b[17]]),
            code: u16::from_le_bytes([b[18], b[19]]),
            value: i32::from_le_bytes([b[20], b[21], b[22], b[23]]),
        }
    }
}

const _: () = assert!(core::mem::size_of::<Event>() == EVENT_SIZE);

// errno (negated by the callers)
const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const EINTR: i64 = 4;
const EAGAIN: i64 = 11;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const EBUSY: i64 = 16;
const ENODEV: i64 = 19;
const EINVAL: i64 = 22;
const ENOSYS: i64 = 38;

// ------------------------------------------------------------- devices

pub const DEV_KBD: usize = 0;
pub const DEV_MOUSE: usize = 1;
pub const N_DEV: usize = 2;

struct DevInfo {
    name: &'static str,
    phys: &'static str,
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

const DEVS: [DevInfo; N_DEV] = [
    DevInfo {
        name: "AT Translated Set 2 keyboard",
        phys: "isa0060/serio0/input0",
        bustype: BUS_I8042,
        vendor: 0x0001,
        product: 0x0001,
        version: 0xab41,
    },
    DevInfo {
        name: "ImPS/2 Generic Wheel Mouse",
        phys: "isa0060/serio1/input0",
        bustype: BUS_I8042,
        vendor: 0x0002,
        product: 0x0006,
        version: 0x0000,
    },
];

pub fn dev_name(dev: usize) -> &'static str {
    DEVS[dev].name
}
pub fn dev_phys(dev: usize) -> &'static str {
    DEVS[dev].phys
}
pub fn dev_id(dev: usize) -> (u16, u16, u16, u16) {
    let d = &DEVS[dev];
    (d.bustype, d.vendor, d.product, d.version)
}

const fn bits(list: &[u16], n: usize) -> [u8; 128] {
    // n is only for documentation; 128 bytes cover KEY_MAX (0x300 bits = 96 bytes).
    let mut out = [0u8; 128];
    let mut i = 0;
    while i < list.len() {
        let b = list[i] as usize;
        out[b / 8] |= 1 << (b % 8);
        i += 1;
    }
    let _ = n;
    out
}

const fn key_range(lo: u16, hi: u16) -> [u8; 128] {
    let mut out = [0u8; 128];
    let mut b = lo as usize;
    while b <= hi as usize {
        out[b / 8] |= 1 << (b % 8);
        b += 1;
    }
    out
}

const KBD_EVBITS: [u8; 128] = bits(&[EV_SYN, EV_KEY, EV_MSC, EV_LED, EV_REP], 0);
const MOUSE_EVBITS: [u8; 128] = bits(&[EV_SYN, EV_KEY, EV_REL], 0);
const KBD_KEYBITS: [u8; 128] = key_range(1, 127);
const MOUSE_KEYBITS: [u8; 128] = bits(&[BTN_LEFT, BTN_RIGHT, BTN_MIDDLE], 0);
const MOUSE_RELBITS: [u8; 128] = bits(&[REL_X, REL_Y, REL_WHEEL], 0);
const KBD_LEDBITS: [u8; 128] = bits(&[LED_NUML, LED_CAPSL, LED_SCROLLL], 0);
const KBD_MSCBITS: [u8; 128] = bits(&[4], 0); // MSC_SCAN
const ZERO_BITS: [u8; 128] = [0u8; 128];

fn evbits(dev: usize) -> &'static [u8; 128] {
    if dev == DEV_KBD {
        &KBD_EVBITS
    } else {
        &MOUSE_EVBITS
    }
}

/// The capability bitmap for event type `ev` (None = type the ioctl
/// rejects), plus its highest bit.
fn codebits(dev: usize, ev: u16) -> Option<(&'static [u8; 128], usize)> {
    let kbd = dev == DEV_KBD;
    Some(match ev {
        EV_KEY => (if kbd { &KBD_KEYBITS } else { &MOUSE_KEYBITS }, KEY_MAX),
        EV_REL => (if kbd { &ZERO_BITS } else { &MOUSE_RELBITS }, REL_MAX),
        EV_ABS => (&ZERO_BITS, ABS_MAX),
        EV_MSC => (if kbd { &KBD_MSCBITS } else { &ZERO_BITS }, MSC_MAX),
        EV_SW => (&ZERO_BITS, SW_MAX),
        EV_LED => (if kbd { &KBD_LEDBITS } else { &ZERO_BITS }, LED_MAX),
        EV_SND => (&ZERO_BITS, SND_MAX),
        EV_FF => (&ZERO_BITS, FF_MAX),
        _ => return None,
    })
}

/// A capability bitmap the way sysfs prints it (/sys/class/input/inputN/
/// capabilities/*): 64-bit words in plain `%lx`, most significant first,
/// space separated, leading zero words dropped ("70000 0 0 0 0" for a mouse's
/// keys); "0" when empty.
pub fn caps_hex(dev: usize, ev: Option<u16>) -> alloc::string::String {
    use alloc::string::String;
    let (bits, maxbit) = match ev {
        None => (evbits(dev), EV_MAX as usize),
        Some(e) => match codebits(dev, e) {
            Some(x) => x,
            None => return String::from("0"),
        },
    };
    let words = (maxbit + 1 + 63) / 64;
    let mut out = String::new();
    for w in (0..words).rev() {
        let mut v = 0u64;
        for b in 0..8 {
            v |= (bits[w * 8 + b] as u64) << (8 * b);
        }
        if out.is_empty() && v == 0 && w != 0 {
            continue; // leading zero words are dropped
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&alloc::format!("{:x}", v));
    }
    if out.is_empty() {
        out.push('0');
    }
    out
}

// --------------------------------------------------------------- state

const QLEN: usize = 256;
const MAX_CLIENTS: usize = 8;
const KEYWORDS: usize = (KEY_MAX + 1 + 63) / 64;

struct Client {
    used: bool,
    dev: u8,
    revoked: bool,
    head: usize,
    tail: usize,
    q: [Event; QLEN],
}

const EMPTY_EV: Event = Event { sec: 0, usec: 0, type_: 0, code: 0, value: 0 };
const IDLE: Client = Client { used: false, dev: 0, revoked: false, head: 0, tail: 0, q: [EMPTY_EV; QLEN] };

struct State {
    clients: [Client; MAX_CLIENTS],
    /// Held keys per device (EVIOCGKEY, repeat detection).
    down: [[u64; KEYWORDS]; N_DEV],
    /// Which client has grabbed each device (index + 1, 0 = nobody).
    grab: [u8; N_DEV],
    /// Mouse button state the packets are diffed against.
    buttons: u8,
}

static mut STATE: State = State {
    clients: [IDLE; MAX_CLIENTS],
    down: [[0; KEYWORDS]; N_DEV],
    grab: [0; N_DEV],
    buttons: 0,
};

fn st() -> &'static mut State {
    // SAFETY: see the locking note at the top of the file.
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
}

fn now() -> (i64, i64) {
    let ms = crate::hw::pit::uptime_ms();
    ((ms / 1000) as i64, ((ms % 1000) * 1000) as i64)
}

fn is_down(s: &State, dev: usize, code: u16) -> bool {
    let c = code as usize;
    c <= KEY_MAX && s.down[dev][c / 64] & (1 << (c % 64)) != 0
}

fn set_down(s: &mut State, dev: usize, code: u16, on: bool) {
    let c = code as usize;
    if c > KEY_MAX {
        return;
    }
    if on {
        s.down[dev][c / 64] |= 1 << (c % 64);
    } else {
        s.down[dev][c / 64] &= !(1 << (c % 64));
    }
}

fn push_one(c: &mut Client, ev: Event) {
    let next = (c.head + 1) % QLEN;
    if next == c.tail {
        // Full: drop everything, mark the loss (the evdev SYN_DROPPED
        // contract), keep going with a fresh ring.
        c.tail = c.head;
        let mut d = ev;
        d.type_ = EV_SYN;
        d.code = SYN_DROPPED;
        d.value = 0;
        let n2 = (c.head + 1) % QLEN;
        c.q[c.head] = d;
        c.head = n2;
        return;
    }
    c.q[c.head] = ev;
    c.head = next;
}

/// Deliver one event to every open file of `dev` (or only its grabber).
fn fan_out(s: &mut State, dev: usize, ev: Event) {
    let grab = s.grab[dev] as usize;
    for (i, c) in s.clients.iter_mut().enumerate() {
        if !c.used || c.revoked || c.dev as usize != dev {
            continue;
        }
        if grab != 0 && grab - 1 != i {
            continue;
        }
        push_one(c, ev);
    }
}

fn has_listener(s: &State, dev: usize) -> bool {
    s.clients.iter().any(|c| c.used && c.dev as usize == dev)
}

fn emit(s: &mut State, dev: usize, t: u16, code: u16, value: i32) {
    let (sec, usec) = now();
    fan_out(s, dev, Event { sec, usec, type_: t, code, value });
}

// ------------------------------------------------------------ producers

/// A key went down (`down` = true) or up on the keyboard.  `code` is the
/// Linux keycode.  Callable from interrupt context.
pub fn key(code: u8, down: bool) {
    let _g = IrqGuard::new();
    let s = st();
    let code = code as u16;
    let was = is_down(s, DEV_KBD, code);
    let value = match (was, down) {
        (false, true) => 1,
        (true, true) => 2, // autorepeat
        (true, false) => 0,
        (false, false) => return, // a release we never saw the press of
    };
    set_down(s, DEV_KBD, code, down);
    if !has_listener(s, DEV_KBD) {
        return;
    }
    emit(s, DEV_KBD, EV_KEY, code, value);
    emit(s, DEV_KBD, EV_SYN, SYN_REPORT, 0);
}

/// One decoded mouse packet: relative motion (right / down positive, like
/// REL_X / REL_Y), wheel (up positive) and the button mask (bit 0 left, 1
/// right, 2 middle).
pub fn mouse(dx: i32, dy: i32, wheel: i32, buttons: u8) {
    let _g = IrqGuard::new();
    let s = st();
    let changed = s.buttons ^ buttons;
    s.buttons = buttons;
    for (bit, code) in [(1u8, BTN_LEFT), (2, BTN_RIGHT), (4, BTN_MIDDLE)] {
        if changed & bit != 0 {
            set_down(s, DEV_MOUSE, code, buttons & bit != 0);
        }
    }
    if !has_listener(s, DEV_MOUSE) {
        return;
    }
    if dx != 0 {
        emit(s, DEV_MOUSE, EV_REL, REL_X, dx);
    }
    if dy != 0 {
        emit(s, DEV_MOUSE, EV_REL, REL_Y, dy);
    }
    if wheel != 0 {
        emit(s, DEV_MOUSE, EV_REL, REL_WHEEL, wheel);
    }
    for (bit, code) in [(1u8, BTN_LEFT), (2, BTN_RIGHT), (4, BTN_MIDDLE)] {
        if changed & bit != 0 {
            emit(s, DEV_MOUSE, EV_KEY, code, (buttons & bit != 0) as i32);
        }
    }
    emit(s, DEV_MOUSE, EV_SYN, SYN_REPORT, 0);
}

// -------------------------------------------------------------- the file

/// One open of an event node.
pub struct EvFile {
    slot: usize,
    dev: usize,
}

/// Open event node `dev`.
pub fn open(dev: usize) -> Result<EvFile, i64> {
    if dev >= N_DEV {
        return Err(-ENODEV);
    }
    let _g = IrqGuard::new();
    let s = st();
    let Some(slot) = s.clients.iter().position(|c| !c.used) else { return Err(-ENOMEM) };
    let c = &mut s.clients[slot];
    c.used = true;
    c.dev = dev as u8;
    c.revoked = false;
    c.head = 0;
    c.tail = 0;
    Ok(EvFile { slot, dev })
}

impl EvFile {
    pub fn dev(&self) -> usize {
        self.dev
    }
}

impl Drop for EvFile {
    fn drop(&mut self) {
        let _g = IrqGuard::new();
        let s = st();
        if s.grab[self.dev] as usize == self.slot + 1 {
            s.grab[self.dev] = 0;
        }
        s.clients[self.slot].used = false;
    }
}

/// POLLIN: an event is queued (a revoked file is readable too: its read
/// reports the error).
pub fn readable(f: &EvFile) -> bool {
    let _g = IrqGuard::new();
    let c = &st().clients[f.slot];
    c.revoked || c.head != c.tail
}

/// `read(2)`: whole events only; `len` must hold at least one.
pub fn read(f: &Rc<EvFile>, buf: u64, len: usize, nonblock: bool) -> i64 {
    if len < EVENT_SIZE {
        return -EINVAL;
    }
    loop {
        {
            let _g = IrqGuard::new();
            let c = &mut st().clients[f.slot];
            if c.revoked {
                return -ENODEV;
            }
            if c.head != c.tail {
                let mut out = [0u8; EVENT_SIZE * 32];
                let max = (len / EVENT_SIZE).min(32);
                let mut n = 0;
                while n < max && c.tail != c.head {
                    out[n * EVENT_SIZE..(n + 1) * EVENT_SIZE].copy_from_slice(&c.q[c.tail].to_bytes());
                    c.tail = (c.tail + 1) % QLEN;
                    n += 1;
                }
                drop(_g);
                if !copy_out(buf, 0, &out[..n * EVENT_SIZE]) {
                    return -EFAULT;
                }
                return (n * EVENT_SIZE) as i64;
            }
        }
        if nonblock {
            return -EAGAIN;
        }
        if crate::user::signal::interrupt_pending() {
            return -EINTR;
        }
        crate::user::process::sleep_current(5);
    }
}

/// `write(2)`: a client may send events (LED / sound); whole events are
/// accepted and the LED ones are acknowledged without effect (the console
/// owns the lock LEDs).
pub fn write(f: &EvFile, buf: u64, len: usize) -> i64 {
    {
        let _g = IrqGuard::new();
        if st().clients[f.slot].revoked {
            return -ENODEV;
        }
    }
    if len % EVENT_SIZE != 0 {
        return -EINVAL;
    }
    let mut n = 0;
    while n < len {
        let mut e = [0u8; EVENT_SIZE];
        if !copy_in(buf, n, &mut e) {
            return if n > 0 { n as i64 } else { -EFAULT };
        }
        n += EVENT_SIZE;
    }
    len as i64
}

// --------------------------------------------------------------- ioctl

const fn ioc_nr(c: u32) -> u32 {
    c & 0xFF
}
const fn ioc_type(c: u32) -> u32 {
    (c >> 8) & 0xFF
}
const fn ioc_size(c: u32) -> usize {
    ((c >> 16) & 0x3FFF) as usize
}
const fn ioc_dir(c: u32) -> u32 {
    c >> 30
}

fn put(arg: u64, data: &[u8]) -> i64 {
    if arg != 0 && copy_out(arg, 0, data) {
        data.len() as i64
    } else {
        -EFAULT
    }
}

/// Copy a capability bitmap out: at most `size` bytes, rounded the way the
/// kernel's bits_to_user() does (whole longs up to the highest bit).
fn put_bits(arg: u64, size: usize, bits: &[u8; 128], maxbit: usize) -> i64 {
    let longs = (maxbit + 1 + 63) / 64;
    let len = size.min(longs * 8).min(128);
    // Anything the user's buffer holds beyond the bitmap is zeroed, like
    // libevdev expects from a freshly cleared array.
    put(arg, &bits[..len])
}

/// `ioctl(fd, cmd, arg)` on an event node.
pub fn ioctl(f: &Rc<EvFile>, cmd: u32, arg: u64) -> i64 {
    if ioc_type(cmd) != b'E' as u32 {
        return -EINVAL;
    }
    let nr = ioc_nr(cmd);
    let size = ioc_size(cmd);
    let dev = f.dev;
    {
        let _g = IrqGuard::new();
        if st().clients[f.slot].revoked && nr != 0x91 {
            return -ENODEV;
        }
    }
    let read_dir = ioc_dir(cmd) & 2 != 0; // _IOC_READ
    match nr {
        0x01 if read_dir => put(arg, &EV_VERSION.to_le_bytes()),
        0x02 if read_dir => {
            let (b, v, p, r) = dev_id(dev);
            let mut o = [0u8; 8];
            o[0..2].copy_from_slice(&b.to_le_bytes());
            o[2..4].copy_from_slice(&v.to_le_bytes());
            o[4..6].copy_from_slice(&p.to_le_bytes());
            o[6..8].copy_from_slice(&r.to_le_bytes());
            put(arg, &o)
        }
        // EVIOCGREP: { delay, period } in ms (the console's 250 / 33).
        0x03 if read_dir => {
            let mut o = [0u8; 8];
            o[0..4].copy_from_slice(&250u32.to_le_bytes());
            o[4..8].copy_from_slice(&33u32.to_le_bytes());
            put(arg, &o)
        }
        0x03 => 0, // EVIOCSREP: accepted, the console owns the repeat
        0x04 => -EINVAL, // (G|S)KEYCODE[_V2]: no remapping through evdev
        0x06 | 0x07 | 0x08 => {
            // GNAME / GPHYS / GUNIQ
            let s: &str = match nr {
                0x06 => DEVS[dev].name,
                0x07 => DEVS[dev].phys,
                _ => return -ENOENT, // no unique id
            };
            let bytes = s.as_bytes();
            let n = (bytes.len() + 1).min(size);
            if n == 0 {
                return 0;
            }
            let mut out = [0u8; 80];
            let m = n.min(out.len());
            let cp = bytes.len().min(m);
            out[..cp].copy_from_slice(&bytes[..cp]);
            // The last byte copied is the NUL when it fits, else the
            // string is cut (the kernel's strlen+1 clamp).
            if m > 0 && cp == m {
                out[m - 1] = 0;
            }
            if put(arg, &out[..m]) < 0 {
                return -EFAULT;
            }
            m as i64
        }
        0x09 => {
            // EVIOCGPROP: no input properties.
            let n = size.min(4);
            put(arg, &ZERO_BITS[..n.max(1)])
        }
        0x0a => -EINVAL, // EVIOCGMTSLOTS: no multitouch
        0x18 => {
            // EVIOCGKEY: held keys.
            let _g = IrqGuard::new();
            let mut raw = [0u8; (KEYWORDS) * 8];
            for (i, w) in st().down[dev].iter().enumerate() {
                raw[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
            }
            let n = size.min(raw.len());
            drop(_g);
            put(arg, &raw[..n])
        }
        0x19 => {
            // EVIOCGLED: lock LEDs, in LED_* bit order.
            let mut v = 0u8;
            if dev == DEV_KBD {
                let l = crate::hw::kbd::leds(); // bit0 scroll, bit1 num, bit2 caps
                if l & 2 != 0 {
                    v |= 1 << LED_NUML;
                }
                if l & 4 != 0 {
                    v |= 1 << LED_CAPSL;
                }
                if l & 1 != 0 {
                    v |= 1 << LED_SCROLLL;
                }
            }
            put(arg, &[v][..size.min(1).max(1)])
        }
        0x1a | 0x1b => put(arg, &ZERO_BITS[..size.min(8).max(1)]), // GSND / GSW
        0x20 => put_bits(arg, size, evbits(dev), EV_MAX as usize),
        0x21..=0x3f => {
            let ev = (nr - 0x20) as u16;
            match codebits(dev, ev) {
                Some((b, max)) => put_bits(arg, size, b, max),
                None => -EINVAL,
            }
        }
        0x40..=0x7f => -EINVAL, // EVIOCGABS: no absolute axes
        0x80 | 0x81 => -EINVAL, // force feedback: not supported
        0x84 if read_dir => put(arg, &0i32.to_le_bytes()),
        0x90 => {
            // EVIOCGRAB: the int is the argument itself.
            let _g = IrqGuard::new();
            let s = st();
            let me = f.slot + 1;
            if arg != 0 {
                if s.grab[dev] != 0 && s.grab[dev] as usize != me {
                    return -EBUSY;
                }
                s.grab[dev] = me as u8;
            } else {
                if s.grab[dev] as usize != me {
                    return -EINVAL;
                }
                s.grab[dev] = 0;
            }
            0
        }
        0x91 => {
            let _g = IrqGuard::new();
            let s = st();
            if s.grab[dev] as usize == f.slot + 1 {
                s.grab[dev] = 0;
            }
            s.clients[f.slot].revoked = true;
            0
        }
        0x92 => 0, // EVIOCSMASK / GMASK: all event types are delivered
        0xa0 => {
            // EVIOCSCLOCKID: pointer to an int; timestamps are monotonic.
            let mut w = [0u8; 4];
            if !copy_in(arg, 0, &mut w) {
                return -EFAULT;
            }
            match i32::from_le_bytes(w) {
                0 | 1 | 4 | 7 => 0, // REALTIME / MONOTONIC / MONOTONIC_RAW / BOOTTIME
                _ => -EINVAL,
            }
        }
        _ => {
            let _ = (EPERM, ENOSYS);
            -EINVAL
        }
    }
}

// Linux x86-64 request numbers, for the tests and the readers of this file.
pub mod ioctls {
    const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
        (dir << 30) | (size << 16) | ((b'E' as u32) << 8) | nr
    }
    const R: u32 = 2;
    const W: u32 = 1;
    pub const EVIOCGVERSION: u32 = ioc(R, 0x01, 4);
    pub const EVIOCGID: u32 = ioc(R, 0x02, 8);
    pub const EVIOCGREP: u32 = ioc(R, 0x03, 8);
    pub const EVIOCSREP: u32 = ioc(W, 0x03, 8);
    pub const EVIOCGRAB: u32 = ioc(W, 0x90, 4);
    pub const EVIOCREVOKE: u32 = ioc(W, 0x91, 4);
    pub const EVIOCSCLOCKID: u32 = ioc(W, 0xa0, 4);
    pub const fn eviocgname(len: u32) -> u32 {
        ioc(R, 0x06, len)
    }
    pub const fn eviocgphys(len: u32) -> u32 {
        ioc(R, 0x07, len)
    }
    pub const fn eviocguniq(len: u32) -> u32 {
        ioc(R, 0x08, len)
    }
    pub const fn eviocgprop(len: u32) -> u32 {
        ioc(R, 0x09, len)
    }
    pub const fn eviocgkey(len: u32) -> u32 {
        ioc(R, 0x18, len)
    }
    pub const fn eviocgled(len: u32) -> u32 {
        ioc(R, 0x19, len)
    }
    pub const fn eviocgbit(ev: u32, len: u32) -> u32 {
        ioc(R, 0x20 + ev, len)
    }
    pub const fn eviocgabs(abs: u32) -> u32 {
        ioc(R, 0x40 + abs, 24)
    }
}
