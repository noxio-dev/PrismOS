// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Mode setting.  One CRTC, one primary plane, one encoder, one connector,
// one mode; every ioctl of <drm/drm_mode.h> that libdrm / Xorg / Mesa issue
// against such a device lives here.
//
// The legacy entry points (SETCRTC, PAGE_FLIP, SETPLANE, DIRTYFB, DPMS) and
// ATOMIC all funnel through the same two functions:
//
//   check_state()   validate a proposed (plane, crtc, connector) triple the
//                   way atomic_check does, with Linux's errno per violation;
//   commit_state()  install it and report what must be put on screen.
//
// so the device cannot end up in a state one API could create and the other
// could not describe.
//
// Wire conventions kept from Linux:
//   * array arguments are two-pass: the kernel always writes back the real
//     count, and copies the elements only when the caller's count is large
//     enough (libdrm calls once with 0 to size its buffers);
//   * object ids are never 0; unknown ids are -ENOENT;
//   * legacy clients do not see atomic-only properties or the primary
//     plane until they enable DRM_CLIENT_CAP_ATOMIC / UNIVERSAL_PLANES.

use alloc::rc::Rc;
use alloc::vec::Vec;

use super::scanout::{self, Blit, Native};
use super::*;

// ------------------------------------------------------------ properties

#[derive(Clone, Copy)]
pub enum PropKind {
    Range(u64, u64),
    SignedRange(i64, i64),
    Enum(&'static [(u64, &'static str)]),
    Blob,
    /// Object reference; the payload is the object type it must name.
    Object(u32),
}

pub struct PropDef {
    pub id: u32,
    pub name: &'static str,
    pub flags: u32,
    pub kind: PropKind,
}

pub const P_DPMS: u32 = 1;
pub const P_EDID: u32 = 2;
pub const P_NON_DESKTOP: u32 = 3;
pub const P_CONN_CRTC_ID: u32 = 4;
pub const P_PLANE_TYPE: u32 = 5;
pub const P_FB_ID: u32 = 6;
pub const P_PLANE_CRTC_ID: u32 = 7;
pub const P_CRTC_X: u32 = 8;
pub const P_CRTC_Y: u32 = 9;
pub const P_CRTC_W: u32 = 10;
pub const P_CRTC_H: u32 = 11;
pub const P_SRC_X: u32 = 12;
pub const P_SRC_Y: u32 = 13;
pub const P_SRC_W: u32 = 14;
pub const P_SRC_H: u32 = 15;
pub const P_ACTIVE: u32 = 16;
pub const P_MODE_ID: u32 = 17;

static DPMS_ENUM: [(u64, &str); 4] = [(0, "On"), (1, "Standby"), (2, "Suspend"), (3, "Off")];
static PLANE_TYPE_ENUM: [(u64, &str); 3] = [(0, "Overlay"), (1, "Primary"), (2, "Cursor")];

pub static PROPS: [PropDef; 17] = [
    PropDef { id: P_DPMS, name: "DPMS", flags: PROP_ENUM, kind: PropKind::Enum(&DPMS_ENUM) },
    PropDef { id: P_EDID, name: "EDID", flags: PROP_BLOB | PROP_IMMUTABLE, kind: PropKind::Blob },
    PropDef {
        id: P_NON_DESKTOP,
        name: "non-desktop",
        flags: PROP_RANGE | PROP_IMMUTABLE,
        kind: PropKind::Range(0, 1),
    },
    PropDef {
        id: P_CONN_CRTC_ID,
        name: "CRTC_ID",
        flags: PROP_ATOMIC | PROP_OBJECT,
        kind: PropKind::Object(OBJECT_CRTC),
    },
    PropDef {
        id: P_PLANE_TYPE,
        name: "type",
        flags: PROP_ENUM | PROP_IMMUTABLE,
        kind: PropKind::Enum(&PLANE_TYPE_ENUM),
    },
    PropDef { id: P_FB_ID, name: "FB_ID", flags: PROP_ATOMIC | PROP_OBJECT, kind: PropKind::Object(OBJECT_FB) },
    PropDef {
        id: P_PLANE_CRTC_ID,
        name: "CRTC_ID",
        flags: PROP_ATOMIC | PROP_OBJECT,
        kind: PropKind::Object(OBJECT_CRTC),
    },
    PropDef {
        id: P_CRTC_X,
        name: "CRTC_X",
        flags: PROP_ATOMIC | PROP_SIGNED_RANGE,
        kind: PropKind::SignedRange(i32::MIN as i64, i32::MAX as i64),
    },
    PropDef {
        id: P_CRTC_Y,
        name: "CRTC_Y",
        flags: PROP_ATOMIC | PROP_SIGNED_RANGE,
        kind: PropKind::SignedRange(i32::MIN as i64, i32::MAX as i64),
    },
    PropDef {
        id: P_CRTC_W,
        name: "CRTC_W",
        flags: PROP_ATOMIC | PROP_RANGE,
        kind: PropKind::Range(0, i32::MAX as u64),
    },
    PropDef {
        id: P_CRTC_H,
        name: "CRTC_H",
        flags: PROP_ATOMIC | PROP_RANGE,
        kind: PropKind::Range(0, i32::MAX as u64),
    },
    PropDef {
        id: P_SRC_X,
        name: "SRC_X",
        flags: PROP_ATOMIC | PROP_RANGE,
        kind: PropKind::Range(0, u32::MAX as u64),
    },
    PropDef {
        id: P_SRC_Y,
        name: "SRC_Y",
        flags: PROP_ATOMIC | PROP_RANGE,
        kind: PropKind::Range(0, u32::MAX as u64),
    },
    PropDef {
        id: P_SRC_W,
        name: "SRC_W",
        flags: PROP_ATOMIC | PROP_RANGE,
        kind: PropKind::Range(0, u32::MAX as u64),
    },
    PropDef {
        id: P_SRC_H,
        name: "SRC_H",
        flags: PROP_ATOMIC | PROP_RANGE,
        kind: PropKind::Range(0, u32::MAX as u64),
    },
    PropDef { id: P_ACTIVE, name: "ACTIVE", flags: PROP_ATOMIC | PROP_RANGE, kind: PropKind::Range(0, 1) },
    PropDef { id: P_MODE_ID, name: "MODE_ID", flags: PROP_ATOMIC | PROP_BLOB, kind: PropKind::Blob },
];

