// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side tests of evdev (input/mod.rs + input/ps2.rs) and the virtual
// terminal state machine (user/vt.rs): the real source files run against a
// simulated kernel (user memory = a byte arena, a clock, a fake process table
// that records signals, a fake keyboard-mode register).
//
//   rustc --edition 2021 --test tests/input_vt_host.rs -o /tmp/input_vt_host \
//     && /tmp/input_vt_host --test-threads=1

#![allow(dead_code, unused_imports)]

extern crate alloc;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

// ------------------------------------------------- the simulated kernel

thread_local! {
    static CLOCK_MS: Cell<u64> = Cell::new(5000);
    static UMEM: RefCell<Vec<u8>> = RefCell::new(Vec::new());
    static KBMODE: Cell<u8> = Cell::new(3);
    static SIGNALS: RefCell<Vec<(u64, u64)>> = RefCell::new(Vec::new());
    static ALIVE: Cell<bool> = Cell::new(true);
    static PID: Cell<u64> = Cell::new(100);
    static TAKEOVER: Cell<u32> = Cell::new(0);
    static TAKEOVER_UNMASTERED: Cell<u32> = Cell::new(0);
    static LEDS: Cell<u8> = Cell::new(0);
}
const UBASE: u64 = 0x1000_0000;

mod arch {
    pub mod io {
        pub struct IrqGuard;
        impl IrqGuard {
            pub fn new() -> IrqGuard {
                IrqGuard
            }
        }
    }
}

mod hw {
    pub mod pit {
        pub fn uptime_ms() -> u64 {
            super::super::CLOCK_MS.with(|c| c.get())
        }
    }
    pub mod keymap {
        pub const K_RAW: u8 = 0;
        pub const K_XLATE: u8 = 1;
        pub const K_MEDIUMRAW: u8 = 2;
        pub const K_UNICODE: u8 = 3;
        pub const K_OFF: u8 = 4;
    }
    pub mod kbd {
        pub fn leds() -> u8 {
            super::super::LEDS.with(|c| c.get())
        }
        pub fn kd_get_mode() -> u8 {
            super::super::KBMODE.with(|c| c.get())
        }
        pub fn kd_set_mode(m: u8) -> bool {
            if m > 4 {
                return false;
            }
            super::super::KBMODE.with(|c| c.set(m));
            true
        }
    }
}

mod drm {
    pub fn console_takeover() {
        super::TAKEOVER.with(|c| c.set(c.get() + 1));
    }
    pub fn console_takeover_unmastered() {
        super::TAKEOVER_UNMASTERED.with(|c| c.set(c.get() + 1));
    }
}

mod user {
    pub mod fd {
        use super::super::{UBASE, UMEM};
        pub(crate) fn copy_out(buf: u64, done: usize, data: &[u8]) -> bool {
            let at = buf + done as u64;
            UMEM.with(|m| {
                let mut m = m.borrow_mut();
                if at < UBASE || (at - UBASE) as usize + data.len() > m.len() {
                    return false;
                }
                let o = (at - UBASE) as usize;
                m[o..o + data.len()].copy_from_slice(data);
                true
            })
        }
        pub(crate) fn copy_in(buf: u64, done: usize, into: &mut [u8]) -> bool {
            let at = buf + done as u64;
            UMEM.with(|m| {
                let m = m.borrow();
                if at < UBASE || (at - UBASE) as usize + into.len() > m.len() {
                    return false;
                }
                let o = (at - UBASE) as usize;
                into.copy_from_slice(&m[o..o + into.len()]);
                true
            })
        }
    }
    pub mod signal {
        pub fn interrupt_pending() -> bool {
            false
        }
    }
    pub mod process {
        pub struct Process {
            pub pid: u64,
        }
        pub fn current() -> Option<&'static mut Process> {
            let pid = super::super::PID.with(|c| c.get());
            Some(Box::leak(Box::new(Process { pid })))
        }
        pub fn post_signal(pid: u64, sig: u64) -> bool {
            if !super::super::ALIVE.with(|c| c.get()) {
                return false;
            }
            super::super::SIGNALS.with(|s| s.borrow_mut().push((pid, sig)));
            true
        }
        pub fn sleep_current(ms: u64) {
            super::super::CLOCK_MS.with(|c| c.set(c.get() + ms.max(1)));
        }
    }
}

