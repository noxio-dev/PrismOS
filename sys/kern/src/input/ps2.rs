// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// PS/2 mouse packet decoder (the ImPS/2 "wheel mouse" protocol): the part of
// the mouse driver that has no hardware in it.  Bytes arrive one at a time
// from the i8042's aux port; a packet is 3 bytes (plain PS/2) or 4 (device id
// 3, with a wheel).
//
//   byte 0: Yo Xo Ys Xs 1 M R L     (overflow, sign bits, always-1, buttons)
//   byte 1: X movement (low 8 bits, sign in byte 0)
//   byte 2: Y movement (same; positive is *up* on the wire)
//   byte 3: Z (wheel), signed 4 bits, only for id 3 / 4
//
// Resync follows psmouse: bit 3 of byte 0 must be set; a byte 0 without it is
// dropped, and a packet that stalls for more than a few ms (a lost byte) is
// abandoned by the caller via `reset()`.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Packet {
    pub dx: i32,
    pub dy: i32, // already flipped: positive = down, like REL_Y
    pub wheel: i32, // positive = up (REL_WHEEL)
    pub buttons: u8, // bit0 left, bit1 right, bit2 middle
}

pub struct Decoder {
    buf: [u8; 4],
    n: usize,
    /// Packet length: 3, or 4 once the device reported a wheel.
    pub len: usize,
}

impl Decoder {
    pub const fn new() -> Decoder {
        Decoder { buf: [0; 4], n: 0, len: 3 }
    }

    pub fn reset(&mut self) {
        self.n = 0;
    }

    /// Feed one byte; a finished packet comes back.
    pub fn feed(&mut self, b: u8) -> Option<Packet> {
        if self.n == 0 && b & 0x08 == 0 {
            return None; // out of sync: not a first byte
        }
        self.buf[self.n] = b;
        self.n += 1;
        if self.n < self.len {
            return None;
        }
        self.n = 0;
        let b0 = self.buf[0];
        if b0 & 0xC0 != 0 {
            return None; // overflow: psmouse discards the packet too
        }
        let dx = self.buf[1] as i32 - (((b0 as i32) << 4) & 0x100);
        let dy = self.buf[2] as i32 - (((b0 as i32) << 3) & 0x100);
        let wheel = if self.len == 4 {
            // Low nibble, two's complement: -8..7.  Positive = towards the
            // user = scroll *down*; REL_WHEEL is positive up.
            let z = (self.buf[3] & 0x0F) as i32;
            -(if z >= 8 { z - 16 } else { z })
        } else {
            0
        };
        Some(Packet { dx, dy: -dy, wheel, buttons: b0 & 7 })
    }
}