const CONNECTOR_PROPS: &[u32] = &[P_DPMS, P_EDID, P_NON_DESKTOP, P_CONN_CRTC_ID];
const PLANE_PROPS: &[u32] = &[
    P_PLANE_TYPE, P_FB_ID, P_PLANE_CRTC_ID, P_CRTC_X, P_CRTC_Y, P_CRTC_W, P_CRTC_H, P_SRC_X, P_SRC_Y, P_SRC_W,
    P_SRC_H,
];
const CRTC_PROPS: &[u32] = &[P_ACTIVE, P_MODE_ID];

pub fn prop_def(id: u32) -> Option<&'static PropDef> {
    PROPS.iter().find(|p| p.id == id)
}

/// The property ids an object carries, in listing order.
fn object_props(obj_type: u32) -> Option<&'static [u32]> {
    match obj_type {
        OBJECT_CONNECTOR => Some(CONNECTOR_PROPS),
        OBJECT_PLANE => Some(PLANE_PROPS),
        OBJECT_CRTC => Some(CRTC_PROPS),
        OBJECT_ENCODER => Some(&[]),
        _ => None,
    }
}

fn object_id_of(obj_type: u32) -> Option<u32> {
    match obj_type {
        OBJECT_CONNECTOR => Some(CONNECTOR_ID),
        OBJECT_PLANE => Some(PLANE_ID),
        OBJECT_CRTC => Some(CRTC_ID),
        OBJECT_ENCODER => Some(ENCODER_ID),
        _ => None,
    }
}

/// Current value of property `prop` (of the one object that carries it).
fn prop_value(d: &Device, prop: u32) -> u64 {
    match prop {
        P_DPMS => d.conn.dpms,
        P_EDID | P_NON_DESKTOP => 0,
        P_CONN_CRTC_ID => d.conn.crtc_id as u64,
        P_PLANE_TYPE => PLANE_TYPE_PRIMARY,
        P_FB_ID => d.plane.fb_id as u64,
        P_PLANE_CRTC_ID => d.plane.crtc_id as u64,
        P_CRTC_X => d.plane.crtc_x as i64 as u64,
        P_CRTC_Y => d.plane.crtc_y as i64 as u64,
        P_CRTC_W => d.plane.crtc_w as u64,
        P_CRTC_H => d.plane.crtc_h as u64,
        P_SRC_X => d.plane.src_x as u64,
        P_SRC_Y => d.plane.src_y as u64,
        P_SRC_W => d.plane.src_w as u64,
        P_SRC_H => d.plane.src_h as u64,
        P_ACTIVE => d.crtc.active as u64,
        P_MODE_ID => d.crtc.mode_blob as u64,
        _ => 0,
    }
}

/// (ids, values) of the properties of `obj_type` a client may see.
fn visible_props(d: &Device, obj_type: u32, atomic: bool) -> (Vec<u32>, Vec<u64>) {
    let mut ids = Vec::new();
    let mut vals = Vec::new();
    for &p in object_props(obj_type).unwrap_or(&[]) {
        let def = prop_def(p).unwrap();
        if !atomic && def.flags & PROP_ATOMIC != 0 {
            continue;
        }
        ids.push(p);
        vals.push(prop_value(d, p));
    }
    (ids, vals)
}

// --------------------------------------------------- two-pass array helpers

fn put_u32s(ptr: u64, have: u32, items: &[u32]) -> Result<u32, i64> {
    let n = items.len();
    if n > 0 && have as usize >= n {
        let mut v = Vec::with_capacity(n * 4);
        for x in items {
            v.extend_from_slice(&x.to_le_bytes());
        }
        if !user_write(ptr, &v) {
            return Err(-EFAULT);
        }
    }
    Ok(n as u32)
}

fn put_u64s(ptr: u64, have: u32, items: &[u64]) -> Result<u32, i64> {
    let n = items.len();
    if n > 0 && have as usize >= n {
        let mut v = Vec::with_capacity(n * 8);
        for x in items {
            v.extend_from_slice(&x.to_le_bytes());
        }
        if !user_write(ptr, &v) {
            return Err(-EFAULT);
        }
    }
    Ok(n as u32)
}

fn put_structs<T: Pod>(ptr: u64, have: u32, items: &[T]) -> Result<u32, i64> {
    let n = items.len();
    if n > 0 && have as usize >= n {
        let mut v = Vec::with_capacity(n * core::mem::size_of::<T>());
        for x in items {
            v.extend_from_slice(as_bytes(x));
        }
        if !user_write(ptr, &v) {
            return Err(-EFAULT);
        }
    }
    Ok(n as u32)
}