#[path = "../sys/kern/src/user/vt.rs"]
mod vt;

#[path = "../sys/kern/src/input/mod.rs"]
mod input;

use input::*;

// --------------------------------------------------------------- tools

fn umem_alloc(n: usize) -> u64 {
    UMEM.with(|m| {
        let mut m = m.borrow_mut();
        let at = (m.len() + 15) & !15;
        m.resize(at + n.max(1), 0);
        UBASE + at as u64
    })
}
fn umem_write(a: u64, b: &[u8]) {
    assert!(user::fd::copy_out(a, 0, b));
}
fn umem_read(a: u64, n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    assert!(user::fd::copy_in(a, 0, &mut v));
    v
}
fn advance(ms: u64) {
    CLOCK_MS.with(|c| c.set(c.get() + ms));
}

/// Fresh state: no clients, no keys down, nothing grabbed.
fn reset() {
    // SAFETY-free: STATE is private; releasing every EvFile (they are
    // dropped by the tests) leaves it clean except held keys / buttons, which
    // the tests release themselves.
}

fn drain(f: &Rc<EvFile>) -> Vec<Event> {
    let mut all = Vec::new();
    loop {
        let buf = umem_alloc(EVENT_SIZE * 32);
        let n = input::read(f, buf, EVENT_SIZE * 32, true);
        if n < 0 {
            assert_eq!(n, -11, "-EAGAIN when empty");
            return all;
        }
        for c in umem_read(buf, n as usize).chunks(EVENT_SIZE) {
            all.push(Event::from_bytes(c.try_into().unwrap()));
        }
    }
}

fn ioctl_out(f: &Rc<EvFile>, cmd: u32, len: usize) -> (i64, Vec<u8>) {
    let a = umem_alloc(len.max(1));
    umem_write(a, &vec![0xEE; len]);
    let r = input::ioctl(f, cmd, a);
    (r, umem_read(a, len))
}

fn ev(t: u16, c: u16, v: i32) -> (u16, u16, i32) {
    (t, c, v)
}
fn triples(v: &[Event]) -> Vec<(u16, u16, i32)> {
    v.iter().map(|e| (e.type_, e.code, e.value)).collect()
}

// ------------------------------------------------------------ evdev ABI

#[test]
fn input_event_wire_layout_is_linux_x86_64() {
    assert_eq!(core::mem::size_of::<Event>(), 24);
    let e = Event { sec: 0x0102030405060708, usec: 0x1112131415161718, type_: 0x2122, code: 0x3132, value: -2 };
    let b = e.to_bytes();
    assert_eq!(&b[0..8], &0x0102030405060708u64.to_le_bytes());
    assert_eq!(&b[8..16], &0x1112131415161718u64.to_le_bytes());
    assert_eq!(&b[16..18], &0x2122u16.to_le_bytes());
    assert_eq!(&b[18..20], &0x3132u16.to_le_bytes());
    assert_eq!(&b[20..24], &(-2i32).to_le_bytes());
    assert_eq!(Event::from_bytes(&b), e);
}

#[test]
fn ioctl_numbers_match_linux_input_h() {
    use input::ioctls::*;
    assert_eq!(EVIOCGVERSION, 0x8004_4501);
    assert_eq!(EVIOCGID, 0x8008_4502);
    assert_eq!(EVIOCGREP, 0x8008_4503);
    assert_eq!(EVIOCSREP, 0x4008_4503);
    assert_eq!(EVIOCGRAB, 0x4004_4590);
    assert_eq!(EVIOCREVOKE, 0x4004_4591);
    assert_eq!(EVIOCSCLOCKID, 0x4004_45a0);
    assert_eq!(eviocgname(256), 0x8100_4506);
    assert_eq!(eviocgphys(256), 0x8100_4507);
    assert_eq!(eviocguniq(256), 0x8100_4508);
    assert_eq!(eviocgprop(8), 0x8008_4509);
    assert_eq!(eviocgkey(96), 0x8060_4518);
    assert_eq!(eviocgled(8), 0x8008_4519);
    assert_eq!(eviocgbit(0, 4), 0x8004_4520);
    assert_eq!(eviocgbit(1, 96), 0x8060_4521);
    assert_eq!(eviocgbit(2, 8), 0x8008_4522);
    assert_eq!(eviocgabs(0), 0x8018_4540);
}

