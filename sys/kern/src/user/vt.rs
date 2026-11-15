// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Virtual terminals: the VT_* / KDSETMODE ioctls a display server drives on
// /dev/tty0 and /dev/ttyN (<linux/vt.h>, <linux/kd.h>), with Linux's
// semantics:
//
//   * six VTs, VT1 is the kernel text console; /dev/tty0 means "the active
//     one", /dev/ttyN is VT N;
//   * VT_OPENQRY finds a VT nobody has open; the last close of a VT resets
//     it (text mode, VT_AUTO, default keyboard mode) -- the safety net that
//     un-wedges a console after a crashed X server;
//   * VT_SETMODE(VT_PROCESS) makes the caller the VT's owner: a switch away
//     sends it `relsig` and waits for VT_RELDISP(1) (or cancels on
//     VT_RELDISP(0)); a switch to the VT sends `acqsig`;
//   * KDSETMODE(KD_GRAPHICS) marks the VT as drawn by a client;
//   * the keyboard mode (KDSKBMODE) is per VT: it is saved when a VT is left
//     and restored when it is entered, so X's K_OFF does not leak to VT1.
//
// There is one text console (one back buffer): switching to a text VT hands
// the screen back to it (drm::console_takeover), switching to a graphics VT
// leaves the screen to whoever draws there (the DRM client takes it again
// with its next modeset, as X's EnterVT does).

use alloc::rc::Rc;

use crate::user::fd::{copy_in, copy_out};

pub const NR_VT: usize = 6;

pub const VT_OPENQRY: u64 = 0x5600;
pub const VT_GETMODE: u64 = 0x5601;
pub const VT_SETMODE: u64 = 0x5602;
pub const VT_GETSTATE: u64 = 0x5603;
pub const VT_SENDSIG: u64 = 0x5604;
pub const VT_RELDISP: u64 = 0x5605;
pub const VT_ACTIVATE: u64 = 0x5606;
pub const VT_WAITACTIVE: u64 = 0x5607;
pub const VT_DISALLOCATE: u64 = 0x5608;
pub const VT_RESIZE: u64 = 0x5609;
pub const VT_RESIZEX: u64 = 0x560A;
pub const VT_LOCKSWITCH: u64 = 0x560B;
pub const VT_UNLOCKSWITCH: u64 = 0x560C;
pub const VT_GETHIFONTMASK: u64 = 0x560D;
pub const KDGETMODE: u64 = 0x4B3B;
pub const KDSETMODE: u64 = 0x4B3A;

pub const VT_AUTO: u8 = 0;
pub const VT_PROCESS: u8 = 1;
pub const VT_ACKACQ: u8 = 2;
pub const KD_TEXT: u64 = 0;
pub const KD_GRAPHICS: u64 = 1;
pub const KD_TEXT0: u64 = 0x10;
pub const KD_TEXT1: u64 = 0x11;

const ENXIO: i64 = 6;
const EINTR: i64 = 4;
const EFAULT: i64 = 14;
const EBUSY: i64 = 16;
const EINVAL: i64 = 22;
const ENOTTY: i64 = 25;

#[derive(Clone, Copy)]
struct Vt {
    opens: u32,
    graphics: bool,
    mode: u8,
    waitv: u8,
    relsig: i16,
    acqsig: i16,
    frsig: i16,
    /// Process that issued VT_SETMODE(VT_PROCESS); 0 = none.
    owner: u64,
    /// KDSKBMODE saved while the VT is not active (0xFF = never set).
    kbmode: u8,
}

const NEW_VT: Vt = Vt {
    opens: 0,
    graphics: false,
    mode: VT_AUTO,
    waitv: 0,
    relsig: 0,
    acqsig: 0,
    frsig: 0,
    owner: 0,
    kbmode: 0xFF,
};

struct State {
    active: u8,
    /// A switch waiting for the active VT's owner to say VT_RELDISP.
    pending: Option<u8>,
    locked: bool,
    vts: [Vt; NR_VT + 1], // index 0 unused: VT numbers are 1-based
    /// The keyboard mode a VT gets that never set one.
    default_kbmode: u8,
}

static mut STATE: State = State {
    active: 1,
    pending: None,
    locked: false,
    vts: [NEW_VT; NR_VT + 1],
    default_kbmode: crate::hw::keymap::K_UNICODE,
};

fn st() -> &'static mut State {
    // SAFETY: process context only, and every mutation below is short and
    // done with interrupts masked where it races the signal path.
    unsafe { &mut *core::ptr::addr_of_mut!(STATE) }
}