fn read_u32s(ptr: u64, n: usize) -> Result<Vec<u32>, i64> {
    let raw = user_read(ptr, n.checked_mul(4).ok_or(-EINVAL)?)?;
    Ok(raw.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

fn read_u64s(ptr: u64, n: usize) -> Result<Vec<u64>, i64> {
    let raw = user_read(ptr, n.checked_mul(8).ok_or(-EINVAL)?)?;
    Ok(raw
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
        .collect())
}

fn name_bytes(s: &str) -> [u8; PROP_NAME_LEN] {
    let mut n = [0u8; PROP_NAME_LEN];
    let b = s.as_bytes();
    let l = b.len().min(PROP_NAME_LEN - 1);
    n[..l].copy_from_slice(&b[..l]);
    n
}

// ----------------------------------------------------- state transitions

/// The mutable part of the device, as one value: what a commit proposes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub plane: PlaneState,
    pub crtc: CrtcState,
    pub conn: ConnState,
}

pub fn current(d: &Device) -> State {
    State { plane: d.plane, crtc: d.crtc, conn: d.conn }
}

/// The mode a blob carries, if it is a well-formed `drm_mode_modeinfo`.
fn mode_of_blob(d: &Device, id: u32) -> Option<ModeInfo> {
    let b = d.blobs.get(&id)?;
    if b.data.len() < core::mem::size_of::<ModeInfo>() {
        return None;
    }
    Some(from_bytes(&b.data))
}

fn mode_fits(d: &Device, m: &ModeInfo) -> bool {
    m.hdisplay as u32 == d.native.width() && m.vdisplay as u32 == d.native.height()
}

pub fn is_supported_format(f: u32) -> bool {
    f == FORMAT_XRGB8888 || f == FORMAT_ARGB8888
}

/// `atomic_check`: is this triple a state the device can be in?  Errno per
/// violation as the kernel's helpers answer it.
pub fn check_state(d: &Device, s: &State) -> Result<(), i64> {
    // -- connector <-> CRTC
    if s.conn.crtc_id != 0 && s.conn.crtc_id != CRTC_ID {
        return Err(-ENOENT);
    }
    if s.crtc.active && !s.crtc.mode_set {
        return Err(-EINVAL);
    }
    if s.crtc.mode_set {
        // An enabled CRTC needs a connector and a mode it can scan out.
        if s.conn.crtc_id != CRTC_ID {
            return Err(-EINVAL);
        }
        match mode_of_blob(d, s.crtc.mode_blob) {
            Some(m) if mode_fits(d, &m) => {}
            _ => return Err(-EINVAL),
        }
    } else if s.conn.crtc_id != 0 {
        return Err(-EINVAL); // connector on a disabled CRTC
    }

    // -- plane
    let p = &s.plane;
    if p.fb_id == 0 {
        if p.crtc_id != 0 {
            return Err(-EINVAL); // a CRTC without a framebuffer
        }
        return Ok(());
    }
    if p.crtc_id != CRTC_ID {
        return Err(if p.crtc_id == 0 { -EINVAL } else { -ENOENT });
    }
    if !s.crtc.mode_set {
        return Err(-EINVAL); // plane on a disabled CRTC
    }
    let Some(fb) = d.fbs.get(&p.fb_id) else { return Err(-ENOENT) };
    let (mw, mh) = (d.native.width(), d.native.height());
    // No scaling: the source is exactly the visible size (whole pixels).
    if p.src_w & 0xFFFF != 0 || p.src_h & 0xFFFF != 0 || p.src_x & 0xFFFF != 0 || p.src_y & 0xFFFF != 0 {
        return Err(-ERANGE);
    }
    if (p.src_w >> 16) != p.crtc_w || (p.src_h >> 16) != p.crtc_h {
        return Err(-ERANGE);
    }
    // No positioning, no clipping: the plane covers the whole mode.
    if p.crtc_x != 0 || p.crtc_y != 0 || p.crtc_w != mw || p.crtc_h != mh {
        return Err(-EINVAL);
    }
    // The source window must lie inside the buffer.
    let (sx, sy) = ((p.src_x >> 16) as u64, (p.src_y >> 16) as u64);
    if sx + (p.src_w >> 16) as u64 > fb.width as u64 || sy + (p.src_h >> 16) as u64 > fb.height as u64 {
        return Err(-ENOSPC);
    }
    Ok(())
}

pub const ERANGE: i64 = 34;

/// A full-frame blit of the primary plane, if the CRTC is lit.
fn full_blit(d: &Device) -> Option<(Native, Blit)> {
    if !d.crtc.active || d.plane.fb_id == 0 {
        return None;
    }
    let fb = d.fbs.get(&d.plane.fb_id)?;
    Some((
        d.native,
        Blit {
            gem: fb.gem.clone(),
            offset: fb.offset as usize,
            pitch: fb.pitch as usize,
            sx: d.plane.src_x >> 16,
            sy: d.plane.src_y >> 16,
            dx: d.plane.crtc_x.max(0) as u32,
            dy: d.plane.crtc_y.max(0) as u32,
            w: d.plane.crtc_w,
            h: d.plane.crtc_h,
        },
    ))
}

/// Install a checked state; returns the frame to show (None = nothing lit).
fn commit_state(d: &mut Device, s: State) -> Option<(Native, Blit)> {
    d.plane = s.plane;
    d.crtc = s.crtc;
    d.conn = s.conn;
    full_blit(d)
}

/// Put the committed frame on screen and settle who owns the screen: a lit
/// CRTC with a buffer takes it from the console, anything else returns it.
/// Runs *outside* `with` (the copy is banded and masks per band itself).
fn show(job: Option<(Native, Blit)>) {
    match job {
        Some((nat, blit)) => {
            scanout::set_owned(true);
            scanout::blit(&nat, &blit);
            let _g = IrqGuard::new();
            drop(blit); // the Rc decrement, serialized like every other
        }
        None => scanout::set_owned(false),
    }
}

/// Queue the completion event of a flip / atomic commit at the next vblank.
fn queue_flip_event(f: &Rc<DrmFile>, user_data: u64) {
    let now = now_ms();
    let (seq, due) = next_vblank(now);
    f.st.borrow_mut().events.push_back(PendingEvent {
        type_: EVENT_FLIP_COMPLETE,
        user_data,
        crtc_id: CRTC_ID,
        sequence: seq as u32,
        due_ms: due,
    });
}

/// Drop everything a closing file owned (called with the device held).
pub fn release_file(d: &mut Device, file: u32) {
    let dead: Vec<u32> = d.fbs.iter().filter(|(_, f)| f.owner == file).map(|(&id, _)| id).collect();
    for id in dead {
        remove_fb(d, id);
    }
    d.blobs.retain(|_, b| b.owner != file);
}

/// Remove a framebuffer; a CRTC scanning it out is switched off first (what
/// `drm_framebuffer_remove` does to a legacy primary plane).
fn remove_fb(d: &mut Device, id: u32) {
    if d.plane.fb_id == id {
        d.plane = PlaneState::default();
        d.crtc = CrtcState::default();
        d.conn.crtc_id = 0;
    }
    d.fbs.remove(&id);
}

// ------------------------------------------------------------- ioctls

pub fn obsolete(_f: &Rc<DrmFile>, _b: &mut [u8]) -> i64 {
    -EINVAL // ATTACHMODE / DETACHMODE: removed from Linux long ago
}

/// Cursor ioctls: no hardware cursor plane.  Linux answers -ENXIO for a
/// CRTC without a cursor, and Xorg then falls back to a software cursor.
pub fn cursor(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let c: Cursor = from_bytes(b);
    if c.flags & !(MODE_CURSOR_BO | MODE_CURSOR_MOVE) != 0 || c.flags == 0 {
        return -EINVAL;
    }
    if c.crtc_id != CRTC_ID {
        return -ENOENT;
    }
    -ENXIO
}

/// Gamma: `gamma_size` is 0 (no LUT); Linux answers -ENOSYS.
pub fn no_gamma(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let c: CrtcLut = from_bytes(b);
    if c.crtc_id != CRTC_ID {
        return -ENOENT;
    }
    -ENOSYS
}

pub fn getresources(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut r: CardRes = from_bytes(b);
    let me = f.st.borrow().id;
    let Some((fbs, w, h)) = with(|d| {
        let fbs: Vec<u32> = d.fbs.values().filter(|x| x.owner == me).map(|x| x.id).collect();
        (fbs, d.native.width(), d.native.height())
    }) else {
        return -ENOENT;
    };
    macro_rules! tri {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => return e,
            }
        };
    }
    r.count_fbs = tri!(put_u32s(r.fb_id_ptr, r.count_fbs, &fbs));
    r.count_crtcs = tri!(put_u32s(r.crtc_id_ptr, r.count_crtcs, &[CRTC_ID]));
    r.count_connectors = tri!(put_u32s(r.connector_id_ptr, r.count_connectors, &[CONNECTOR_ID]));
    r.count_encoders = tri!(put_u32s(r.encoder_id_ptr, r.count_encoders, &[ENCODER_ID]));
    // The one scanout size is the only size: nothing larger could be shown.
    r.min_width = 0;
    r.min_height = 0;
    r.max_width = w;
    r.max_height = h;
    to_bytes(&r, b);
    0
}