// ----------------------------------------------------------- the keyboard

#[test]
fn key_press_repeat_release_have_linux_values() {
    let f = Rc::new(input::open(DEV_KBD).unwrap());
    input::key(30, true); // KEY_A
    input::key(30, true); // the USB repeat timer / a repeated make code
    input::key(30, false);
    input::key(30, false); // a stray release: dropped
    let got = triples(&drain(&f));
    assert_eq!(
        got,
        vec![
            ev(EV_KEY, 30, 1),
            ev(EV_SYN, SYN_REPORT, 0),
            ev(EV_KEY, 30, 2),
            ev(EV_SYN, SYN_REPORT, 0),
            ev(EV_KEY, 30, 0),
            ev(EV_SYN, SYN_REPORT, 0),
        ]
    );
}

#[test]
fn timestamps_are_monotonic_milliseconds() {
    let f = Rc::new(input::open(DEV_KBD).unwrap());
    CLOCK_MS.with(|c| c.set(12_345));
    input::key(31, true);
    input::key(31, false);
    let e = drain(&f);
    assert_eq!((e[0].sec, e[0].usec), (12, 345_000));
}

#[test]
fn every_open_file_sees_the_whole_stream() {
    let a = Rc::new(input::open(DEV_KBD).unwrap());
    let b = Rc::new(input::open(DEV_KBD).unwrap());
    input::key(32, true);
    input::key(32, false);
    assert_eq!(drain(&a).len(), 4);
    assert_eq!(drain(&b).len(), 4);
}

#[test]
fn nobody_listening_costs_nothing_but_state_is_kept() {
    input::key(33, true); // no client: the key is recorded as down
    let f = Rc::new(input::open(DEV_KBD).unwrap());
    // EVIOCGKEY shows it held, so a late opener can resync.
    let (r, k) = ioctl_out(&f, input::ioctls::eviocgkey(96), 96);
    assert!(r > 0);
    assert_eq!(k[33 / 8] & (1 << (33 % 8)), 1 << (33 % 8));
    input::key(33, false);
    assert_eq!(triples(&drain(&f)), vec![ev(EV_KEY, 33, 0), ev(EV_SYN, SYN_REPORT, 0)]);
}

#[test]
fn read_rules() {
    let f = Rc::new(input::open(DEV_KBD).unwrap());
    let buf = umem_alloc(64);
    assert_eq!(input::read(&f, buf, 23, true), -22, "a buffer under one event is EINVAL");
    assert_eq!(input::read(&f, buf, 24, true), -11);
    assert!(!input::readable(&f));
    input::key(34, true);
    assert!(input::readable(&f));
    // Room for 1 event: exactly one comes out, the SYN stays queued.
    assert_eq!(input::read(&f, buf, 47, true), 24);
    assert!(input::readable(&f));
    assert_eq!(input::read(&f, buf, 48, true), 24);
    assert!(!input::readable(&f));
    input::key(34, false);
    drain(&f);
    // A blocking read on an empty queue sleeps (the stub clock advances) and
    // returns once data is there; here a producer runs "while it sleeps".
    assert_eq!(input::read(&f, umem_alloc(24), 24, true), -11);
}

#[test]
fn queue_overflow_becomes_syn_dropped() {
    let f = Rc::new(input::open(DEV_KBD).unwrap());
    for i in 0..400 {
        input::key(40 + (i % 20) as u8, i % 2 == 0);
    }
    let all = drain(&f);
    assert!(all.iter().any(|e| e.type_ == EV_SYN && e.code == SYN_DROPPED), "loss is announced");
    assert!(all.len() < 800, "the ring is bounded");
    // Release anything still down so later tests start clean.
    for k in 40..60u8 {
        input::key(k, false);
    }
    drain(&f);
}

// ------------------------------------------------------------ ioctl bodies

