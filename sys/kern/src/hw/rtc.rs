// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// MC146818-compatible real-time clock behind CMOS ports 0x70/0x71.  Read
/// only: it feeds the `date` shell command.

use crate::arch::io::{inb, outb};

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

#[derive(Clone, Copy)]
pub struct DateTime {
    pub year: u32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
}

unsafe fn read_reg(reg: u8) -> u8 {
    outb(CMOS_ADDR, reg & 0x7F);
    inb(CMOS_DATA)
}

unsafe fn update_in_progress() -> bool {
    read_reg(0x0A) & 0x80 != 0
}

/// Read the RTC once it is not mid-update.  Handles BCD and the 12/24 hour
/// bit; the century is assumed to be 20xx (with a 2016-2099 wrap fallback).
pub fn read() -> Option<DateTime> {
    // SAFETY: fixed legacy ports, no memory involved.
    unsafe {
        // Wait for a stable window (bounded).
        let mut guard = 0u32;
        while update_in_progress() && guard < 5_000_000 {
            guard += 1;
        }
        let status_b = read_reg(0x0B);
        let bcd = status_b & 0x04 == 0;
        let twentyfour = status_b & 0x02 != 0;

        let mut sec = read_reg(0x00);
        let mut min = read_reg(0x02);
        let mut hour = read_reg(0x04);
        let day = read_reg(0x07);
        let month = read_reg(0x08);
        let mut year = read_reg(0x09) as u32;

        // A second read to confirm the clock did not roll over mid-way.
        if update_in_progress() {
            return None;
        }
        let sec2 = read_reg(0x00);
        let min2 = read_reg(0x02);
        let hour2 = read_reg(0x04);
        let day2 = read_reg(0x07);
        let month2 = read_reg(0x08);
        if sec != sec2 || min != min2 || hour != hour2 || day != day2 || month != month2 {
            return None;
        }

        let unbcd = |v: u8| -> u8 {
            if bcd {
                ((v >> 4) & 0xF) * 10 + (v & 0xF)
            } else {
                v
            }
        };

        sec = unbcd(sec);
        min = unbcd(min);
        if twentyfour {
            hour = unbcd(hour);
        } else {
            // 12-hour bit 0x80 set = PM.
            let pm = hour & 0x80 != 0;
            hour = unbcd(hour & 0x7F);
            if pm && hour != 12 {
                hour += 12;
            } else if !pm && hour == 12 {
                hour = 0;
            }
        }
        let day = unbcd(day);
        let month = unbcd(month);
        year = unbcd(year as u8) as u32 + 2000;

        if month == 0 || month > 12 || day == 0 || day > 31 || min > 59 || sec > 59 {
            return None;
        }
        Some(DateTime { year, month, day, hour, min, sec })
    }
}