pub fn getcrtc(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut c: Crtc = from_bytes(b);
    if c.crtc_id != CRTC_ID {
        return -ENOENT;
    }
    let Some(()) = with(|d| {
        c.fb_id = d.plane.fb_id;
        c.x = d.plane.src_x >> 16;
        c.y = d.plane.src_y >> 16;
        c.gamma_size = 0;
        c.mode_valid = d.crtc.mode_set as u32;
        c.mode = if d.crtc.mode_set { mode_of_blob(d, d.crtc.mode_blob).unwrap_or(d.mode) } else { ModeInfo::default() };
    }) else {
        return -ENOENT;
    };
    to_bytes(&c, b);
    0
}

/// SETCRTC (master only, enforced by the dispatcher).
pub fn setcrtc(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let c: Crtc = from_bytes(b);
    if c.crtc_id != CRTC_ID {
        return -ENOENT;
    }
    if c.x & 0xFFFF_0000 != 0 || c.y & 0xFFFF_0000 != 0 {
        return -ERANGE;
    }
    // Connector list (read before the device is held).
    let conns = if c.count_connectors > 0 {
        if c.count_connectors > 8 {
            return -EINVAL;
        }
        match read_u32s(c.set_connectors_ptr, c.count_connectors as usize) {
            Ok(v) => v,
            Err(e) => return e,
        }
    } else {
        Vec::new()
    };

    let job = with(|d| -> Result<Option<(Native, Blit)>, i64> {
        let mut s = current(d);
        if c.mode_valid == 0 {
            // Disable: no mode, so no connectors either.
            if !conns.is_empty() {
                return Err(-EINVAL);
            }
            s.plane = PlaneState::default();
            s.crtc = CrtcState::default();
            s.conn.crtc_id = 0;
        } else {
            if conns.is_empty() {
                return Err(-EINVAL);
            }
            if conns.iter().any(|&id| id != CONNECTOR_ID) {
                return Err(-ENOENT);
            }
            let um: ModeInfo = c.mode;
            if !mode_fits(d, &um) {
                return Err(-EINVAL);
            }
            // fb_id -1 keeps the current primary framebuffer (legacy).
            let fb_id = if c.fb_id == u32::MAX { d.plane.fb_id } else { c.fb_id };
            if fb_id == 0 || !d.fbs.contains_key(&fb_id) {
                return Err(-ENOENT);
            }
            let (mw, mh) = (d.native.width(), d.native.height());
            s.plane = PlaneState {
                fb_id,
                crtc_id: CRTC_ID,
                crtc_x: 0,
                crtc_y: 0,
                crtc_w: mw,
                crtc_h: mh,
                src_x: c.x << 16,
                src_y: c.y << 16,
                src_w: mw << 16,
                src_h: mh << 16,
            };
            s.crtc = CrtcState { active: true, mode_blob: d.mode_blob, mode_set: true };
            s.conn = ConnState { crtc_id: CRTC_ID, dpms: DPMS_ON };
        }
        check_state(d, &s)?;
        Ok(commit_state(d, s))
    });
    match job {
        Some(Ok(j)) => {
            show(j);
            0
        }
        Some(Err(e)) => e,
        None => -ENOENT,
    }
}

pub fn getencoder(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut e: GetEncoder = from_bytes(b);
    if e.encoder_id != ENCODER_ID {
        return -ENOENT;
    }
    let lit = with(|d| d.conn.crtc_id != 0).unwrap_or(false);
    e.encoder_type = MODE_ENCODER_VIRTUAL;
    e.crtc_id = if lit { CRTC_ID } else { 0 };
    e.possible_crtcs = 1;
    e.possible_clones = 0;
    to_bytes(&e, b);
    0
}

pub fn getconnector(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut c: GetConnector = from_bytes(b);
    if c.connector_id != CONNECTOR_ID {
        return -ENOENT;
    }
    let atomic = f.st.borrow().atomic;
    let Some((mode, w, h, conn, ids, vals)) = with(|d| {
        let (ids, vals) = visible_props(d, OBJECT_CONNECTOR, atomic);
        (d.mode, d.native.width(), d.native.height(), d.conn, ids, vals)
    }) else {
        return -ENOENT;
    };
    macro_rules! tri {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => return e,
            }
        };
    }
    c.count_encoders = tri!(put_u32s(c.encoders_ptr, c.count_encoders, &[ENCODER_ID]));
    c.count_modes = tri!(put_structs(c.modes_ptr, c.count_modes, &[mode]));
    // Both arrays are written together, only when the caller can hold them.
    let n = ids.len() as u32;
    if c.count_props >= n {
        tri!(put_u32s(c.props_ptr, c.count_props, &ids));
        tri!(put_u64s(c.prop_values_ptr, c.count_props, &vals));
    }
    c.count_props = n;
    c.encoder_id = if conn.crtc_id != 0 { ENCODER_ID } else { 0 };
    c.connector_type = MODE_CONNECTOR_VIRTUAL;
    c.connector_type_id = 1;
    c.connection = MODE_CONNECTED;
    c.mm_width = scanout::size_mm(w);
    c.mm_height = scanout::size_mm(h);
    c.subpixel = MODE_SUBPIXEL_UNKNOWN;
    c.pad = 0;
    to_bytes(&c, b);
    0
}