#[test]
fn identity_ioctls() {
    let f = Rc::new(input::open(DEV_KBD).unwrap());
    let (r, b) = ioctl_out(&f, input::ioctls::EVIOCGVERSION, 4);
    assert_eq!((r, u32::from_le_bytes(b[..4].try_into().unwrap())), (4, 0x010001));
    let (r, b) = ioctl_out(&f, input::ioctls::EVIOCGID, 8);
    assert_eq!(r, 8);
    assert_eq!(u16::from_le_bytes([b[0], b[1]]), 0x0011); // BUS_I8042
    assert_eq!(u16::from_le_bytes([b[6], b[7]]), 0xab41);
    let (r, b) = ioctl_out(&f, input::ioctls::eviocgname(256), 256);
    let name = b"AT Translated Set 2 keyboard";
    assert_eq!(r as usize, name.len() + 1);
    assert_eq!(&b[..name.len()], name);
    assert_eq!(b[name.len()], 0);
    // A short buffer: truncated and still NUL terminated.
    let (r, b) = ioctl_out(&f, input::ioctls::eviocgname(5), 5);
    assert_eq!(r, 5);
    assert_eq!(&b, b"AT Tr"[..4].iter().chain(&[0u8]).copied().collect::<Vec<_>>().as_slice());
    let (r, b) = ioctl_out(&f, input::ioctls::eviocgphys(64), 64);
    assert!(r > 0 && b.starts_with(b"isa0060/serio0/input0"));
    let (r, _) = ioctl_out(&f, input::ioctls::eviocguniq(64), 64);
    assert_eq!(r, -2, "no unique id: ENOENT");
    // Not an evdev request at all.
    assert_eq!(input::ioctl(&f, 0x5401, 0), -22);
}

#[test]
fn capability_bitmaps() {
    let k = Rc::new(input::open(DEV_KBD).unwrap());
    let (_, b) = ioctl_out(&k, input::ioctls::eviocgbit(0, 4), 4);
    assert_eq!(u32::from_le_bytes(b[..4].try_into().unwrap()), 0x0012_0013); // SYN KEY MSC LED REP
    let (r, b) = ioctl_out(&k, input::ioctls::eviocgbit(1, 96), 96);
    assert_eq!(r, 96);
    assert_eq!(b[0] & 0b10, 0b10, "KEY_ESC (1)");
    assert_eq!(b[30 / 8] & (1 << (30 % 8)) != 0, true, "KEY_A (30)");
    assert_eq!(b[127 / 8] & 0x80, 0x80);
    assert_eq!(b[0x110 / 8], 0, "no mouse buttons on a keyboard");
    // Types the device lacks answer zeroes; unknown types are EINVAL.
    let (r, b) = ioctl_out(&k, input::ioctls::eviocgbit(2, 8), 8);
    assert_eq!((r, b), (8, vec![0; 8]));
    assert_eq!(ioctl_out(&k, input::ioctls::eviocgbit(0x14, 8), 8).0, -22);
    assert_eq!(ioctl_out(&k, input::ioctls::eviocgabs(0), 24).0, -22);

    let m = Rc::new(input::open(DEV_MOUSE).unwrap());
    let (_, b) = ioctl_out(&m, input::ioctls::eviocgbit(0, 4), 4);
    assert_eq!(u32::from_le_bytes(b[..4].try_into().unwrap()), 0x7); // SYN KEY REL
    let (_, b) = ioctl_out(&m, input::ioctls::eviocgbit(2, 8), 8);
    assert_eq!(u64::from_le_bytes(b[..8].try_into().unwrap()), 0x103); // X Y WHEEL
    let (_, b) = ioctl_out(&m, input::ioctls::eviocgbit(1, 96), 96);
    assert_eq!(b[0x110 / 8], 0b0000_0111);
}

#[test]
fn sysfs_capability_strings_use_plain_hex_words() {
    assert_eq!(input::caps_hex(DEV_KBD, None), "120013");
    assert_eq!(input::caps_hex(DEV_MOUSE, None), "7");
    assert_eq!(input::caps_hex(DEV_MOUSE, Some(EV_REL)), "103");
    assert_eq!(input::caps_hex(DEV_MOUSE, Some(EV_KEY)), "70000 0 0 0 0");
    assert_eq!(input::caps_hex(DEV_KBD, Some(EV_LED)), "7");
    assert_eq!(input::caps_hex(DEV_KBD, Some(EV_ABS)), "0");
}

#[test]
fn led_state_follows_the_console_locks() {
    let k = Rc::new(input::open(DEV_KBD).unwrap());
    LEDS.with(|c| c.set(0b110)); // num lock + caps lock (console bit order)
    let (r, b) = ioctl_out(&k, input::ioctls::eviocgled(1), 1);
    assert_eq!(r, 1);
    assert_eq!(b[0], 0b011, "LED_NUML | LED_CAPSL");
    LEDS.with(|c| c.set(0));
}

