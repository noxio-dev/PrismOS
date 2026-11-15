// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// DRM / KMS for PrismOS, written against the Linux userspace ABI (libdrm,
// Xorg's modesetting driver, Mesa's kms_swrast): the ioctl numbers, the
// structures, their padding and the observable behaviour follow
// <drm/drm.h> / <drm/drm_mode.h> -- see uapi.rs, whose layout is pinned by
// compile-time assertions.
//
// Layers
//
//   uapi.rs     the ABI: constants, #[repr(C)] structs, `_IOC` encoding
//   gem.rs      buffer objects (page lists, refcounted through mem/cow.rs)
//   scanout.rs  the one "display": the firmware framebuffer; blit + console
//               hand-over
//   kms.rs      mode-setting ioctls: resources, CRTC / plane / connector /
//               encoder, properties and blobs, framebuffers, page flips,
//               DIRTYFB, atomic
//   mod.rs      this file: device + per-open state, the `drm_ioctl()`
//               dispatcher, the core ioctls (version, caps, master, GEM,
//               vblank), the event queue behind read() / poll()
//
// What the device is: a simpledrm-class driver -- one CRTC, one primary
// plane, one virtual encoder + connector, a single fixed mode (the firmware
// framebuffer's), dumb buffers only.  What it is not: a GPU driver; there is
// no acceleration, no PRIME, no hardware cursor, no gamma LUT and no real
// vertical blank (flips complete on a 60 Hz software clock).
//
// Concurrency: kernel state lives in one `static mut` guarded, like cow.rs,
// by short IRQ-masked sections (`with`).  User-memory copies and the pixel
// copy of a flip always happen *outside* those sections.