pub fn getproperty(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut p: GetProperty = from_bytes(b);
    let Some(def) = prop_def(p.prop_id) else { return -ENOENT };
    p.flags = def.flags;
    p.name = name_bytes(def.name);
    let mut values: Vec<u64> = Vec::new();
    let mut enums: Vec<PropertyEnum> = Vec::new();
    match def.kind {
        PropKind::Range(lo, hi) => {
            values.push(lo);
            values.push(hi);
        }
        PropKind::SignedRange(lo, hi) => {
            values.push(lo as u64);
            values.push(hi as u64);
        }
        PropKind::Enum(list) => {
            for &(v, n) in list {
                values.push(v);
                enums.push(PropertyEnum { value: v, name: name_bytes(n) });
            }
        }
        PropKind::Blob => {}
        PropKind::Object(t) => values.push(t as u64),
    }
    match put_u64s(p.values_ptr, p.count_values, &values) {
        Ok(n) => p.count_values = n,
        Err(e) => return e,
    }
    match put_structs(p.enum_blob_ptr, p.count_enum_blobs, &enums) {
        Ok(n) => p.count_enum_blobs = n,
        Err(e) => return e,
    }
    to_bytes(&p, b);
    0
}

/// Legacy connector property set: only DPMS is writable.
pub fn connector_setproperty(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let c: ConnectorSetProperty = from_bytes(b);
    if c.connector_id != CONNECTOR_ID {
        return -ENOENT;
    }
    set_dpms_prop(f, c.prop_id, c.value)
}

fn set_dpms_prop(_f: &Rc<DrmFile>, prop: u32, value: u64) -> i64 {
    let Some(def) = prop_def(prop) else { return -ENOENT };
    if !CONNECTOR_PROPS.contains(&prop) {
        return -EINVAL;
    }
    if prop != P_DPMS || def.flags & PROP_IMMUTABLE != 0 {
        return -EINVAL; // everything else is immutable or atomic-only
    }
    if value > DPMS_OFF {
        return -EINVAL;
    }
    let job = with(|d| {
        let mut s = current(d);
        s.conn.dpms = value;
        // DPMS On/Off is the CRTC's ACTIVE bit; the mode stays set.
        s.crtc.active = value == DPMS_ON && s.crtc.mode_set;
        commit_state(d, s)
    });
    match job {
        Some(j) => {
            // Off keeps the last frame on the panel (no hardware to power
            // down); On repaints from the plane's buffer.
            if j.is_some() {
                show(j);
            }
            0
        }
        None => -ENOENT,
    }
}

pub fn obj_getproperties(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut o: ObjGetProperties = from_bytes(b);
    let atomic = f.st.borrow().atomic;
    if object_props(o.obj_type).is_none() || object_id_of(o.obj_type) != Some(o.obj_id) {
        return -ENOENT;
    }
    let Some((ids, vals)) = with(|d| visible_props(d, o.obj_type, atomic)) else { return -ENOENT };
    match put_u32s(o.props_ptr, o.count_props, &ids) {
        Ok(_) => {}
        Err(e) => return e,
    }
    match put_u64s(o.prop_values_ptr, o.count_props, &vals) {
        Ok(n) => o.count_props = n,
        Err(e) => return e,
    }
    to_bytes(&o, b);
    0
}

pub fn obj_setproperty(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let o: ObjSetProperty = from_bytes(b);
    if object_props(o.obj_type).is_none() || object_id_of(o.obj_type) != Some(o.obj_id) {
        return -ENOENT;
    }
    if o.obj_type == OBJECT_CONNECTOR {
        return set_dpms_prop(f, o.prop_id, o.value);
    }
    // Legacy property writes on a plane / CRTC go through the atomic
    // properties only (nothing there is legacy-writable).
    if prop_def(o.prop_id).is_none() {
        return -ENOENT;
    }
    -EINVAL
}

pub fn getpropblob(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut g: GetBlob = from_bytes(b);
    let blob = with(|d| d.blobs.get(&g.blob_id).map(|x| x.data.clone())).flatten();
    let Some(data) = blob else { return -ENOENT };
    // drm_mode_getblob: copy only when the caller's length matches exactly.
    if g.length as usize == data.len() && !data.is_empty() && !user_write(g.data, &data) {
        return -EFAULT;
    }
    g.length = data.len() as u32;
    to_bytes(&g, b);
    0
}

pub fn create_blob(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut c: CreateBlob = from_bytes(b);
    if c.length == 0 || c.length > MAX_BLOB_LEN {
        return -EINVAL;
    }
    let data = match user_read(c.data, c.length as usize) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let me = f.st.borrow().id;
    let id = with(|d| {
        if d.blobs.len() >= MAX_BLOBS {
            return None;
        }
        let id = d.alloc_id();
        d.blobs.insert(id, Blob { id, data, owner: me });
        Some(id)
    })
    .flatten();
    match id {
        Some(id) => {
            c.blob_id = id;
            to_bytes(&c, b);
            0
        }
        None => -ENOMEM,
    }
}

pub fn destroy_blob(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let c: DestroyBlob = from_bytes(b);
    let me = f.st.borrow().id;
    let r = with(|d| match d.blobs.get(&c.blob_id) {
        Some(x) if x.owner == me => {
            d.blobs.remove(&c.blob_id);
            0
        }
        _ => -EINVAL, // not found, or not ours (kernel blobs included)
    });
    r.unwrap_or(-EINVAL)
}

// ---------------------------------------------------------- framebuffers

/// Pixel formats the primary plane scans out (what GETPLANE lists).
const FORMATS: [u32; 2] = [FORMAT_XRGB8888, FORMAT_ARGB8888];