#[test]
fn grab_is_exclusive_and_released_on_close() {
    use input::ioctls::EVIOCGRAB;
    let x = Rc::new(input::open(DEV_KBD).unwrap());
    let spy = Rc::new(input::open(DEV_KBD).unwrap());
    assert_eq!(input::ioctl(&x, EVIOCGRAB, 1), 0);
    assert_eq!(input::ioctl(&spy, EVIOCGRAB, 1), -16, "EBUSY");
    assert_eq!(input::ioctl(&spy, EVIOCGRAB, 0), -22, "not the grabber");
    input::key(44, true);
    input::key(44, false);
    assert_eq!(drain(&x).len(), 4);
    assert_eq!(drain(&spy).len(), 0, "a grabbed device is private");
    drop(x); // closing releases the grab
    input::key(44, true);
    input::key(44, false);
    assert_eq!(drain(&spy).len(), 4);
}

#[test]
fn revoke_ends_the_file() {
    let f = Rc::new(input::open(DEV_KBD).unwrap());
    assert_eq!(input::ioctl(&f, input::ioctls::EVIOCREVOKE, 0), 0);
    assert!(input::readable(&f), "readable so the reader learns of it");
    assert_eq!(input::read(&f, umem_alloc(24), 24, true), -19, "ENODEV");
    assert_eq!(input::ioctl(&f, input::ioctls::EVIOCGVERSION, umem_alloc(4)), -19);
}

#[test]
fn clock_and_rep_ioctls() {
    let f = Rc::new(input::open(DEV_KBD).unwrap());
    let a = umem_alloc(4);
    umem_write(a, &1i32.to_le_bytes()); // CLOCK_MONOTONIC
    assert_eq!(input::ioctl(&f, input::ioctls::EVIOCSCLOCKID, a), 0);
    umem_write(a, &99i32.to_le_bytes());
    assert_eq!(input::ioctl(&f, input::ioctls::EVIOCSCLOCKID, a), -22);
    let (r, b) = ioctl_out(&f, input::ioctls::EVIOCGREP, 8);
    assert_eq!(r, 8);
    assert_eq!(u32::from_le_bytes(b[..4].try_into().unwrap()), 250);
    assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 33);
}

#[test]
fn write_takes_whole_events_only() {
    let f = input::open(DEV_KBD).unwrap();
    let e = Event { type_: EV_LED, code: LED_CAPSL, value: 1, ..Default::default() };
    let a = umem_alloc(48);
    umem_write(a, &e.to_bytes());
    umem_write(a + 24, &e.to_bytes());
    assert_eq!(input::write(&f, a, 48), 48);
    assert_eq!(input::write(&f, a, 25), -22);
}

#[test]
fn too_many_opens_is_refused_and_close_frees_the_slot() {
    let mut v = Vec::new();
    loop {
        match input::open(DEV_KBD) {
            Ok(f) => v.push(f),
            Err(e) => {
                assert_eq!(e, -12);
                break;
            }
        }
    }
    assert!(v.len() >= 4);
    v.pop();
    assert!(input::open(DEV_MOUSE).is_ok());
}

// --------------------------------------------------------------- the mouse

#[test]
fn ps2_decoder_three_byte_packets() {
    use input::ps2::*;
    let mut d = Decoder::new();
    // Right 5, up 3 (wire Y positive), left button.
    assert_eq!(d.feed(0x09), None);
    assert_eq!(d.feed(5), None);
    assert_eq!(
        d.feed(3),
        Some(Packet { dx: 5, dy: -3, wheel: 0, buttons: 1 })
    );
    // Left 2 (sign bit 4), down 4 (wire Y -4: sign bit 5), right + middle.
    assert_eq!(d.feed(0x08 | 0x10 | 0x20 | 0x02 | 0x04), None);
    assert_eq!(d.feed(0xFE), None);
    assert_eq!(d.feed(0xFC), Some(Packet { dx: -2, dy: 4, wheel: 0, buttons: 6 }));
}

