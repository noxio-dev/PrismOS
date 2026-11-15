// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// The DRM / KMS userspace ABI, laid out exactly like Linux's
// <drm/drm.h>, <drm/drm_mode.h> and <drm/drm_fourcc.h> on x86-64 LP64:
// same ioctl numbers (`_IOC` with magic 'd'), same field order, same
// padding, same sizes.  Every structure carries a compile-time size and
// field-offset assertion, so an ABI drift is a build error, not a Xorg
// crash.  Nothing here has any logic: constants, plain-old-data structs and
// the `_IOC` encoder.
//
// Naming follows the Linux headers minus the `drm_` / `DRM_` prefix where a
// module path already says it (`uapi::ModeInfo` is `struct drm_mode_modeinfo`).

#![allow(dead_code)]

use core::mem::{offset_of, size_of};

// ------------------------------------------------------------------ _IOC

pub const IOC_NONE: u32 = 0;
pub const IOC_WRITE: u32 = 1;
pub const IOC_READ: u32 = 2;
const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

/// `_IOC(dir, type, nr, size)` of <asm-generic/ioctl.h> (the x86-64 layout).
pub const fn ioc(dir: u32, ty: u32, nr: u32, size: usize) -> u32 {
    (dir << IOC_DIRSHIFT) | ((ty) << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | ((size as u32) << IOC_SIZESHIFT)
}
pub const fn ioc_dir(cmd: u32) -> u32 {
    (cmd >> IOC_DIRSHIFT) & 3
}
pub const fn ioc_type(cmd: u32) -> u32 {
    (cmd >> IOC_TYPESHIFT) & 0xFF
}
pub const fn ioc_nr(cmd: u32) -> u32 {
    cmd & 0xFF
}
pub const fn ioc_size(cmd: u32) -> usize {
    ((cmd >> IOC_SIZESHIFT) & 0x3FFF) as usize
}

/// DRM_IOCTL_BASE.
pub const DRM_IOCTL_BASE: u32 = b'd' as u32;

pub const fn drm_io(nr: u32) -> u32 {
    ioc(IOC_NONE, DRM_IOCTL_BASE, nr, 0)
}
pub const fn drm_ior(nr: u32, size: usize) -> u32 {
    ioc(IOC_READ, DRM_IOCTL_BASE, nr, size)
}
pub const fn drm_iow(nr: u32, size: usize) -> u32 {
    ioc(IOC_WRITE, DRM_IOCTL_BASE, nr, size)
}
pub const fn drm_iowr(nr: u32, size: usize) -> u32 {
    ioc(IOC_READ | IOC_WRITE, DRM_IOCTL_BASE, nr, size)
}

/// Marker for the plain-old-data structs below: `#[repr(C)]`, only integer
/// fields and arrays of them, every bit pattern valid, no padding bytes the
/// kernel could leak (explicit `pad` fields where Linux has them; the few
/// structs with *implicit* tail padding are zeroed before being filled).
pub unsafe trait Pod: Copy {}

/// Read a `T` out of `b`, zero-extending a short buffer (Linux's rule for an
/// ioctl argument smaller than the kernel's structure).
pub fn from_bytes<T: Pod>(b: &[u8]) -> T {
    let mut v = core::mem::MaybeUninit::<T>::zeroed();
    let n = b.len().min(size_of::<T>());
    // SAFETY: `v` is `size_of::<T>()` writable bytes; `T: Pod` accepts any
    // bit pattern.
    unsafe {
        core::ptr::copy_nonoverlapping(b.as_ptr(), v.as_mut_ptr() as *mut u8, n);
        v.assume_init()
    }
}

/// Write `t` into `out` (truncated to `out.len()`).
pub fn to_bytes<T: Pod>(t: &T, out: &mut [u8]) {
    let n = out.len().min(size_of::<T>());
    // SAFETY: `t` is `size_of::<T>()` readable bytes.
    unsafe { core::ptr::copy_nonoverlapping(t as *const T as *const u8, out.as_mut_ptr(), n) };
}

/// A `T` as a byte vector (for user arrays of structs).
pub fn as_bytes<T: Pod>(t: &T) -> &[u8] {
    // SAFETY: Pod, readable for size_of::<T>() bytes.
    unsafe { core::slice::from_raw_parts(t as *const T as *const u8, size_of::<T>()) }
}

macro_rules! pod {
    ($($t:ty),* $(,)?) => { $( unsafe impl Pod for $t {} )* };
}

// -------------------------------------------------------- core (drm.h)

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Version {
    pub version_major: i32,
    pub version_minor: i32,
    pub version_patchlevel: i32,
    pub name_len: u64,
    pub name: u64, // char *
    pub date_len: u64,
    pub date: u64,
    pub desc_len: u64,
    pub desc: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Unique {
    pub unique_len: u64,
    pub unique: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Auth {
    pub magic: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SetVersion {
    pub drm_di_major: i32,
    pub drm_di_minor: i32,
    pub drm_dd_major: i32,
    pub drm_dd_minor: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ModesetCtl {
    pub crtc: u32,
    pub cmd: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GemClose {
    pub handle: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GemFlink {
    pub handle: u32,
    pub name: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GemOpen {
    pub name: u32,
    pub handle: u32,
    pub size: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GetCap {
    pub capability: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SetClientCap {
    pub capability: u64,
    pub value: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PrimeHandle {
    pub handle: u32,
    pub flags: u32,
    pub fd: i32,
}

/// `union drm_wait_vblank`: the request half is {u32 type; u32 sequence;
/// unsigned long signal}, the reply {u32 type; u32 sequence; long tval_sec;
/// long tval_usec}; both 24 bytes on LP64, so one struct serves both.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct WaitVblank {
    pub type_: u32,
    pub sequence: u32,
    pub a: u64, // request: signal      reply: tval_sec
    pub b: u64, //                       reply: tval_usec
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Event {
    pub type_: u32,
    pub length: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct EventVblank {
    pub base: Event,
    pub user_data: u64,
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub sequence: u32,
    pub crtc_id: u32,
}

pod!(
    Version, Unique, Auth, SetVersion, ModesetCtl, GemClose, GemFlink, GemOpen, GetCap, SetClientCap,
    PrimeHandle, WaitVblank, Event, EventVblank
);

// ------------------------------------------------- KMS (drm_mode.h)

pub const DISPLAY_MODE_LEN: usize = 32;
pub const PROP_NAME_LEN: usize = 32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub type_: u32,
    pub name: [u8; DISPLAY_MODE_LEN],
}

impl Default for ModeInfo {
    fn default() -> Self {
        // SAFETY: all-zero is a valid ModeInfo (plain integers).
        unsafe { core::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CardRes {
    pub fb_id_ptr: u64,
    pub crtc_id_ptr: u64,
    pub connector_id_ptr: u64,
    pub encoder_id_ptr: u64,
    pub count_fbs: u32,
    pub count_crtcs: u32,
    pub count_connectors: u32,
    pub count_encoders: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Crtc {
    pub set_connectors_ptr: u64,
    pub count_connectors: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub x: u32,
    pub y: u32,
    pub gamma_size: u32,
    pub mode_valid: u32,
    pub mode: ModeInfo,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CrtcLut {
    pub crtc_id: u32,
    pub gamma_size: u32,
    pub red: u64,
    pub green: u64,
    pub blue: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GetEncoder {
    pub encoder_id: u32,
    pub encoder_type: u32,
    pub crtc_id: u32,
    pub possible_crtcs: u32,
    pub possible_clones: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GetConnector {
    pub encoders_ptr: u64,
    pub modes_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_modes: u32,
    pub count_props: u32,
    pub count_encoders: u32,
    pub encoder_id: u32,
    pub connector_id: u32,
    pub connector_type: u32,
    pub connector_type_id: u32,
    pub connection: u32,
    pub mm_width: u32,
    pub mm_height: u32,
    pub subpixel: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GetProperty {
    pub values_ptr: u64,
    pub enum_blob_ptr: u64,
    pub prop_id: u32,
    pub flags: u32,
    pub name: [u8; PROP_NAME_LEN],
    pub count_values: u32,
    pub count_enum_blobs: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PropertyEnum {
    pub value: u64,
    pub name: [u8; PROP_NAME_LEN],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ConnectorSetProperty {
    pub value: u64,
    pub prop_id: u32,
    pub connector_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ObjGetProperties {
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub count_props: u32,
    pub obj_id: u32,
    pub obj_type: u32,
    pub pad: u32, // implicit tail padding in Linux's header, explicit here
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ObjSetProperty {
    pub value: u64,
    pub prop_id: u32,
    pub obj_id: u32,
    pub obj_type: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GetBlob {
    pub blob_id: u32,
    pub length: u32,
    pub data: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CreateBlob {
    pub data: u64,
    pub length: u32,
    pub blob_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DestroyBlob {
    pub blob_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FbCmd {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub depth: u32,
    pub handle: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FbCmd2 {
    pub fb_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub handles: [u32; 4],
    pub pitches: [u32; 4],
    pub offsets: [u32; 4],
    pub pad: u32, // implicit before the u64 array in Linux's header
    pub modifier: [u64; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FbDirtyCmd {
    pub fb_id: u32,
    pub flags: u32,
    pub color: u32,
    pub num_clips: u32,
    pub clips_ptr: u64,
}

/// `struct drm_clip_rect`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ClipRect {
    pub x1: u16,
    pub y1: u16,
    pub x2: u16,
    pub y2: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CreateDumb {
    pub height: u32,
    pub width: u32,
    pub bpp: u32,
    pub flags: u32,
    pub handle: u32,
    pub pitch: u32,
    pub size: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MapDumb {
    pub handle: u32,
    pub pad: u32,
    pub offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct DestroyDumb {
    pub handle: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CrtcPageFlip {
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub reserved: u32,
    pub user_data: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GetPlaneRes {
    pub plane_id_ptr: u64,
    pub count_planes: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub possible_crtcs: u32,
    pub gamma_size: u32,
    pub count_format_types: u32,
    pub format_type_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SetPlane {
    pub plane_id: u32,
    pub crtc_id: u32,
    pub fb_id: u32,
    pub flags: u32,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_h: u32,
    pub src_w: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Cursor {
    pub flags: u32,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub handle: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Cursor2 {
    pub flags: u32,
    pub crtc_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub handle: u32,
    pub hot_x: i32,
    pub hot_y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Atomic {
    pub flags: u32,
    pub count_objs: u32,
    pub objs_ptr: u64,
    pub count_props_ptr: u64,
    pub props_ptr: u64,
    pub prop_values_ptr: u64,
    pub reserved: u64,
    pub user_data: u64,
}

pod!(
    ModeInfo, CardRes, Crtc, CrtcLut, GetEncoder, GetConnector, GetProperty, PropertyEnum,
    ConnectorSetProperty, ObjGetProperties, ObjSetProperty, GetBlob, CreateBlob, DestroyBlob, FbCmd,
    FbCmd2, FbDirtyCmd, ClipRect, CreateDumb, MapDumb, DestroyDumb, CrtcPageFlip, GetPlaneRes,
    GetPlane, SetPlane, Cursor, Cursor2, Atomic
);

// ------------------------------------------------------------ constants

// Capabilities (DRM_CAP_*).
pub const CAP_DUMB_BUFFER: u64 = 0x1;
pub const CAP_VBLANK_HIGH_CRTC: u64 = 0x2;
pub const CAP_DUMB_PREFERRED_DEPTH: u64 = 0x3;
pub const CAP_DUMB_PREFER_SHADOW: u64 = 0x4;
pub const CAP_PRIME: u64 = 0x5;
pub const CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
pub const CAP_ASYNC_PAGE_FLIP: u64 = 0x7;
pub const CAP_CURSOR_WIDTH: u64 = 0x8;
pub const CAP_CURSOR_HEIGHT: u64 = 0x9;
pub const CAP_ADDFB2_MODIFIERS: u64 = 0x10;
pub const CAP_PAGE_FLIP_TARGET: u64 = 0x11;
pub const CAP_CRTC_IN_VBLANK_EVENT: u64 = 0x12;
pub const CAP_SYNCOBJ: u64 = 0x13;
pub const CAP_SYNCOBJ_TIMELINE: u64 = 0x14;
pub const CAP_ATOMIC_ASYNC_PAGE_FLIP: u64 = 0x15;

// Client capabilities (DRM_CLIENT_CAP_*).
pub const CLIENT_CAP_STEREO_3D: u64 = 1;
pub const CLIENT_CAP_UNIVERSAL_PLANES: u64 = 2;
pub const CLIENT_CAP_ATOMIC: u64 = 3;
pub const CLIENT_CAP_ASPECT_RATIO: u64 = 4;
pub const CLIENT_CAP_WRITEBACK_CONNECTORS: u64 = 5;
pub const CLIENT_CAP_CURSOR_PLANE_HOTSPOT: u64 = 6;

// Object types (DRM_MODE_OBJECT_*).
pub const OBJECT_CRTC: u32 = 0xcccc_cccc;
pub const OBJECT_CONNECTOR: u32 = 0xc0c0_c0c0;
pub const OBJECT_ENCODER: u32 = 0xe0e0_e0e0;
pub const OBJECT_MODE: u32 = 0xdede_dede;
pub const OBJECT_PROPERTY: u32 = 0xb0b0_b0b0;
pub const OBJECT_FB: u32 = 0xfbfb_fbfb;
pub const OBJECT_BLOB: u32 = 0xbbbb_bbbb;
pub const OBJECT_PLANE: u32 = 0xeeee_eeee;
pub const OBJECT_ANY: u32 = 0;

// Connector / encoder types and states.
pub const MODE_CONNECTOR_VIRTUAL: u32 = 15;
pub const MODE_ENCODER_VIRTUAL: u32 = 5;
pub const MODE_CONNECTED: u32 = 1;
pub const MODE_DISCONNECTED: u32 = 2;
pub const MODE_UNKNOWNCONNECTION: u32 = 3;
pub const MODE_SUBPIXEL_UNKNOWN: u32 = 1;

// Plane types.
pub const PLANE_TYPE_OVERLAY: u64 = 0;
pub const PLANE_TYPE_PRIMARY: u64 = 1;
pub const PLANE_TYPE_CURSOR: u64 = 2;

// Property flags.
pub const PROP_PENDING: u32 = 1 << 0;
pub const PROP_RANGE: u32 = 1 << 1;
pub const PROP_IMMUTABLE: u32 = 1 << 2;
pub const PROP_ENUM: u32 = 1 << 3;
pub const PROP_BLOB: u32 = 1 << 4;
pub const PROP_BITMASK: u32 = 1 << 5;
pub const PROP_LEGACY_TYPE: u32 = PROP_RANGE | PROP_ENUM | PROP_BLOB | PROP_BITMASK;
pub const PROP_EXTENDED_TYPE: u32 = 0x0000_ffc0;
pub const PROP_OBJECT: u32 = 1 << 6;
pub const PROP_SIGNED_RANGE: u32 = 2 << 6;
pub const PROP_ATOMIC: u32 = 0x8000_0000;

// DPMS.
pub const DPMS_ON: u64 = 0;
pub const DPMS_STANDBY: u64 = 1;
pub const DPMS_SUSPEND: u64 = 2;
pub const DPMS_OFF: u64 = 3;

// Mode types / flags.
pub const MODE_TYPE_BUILTIN: u32 = 1 << 0;
pub const MODE_TYPE_PREFERRED: u32 = 1 << 3;
pub const MODE_TYPE_DRIVER: u32 = 1 << 6;
pub const MODE_FLAG_PHSYNC: u32 = 1 << 0;
pub const MODE_FLAG_NHSYNC: u32 = 1 << 1;
pub const MODE_FLAG_PVSYNC: u32 = 1 << 2;
pub const MODE_FLAG_NVSYNC: u32 = 1 << 3;

// Page flip / atomic / vblank flags.
pub const MODE_PAGE_FLIP_EVENT: u32 = 0x01;
pub const MODE_PAGE_FLIP_ASYNC: u32 = 0x02;
pub const MODE_PAGE_FLIP_TARGET_ABSOLUTE: u32 = 0x04;
pub const MODE_PAGE_FLIP_TARGET_RELATIVE: u32 = 0x08;
pub const MODE_PAGE_FLIP_FLAGS: u32 = 0x0f;
pub const MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;
pub const MODE_ATOMIC_NONBLOCK: u32 = 0x0200;
pub const MODE_ATOMIC_ALLOW_MODESET: u32 = 0x0400;
pub const MODE_ATOMIC_FLAGS: u32 = MODE_PAGE_FLIP_EVENT | MODE_PAGE_FLIP_ASYNC | 0x0700;
pub const MODE_FB_INTERLACED: u32 = 1 << 0;
pub const MODE_FB_MODIFIERS: u32 = 1 << 1;
pub const MODE_CURSOR_BO: u32 = 0x01;
pub const MODE_CURSOR_MOVE: u32 = 0x02;

pub const VBLANK_ABSOLUTE: u32 = 0x0;
pub const VBLANK_RELATIVE: u32 = 0x1;
pub const VBLANK_HIGH_CRTC_MASK: u32 = 0x0000_003e;
pub const VBLANK_EVENT: u32 = 0x0400_0000;
pub const VBLANK_FLIP: u32 = 0x0800_0000;
pub const VBLANK_NEXTONMISS: u32 = 0x1000_0000;
pub const VBLANK_SECONDARY: u32 = 0x2000_0000;
pub const VBLANK_SIGNAL: u32 = 0x4000_0000;
pub const VBLANK_TYPES_MASK: u32 = VBLANK_ABSOLUTE | VBLANK_RELATIVE;
pub const VBLANK_FLAGS_MASK: u32 = VBLANK_EVENT | VBLANK_SIGNAL | VBLANK_SECONDARY | VBLANK_NEXTONMISS;

// Event types delivered by read().
pub const EVENT_VBLANK: u32 = 0x01;
pub const EVENT_FLIP_COMPLETE: u32 = 0x02;
pub const EVENT_CRTC_SEQUENCE: u32 = 0x03;

// Pixel formats (fourcc_code(a,b,c,d) = a | b<<8 | c<<16 | d<<24).
pub const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}
pub const FORMAT_XRGB8888: u32 = fourcc(b'X', b'R', b'2', b'4');
pub const FORMAT_ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');
pub const FORMAT_XBGR8888: u32 = fourcc(b'X', b'B', b'2', b'4');
pub const FORMAT_ABGR8888: u32 = fourcc(b'A', b'B', b'2', b'4');
pub const FORMAT_RGB565: u32 = fourcc(b'R', b'G', b'1', b'6');
pub const FORMAT_RGB888: u32 = fourcc(b'R', b'G', b'2', b'4');
pub const FORMAT_MOD_LINEAR: u64 = 0;
pub const FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

// ---------------------------------------------------------- ioctl numbers

macro_rules! nrs {
    ($($name:ident = $nr:expr),* $(,)?) => { $( pub const $name: u32 = $nr; )* };
}

/// `DRM_COMMAND_BASE`: where driver ioctls start; the KMS ones sit at 0xA0+
/// (the core range), see drm_mode.h.
pub const COMMAND_BASE: u32 = 0x40;

// Core ioctl numbers (nr only).
nrs! {
    NR_VERSION = 0x00, NR_GET_UNIQUE = 0x01, NR_GET_MAGIC = 0x02, NR_SET_VERSION = 0x07,
    NR_MODESET_CTL = 0x08, NR_GEM_CLOSE = 0x09, NR_GEM_FLINK = 0x0a, NR_GEM_OPEN = 0x0b,
    NR_GET_CAP = 0x0c, NR_SET_CLIENT_CAP = 0x0d, NR_SET_UNIQUE = 0x10, NR_AUTH_MAGIC = 0x11,
    NR_SET_MASTER = 0x1e, NR_DROP_MASTER = 0x1f, NR_WAIT_VBLANK = 0x3a,
    NR_PRIME_HANDLE_TO_FD = 0x2d, NR_PRIME_FD_TO_HANDLE = 0x2e,
}

// KMS ioctl numbers.
nrs! {
    NR_MODE_GETRESOURCES = 0xA0, NR_MODE_GETCRTC = 0xA1, NR_MODE_SETCRTC = 0xA2,
    NR_MODE_CURSOR = 0xA3, NR_MODE_GETGAMMA = 0xA4, NR_MODE_SETGAMMA = 0xA5,
    NR_MODE_GETENCODER = 0xA6, NR_MODE_GETCONNECTOR = 0xA7, NR_MODE_ATTACHMODE = 0xA8,
    NR_MODE_DETACHMODE = 0xA9, NR_MODE_GETPROPERTY = 0xAA, NR_MODE_SETPROPERTY = 0xAB,
    NR_MODE_GETPROPBLOB = 0xAC, NR_MODE_GETFB = 0xAD, NR_MODE_ADDFB = 0xAE, NR_MODE_RMFB = 0xAF,
    NR_MODE_PAGE_FLIP = 0xB0, NR_MODE_DIRTYFB = 0xB1, NR_MODE_CREATE_DUMB = 0xB2,
    NR_MODE_MAP_DUMB = 0xB3, NR_MODE_DESTROY_DUMB = 0xB4, NR_MODE_GETPLANERESOURCES = 0xB5,
    NR_MODE_GETPLANE = 0xB6, NR_MODE_SETPLANE = 0xB7, NR_MODE_ADDFB2 = 0xB8,
    NR_MODE_OBJ_GETPROPERTIES = 0xB9, NR_MODE_OBJ_SETPROPERTY = 0xBA, NR_MODE_CURSOR2 = 0xBB,
    NR_MODE_ATOMIC = 0xBC, NR_MODE_CREATEPROPBLOB = 0xBD, NR_MODE_DESTROYPROPBLOB = 0xBE,
}

/// Fully encoded ioctl numbers, the values libdrm passes to `ioctl(2)`.
pub mod ioctls {
    use super::*;
    use core::mem::size_of;

    pub const VERSION: u32 = drm_iowr(NR_VERSION, size_of::<Version>());
    pub const GET_UNIQUE: u32 = drm_iowr(NR_GET_UNIQUE, size_of::<Unique>());
    pub const GET_MAGIC: u32 = drm_ior(NR_GET_MAGIC, size_of::<Auth>());
    pub const SET_VERSION: u32 = drm_iowr(NR_SET_VERSION, size_of::<SetVersion>());
    pub const MODESET_CTL: u32 = drm_iow(NR_MODESET_CTL, size_of::<ModesetCtl>());
    pub const GEM_CLOSE: u32 = drm_iow(NR_GEM_CLOSE, size_of::<GemClose>());
    pub const GEM_FLINK: u32 = drm_iowr(NR_GEM_FLINK, size_of::<GemFlink>());
    pub const GEM_OPEN: u32 = drm_iowr(NR_GEM_OPEN, size_of::<GemOpen>());
    pub const GET_CAP: u32 = drm_iowr(NR_GET_CAP, size_of::<GetCap>());
    pub const SET_CLIENT_CAP: u32 = drm_iow(NR_SET_CLIENT_CAP, size_of::<SetClientCap>());
    pub const SET_UNIQUE: u32 = drm_iow(NR_SET_UNIQUE, size_of::<Unique>());
    pub const AUTH_MAGIC: u32 = drm_iow(NR_AUTH_MAGIC, size_of::<Auth>());
    pub const SET_MASTER: u32 = drm_io(NR_SET_MASTER);
    pub const DROP_MASTER: u32 = drm_io(NR_DROP_MASTER);
    pub const WAIT_VBLANK: u32 = drm_iowr(NR_WAIT_VBLANK, size_of::<WaitVblank>());
    pub const PRIME_HANDLE_TO_FD: u32 = drm_iowr(NR_PRIME_HANDLE_TO_FD, size_of::<PrimeHandle>());
    pub const PRIME_FD_TO_HANDLE: u32 = drm_iowr(NR_PRIME_FD_TO_HANDLE, size_of::<PrimeHandle>());

    pub const MODE_GETRESOURCES: u32 = drm_iowr(NR_MODE_GETRESOURCES, size_of::<CardRes>());
    pub const MODE_GETCRTC: u32 = drm_iowr(NR_MODE_GETCRTC, size_of::<Crtc>());
    pub const MODE_SETCRTC: u32 = drm_iowr(NR_MODE_SETCRTC, size_of::<Crtc>());
    pub const MODE_CURSOR: u32 = drm_iowr(NR_MODE_CURSOR, size_of::<Cursor>());
    pub const MODE_GETGAMMA: u32 = drm_iowr(NR_MODE_GETGAMMA, size_of::<CrtcLut>());
    pub const MODE_SETGAMMA: u32 = drm_iowr(NR_MODE_SETGAMMA, size_of::<CrtcLut>());
    pub const MODE_GETENCODER: u32 = drm_iowr(NR_MODE_GETENCODER, size_of::<GetEncoder>());
    pub const MODE_GETCONNECTOR: u32 = drm_iowr(NR_MODE_GETCONNECTOR, size_of::<GetConnector>());
    pub const MODE_GETPROPERTY: u32 = drm_iowr(NR_MODE_GETPROPERTY, size_of::<GetProperty>());
    pub const MODE_SETPROPERTY: u32 = drm_iowr(NR_MODE_SETPROPERTY, size_of::<ConnectorSetProperty>());
    pub const MODE_GETPROPBLOB: u32 = drm_iowr(NR_MODE_GETPROPBLOB, size_of::<GetBlob>());
    pub const MODE_GETFB: u32 = drm_iowr(NR_MODE_GETFB, size_of::<FbCmd>());
    pub const MODE_ADDFB: u32 = drm_iowr(NR_MODE_ADDFB, size_of::<FbCmd>());
    pub const MODE_RMFB: u32 = drm_iowr(NR_MODE_RMFB, size_of::<u32>());
    pub const MODE_PAGE_FLIP: u32 = drm_iowr(NR_MODE_PAGE_FLIP, size_of::<CrtcPageFlip>());
    pub const MODE_DIRTYFB: u32 = drm_iowr(NR_MODE_DIRTYFB, size_of::<FbDirtyCmd>());
    pub const MODE_CREATE_DUMB: u32 = drm_iowr(NR_MODE_CREATE_DUMB, size_of::<CreateDumb>());
    pub const MODE_MAP_DUMB: u32 = drm_iowr(NR_MODE_MAP_DUMB, size_of::<MapDumb>());
    pub const MODE_DESTROY_DUMB: u32 = drm_iowr(NR_MODE_DESTROY_DUMB, size_of::<DestroyDumb>());
    pub const MODE_GETPLANERESOURCES: u32 = drm_iowr(NR_MODE_GETPLANERESOURCES, size_of::<GetPlaneRes>());
    pub const MODE_GETPLANE: u32 = drm_iowr(NR_MODE_GETPLANE, size_of::<GetPlane>());
    pub const MODE_SETPLANE: u32 = drm_iowr(NR_MODE_SETPLANE, size_of::<SetPlane>());
    pub const MODE_ADDFB2: u32 = drm_iowr(NR_MODE_ADDFB2, size_of::<FbCmd2>());
    pub const MODE_OBJ_GETPROPERTIES: u32 = drm_iowr(NR_MODE_OBJ_GETPROPERTIES, size_of::<ObjGetProperties>());
    pub const MODE_OBJ_SETPROPERTY: u32 = drm_iowr(NR_MODE_OBJ_SETPROPERTY, size_of::<ObjSetProperty>());
    pub const MODE_CURSOR2: u32 = drm_iowr(NR_MODE_CURSOR2, size_of::<Cursor2>());
    pub const MODE_ATOMIC: u32 = drm_iowr(NR_MODE_ATOMIC, size_of::<Atomic>());
    pub const MODE_CREATEPROPBLOB: u32 = drm_iowr(NR_MODE_CREATEPROPBLOB, size_of::<CreateBlob>());
    pub const MODE_DESTROYPROPBLOB: u32 = drm_iowr(NR_MODE_DESTROYPROPBLOB, size_of::<DestroyBlob>());
}

// -------------------------------------------------- layout assertions
//
// Sizes and the offset of every field that follows padding, checked
// against Linux's x86-64 layout.  The corresponding encoded ioctl numbers
// are pinned again, as literals, by tests/drm_host.rs.

macro_rules! size_is {
    ($t:ty, $n:expr) => {
        const _: () = assert!(size_of::<$t>() == $n, concat!("ABI: size of ", stringify!($t)));
    };
}
macro_rules! off_is {
    ($t:ty, $f:ident, $n:expr) => {
        const _: () = assert!(offset_of!($t, $f) == $n, concat!("ABI: offset of ", stringify!($t), ".", stringify!($f)));
    };
}

size_is!(Version, 64);
off_is!(Version, name_len, 16);
off_is!(Version, date_len, 32);
off_is!(Version, desc_len, 48);
size_is!(Unique, 16);
size_is!(Auth, 4);
size_is!(SetVersion, 16);
size_is!(ModesetCtl, 8);
size_is!(GemClose, 8);
size_is!(GemFlink, 8);
size_is!(GemOpen, 16);
size_is!(GetCap, 16);
size_is!(SetClientCap, 16);
size_is!(PrimeHandle, 12);
size_is!(WaitVblank, 24);
size_is!(Event, 8);
size_is!(EventVblank, 32);
off_is!(EventVblank, user_data, 8);
off_is!(EventVblank, tv_sec, 16);
off_is!(EventVblank, crtc_id, 28);

size_is!(ModeInfo, 68);
off_is!(ModeInfo, hdisplay, 4);
off_is!(ModeInfo, vdisplay, 14);
off_is!(ModeInfo, vrefresh, 24);
off_is!(ModeInfo, flags, 28);
off_is!(ModeInfo, type_, 32);
off_is!(ModeInfo, name, 36);
size_is!(CardRes, 64);
off_is!(CardRes, count_fbs, 32);
size_is!(Crtc, 104);
off_is!(Crtc, crtc_id, 12);
off_is!(Crtc, gamma_size, 28);
off_is!(Crtc, mode_valid, 32);
off_is!(Crtc, mode, 36);
size_is!(CrtcLut, 32);
size_is!(GetEncoder, 20);
size_is!(GetConnector, 80);
off_is!(GetConnector, count_modes, 32);
off_is!(GetConnector, connection, 60);
size_is!(GetProperty, 64);
off_is!(GetProperty, prop_id, 16);
off_is!(GetProperty, name, 24);
off_is!(GetProperty, count_values, 56);
size_is!(PropertyEnum, 40);
size_is!(ConnectorSetProperty, 16);
size_is!(ObjGetProperties, 32);
size_is!(ObjSetProperty, 24);
size_is!(GetBlob, 16);
size_is!(CreateBlob, 16);
size_is!(DestroyBlob, 4);
size_is!(FbCmd, 28);
size_is!(FbCmd2, 104);
off_is!(FbCmd2, handles, 20);
off_is!(FbCmd2, pitches, 36);
off_is!(FbCmd2, offsets, 52);
off_is!(FbCmd2, modifier, 72);
size_is!(FbDirtyCmd, 24);
size_is!(ClipRect, 8);
size_is!(CreateDumb, 32);
off_is!(CreateDumb, handle, 16);
off_is!(CreateDumb, size, 24);
size_is!(MapDumb, 16);
size_is!(DestroyDumb, 4);
size_is!(CrtcPageFlip, 24);
size_is!(GetPlaneRes, 16);
size_is!(GetPlane, 32);
size_is!(SetPlane, 48);
size_is!(Cursor, 28);
size_is!(Cursor2, 36);
size_is!(Atomic, 56);