/// Create a framebuffer over a handle of `file` (`drm_internal_framebuffer_create`).
fn add_fb(f: &Rc<DrmFile>, width: u32, height: u32, format: u32, pitch: u32, offset: u32, handle: u32) -> Result<u32, i64> {
    if width == 0 || height == 0 {
        return Err(-EINVAL);
    }
    if !is_supported_format(format) {
        return Err(-EINVAL);
    }
    let Some((mw, mh)) = with(|d| (d.native.width(), d.native.height())) else { return Err(-ENOENT) };
    if width > mw || height > mh {
        return Err(-EINVAL); // larger than anything the device can scan out
    }
    if (pitch as u64) < width as u64 * 4 || pitch % 4 != 0 || offset % 4 != 0 {
        return Err(-EINVAL);
    }
    let Some(gem) = handle_lookup(f, handle) else { return Err(-ENOENT) };
    let last = offset as u64 + pitch as u64 * (height as u64 - 1) + width as u64 * 4;
    if last > gem.size as u64 {
        return Err(-EINVAL); // the buffer is too small for the geometry
    }
    let owner = f.st.borrow().id;
    let r = with(|d| {
        if d.fbs.len() >= MAX_FBS {
            return Err(-ENOMEM);
        }
        let id = d.alloc_id();
        d.fbs.insert(id, Fb { id, width, height, format, pitch, offset, gem, owner });
        Ok(id)
    });
    r.unwrap_or(Err(-ENOENT))
}

pub fn addfb2(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut c: FbCmd2 = from_bytes(b);
    if c.flags & !(MODE_FB_INTERLACED | MODE_FB_MODIFIERS) != 0 {
        return -EINVAL;
    }
    if c.flags & (MODE_FB_INTERLACED | MODE_FB_MODIFIERS) != 0 {
        return -EINVAL; // no interlacing, no modifier support
    }
    // Single-plane formats only: the other plane slots must be empty.
    if c.handles[1..].iter().any(|&x| x != 0)
        || c.pitches[1..].iter().any(|&x| x != 0)
        || c.offsets[1..].iter().any(|&x| x != 0)
        || c.modifier.iter().any(|&m| m != 0)
    {
        return -EINVAL;
    }
    match add_fb(f, c.width, c.height, c.pixel_format, c.pitches[0], c.offsets[0], c.handles[0]) {
        Ok(id) => {
            c.fb_id = id;
            to_bytes(&c, b);
            0
        }
        Err(e) => e,
    }
}

pub fn addfb(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut c: FbCmd = from_bytes(b);
    // drm_mode_legacy_fb_format: (bpp, depth) -> fourcc.
    let format = match (c.bpp, c.depth) {
        (32, 24) => FORMAT_XRGB8888,
        (32, 32) => FORMAT_ARGB8888,
        _ => return -EINVAL,
    };
    match add_fb(f, c.width, c.height, format, c.pitch, 0, c.handle) {
        Ok(id) => {
            c.fb_id = id;
            to_bytes(&c, b);
            0
        }
        Err(e) => e,
    }
}

pub fn getfb(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut c: FbCmd = from_bytes(b);
    let info = with(|d| d.fbs.get(&c.fb_id).map(|x| (x.width, x.height, x.pitch, x.format, x.gem.clone()))).flatten();
    let Some((w, h, pitch, format, gem)) = info else { return -ENOENT };
    // GETFB hands back a *new* handle to the buffer in the caller's table.
    let handle = match handle_create(f, gem) {
        Ok(h) => h,
        Err(e) => return e,
    };
    c.width = w;
    c.height = h;
    c.pitch = pitch;
    c.bpp = 32;
    c.depth = if format == FORMAT_ARGB8888 { 32 } else { 24 };
    c.handle = handle;
    to_bytes(&c, b);
    0
}

pub fn rmfb(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let id = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let me = f.st.borrow().id;
    let r = with(|d| match d.fbs.get(&id) {
        Some(x) if x.owner == me => {
            let was_lit = d.plane.fb_id == id;
            remove_fb(d, id);
            Some(was_lit)
        }
        _ => None,
    });
    match r {
        Some(Some(was_lit)) => {
            if was_lit {
                scanout::set_owned(false); // the CRTC went off: console back
            }
            0
        }
        _ => -ENOENT,
    }
}

pub fn page_flip(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let pf: CrtcPageFlip = from_bytes(b);
    if pf.flags & !MODE_PAGE_FLIP_FLAGS != 0 || pf.reserved != 0 {
        return -EINVAL;
    }
    // Async flips and flip targets are not offered (their caps read 0).
    if pf.flags & (MODE_PAGE_FLIP_ASYNC | MODE_PAGE_FLIP_TARGET_ABSOLUTE | MODE_PAGE_FLIP_TARGET_RELATIVE) != 0 {
        return -EINVAL;
    }
    if pf.crtc_id != CRTC_ID {
        return -ENOENT;
    }
    let r = with(|d| -> Result<Option<(Native, Blit)>, i64> {
        // A flip needs a lit CRTC with a buffer already on it.
        if !d.crtc.mode_set || d.plane.fb_id == 0 {
            return Err(-EBUSY);
        }
        let Some(new) = d.fbs.get(&pf.fb_id) else { return Err(-ENOENT) };
        let old_format = d.fbs.get(&d.plane.fb_id).map(|o| o.format);
        if old_format != Some(new.format) {
            return Err(-EINVAL); // a flip never changes the pixel format
        }
        let mut s = current(d);
        s.plane.fb_id = pf.fb_id;
        check_state(d, &s)?;
        Ok(commit_state(d, s))
    });
    match r {
        Some(Ok(job)) => {
            show(job);
            if pf.flags & MODE_PAGE_FLIP_EVENT != 0 {
                queue_flip_event(f, pf.user_data);
            }
            0
        }
        Some(Err(e)) => e,
        None => -ENOENT,
    }
}