#[test]
fn ps2_decoder_resyncs_and_drops_overflow() {
    use input::ps2::*;
    let mut d = Decoder::new();
    assert_eq!(d.feed(0x00), None); // not a first byte: skipped
    assert_eq!(d.feed(0x00), None);
    assert_eq!(d.feed(0x08), None);
    assert_eq!(d.feed(1), None);
    assert_eq!(d.feed(1), Some(Packet { dx: 1, dy: -1, wheel: 0, buttons: 0 }));
    // Overflow bits: the whole packet is discarded.
    assert_eq!(d.feed(0x08 | 0x40), None);
    assert_eq!(d.feed(1), None);
    assert_eq!(d.feed(1), None);
    d.feed(0x08);
    d.reset();
    assert_eq!(d.feed(0x08), None, "a reset abandons a half packet");
}

#[test]
fn ps2_decoder_wheel_packets() {
    use input::ps2::*;
    let mut d = Decoder::new();
    d.len = 4;
    assert_eq!(d.feed(0x08), None);
    assert_eq!(d.feed(0), None);
    assert_eq!(d.feed(0), None);
    // Z = -1 (0xF): wheel up on a PS/2 wire, REL_WHEEL +1.
    assert_eq!(d.feed(0x0F), Some(Packet { dx: 0, dy: 0, wheel: 1, buttons: 0 }));
    d.feed(0x08);
    d.feed(0);
    d.feed(0);
    assert_eq!(d.feed(0x01), Some(Packet { dx: 0, dy: 0, wheel: -1, buttons: 0 }));
}

#[test]
fn mouse_packets_become_rel_and_button_events() {
    let f = Rc::new(input::open(DEV_MOUSE).unwrap());
    input::mouse(5, -3, 1, 0b001);
    input::mouse(0, 0, 0, 0b001); // nothing changed: just a SYN-less no-op?
    input::mouse(0, 0, 0, 0);
    let got = triples(&drain(&f));
    assert_eq!(
        &got[..6],
        &[
            ev(EV_REL, REL_X, 5),
            ev(EV_REL, REL_Y, -3),
            ev(EV_REL, REL_WHEEL, 1),
            ev(EV_KEY, BTN_LEFT, 1),
            ev(EV_SYN, SYN_REPORT, 0),
            ev(EV_SYN, SYN_REPORT, 0),
        ]
    );
    assert!(got.contains(&ev(EV_KEY, BTN_LEFT, 0)), "release reported");
    // The mouse queue is separate from the keyboard's.
    let k = Rc::new(input::open(DEV_KBD).unwrap());
    assert!(drain(&k).is_empty());
}

// ==================================================================== VT


fn vt_reset() {
    SIGNALS.with(|s| s.borrow_mut().clear());
    ALIVE.with(|c| c.set(true));
    PID.with(|c| c.set(100));
    KBMODE.with(|c| c.set(3));
    TAKEOVER.with(|c| c.set(0));
    TAKEOVER_UNMASTERED.with(|c| c.set(0));
}

fn vt_int(vtn: u8, req: u64) -> (i64, i32) {
    let a = umem_alloc(4);
    let r = vt::ioctl(vtn, req, a);
    (r, i32::from_le_bytes(umem_read(a, 4).try_into().unwrap()))
}

fn setmode(vtn: u8, mode: u8, rel: i16, acq: i16) -> i64 {
    let a = umem_alloc(8);
    let mut b = [0u8; 8];
    b[0] = mode;
    b[2..4].copy_from_slice(&rel.to_le_bytes());
    b[4..6].copy_from_slice(&acq.to_le_bytes());
    umem_write(a, &b);
    vt::ioctl(vtn, vt::VT_SETMODE, a)
}

fn getstate() -> (u16, u16) {
    let a = umem_alloc(6);
    assert_eq!(vt::ioctl(0, vt::VT_GETSTATE, a), 0);
    let b = umem_read(a, 6);
    (u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[4], b[5]]))
}

#[test]
fn vt_request_numbers() {
    assert_eq!(vt::VT_OPENQRY, 0x5600);
    assert_eq!(vt::VT_SETMODE, 0x5602);
    assert_eq!(vt::VT_GETSTATE, 0x5603);
    assert_eq!(vt::VT_RELDISP, 0x5605);
    assert_eq!(vt::VT_ACTIVATE, 0x5606);
    assert_eq!(vt::VT_WAITACTIVE, 0x5607);
    assert_eq!(vt::KDSETMODE, 0x4B3A);
    assert_eq!(vt::KDGETMODE, 0x4B3B);
    assert!(vt::is_vt_request(0x5606) && !vt::is_vt_request(0x5401));
}