/// The active VT number (1..=6).
pub fn active() -> u8 {
    st().active
}

/// One open of /dev/ttyN: keeps the VT "in use" until it is dropped.
pub struct VtHandle {
    n: u8,
}

impl VtHandle {
    pub fn num(&self) -> u8 {
        self.n
    }
}

/// Open VT `n` (1..=6).
pub fn open(n: u8) -> Option<Rc<VtHandle>> {
    if n == 0 || n as usize > NR_VT {
        return None;
    }
    st().vts[n as usize].opens += 1;
    Some(Rc::new(VtHandle { n }))
}

impl Drop for VtHandle {
    fn drop(&mut self) {
        let s = st();
        let v = &mut s.vts[self.n as usize];
        v.opens = v.opens.saturating_sub(1);
        if v.opens > 0 {
            return;
        }
        // Last close: the VT goes back to a plain text console (Linux's
        // reset_vc on disallocate).
        let was_graphics = v.graphics;
        let kb = s.default_kbmode;
        *v = NEW_VT;
        if s.pending.is_some() && s.active == self.n {
            s.pending = None;
        }
        if s.active == self.n {
            let _ = crate::hw::kbd::kd_set_mode(kb);
            if was_graphics {
                crate::drm::console_takeover_unmastered();
            }
        }
    }
}

fn pid_now() -> u64 {
    crate::user::process::current().map_or(0, |p| p.pid)
}

/// Make `target` the active VT now (the release was granted or not needed).
fn do_switch(target: u8) {
    let s = st();
    let old = s.active;
    if old == target {
        return;
    }
    // Save the outgoing keyboard mode, load the incoming one.
    let cur = crate::hw::kbd::kd_get_mode();
    s.vts[old as usize].kbmode = cur;
    if s.vts[old as usize].kbmode != crate::hw::keymap::K_OFF
        && s.vts[old as usize].kbmode != crate::hw::keymap::K_RAW
        && !s.vts[old as usize].graphics
    {
        s.default_kbmode = cur;
    }
    s.active = target;
    s.pending = None;
    let kb = match s.vts[target as usize].kbmode {
        0xFF => s.default_kbmode,
        m => m,
    };
    let _ = crate::hw::kbd::kd_set_mode(kb);
    let v = s.vts[target as usize];
    if !v.graphics {
        crate::drm::console_takeover();
    }
    if v.mode == VT_PROCESS && v.owner != 0 {
        let _ = crate::user::process::post_signal(v.owner, v.acqsig as u64);
    }
}

/// Start a switch to `target` (VT_ACTIVATE).
fn begin_switch(target: u8) {
    let s = st();
    if s.locked || target == s.active {
        return;
    }
    let cur = s.vts[s.active as usize];
    if cur.mode == VT_PROCESS && cur.owner != 0 {
        // Ask the owner to let go; the switch completes at VT_RELDISP(1).
        if crate::user::process::post_signal(cur.owner, cur.relsig as u64) {
            s.pending = Some(target);
            return;
        }
        // The owner is gone or never installed a handler: take the VT.
        s.vts[s.active as usize].mode = VT_AUTO;
        s.vts[s.active as usize].owner = 0;
    }
    do_switch(target);
}

fn put(arg: u64, data: &[u8]) -> i64 {
    if arg != 0 && copy_out(arg, 0, data) {
        0
    } else {
        -EFAULT
    }
}

/// Is `req` one of ours?  (So `sys_ioctl` can tell -ENOTTY from "handled".)
pub fn is_vt_request(req: u64) -> bool {
    matches!(
        req,
        VT_OPENQRY
            | VT_GETMODE
            | VT_SETMODE
            | VT_GETSTATE
            | VT_SENDSIG
            | VT_RELDISP
            | VT_ACTIVATE
            | VT_WAITACTIVE
            | VT_DISALLOCATE
            | VT_RESIZE
            | VT_RESIZEX
            | VT_LOCKSWITCH
            | VT_UNLOCKSWITCH
            | VT_GETHIFONTMASK
            | KDGETMODE
            | KDSETMODE
    )
}