/// DIRTYFB: the CPU wrote into a buffer; if it is the one on screen, copy
/// the damaged rectangles to the panel (all of it when no clips are given).
pub fn dirtyfb(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let c: FbDirtyCmd = from_bytes(b);
    if c.flags & !0x3 != 0 {
        return -EINVAL; // DRM_MODE_FB_DIRTY_FLAGS
    }
    if c.num_clips > MAX_DIRTY_CLIPS || (c.num_clips > 0) != (c.clips_ptr != 0) {
        return -EINVAL;
    }
    let clips: Vec<ClipRect> = if c.num_clips > 0 {
        let raw = match user_read(c.clips_ptr, c.num_clips as usize * core::mem::size_of::<ClipRect>()) {
            Ok(v) => v,
            Err(e) => return e,
        };
        raw.chunks_exact(core::mem::size_of::<ClipRect>()).map(from_bytes::<ClipRect>).collect()
    } else {
        Vec::new()
    };
    // Build the blits under the device, run them outside.
    let jobs = with(|d| -> Result<Vec<(Native, Blit)>, i64> {
        let Some(fb) = d.fbs.get(&c.fb_id) else { return Err(-ENOENT) };
        if d.plane.fb_id != c.fb_id || !d.crtc.active {
            return Ok(Vec::new()); // not on screen: nothing to copy
        }
        let (sx, sy) = (d.plane.src_x >> 16, d.plane.src_y >> 16);
        let (vw, vh) = (d.plane.src_w >> 16, d.plane.src_h >> 16);
        let full = ClipRect { x1: 0, y1: 0, x2: fb.width.min(0xFFFF) as u16, y2: fb.height.min(0xFFFF) as u16 };
        let list: &[ClipRect] = if clips.is_empty() { core::slice::from_ref(&full) } else { &clips };
        let mut out = Vec::new();
        for r in list {
            // Clip to the visible source window, then translate to panel
            // coordinates.  A malformed (empty / inverted) rect is skipped.
            let x1 = (r.x1 as u32).max(sx);
            let y1 = (r.y1 as u32).max(sy);
            let x2 = (r.x2 as u32).min(sx + vw);
            let y2 = (r.y2 as u32).min(sy + vh);
            if x2 <= x1 || y2 <= y1 {
                continue;
            }
            out.push((
                d.native,
                Blit {
                    gem: fb.gem.clone(),
                    offset: fb.offset as usize,
                    pitch: fb.pitch as usize,
                    sx: x1,
                    sy: y1,
                    dx: (x1 - sx) + d.plane.crtc_x.max(0) as u32,
                    dy: (y1 - sy) + d.plane.crtc_y.max(0) as u32,
                    w: x2 - x1,
                    h: y2 - y1,
                },
            ));
        }
        Ok(out)
    });
    match jobs {
        Some(Ok(v)) => {
            // While the text console has the screen (the client's VT is not
            // the active one) a damage flush must not paint over it.
            let owned = scanout::owned();
            for (nat, job) in v {
                if owned {
                    scanout::blit(&nat, &job);
                }
                let _g = IrqGuard::new();
                drop(job);
            }
            0
        }
        Some(Err(e)) => e,
        None => -ENOENT,
    }
}

pub fn setplane(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let p: SetPlane = from_bytes(b);
    if p.plane_id != PLANE_ID {
        return -ENOENT;
    }
    if p.flags != 0 {
        return -EINVAL;
    }
    let r = with(|d| -> Result<Option<(Native, Blit)>, i64> {
        let mut s = current(d);
        if p.fb_id == 0 {
            // Disable the plane (the CRTC itself stays as it is).
            s.plane = PlaneState::default();
        } else {
            if p.crtc_id != CRTC_ID {
                return Err(-ENOENT);
            }
            s.plane = PlaneState {
                fb_id: p.fb_id,
                crtc_id: p.crtc_id,
                crtc_x: p.crtc_x,
                crtc_y: p.crtc_y,
                crtc_w: p.crtc_w,
                crtc_h: p.crtc_h,
                src_x: p.src_x,
                src_y: p.src_y,
                src_w: p.src_w,
                src_h: p.src_h,
            };
        }
        check_state(d, &s)?;
        Ok(commit_state(d, s))
    });
    match r {
        Some(Ok(job)) => {
            show(job);
            0
        }
        Some(Err(e)) => e,
        None => -ENOENT,
    }
}

// ------------------------------------------------------------- planes

pub fn getplaneresources(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut r: GetPlaneRes = from_bytes(b);
    // The primary plane is hidden from legacy clients.
    let planes: &[u32] = if f.st.borrow().universal_planes { &[PLANE_ID] } else { &[] };
    match put_u32s(r.plane_id_ptr, r.count_planes, planes) {
        Ok(n) => r.count_planes = n,
        Err(e) => return e,
    }
    to_bytes(&r, b);
    0
}

pub fn getplane(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut p: GetPlane = from_bytes(b);
    if p.plane_id != PLANE_ID || !f.st.borrow().universal_planes {
        return -ENOENT;
    }
    let Some((crtc, fb)) = with(|d| (d.plane.crtc_id, d.plane.fb_id)) else { return -ENOENT };
    p.crtc_id = crtc;
    p.fb_id = fb;
    p.possible_crtcs = 1;
    p.gamma_size = 0;
    match put_u32s(p.format_type_ptr, p.count_format_types, &FORMATS) {
        Ok(n) => p.count_format_types = n,
        Err(e) => return e,
    }
    to_bytes(&p, b);
    0
}

// -------------------------------------------------------- dumb buffers

/// Largest dumb buffer: 64 MiB (a 4096x4096x32 surface).
const MAX_DUMB_BYTES: u64 = 64 << 20;

pub fn create_dumb(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut c: CreateDumb = from_bytes(b);
    if c.width == 0 || c.height == 0 || c.bpp == 0 || c.flags != 0 {
        return -EINVAL;
    }
    if !matches!(c.bpp, 8 | 16 | 24 | 32) {
        return -EINVAL;
    }
    if c.width > 16384 || c.height > 16384 {
        return -EINVAL;
    }
    // drm_mode_align_dumb: pitch to 64 bytes, size to a page.
    let cpp = (c.bpp as u64).div_ceil(8);
    let pitch = (c.width as u64 * cpp + 63) & !63;
    let size = (pitch * c.height as u64 + 4095) & !4095;
    if size > MAX_DUMB_BYTES {
        return -EINVAL;
    }
    let Some(id) = with(|d| {
        let id = d.next_gem;
        d.next_gem = d.next_gem.wrapping_add(1).max(1);
        id
    }) else {
        return -ENOENT;
    };
    let Some(gem) = gem::Gem::new(id, size as usize) else { return -ENOMEM };
    let gem = Rc::new(gem);
    let h = match handle_create(f, gem) {
        Ok(h) => h,
        Err(e) => return e,
    };
    c.handle = h;
    c.pitch = pitch as u32;
    c.size = size;
    to_bytes(&c, b);
    0
}

pub fn map_dumb(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut m: MapDumb = from_bytes(b);
    let Some(gem) = handle_lookup(f, m.handle) else { return -ENOENT };
    m.offset = gem.mmap_offset();
    to_bytes(&m, b);
    0
}