#[test]
fn x_server_vt_session() {
    // The sequence xf86OpenConsole / xf86CloseConsole make.
    vt_reset();
    // open /dev/tty0, VT_OPENQRY: first VT nobody has open.
    assert_eq!(vt_int(0, vt::VT_OPENQRY), (0, 1));
    let h1 = vt::open(1).unwrap(); // the text console's own getty holds VT1
    assert_eq!(vt_int(0, vt::VT_OPENQRY), (0, 2));
    let x = vt::open(2).unwrap(); // X opens /dev/tty2
    assert_eq!(x.num(), 2);
    assert_eq!(vt_int(0, vt::VT_OPENQRY), (0, 3));
    // VT_GETSTATE: active 1, VT0 + VT1 + VT2 in the mask.
    assert_eq!(getstate(), (1, 0b111));
    // Switch to our VT, wait, VT_SETMODE(PROCESS, SIGUSR1/SIGUSR1), graphics + raw off.
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 2), 0);
    assert_eq!(vt::active(), 2);
    assert_eq!(vt::ioctl(0, vt::VT_WAITACTIVE, 2), 0);
    assert_eq!(setmode(2, vt::VT_PROCESS, 10, 10), 0);
    assert_eq!(vt::ioctl(2, vt::KDSETMODE, vt::KD_GRAPHICS), 0);
    assert_eq!(vt_int(2, vt::KDGETMODE), (0, 1));
    KBMODE.with(|c| c.set(4)); // KDSKBMODE K_OFF
    // The user presses Ctrl-Alt-F1: the kernel asks X to release VT2.
    SIGNALS.with(|s| s.borrow_mut().clear());
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 1), 0);
    assert_eq!(vt::active(), 2, "not switched until X agrees");
    assert_eq!(SIGNALS.with(|s| s.borrow().clone()), vec![(100, 10)]);
    // X: VT_RELDISP(1).
    assert_eq!(vt::ioctl(2, vt::VT_RELDISP, 1), 0);
    assert_eq!(vt::active(), 1);
    assert_eq!(KBMODE.with(|c| c.get()), 3, "the console gets its own keyboard mode back");
    assert!(TAKEOVER.with(|c| c.get()) >= 1, "the text VT takes the screen");
    // Back to X: acqsig, K_OFF restored, no takeover (graphics VT).
    TAKEOVER.with(|c| c.set(0));
    SIGNALS.with(|s| s.borrow_mut().clear());
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 2), 0);
    assert_eq!(vt::active(), 2);
    assert_eq!(SIGNALS.with(|s| s.borrow().clone()), vec![(100, 10)], "acquire signal");
    assert_eq!(KBMODE.with(|c| c.get()), 4);
    assert_eq!(TAKEOVER.with(|c| c.get()), 0);
    assert_eq!(vt::ioctl(2, vt::VT_RELDISP, vt::VT_ACKACQ as u64), 0);
    assert_eq!(vt::ioctl(2, vt::VT_RELDISP, 0), -22, "no switch pending, not an ACKACQ");
    // Exit: KD_TEXT, VT_AUTO, back to VT1.
    assert_eq!(vt::ioctl(2, vt::KDSETMODE, vt::KD_TEXT), 0);
    assert_eq!(TAKEOVER_UNMASTERED.with(|c| c.get()), 1);
    assert_eq!(setmode(2, vt::VT_AUTO, 0, 0), 0);
    KBMODE.with(|c| c.set(3));
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 1), 0);
    assert_eq!(vt::active(), 1);
    drop(x);
    drop(h1);
}

#[test]
fn release_can_be_refused() {
    vt_reset();
    let _h = vt::open(1).unwrap();
    let x = vt::open(3).unwrap();
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 3), 0);
    assert_eq!(setmode(3, vt::VT_PROCESS, 12, 12), 0);
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 1), 0);
    assert_eq!(vt::active(), 3);
    assert_eq!(vt::ioctl(3, vt::VT_RELDISP, 0), 0, "VT_RELDISP(0) = stay");
    assert_eq!(vt::active(), 3);
    // After a refusal a second request starts over.
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 1), 0);
    assert_eq!(vt::ioctl(3, vt::VT_RELDISP, 1), 0);
    assert_eq!(vt::active(), 1);
    drop(x);
    assert_eq!(vt::active(), 1);
}