pub mod gem;
pub mod kms;
pub mod scanout;
pub mod uapi;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::{Rc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::ptr::addr_of_mut;

use crate::arch::io::IrqGuard;
use crate::boot::Framebuffer;
use gem::Gem;
use uapi::*;

// ------------------------------------------------------------------ errno

pub const ENOENT: i64 = 2;
pub const EINTR: i64 = 4;
pub const ENXIO: i64 = 6;
pub const EAGAIN: i64 = 11;
pub const ENOMEM: i64 = 12;
pub const EACCES: i64 = 13;
pub const EFAULT: i64 = 14;
pub const EBUSY: i64 = 16;
pub const EINVAL: i64 = 22;
pub const ENOTTY: i64 = 25;
pub const ENOSPC: i64 = 28;
pub const ENOSYS: i64 = 38;
pub const EOPNOTSUPP: i64 = 95;

// ------------------------------------------------------------- identity

pub const DRIVER_NAME: &str = "prismfb";
pub const DRIVER_DATE: &str = "20261114";
pub const DRIVER_DESC: &str = "PrismOS framebuffer DRM driver";
pub const DRIVER_MAJOR: i32 = 1;
pub const DRIVER_MINOR: i32 = 0;
pub const DRIVER_PATCH: i32 = 0;
/// The DRM interface version `DRM_IOCTL_SET_VERSION` negotiates (1.4 is
/// what every libdrm since 2008 asks for).
pub const IF_MAJOR: i32 = 1;
pub const IF_MINOR: i32 = 4;
/// What `GET_UNIQUE` answers (Linux gives the platform device name here).
pub const UNIQUE: &str = "prismfb.0";

/// Character device numbers: Linux's DRM major, card0 minor.  (Libdrm only
/// uses them to find /sys/dev/char/226:0 -- see the notes on Xorg.)
pub const DRM_MAJOR: u64 = 226;

// Fixed object ids.  Properties and objects live in separate namespaces on
// the wire (the ioctl's object type disambiguates), so small stable numbers
// are fine; fbs and blobs, which are created at run time, count up from 100.
pub const PLANE_ID: u32 = 40;
pub const CRTC_ID: u32 = 41;
pub const ENCODER_ID: u32 = 42;
pub const CONNECTOR_ID: u32 = 43;
const FIRST_DYNAMIC_ID: u32 = 100;

// Resource limits (the machine is small: 256 MiB).
pub const MAX_HANDLES_PER_FILE: usize = 256;
pub const MAX_FBS: usize = 128;
pub const MAX_BLOBS: usize = 128;
pub const MAX_BLOB_LEN: u32 = 0x10000;
pub const MAX_DIRTY_CLIPS: u32 = 256;

// ---------------------------------------------------------- device state

/// One framebuffer object (`drm_framebuffer`): a view of a GEM object.
pub struct Fb {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub pitch: u32,
    pub offset: u32,
    pub gem: Rc<Gem>,
    /// The open file that created it (RMFB / close semantics).
    pub owner: u32,
}

/// A property blob (`drm_property_blob`).
pub struct Blob {
    pub id: u32,
    pub data: Vec<u8>,
    pub owner: u32,
}

/// Primary plane state (the atomic property values, 16.16 for `src_*`).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaneState {
    pub fb_id: u32,
    pub crtc_id: u32,
    pub crtc_x: i32,
    pub crtc_y: i32,
    pub crtc_w: u32,
    pub crtc_h: u32,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct CrtcState {
    pub active: bool,
    /// Blob id of the mode property (0 = none).  Legacy SETCRTC creates a
    /// kernel-owned blob so atomic readers see a consistent MODE_ID.
    pub mode_blob: u32,
    /// The mode is set (GETCRTC's `mode_valid`).
    pub mode_set: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ConnState {
    pub crtc_id: u32,
    pub dpms: u64,
}

pub struct Device {
    pub native: scanout::Native,
    pub mode: ModeInfo,
    pub next_id: u32,
    pub next_gem: u32,
    pub next_file: u32,
    pub fbs: BTreeMap<u32, Fb>,
    pub blobs: BTreeMap<u32, Blob>,
    /// GEM names handed out by GEM_FLINK.
    pub flink: BTreeMap<u32, Weak<Gem>>,
    pub plane: PlaneState,
    pub crtc: CrtcState,
    pub conn: ConnState,
    /// Kernel-owned blob holding the native mode: the MODE_ID legacy
    /// SETCRTC installs.
    pub mode_blob: u32,
    /// File id of the current DRM master, if any.
    pub master: Option<u32>,
    pub open_files: u32,
}

static mut DEV: Option<Device> = None;

/// Run `f` on the device with interrupts masked.  None when no device
/// exists (no usable framebuffer).  Never nest, never copy user memory or
/// blit inside.
pub fn with<R>(f: impl FnOnce(&mut Device) -> R) -> Option<R> {
    let _g = IrqGuard::new();
    // SAFETY: interrupts are masked and callers do not nest, so this is the
    // only live reference to the device.
    unsafe { (*addr_of_mut!(DEV)).as_mut().map(f) }
}

/// Is there a DRM device (is /dev/dri/card0 openable)?
pub fn present() -> bool {
    with(|_| ()).is_some()
}

/// True while a client's buffer owns the screen (console.rs asks).
pub fn scanout_owned() -> bool {
    scanout::owned()
}

/// Register the device on the boot framebuffer.  Returns false when the
/// panel cannot be driven (no framebuffer / not 32 bpp): there is then no
/// /dev/dri/card0 and `open` answers -ENODEV.
pub fn init(fb: Framebuffer) -> bool {
    let Some(native) = scanout::Native::new(fb) else { return false };
    let mode = scanout::native_mode(fb.width, fb.height);
    let mut blobs = BTreeMap::new();
    let mode_blob = FIRST_DYNAMIC_ID;
    blobs.insert(mode_blob, Blob { id: mode_blob, data: as_bytes(&mode).to_vec(), owner: 0 });
    let dev = Device {
        native,
        mode,
        mode_blob,
        next_id: FIRST_DYNAMIC_ID + 1,
        next_gem: 1,
        next_file: 1,
        fbs: BTreeMap::new(),
        blobs,
        flink: BTreeMap::new(),
        plane: PlaneState::default(),
        crtc: CrtcState::default(),
        conn: ConnState { crtc_id: 0, dpms: DPMS_ON },
        master: None,
        open_files: 0,
    };
    let _g = IrqGuard::new();
    // SAFETY: boot-time, single caller, interrupts masked.
    unsafe { *addr_of_mut!(DEV) = Some(dev) };
    true
}

impl Device {
    pub fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(FIRST_DYNAMIC_ID);
        id
    }
}

// ------------------------------------------------------- per-open state

/// A queued event, delivered by `read()` once its time has come.
#[derive(Clone, Copy)]
pub struct PendingEvent {
    pub type_: u32,
    pub user_data: u64,
    pub crtc_id: u32,
    pub sequence: u32,
    /// Monotonic ms at which the vblank this event reports happens.
    pub due_ms: u64,
}

pub struct FileState {
    pub id: u32,
    pub master: bool,
    pub authenticated: bool,
    pub magic: u32,
    pub universal_planes: bool,
    pub atomic: bool,
    pub stereo: bool,
    pub aspect_ratio: bool,
    pub writeback: bool,
    pub handles: BTreeMap<u32, Rc<Gem>>,
    pub next_handle: u32,
    pub events: VecDeque<PendingEvent>,
}

/// One open of /dev/dri/card0 (what the fd layer wraps in an `Rc`).
pub struct DrmFile {
    pub st: RefCell<FileState>,
}

/// `open("/dev/dri/card0")`.  The first opener while nobody is master
/// becomes master (Linux: `drm_master_open`).  Err = -errno.
pub fn open() -> Result<DrmFile, i64> {
    let st = with(|d| {
        let id = d.next_file;
        d.next_file = d.next_file.wrapping_add(1).max(1);
        d.open_files += 1;
        let master = d.master.is_none();
        if master {
            d.master = Some(id);
        }
        FileState {
            id,
            master,
            authenticated: master,
            magic: 0,
            universal_planes: false,
            atomic: false,
            stereo: false,
            aspect_ratio: false,
            writeback: false,
            handles: BTreeMap::new(),
            next_handle: 1,
            events: VecDeque::new(),
        }
    });
    match st {
        Some(st) => Ok(DrmFile { st: RefCell::new(st) }),
        None => Err(-19), // ENODEV
    }
}

impl Drop for DrmFile {
    /// Last close of the description (`drm_release`): the file's
    /// framebuffers and blobs go, a CRTC left showing one of them is turned
    /// off, mastership is released and -- when nobody else keeps the device
    /// open -- the console gets the screen back.
    fn drop(&mut self) {
        let id = self.st.borrow().id;
        let was_master = self.st.borrow().master;
        // Handles drop with the FileState below; take them out under the
        // guard so the Rc decrements are serialized with everything else.
        let handles = core::mem::take(&mut self.st.borrow_mut().handles);
        let release_screen = with(|d| {
            kms::release_file(d, id);
            if was_master && d.master == Some(id) {
                d.master = None;
            }
            d.open_files = d.open_files.saturating_sub(1);
            drop(handles);
            !d.crtc.active && d.plane.fb_id == 0
        });
        if release_screen == Some(true) {
            scanout::set_owned(false);
        }
    }
}

// ----------------------------------------------------- user-memory glue

pub(crate) fn user_read(ptr: u64, len: usize) -> Result<Vec<u8>, i64> {
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(len).map_err(|_| -ENOMEM)?;
    v.resize(len, 0);
    if len > 0 && !crate::user::fd::copy_in(ptr, 0, &mut v) {
        return Err(-EFAULT);
    }
    Ok(v)
}

pub(crate) fn user_write(ptr: u64, data: &[u8]) -> bool {
    data.is_empty() || crate::user::fd::copy_out(ptr, 0, data)
}

/// Linux's `drm_copy_field`: copy up to `len` bytes of `value` (no NUL), and
/// report the full length back through the struct.
fn copy_field(ptr: u64, len: &mut u64, value: &str) -> i64 {
    let n = (*len as usize).min(value.len());
    if n > 0 && !user_write(ptr, &value.as_bytes()[..n]) {
        return -EFAULT;
    }
    *len = value.len() as u64;
    0
}

// ------------------------------------------------------------- time

pub const REFRESH_HZ: u64 = 60;

pub fn now_ms() -> u64 {
    crate::hw::pit::uptime_ms()
}
pub fn vblank_seq(ms: u64) -> u64 {
    ms * REFRESH_HZ / 1000
}
/// Time of the vblank numbered `seq` (rounded up to a whole ms).
pub fn vblank_time_ms(seq: u64) -> u64 {
    (seq * 1000).div_ceil(REFRESH_HZ)
}
/// (sequence, time) of the first vblank strictly after `ms`.
pub fn next_vblank(ms: u64) -> (u64, u64) {
    let s = vblank_seq(ms) + 1;
    (s, vblank_time_ms(s))
}

// ----------------------------------------------------------- dispatch

/// Signature of an ioctl implementation: the (zero-extended) argument
/// structure in kernel memory, filled in place for the copy-back.
pub type Handler = fn(&Rc<DrmFile>, &mut [u8]) -> i64;

pub struct IoctlDef {
    pub nr: u32,
    /// The kernel's own definition of the direction (`IOC_WRITE` = userspace
    /// passes data in, `IOC_READ` = kernel writes it back).
    pub dir: u32,
    pub size: usize,
    /// DRM_MASTER: -EACCES unless the caller is master.
    pub master: bool,
    pub func: Handler,
}

macro_rules! def {
    ($nr:expr, $dir:expr, $ty:ty, $master:expr, $f:path) => {
        IoctlDef { nr: $nr, dir: $dir, size: core::mem::size_of::<$ty>(), master: $master, func: $f }
    };
}
const RW: u32 = IOC_READ | IOC_WRITE;

/// The ioctl table; the flags mirror `drm_ioctls[]` in drm_ioctl.c.
pub static IOCTLS: &[IoctlDef] = &[
    // ---- core
    def!(NR_VERSION, RW, Version, false, ioctl_version),
    def!(NR_GET_UNIQUE, RW, Unique, false, ioctl_get_unique),
    def!(NR_GET_MAGIC, IOC_READ, Auth, false, ioctl_get_magic),
    def!(NR_SET_VERSION, RW, SetVersion, false, ioctl_set_version),
    def!(NR_MODESET_CTL, IOC_WRITE, ModesetCtl, false, ioctl_modeset_ctl),
    def!(NR_GEM_CLOSE, IOC_WRITE, GemClose, false, ioctl_gem_close),
    def!(NR_GEM_FLINK, RW, GemFlink, false, ioctl_gem_flink),
    def!(NR_GEM_OPEN, RW, GemOpen, false, ioctl_gem_open),
    def!(NR_GET_CAP, RW, GetCap, false, ioctl_get_cap),
    def!(NR_SET_CLIENT_CAP, IOC_WRITE, SetClientCap, false, ioctl_set_client_cap),
    def!(NR_SET_UNIQUE, IOC_WRITE, Unique, true, ioctl_set_unique),
    def!(NR_AUTH_MAGIC, IOC_WRITE, Auth, false, ioctl_auth_magic),
    def!(NR_SET_MASTER, 0, (), false, ioctl_set_master),
    def!(NR_DROP_MASTER, 0, (), false, ioctl_drop_master),
    def!(NR_WAIT_VBLANK, RW, WaitVblank, false, ioctl_wait_vblank),
    def!(NR_PRIME_HANDLE_TO_FD, RW, PrimeHandle, false, ioctl_no_prime),
    def!(NR_PRIME_FD_TO_HANDLE, RW, PrimeHandle, false, ioctl_no_prime),
    // ---- KMS
    def!(NR_MODE_GETRESOURCES, RW, CardRes, false, kms::getresources),
    def!(NR_MODE_GETCRTC, RW, Crtc, false, kms::getcrtc),
    def!(NR_MODE_SETCRTC, RW, Crtc, true, kms::setcrtc),
    def!(NR_MODE_CURSOR, RW, Cursor, true, kms::cursor),
    def!(NR_MODE_GETGAMMA, RW, CrtcLut, false, kms::no_gamma),
    def!(NR_MODE_SETGAMMA, RW, CrtcLut, true, kms::no_gamma),
    def!(NR_MODE_GETENCODER, RW, GetEncoder, false, kms::getencoder),
    def!(NR_MODE_GETCONNECTOR, RW, GetConnector, false, kms::getconnector),
    def!(NR_MODE_ATTACHMODE, RW, (), true, kms::obsolete),
    def!(NR_MODE_DETACHMODE, RW, (), true, kms::obsolete),
    def!(NR_MODE_GETPROPERTY, RW, GetProperty, false, kms::getproperty),
    def!(NR_MODE_SETPROPERTY, RW, ConnectorSetProperty, true, kms::connector_setproperty),
    def!(NR_MODE_GETPROPBLOB, RW, GetBlob, false, kms::getpropblob),
    def!(NR_MODE_GETFB, RW, FbCmd, false, kms::getfb),
    def!(NR_MODE_ADDFB, RW, FbCmd, false, kms::addfb),
    def!(NR_MODE_RMFB, RW, u32, false, kms::rmfb),
    def!(NR_MODE_PAGE_FLIP, RW, CrtcPageFlip, true, kms::page_flip),
    def!(NR_MODE_DIRTYFB, RW, FbDirtyCmd, false, kms::dirtyfb),
    def!(NR_MODE_CREATE_DUMB, RW, CreateDumb, false, kms::create_dumb),
    def!(NR_MODE_MAP_DUMB, RW, MapDumb, false, kms::map_dumb),
    def!(NR_MODE_DESTROY_DUMB, RW, DestroyDumb, false, kms::destroy_dumb),
    def!(NR_MODE_GETPLANERESOURCES, RW, GetPlaneRes, false, kms::getplaneresources),
    def!(NR_MODE_GETPLANE, RW, GetPlane, false, kms::getplane),
    def!(NR_MODE_SETPLANE, RW, SetPlane, true, kms::setplane),
    def!(NR_MODE_ADDFB2, RW, FbCmd2, false, kms::addfb2),
    def!(NR_MODE_OBJ_GETPROPERTIES, RW, ObjGetProperties, false, kms::obj_getproperties),
    def!(NR_MODE_OBJ_SETPROPERTY, RW, ObjSetProperty, true, kms::obj_setproperty),
    def!(NR_MODE_CURSOR2, RW, Cursor2, true, kms::cursor),
    def!(NR_MODE_ATOMIC, RW, Atomic, true, kms::atomic),
    def!(NR_MODE_CREATEPROPBLOB, RW, CreateBlob, false, kms::create_blob),
    def!(NR_MODE_DESTROYPROPBLOB, RW, DestroyBlob, false, kms::destroy_blob),
];

/// Largest ioctl argument we will copy (every structure above is < 128).
const MAX_IOCTL_SIZE: usize = 1024;

/// `ioctl(fd, cmd, arg)` on a DRM descriptor, following `drm_ioctl()`:
///   * a request whose type is not 'd' is -ENOTTY;
///   * an unknown number is -EINVAL;
///   * the *kernel's* table decides the direction; the user's size field
///     only says how many bytes to read, and a short argument is
///     zero-extended (the extensibility rule that lets old libdrm talk to a
///     new kernel);
///   * DRM_MASTER ioctls from a non-master are -EACCES.
pub fn ioctl(file: &Rc<DrmFile>, cmd: u32, arg: u64) -> i64 {
    if ioc_type(cmd) != DRM_IOCTL_BASE {
        return -ENOTTY;
    }
    let nr = ioc_nr(cmd);
    let Some(def) = IOCTLS.iter().find(|d| d.nr == nr) else { return -EINVAL };
    let mut in_size = ioc_size(cmd);
    let mut out_size = def.size;
    if cmd_dir_in(cmd) == 0 || def.dir & IOC_WRITE == 0 {
        in_size = 0;
    }
    if cmd_dir_out(cmd) == 0 || def.dir & IOC_READ == 0 {
        out_size = 0;
    }
    let ksize = in_size.max(out_size).max(def.size);
    if ksize > MAX_IOCTL_SIZE {
        return -EINVAL;
    }
    if def.master && !file.st.borrow().master {
        return -EACCES;
    }
    let mut kdata = vec![0u8; ksize];
    if in_size > 0 {
        match user_read(arg, in_size) {
            Ok(v) => kdata[..in_size].copy_from_slice(&v),
            Err(e) => return e,
        }
    }
    let ret = (def.func)(file, &mut kdata[..def.size]);
    if out_size > 0 && !user_write(arg, &kdata[..out_size]) {
        return -EFAULT;
    }
    ret
}

fn cmd_dir_in(cmd: u32) -> u32 {
    ioc_dir(cmd) & IOC_WRITE
}
fn cmd_dir_out(cmd: u32) -> u32 {
    ioc_dir(cmd) & IOC_READ
}

// ---------------------------------------------------------- core ioctls

fn ioctl_version(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut v: Version = from_bytes(b);
    v.version_major = DRIVER_MAJOR;
    v.version_minor = DRIVER_MINOR;
    v.version_patchlevel = DRIVER_PATCH;
    for (ptr, len, s) in [
        (v.name, &mut v.name_len, DRIVER_NAME),
        (v.date, &mut v.date_len, DRIVER_DATE),
        (v.desc, &mut v.desc_len, DRIVER_DESC),
    ] {
        let r = copy_field(ptr, len, s);
        if r < 0 {
            return r;
        }
    }
    to_bytes(&v, b);
    0
}

fn ioctl_get_unique(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut u: Unique = from_bytes(b);
    let r = copy_field(u.unique, &mut u.unique_len, UNIQUE);
    if r < 0 {
        return r;
    }
    to_bytes(&u, b);
    0
}

/// SET_UNIQUE: the bus id is fixed for a platform device; a master that
/// tries to set one gets what Linux answers once it is set.
fn ioctl_set_unique(_f: &Rc<DrmFile>, _b: &mut [u8]) -> i64 {
    -EBUSY
}

fn ioctl_get_magic(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut st = f.st.borrow_mut();
    if st.magic == 0 {
        // Any non-zero value identifies the file; the id is unique.
        st.magic = st.id.wrapping_mul(0x9E37_79B1) | 1;
    }
    to_bytes(&Auth { magic: st.magic }, b);
    0
}

/// AUTH_MAGIC: authenticate the file that owns the magic.  (Authentication
/// gates nothing here -- every client is trusted, the system is single-user
/// -- but libdrm's `drmAuthMagic` handshake must succeed.)
fn ioctl_auth_magic(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let a: Auth = from_bytes(b);
    if a.magic == 0 {
        return -EINVAL;
    }
    0
}

fn ioctl_set_version(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut sv: SetVersion = from_bytes(b);
    // Reply with what we are, then validate what was asked (drm_setversion).
    let (di_major, di_minor, dd_major, dd_minor) =
        (sv.drm_di_major, sv.drm_di_minor, sv.drm_dd_major, sv.drm_dd_minor);
    sv.drm_di_major = IF_MAJOR;
    sv.drm_di_minor = IF_MINOR;
    sv.drm_dd_major = DRIVER_MAJOR;
    sv.drm_dd_minor = DRIVER_MINOR;
    to_bytes(&sv, b);
    if di_major != -1 && (di_major != IF_MAJOR || di_minor < 0 || di_minor > IF_MINOR) {
        return -EINVAL;
    }
    if dd_major != -1 && (dd_major != DRIVER_MAJOR || dd_minor < 0 || dd_minor > DRIVER_MINOR) {
        return -EINVAL;
    }
    0
}

/// MODESET_CTL: a legacy (UMS) vblank-enable handshake; accepted and
/// ignored (Linux does the same for KMS drivers).
fn ioctl_modeset_ctl(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let m: ModesetCtl = from_bytes(b);
    if m.crtc != 0 || m.cmd > 1 {
        return -EINVAL;
    }
    0
}

fn ioctl_no_prime(_f: &Rc<DrmFile>, _b: &mut [u8]) -> i64 {
    -ENOSYS
}

fn ioctl_get_cap(_f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut c: GetCap = from_bytes(b);
    let (w, h) = with(|d| (d.native.width(), d.native.height())).unwrap_or((0, 0));
    c.value = match c.capability {
        CAP_DUMB_BUFFER => 1,
        CAP_VBLANK_HIGH_CRTC => 1,
        CAP_DUMB_PREFERRED_DEPTH => 24,
        // Scanout is a copy: X is told to render into a shadow and flush.
        CAP_DUMB_PREFER_SHADOW => 1,
        CAP_PRIME => 0,
        CAP_TIMESTAMP_MONOTONIC => 1,
        CAP_ASYNC_PAGE_FLIP => 0,
        CAP_CURSOR_WIDTH => 64.min(w as u64).max(1),
        CAP_CURSOR_HEIGHT => 64.min(h as u64).max(1),
        CAP_ADDFB2_MODIFIERS => 0,
        CAP_PAGE_FLIP_TARGET => 0,
        CAP_CRTC_IN_VBLANK_EVENT => 1,
        CAP_SYNCOBJ | CAP_SYNCOBJ_TIMELINE | CAP_ATOMIC_ASYNC_PAGE_FLIP => 0,
        _ => return -EINVAL,
    };
    to_bytes(&c, b);
    0
}

fn ioctl_set_client_cap(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let c: SetClientCap = from_bytes(b);
    let mut st = f.st.borrow_mut();
    match c.capability {
        CLIENT_CAP_STEREO_3D => {
            if c.value > 1 {
                return -EINVAL;
            }
            // No stereo modes exist; Linux accepts the request regardless.
            st.stereo = c.value != 0;
        }
        CLIENT_CAP_UNIVERSAL_PLANES => {
            if c.value > 1 {
                return -EINVAL;
            }
            st.universal_planes = c.value != 0;
        }
        CLIENT_CAP_ATOMIC => {
            if c.value > 2 {
                return -EINVAL;
            }
            st.atomic = c.value != 0;
            if st.atomic {
                // Atomic implies universal planes (and aspect ratio).
                st.universal_planes = true;
                st.aspect_ratio = true;
            }
        }
        CLIENT_CAP_ASPECT_RATIO => {
            if c.value > 1 {
                return -EINVAL;
            }
            st.aspect_ratio = c.value != 0;
        }
        CLIENT_CAP_WRITEBACK_CONNECTORS => {
            if !st.atomic {
                return -EINVAL;
            }
            if c.value > 1 {
                return -EINVAL;
            }
            st.writeback = c.value != 0;
        }
        // The cursor-plane hotspot cap needs a cursor plane: not here.
        CLIENT_CAP_CURSOR_PLANE_HOTSPOT => return -EOPNOTSUPP,
        _ => return -EINVAL,
    }
    0
}

/// SET_MASTER (`drm_setmaster_ioctl`): a no-op for the current master,
/// -EINVAL while someone else holds it, otherwise the caller takes over.
fn ioctl_set_master(f: &Rc<DrmFile>, _b: &mut [u8]) -> i64 {
    let mut st = f.st.borrow_mut();
    if st.master {
        return 0;
    }
    let id = st.id;
    let took = with(|d| {
        if d.master.is_some() {
            false
        } else {
            d.master = Some(id);
            true
        }
    });
    match took {
        Some(true) => {
            st.master = true;
            st.authenticated = true;
            0
        }
        _ => -EINVAL,
    }
}

/// DROP_MASTER: only the master may; the CRTC keeps showing what it shows.
fn ioctl_drop_master(f: &Rc<DrmFile>, _b: &mut [u8]) -> i64 {
    let mut st = f.st.borrow_mut();
    if !st.master {
        return -EINVAL;
    }
    let id = st.id;
    with(|d| {
        if d.master == Some(id) {
            d.master = None;
        }
    });
    st.master = false;
    0
}

// ------------------------------------------------------------- GEM

/// Install `gem` in the file's handle table, returning the new handle.
pub fn handle_create(f: &Rc<DrmFile>, gem: Rc<Gem>) -> Result<u32, i64> {
    let mut st = f.st.borrow_mut();
    if st.handles.len() >= MAX_HANDLES_PER_FILE {
        return Err(-ENOMEM);
    }
    let h = st.next_handle;
    st.next_handle = st.next_handle.wrapping_add(1).max(1);
    st.handles.insert(h, gem);
    Ok(h)
}

pub fn handle_lookup(f: &Rc<DrmFile>, handle: u32) -> Option<Rc<Gem>> {
    f.st.borrow().handles.get(&handle).cloned()
}

fn ioctl_gem_close(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let c: GemClose = from_bytes(b);
    if c.handle == 0 {
        return -EINVAL;
    }
    let removed = f.st.borrow_mut().handles.remove(&c.handle);
    // The Rc (and the pages, unless a framebuffer or a mapping still holds
    // them) goes with it; drop under the guard like every other decrement.
    match removed {
        Some(g) => {
            with(|_| drop(g));
            0
        }
        None => -EINVAL,
    }
}

fn ioctl_gem_flink(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut fl: GemFlink = from_bytes(b);
    let Some(gem) = handle_lookup(f, fl.handle) else { return -ENOENT };
    let name = gem.id;
    with(|d| {
        d.flink.insert(name, Rc::downgrade(&gem));
    });
    fl.name = name;
    to_bytes(&fl, b);
    0
}

fn ioctl_gem_open(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut o: GemOpen = from_bytes(b);
    let gem = with(|d| d.flink.get(&o.name).and_then(|w| w.upgrade())).flatten();
    let Some(gem) = gem else { return -ENOENT };
    let size = gem.size as u64;
    match handle_create(f, gem) {
        Ok(h) => {
            o.handle = h;
            o.size = size;
            to_bytes(&o, b);
            0
        }
        Err(e) => e,
    }
}

// ------------------------------------------------------------- vblank

fn queue_event(f: &Rc<DrmFile>, ev: PendingEvent) {
    f.st.borrow_mut().events.push_back(ev);
}

/// `DRM_IOCTL_WAIT_VBLANK` against the 60 Hz software clock.
fn ioctl_wait_vblank(f: &Rc<DrmFile>, b: &mut [u8]) -> i64 {
    let mut vbl: WaitVblank = from_bytes(b);
    let t = vbl.type_;
    // Only CRTC 0 exists: the high-CRTC bits and the SECONDARY flag both
    // name another pipe.
    if t & VBLANK_HIGH_CRTC_MASK != 0 || t & VBLANK_SECONDARY != 0 {
        return -EINVAL;
    }
    if t & !(VBLANK_TYPES_MASK | VBLANK_FLAGS_MASK | VBLANK_HIGH_CRTC_MASK) != 0 {
        return -EINVAL;
    }
    if t & VBLANK_SIGNAL != 0 {
        return -EINVAL; // the legacy signal delivery is long gone
    }
    let now = now_ms();
    let cur = vblank_seq(now);
    let mut target = vbl.sequence as u64;
    if t & VBLANK_TYPES_MASK == VBLANK_RELATIVE {
        target = cur.wrapping_add(target);
    } else if t & VBLANK_NEXTONMISS != 0 && target < cur {
        target = cur + 1;
    }
    // The 32-bit sequence counter wraps; compare modulo 2^32 like Linux.
    let target32 = target as u32;
    let missed = (target32.wrapping_sub(cur as u32)) > (1 << 23);

    if t & VBLANK_EVENT != 0 {
        // Queue the reply as an event and answer right away.
        let seq = if missed { cur + 1 } else { target };
        queue_event(
            f,
            PendingEvent {
                type_: EVENT_VBLANK,
                user_data: vbl.a, // `signal` doubles as the user data
                crtc_id: CRTC_ID,
                sequence: seq as u32,
                due_ms: vblank_time_ms(seq).max(now),
            },
        );
        vbl.sequence = cur as u32;
        vbl.a = now / 1000;
        vbl.b = (now % 1000) * 1000;
        to_bytes(&vbl, b);
        return 0;
    }

    // Blocking form: sleep to the target vblank (a missed one returns now).
    if !missed && target > cur {
        let due = vblank_time_ms(target);
        loop {
            let n = now_ms();
            if n >= due {
                break;
            }
            if crate::user::signal::interrupt_pending() {
                return -EINTR;
            }
            crate::user::process::sleep_current((due - n).min(20));
        }
    }
    let at = now_ms();
    vbl.sequence = vblank_seq(at) as u32;
    vbl.a = at / 1000;
    vbl.b = (at % 1000) * 1000;
    to_bytes(&vbl, b);
    0
}

// ------------------------------------------------- events: read / poll

/// A due event, serialized as `struct drm_event_vblank`.
fn event_bytes(e: &PendingEvent) -> [u8; core::mem::size_of::<EventVblank>()] {
    let ev = EventVblank {
        base: Event { type_: e.type_, length: core::mem::size_of::<EventVblank>() as u32 },
        user_data: e.user_data,
        tv_sec: (e.due_ms / 1000) as u32,
        tv_usec: ((e.due_ms % 1000) * 1000) as u32,
        sequence: e.sequence,
        crtc_id: e.crtc_id,
    };
    let mut out = [0u8; core::mem::size_of::<EventVblank>()];
    to_bytes(&ev, &mut out);
    out
}

/// poll(): readable when an event is due.
pub fn readable(f: &Rc<DrmFile>) -> bool {
    let now = now_ms();
    f.st.borrow().events.iter().any(|e| e.due_ms <= now)
}

/// `read()` on the DRM fd: whole events, oldest first; -EINVAL when even the
/// first does not fit (drm_read); blocks until one is due unless O_NONBLOCK.
pub fn read(f: &Rc<DrmFile>, buf: u64, len: usize, nonblock: bool) -> i64 {
    const EV: usize = core::mem::size_of::<EventVblank>();
    let start = now_ms();
    loop {
        let now = now_ms();
        let mut out: Vec<u8> = Vec::new();
        {
            let mut st = f.st.borrow_mut();
            // Sources queue in their own time order (a flip event and a
            // later-targeted vblank event interleave), so scan the whole
            // queue: deliver every due event that fits, oldest first, and
            // keep the rest in place.
            let mut keep: VecDeque<PendingEvent> = VecDeque::with_capacity(st.events.len());
            let mut full = false;
            while let Some(e) = st.events.pop_front() {
                if e.due_ms > now || full {
                    keep.push_back(e);
                    continue;
                }
                if out.len() + EV > len {
                    if out.is_empty() {
                        st.events.push_front(e);
                        while let Some(k) = keep.pop_back() {
                            st.events.push_front(k);
                        }
                        return -EINVAL; // not even one event fits
                    }
                    full = true;
                    keep.push_back(e);
                    continue;
                }
                out.extend_from_slice(&event_bytes(&e));
            }
            st.events = keep;
        }
        if !out.is_empty() {
            return if user_write(buf, &out) { out.len() as i64 } else { -EFAULT };
        }
        if nonblock {
            return -EAGAIN;
        }
        if crate::user::signal::interrupt_pending() {
            return -EINTR;
        }
        // Park until the earliest queued event is due (or a short slice),
        // bounded like every blocking wait in this kernel.
        let next = f.st.borrow().events.iter().map(|e| e.due_ms).min();
        let nap = match next {
            Some(t) => t.saturating_sub(now).clamp(1, 20),
            None => 20,
        };
        if now.saturating_sub(start) > 10_000 {
            return -EAGAIN;
        }
        crate::user::process::sleep_current(nap);
    }
}

// ----------------------------------------------------------------- mmap

/// What `mmap(2)` of the DRM fd resolves to: the object behind a fake
/// offset from MAP_DUMB, restricted to objects this file holds a handle to.
pub fn mmap_lookup(f: &Rc<DrmFile>, offset: u64) -> Option<(Rc<Gem>, usize)> {
    let id = (offset >> 32) as u32;
    let inner = (offset & 0xFFFF_FFFF) as usize;
    let st = f.st.borrow();
    let gem = st.handles.values().find(|g| g.id == id)?.clone();
    if inner >= gem.page_count() * gem::PAGE {
        return None;
    }
    Some((gem, inner))
}