pub fn destroy_dumb(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let d: DestroyDumb = from_bytes(b);
    if d.handle == 0 {
        return -EINVAL;
    }
    let removed = f.st.borrow_mut().handles.remove(&d.handle);
    match removed {
        Some(g) => {
            with(|_| drop(g));
            0
        }
        None => -EINVAL,
    }
}

// -------------------------------------------------------------- atomic

/// Apply one (object, property, value) triple to a proposed state.
fn apply_prop(d: &Device, s: &mut State, obj_type: u32, prop: u32, value: u64) -> Result<(), i64> {
    let Some(def) = prop_def(prop) else { return Err(-ENOENT) };
    let carried = match object_props(obj_type) {
        Some(l) => l.contains(&prop),
        None => false,
    };
    if !carried {
        return Err(-EINVAL);
    }
    if def.flags & PROP_IMMUTABLE != 0 {
        return Err(-EINVAL);
    }
    let u32v = |v: u64| u32::try_from(v).map_err(|_| -EINVAL);
    let i32v = |v: u64| i32::try_from(v as i64).map_err(|_| -EINVAL);
    match prop {
        // DPMS belongs to the legacy API; atomic clients drive ACTIVE.
        P_DPMS => return Err(-EINVAL),
        P_CONN_CRTC_ID => s.conn.crtc_id = u32v(value)?,
        P_FB_ID => {
            let id = u32v(value)?;
            if id != 0 && !d.fbs.contains_key(&id) {
                return Err(-ENOENT);
            }
            s.plane.fb_id = id;
        }
        P_PLANE_CRTC_ID => s.plane.crtc_id = u32v(value)?,
        P_CRTC_X => s.plane.crtc_x = i32v(value)?,
        P_CRTC_Y => s.plane.crtc_y = i32v(value)?,
        P_CRTC_W => s.plane.crtc_w = u32v(value)?,
        P_CRTC_H => s.plane.crtc_h = u32v(value)?,
        P_SRC_X => s.plane.src_x = u32v(value)?,
        P_SRC_Y => s.plane.src_y = u32v(value)?,
        P_SRC_W => s.plane.src_w = u32v(value)?,
        P_SRC_H => s.plane.src_h = u32v(value)?,
        P_ACTIVE => {
            if value > 1 {
                return Err(-EINVAL);
            }
            s.crtc.active = value == 1;
            // The legacy DPMS view follows ACTIVE.
            s.conn.dpms = if value == 1 { DPMS_ON } else { DPMS_OFF };
        }
        P_MODE_ID => {
            let id = u32v(value)?;
            if id == 0 {
                s.crtc.mode_set = false;
                s.crtc.mode_blob = 0;
            } else {
                if !d.blobs.contains_key(&id) {
                    return Err(-ENOENT);
                }
                s.crtc.mode_set = true;
                s.crtc.mode_blob = id;
            }
        }
        _ => return Err(-EINVAL),
    }
    Ok(())
}

pub fn atomic(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let a: Atomic = from_bytes(b);
    if !f.st.borrow().atomic {
        return -EINVAL; // DRM_CLIENT_CAP_ATOMIC not enabled
    }
    if a.flags & !MODE_ATOMIC_FLAGS != 0 || a.reserved != 0 {
        return -EINVAL;
    }
    // Test-only commits cannot ask for an event; async commits are not
    // supported (cap ATOMIC_ASYNC_PAGE_FLIP is 0).
    if a.flags & MODE_ATOMIC_TEST_ONLY != 0 && a.flags & MODE_PAGE_FLIP_EVENT != 0 {
        return -EINVAL;
    }
    if a.flags & MODE_PAGE_FLIP_ASYNC != 0 {
        return -EINVAL;
    }
    let n = a.count_objs as usize;
    if n > 16 {
        return -EINVAL;
    }
    let (objs, counts) = if n > 0 {
        match (read_u32s(a.objs_ptr, n), read_u32s(a.count_props_ptr, n)) {
            (Ok(o), Ok(c)) => (o, c),
            (Err(e), _) | (_, Err(e)) => return e,
        }
    } else {
        (Vec::new(), Vec::new())
    };
    let total: usize = counts.iter().map(|&c| c as usize).sum();
    if total > 128 {
        return -EINVAL;
    }
    let (props, values) = if total > 0 {
        match (read_u32s(a.props_ptr, total), read_u64s(a.prop_values_ptr, total)) {
            (Ok(p), Ok(v)) => (p, v),
            (Err(e), _) | (_, Err(e)) => return e,
        }
    } else {
        (Vec::new(), Vec::new())
    };

    let flags = a.flags;
    let r = with(|d| -> Result<(Option<(Native, Blit)>, bool), i64> {
        let mut s = current(d);
        let mut touched_crtc = false;
        let mut at = 0usize;
        for (i, &obj_id) in objs.iter().enumerate() {
            // Which object is this id?  (Ids are unique across our types.)
            let obj_type = match obj_id {
                CONNECTOR_ID => OBJECT_CONNECTOR,
                PLANE_ID => OBJECT_PLANE,
                CRTC_ID => OBJECT_CRTC,
                ENCODER_ID => OBJECT_ENCODER,
                _ => return Err(-ENOENT),
            };
            if obj_type == OBJECT_CRTC {
                touched_crtc = true;
            }
            for _ in 0..counts[i] {
                apply_prop(d, &mut s, obj_type, props[at], values[at])?;
                at += 1;
            }
        }
        check_state(d, &s)?;
        // Anything that changes the CRTC / connector wiring is a modeset.
        let modeset = s.crtc != d.crtc || s.conn.crtc_id != d.conn.crtc_id;
        if modeset && flags & MODE_ATOMIC_ALLOW_MODESET == 0 {
            return Err(-EINVAL);
        }
        if flags & MODE_ATOMIC_TEST_ONLY != 0 {
            return Ok((None, false));
        }
        Ok((commit_state(d, s), touched_crtc))
    });
    match r {
        Some(Ok((job, touched))) => {
            if flags & MODE_ATOMIC_TEST_ONLY != 0 {
                return 0;
            }
            show(job);
            // One completion event per CRTC in the request.
            if flags & MODE_PAGE_FLIP_EVENT != 0 && touched {
                queue_flip_event(f, a.user_data);
            }
            0
        }
        Some(Err(e)) => e,
        None => -ENOENT,
    }
}