#[test]
fn a_dead_owner_cannot_hold_the_vt() {
    vt_reset();
    let x = vt::open(4).unwrap();
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 4), 0);
    assert_eq!(setmode(4, vt::VT_PROCESS, 10, 10), 0);
    ALIVE.with(|c| c.set(false)); // X crashed: no process to signal
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 1), 0);
    assert_eq!(vt::active(), 1, "the kernel just takes the VT");
    assert_eq!(setmode_get(4), vt::VT_AUTO);
    drop(x);
}

fn setmode_get(vtn: u8) -> u8 {
    let a = umem_alloc(8);
    assert_eq!(vt::ioctl(vtn, vt::VT_GETMODE, a), 0);
    umem_read(a, 8)[0]
}

#[test]
fn last_close_resets_the_vt() {
    vt_reset();
    let x = vt::open(5).unwrap();
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 5), 0);
    assert_eq!(setmode(5, vt::VT_PROCESS, 10, 10), 0);
    assert_eq!(vt::ioctl(5, vt::KDSETMODE, vt::KD_GRAPHICS), 0);
    KBMODE.with(|c| c.set(4));
    TAKEOVER_UNMASTERED.with(|c| c.set(0));
    drop(x); // X died with the VT in graphics + process mode
    assert_eq!(vt_int(5, vt::KDGETMODE).1, 0, "back to text");
    assert_eq!(setmode_get(5), vt::VT_AUTO);
    assert_eq!(KBMODE.with(|c| c.get()), 3, "keyboard usable again");
    assert_eq!(TAKEOVER_UNMASTERED.with(|c| c.get()), 1, "screen returned to the console");
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 1), 0);
}

#[test]
fn vt_argument_validation() {
    vt_reset();
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 0), -6, "ENXIO");
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 7), -6);
    assert_eq!(vt::ioctl(0, vt::VT_WAITACTIVE, 0), -6);
    assert_eq!(setmode(1, 2, 0, 0), -22, "only AUTO / PROCESS");
    assert_eq!(setmode(1, 9, 0, 0), -22);
    assert_eq!(vt::ioctl(1, vt::KDSETMODE, 7), -22);
    assert_eq!(vt::ioctl(1, vt::VT_RELDISP, 1), -22, "not in process mode");
    assert_eq!(vt::ioctl(1, 0x1234, 0), -25, "ENOTTY");
    assert_eq!(vt::ioctl(0, vt::VT_GETSTATE, 0xdead_0000), -14, "EFAULT");
    // VT_DISALLOCATE: the active VT is busy, a free one is fine.
    assert_eq!(vt::ioctl(0, vt::VT_DISALLOCATE, 1), -16);
    assert_eq!(vt::ioctl(0, vt::VT_DISALLOCATE, 6), 0);
    assert_eq!(vt::ioctl(0, vt::VT_DISALLOCATE, 9), -6);
    // VT_LOCKSWITCH freezes the switch silently, UNLOCK frees it.
    assert_eq!(vt::ioctl(0, vt::VT_LOCKSWITCH, 0), 0);
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 2), 0);
    assert_eq!(vt::active(), 1);
    assert_eq!(vt::ioctl(0, vt::VT_UNLOCKSWITCH, 0), 0);
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 2), 0);
    assert_eq!(vt::active(), 2);
    assert_eq!(vt::ioctl(0, vt::VT_ACTIVATE, 1), 0);
}

#[test]
fn vt_getmode_roundtrip_wire_layout() {
    vt_reset();
    assert_eq!(setmode(1, vt::VT_PROCESS, 0x0A0B, 0x0C0D), 0);
    let a = umem_alloc(8);
    assert_eq!(vt::ioctl(1, vt::VT_GETMODE, a), 0);
    let b = umem_read(a, 8);
    assert_eq!(b[0], 1);
    assert_eq!(i16::from_le_bytes([b[2], b[3]]), 0x0A0B);
    assert_eq!(i16::from_le_bytes([b[4], b[5]]), 0x0C0D);
    assert_eq!(setmode(1, vt::VT_AUTO, 0, 0), 0);
}