/// Run a VT / KD-mode ioctl.  `vt` is the VT of the descriptor (0 for
/// /dev/tty0 and /dev/console = the active one).
pub fn ioctl(vt: u8, req: u64, arg: u64) -> i64 {
    let me = if vt == 0 { st().active } else { vt } as usize;
    match req {
        VT_OPENQRY => {
            let s = st();
            for n in 1..=NR_VT {
                if s.vts[n].opens == 0 {
                    return put(arg, &(n as i32).to_le_bytes());
                }
            }
            put(arg, &(-1i32).to_le_bytes()) // Linux: -1 in *arg, still success
        }
        VT_GETMODE => {
            let v = st().vts[me];
            let mut o = [0u8; 8];
            o[0] = v.mode;
            o[1] = v.waitv;
            o[2..4].copy_from_slice(&v.relsig.to_le_bytes());
            o[4..6].copy_from_slice(&v.acqsig.to_le_bytes());
            o[6..8].copy_from_slice(&v.frsig.to_le_bytes());
            put(arg, &o)
        }
        VT_SETMODE => {
            let mut i = [0u8; 8];
            if !copy_in(arg, 0, &mut i) {
                return -EFAULT;
            }
            if i[0] != VT_AUTO && i[0] != VT_PROCESS {
                return -EINVAL;
            }
            let s = st();
            let v = &mut s.vts[me];
            v.mode = i[0];
            v.waitv = i[1];
            v.relsig = i16::from_le_bytes([i[2], i[3]]);
            v.acqsig = i16::from_le_bytes([i[4], i[5]]);
            v.frsig = i16::from_le_bytes([i[6], i[7]]);
            v.owner = if i[0] == VT_PROCESS { pid_now() } else { 0 };
            if s.pending.is_some() && s.active as usize == me {
                s.pending = None;
            }
            0
        }
        VT_GETSTATE => {
            let s = st();
            let mut mask: u16 = 1; // VT0 (the "current" alias) is always there
            for n in 1..=NR_VT {
                if s.vts[n].opens > 0 || n as u8 == s.active {
                    mask |= 1 << n;
                }
            }
            let mut o = [0u8; 6];
            o[0..2].copy_from_slice(&(s.active as u16).to_le_bytes());
            o[2..4].copy_from_slice(&0u16.to_le_bytes()); // v_signal: obsolete
            o[4..6].copy_from_slice(&mask.to_le_bytes());
            put(arg, &o)
        }
        VT_RELDISP => {
            let s = st();
            if s.vts[me].mode != VT_PROCESS {
                return -EINVAL;
            }
            match s.pending {
                Some(target) if s.active as usize == me => {
                    s.pending = None;
                    if arg != 0 {
                        do_switch(target);
                    }
                    0
                }
                // "Switched to" response: only VT_ACKACQ is expected.
                _ => {
                    if arg == VT_ACKACQ as u64 {
                        0
                    } else {
                        -EINVAL
                    }
                }
            }
        }
        VT_ACTIVATE => {
            if arg == 0 || arg as usize > NR_VT {
                return -ENXIO;
            }
            begin_switch(arg as u8);
            0
        }
        VT_WAITACTIVE => {
            if arg == 0 || arg as usize > NR_VT {
                return -ENXIO;
            }
            loop {
                if st().active as u64 == arg {
                    return 0;
                }
                if crate::user::signal::interrupt_pending() {
                    return -EINTR;
                }
                crate::user::process::sleep_current(10);
            }
        }
        VT_DISALLOCATE => {
            let s = st();
            if arg == 0 {
                return 0; // free every unused VT: they are static here
            }
            if arg as usize > NR_VT {
                return -ENXIO;
            }
            if s.vts[arg as usize].opens > 0 || s.active as u64 == arg {
                return -EBUSY;
            }
            s.vts[arg as usize] = NEW_VT;
            0
        }
        VT_LOCKSWITCH => {
            st().locked = true;
            0
        }
        VT_UNLOCKSWITCH => {
            st().locked = false;
            0
        }
        VT_SENDSIG | VT_RESIZE | VT_RESIZEX | VT_GETHIFONTMASK => {
            if req == VT_GETHIFONTMASK {
                put(arg, &0u16.to_le_bytes())
            } else {
                0
            }
        }
        KDGETMODE => {
            let g = st().vts[me].graphics;
            put(arg, &(g as i32).to_le_bytes())
        }
        KDSETMODE => {
            let s = st();
            match arg {
                KD_GRAPHICS => {
                    s.vts[me].graphics = true;
                    0
                }
                KD_TEXT | KD_TEXT0 | KD_TEXT1 => {
                    s.vts[me].graphics = false;
                    if s.active as usize == me {
                        crate::drm::console_takeover_unmastered();
                    }
                    0
                }
                _ => -EINVAL,
            }
        }
        _ => -ENOTTY,
    }
}
