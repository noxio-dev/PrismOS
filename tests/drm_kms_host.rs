// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side test of the whole DRM / KMS driver: the real drm/{mod,kms,gem,
// scanout,uapi}.rs run against a simulated kernel -- user memory is a byte
// arena, the PMM hands out host pages, the COW table is a map, the clock is
// a counter and the "panel" is a Vec the test inspects.  The tests replay the
// ioctl sequences libdrm / Xorg's modesetting driver / Mesa use, calling
// `drm::ioctl` with encoded request numbers exactly as `ioctl(2)` would.
//
//   rustc --edition 2021 --test tests/drm_kms_host.rs -o /tmp/drm_kms_host \
//     && /tmp/drm_kms_host --test-threads=1

#![allow(dead_code, unused_imports)]

extern crate alloc;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};

// ------------------------------------------------- the simulated kernel

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

mod boot {
    #[derive(Clone, Copy)]
    pub struct Framebuffer {
        pub addr: u64,
        pub width: u32,
        pub height: u32,
        pub pitch: u32,
        pub bpp: u32,
        pub red_shift: u8,
        pub red_bits: u8,
        pub green_shift: u8,
        pub green_bits: u8,
        pub blue_shift: u8,
        pub blue_bits: u8,
        pub reserved: u16,
    }
}

pub static REPAINTS: AtomicUsize = AtomicUsize::new(0);
pub static LIVE_FRAMES: AtomicI64 = AtomicI64::new(0);

mod console {
    pub fn repaint() {
        super::REPAINTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

thread_local! {
    static CLOCK_MS: Cell<u64> = Cell::new(1000);
    static UMEM: RefCell<Vec<u8>> = RefCell::new(Vec::new());
    static COW: RefCell<BTreeMap<u64, u32>> = RefCell::new(BTreeMap::new());
}
const UBASE: u64 = 0x1000_0000;

fn set_clock(ms: u64) {
    CLOCK_MS.with(|c| c.set(ms));
}
fn advance(ms: u64) {
    CLOCK_MS.with(|c| c.set(c.get() + ms));
}

mod hw {
    pub mod pit {
        pub fn uptime_ms() -> u64 {
            super::super::CLOCK_MS.with(|c| c.get())
        }
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
        pub fn sleep_current(ms: u64) {
            super::super::advance(ms.max(1));
        }
    }
}

mod mem {
    use std::sync::atomic::Ordering;
    pub mod cow {
        /// Same contract as the kernel's mem/cow.rs: absent = one owner.
        pub fn track(phys: u64) -> bool {
            super::super::COW.with(|c| {
                let mut c = c.borrow_mut();
                let e = c.entry(phys).or_insert(1);
                *e += 1;
                true
            })
        }
        pub fn release(phys: u64) -> bool {
            super::super::COW.with(|c| {
                let mut c = c.borrow_mut();
                match c.get(&phys).copied() {
                    None | Some(1) => {
                        c.remove(&phys);
                        true
                    }
                    Some(2) => {
                        c.remove(&phys);
                        false
                    }
                    Some(n) => {
                        c.insert(phys, n - 1);
                        false
                    }
                }
            })
        }
    }
    pub mod pmm {
        use super::super::LIVE_FRAMES;
        use std::sync::atomic::{AtomicU64, Ordering};
        pub struct Stats {
            pub free_pages: AtomicU64,
        }
        pub fn stats_snapshot() -> Stats {
            Stats { free_pages: AtomicU64::new(1 << 20) }
        }
        pub fn alloc_page() -> Option<u64> {
            let l = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
            // SAFETY: non-zero layout.
            let p = unsafe { std::alloc::alloc_zeroed(l) };
            if p.is_null() {
                return None;
            }
            LIVE_FRAMES.fetch_add(1, Ordering::SeqCst);
            Some(p as u64)
        }
        pub fn free_page(p: u64) {
            let l = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
            // SAFETY: allocated by alloc_page with the same layout.
            unsafe { std::alloc::dealloc(p as *mut u8, l) };
            LIVE_FRAMES.fetch_sub(1, Ordering::SeqCst);
        }
    }
    pub mod vmm {
        pub fn phys_access(p: u64) -> Option<u64> {
            Some(p)
        }
    }
}

#[path = "../sys/kern/src/drm/mod.rs"]
pub mod drm;

use boot::Framebuffer;
use drm::uapi::*;
use drm::{DrmFile, CONNECTOR_ID, CRTC_ID, ENCODER_ID, PLANE_ID};

// ----------------------------------------------------------- test tools

fn umem_alloc(n: usize) -> u64 {
    UMEM.with(|m| {
        let mut m = m.borrow_mut();
        let at = (m.len() + 15) & !15;
        m.resize(at + n.max(1), 0);
        UBASE + at as u64
    })
}
fn umem_write(addr: u64, b: &[u8]) {
    assert!(user::fd::copy_out(addr, 0, b));
}
fn umem_read(addr: u64, n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    assert!(user::fd::copy_in(addr, 0, &mut v));
    v
}
fn umem_u32s(vals: &[u32]) -> u64 {
    let a = umem_alloc(vals.len() * 4);
    let mut b = Vec::new();
    for v in vals {
        b.extend_from_slice(&v.to_le_bytes());
    }
    umem_write(a, &b);
    a
}
fn read_u32s(addr: u64, n: usize) -> Vec<u32> {
    umem_read(addr, n * 4).chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}
fn read_u64s(addr: u64, n: usize) -> Vec<u64> {
    umem_read(addr, n * 8).chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

/// `ioctl(fd, cmd, &arg)`.
fn call<T: Pod>(f: &Rc<DrmFile>, cmd: u32, arg: &mut T) -> i64 {
    let n = core::mem::size_of::<T>();
    let addr = umem_alloc(n);
    umem_write(addr, as_bytes(arg));
    let r = drm::ioctl(f, cmd, addr);
    *arg = from_bytes(&umem_read(addr, n));
    r
}

const W: u32 = 64;
const H: u32 = 48;
const PITCH: u32 = 256;

struct Rig {
    panel: Vec<u8>,
}

/// A fresh device over a 64x48 B8G8R8X8 panel.
fn rig() -> Rig {
    set_clock(1000);
    REPAINTS.store(0, Ordering::SeqCst);
    let mut r = Rig { panel: vec![0u8; (PITCH * H) as usize] };
    let ok = drm::init(Framebuffer {
        addr: r.panel.as_mut_ptr() as u64,
        width: W,
        height: H,
        pitch: PITCH,
        bpp: 32,
        red_shift: 16,
        red_bits: 8,
        green_shift: 8,
        green_bits: 8,
        blue_shift: 0,
        blue_bits: 8,
        reserved: 0,
    });
    assert!(ok);
    r
}

impl Rig {
    fn px(&self, x: u32, y: u32) -> u32 {
        let o = (y * PITCH + x * 4) as usize;
        u32::from_le_bytes(self.panel[o..o + 4].try_into().unwrap())
    }
}

fn open_file() -> Rc<DrmFile> {
    Rc::new(drm::open().expect("open"))
}

/// CREATE_DUMB + fill with `color` + ADDFB2; returns (handle, fb_id).
fn dumb_fb(f: &Rc<DrmFile>, color: u32) -> (u32, u32) {
    let mut cd = CreateDumb { width: W, height: H, bpp: 32, ..Default::default() };
    assert_eq!(call(f, ioctls::MODE_CREATE_DUMB, &mut cd), 0);
    assert_eq!(cd.pitch, PITCH);
    let gem = drm::handle_lookup(f, cd.handle).unwrap();
    for i in 0..gem.page_count() {
        let p = gem.page_phys(i).unwrap() as *mut u32;
        for k in 0..1024 {
            // SAFETY: a whole host page we own.
            unsafe { p.add(k).write(color) };
        }
    }
    let mut fb = FbCmd2 { width: W, height: H, pixel_format: FORMAT_XRGB8888, ..Default::default() };
    fb.handles[0] = cd.handle;
    fb.pitches[0] = cd.pitch;
    assert_eq!(call(f, ioctls::MODE_ADDFB2, &mut fb), 0);
    (cd.handle, fb.fb_id)
}

fn native_mode(f: &Rc<DrmFile>) -> ModeInfo {
    let mut modes = vec![ModeInfo::default()];
    let m_addr = umem_alloc(core::mem::size_of::<ModeInfo>());
    let mut c = GetConnector { connector_id: CONNECTOR_ID, count_modes: 1, modes_ptr: m_addr, ..Default::default() };
    assert_eq!(call(f, ioctls::MODE_GETCONNECTOR, &mut c), 0);
    from_bytes(&umem_read(m_addr, core::mem::size_of::<ModeInfo>()))
}

fn setcrtc(f: &Rc<DrmFile>, fb_id: u32) -> i64 {
    let mode = native_mode(f);
    let conns = umem_u32s(&[CONNECTOR_ID]);
    let mut c = Crtc {
        crtc_id: CRTC_ID,
        fb_id,
        mode_valid: 1,
        mode,
        count_connectors: 1,
        set_connectors_ptr: conns,
        ..Default::default()
    };
    call(f, ioctls::MODE_SETCRTC, &mut c)
}

fn read_events(f: &Rc<DrmFile>, nonblock: bool) -> Vec<EventVblank> {
    let buf = umem_alloc(32 * 8);
    let n = drm::read(f, buf, 32 * 8, nonblock);
    assert!(n >= 0, "read -> {n}");
    umem_read(buf, n as usize).chunks(32).map(from_bytes::<EventVblank>).collect()
}

const E: i64 = 1;
fn errno(e: i64) -> i64 {
    -e
}

// ---------------------------------------------------- open / master / auth

#[test]
fn first_open_is_master_and_only_one() {
    let _r = rig();
    let a = open_file();
    let b = open_file();
    assert!(a.st.borrow().master);
    assert!(!b.st.borrow().master);
    // A modeset ioctl from the non-master is -EACCES...
    let mut c = Crtc { crtc_id: CRTC_ID, ..Default::default() };
    assert_eq!(call(&b, ioctls::MODE_SETCRTC, &mut c), errno(drm::EACCES));
    // ...but reading state is open to everyone.
    let mut g = Crtc { crtc_id: CRTC_ID, ..Default::default() };
    assert_eq!(call(&b, ioctls::MODE_GETCRTC, &mut g), 0);
}

#[test]
fn set_master_drop_master_semantics() {
    let _r = rig();
    let a = open_file();
    let b = open_file();
    let mut nothing = ();
    let z = |f: &Rc<DrmFile>, cmd: u32| drm::ioctl(f, cmd, 0);
    assert_eq!(z(&b, ioctls::SET_MASTER), errno(drm::EINVAL)); // someone else is master
    assert_eq!(z(&b, ioctls::DROP_MASTER), errno(drm::EINVAL)); // not master
    assert_eq!(z(&a, ioctls::SET_MASTER), 0); // already master: no-op
    assert_eq!(z(&a, ioctls::DROP_MASTER), 0);
    assert_eq!(z(&a, ioctls::DROP_MASTER), errno(drm::EINVAL));
    assert_eq!(z(&b, ioctls::SET_MASTER), 0); // free now
    assert!(b.st.borrow().master && !a.st.borrow().master);
}

#[test]
fn closing_the_master_frees_mastership() {
    let _r = rig();
    let a = open_file();
    drop(a);
    let b = open_file();
    assert!(b.st.borrow().master, "a new opener becomes master after the old one closed");
}

#[test]
fn dispatcher_errors_follow_drm_ioctl() {
    let _r = rig();
    let f = open_file();
    assert_eq!(drm::ioctl(&f, 0x5401, 0), errno(drm::ENOTTY)); // TCGETS: not our magic
    assert_eq!(drm::ioctl(&f, ioc(IOC_READ | IOC_WRITE, b'd' as u32, 0x77, 8), 0), errno(drm::EINVAL));
    // Bad user pointer for an IN argument.
    assert_eq!(drm::ioctl(&f, ioctls::GET_CAP, 0xdead_0000), errno(drm::EFAULT));
}

#[test]
fn short_argument_is_zero_extended() {
    // An old libdrm that only knows the first 8 bytes of create_dumb: the
    // request number carries size 8, the kernel zero-fills the rest.
    let _r = rig();
    let f = open_file();
    let a = umem_alloc(32);
    let mut raw = [0u8; 8];
    raw[0..4].copy_from_slice(&H.to_le_bytes());
    raw[4..8].copy_from_slice(&W.to_le_bytes());
    umem_write(a, &raw);
    // bpp is beyond the 8 bytes, so it reads as 0 -> EINVAL (not garbage).
    let cmd = ioc(IOC_READ | IOC_WRITE, b'd' as u32, NR_MODE_CREATE_DUMB, 8);
    assert_eq!(drm::ioctl(&f, cmd, a), errno(drm::EINVAL));
}

// ------------------------------------------------------- version & caps

#[test]
fn version_is_two_pass_like_drm_copy_field() {
    let _r = rig();
    let f = open_file();
    // Pass 1: lengths only.
    let mut v = Version::default();
    assert_eq!(call(&f, ioctls::VERSION, &mut v), 0);
    assert_eq!((v.version_major, v.version_minor), (1, 0));
    assert_eq!(v.name_len as usize, drm::DRIVER_NAME.len());
    assert_eq!(v.date_len as usize, drm::DRIVER_DATE.len());
    // Pass 2: buffers; no NUL is written.
    let nb = umem_alloc(64);
    umem_write(nb, &[0xAA; 64]);
    let mut v2 = Version { name_len: 64, name: nb, ..Default::default() };
    assert_eq!(call(&f, ioctls::VERSION, &mut v2), 0);
    let got = umem_read(nb, drm::DRIVER_NAME.len() + 1);
    assert_eq!(&got[..drm::DRIVER_NAME.len()], drm::DRIVER_NAME.as_bytes());
    assert_eq!(got[drm::DRIVER_NAME.len()], 0xAA);
    // A short buffer: truncated copy, full length reported.
    let mut v3 = Version { name_len: 3, name: nb, ..Default::default() };
    assert_eq!(call(&f, ioctls::VERSION, &mut v3), 0);
    assert_eq!(v3.name_len as usize, drm::DRIVER_NAME.len());
}

#[test]
fn caps_match_what_xorg_expects_from_a_dumb_driver() {
    let _r = rig();
    let f = open_file();
    let cap = |c: u64| {
        let mut g = GetCap { capability: c, value: 99 };
        let r = call(&f, ioctls::GET_CAP, &mut g);
        (r, g.value)
    };
    assert_eq!(cap(CAP_DUMB_BUFFER), (0, 1));
    assert_eq!(cap(CAP_DUMB_PREFERRED_DEPTH), (0, 24));
    assert_eq!(cap(CAP_DUMB_PREFER_SHADOW), (0, 1));
    assert_eq!(cap(CAP_PRIME), (0, 0));
    assert_eq!(cap(CAP_TIMESTAMP_MONOTONIC), (0, 1));
    assert_eq!(cap(CAP_ASYNC_PAGE_FLIP), (0, 0));
    assert_eq!(cap(CAP_ADDFB2_MODIFIERS), (0, 0));
    assert_eq!(cap(0x7777).0, errno(drm::EINVAL));
}

#[test]
fn client_caps() {
    let _r = rig();
    let f = open_file();
    let set = |c: u64, v: u64| {
        let mut s = SetClientCap { capability: c, value: v };
        call(&f, ioctls::SET_CLIENT_CAP, &mut s)
    };
    assert_eq!(set(CLIENT_CAP_WRITEBACK_CONNECTORS, 1), errno(drm::EINVAL)); // needs atomic first
    assert_eq!(set(CLIENT_CAP_UNIVERSAL_PLANES, 1), 0);
    assert_eq!(set(CLIENT_CAP_UNIVERSAL_PLANES, 2), errno(drm::EINVAL));
    assert_eq!(set(CLIENT_CAP_ATOMIC, 1), 0);
    assert!(f.st.borrow().atomic && f.st.borrow().universal_planes);
    assert_eq!(set(CLIENT_CAP_WRITEBACK_CONNECTORS, 1), 0);
    assert_eq!(set(CLIENT_CAP_CURSOR_PLANE_HOTSPOT, 1), errno(drm::EOPNOTSUPP));
    assert_eq!(set(0x99, 1), errno(drm::EINVAL));
}

#[test]
fn set_version_negotiation() {
    let _r = rig();
    let f = open_file();
    let mut sv = SetVersion { drm_di_major: 1, drm_di_minor: 4, drm_dd_major: -1, drm_dd_minor: -1 };
    assert_eq!(call(&f, ioctls::SET_VERSION, &mut sv), 0);
    assert_eq!((sv.drm_di_major, sv.drm_di_minor), (1, 4));
    assert_eq!((sv.drm_dd_major, sv.drm_dd_minor), (1, 0));
    let mut bad = SetVersion { drm_di_major: 2, drm_di_minor: 0, drm_dd_major: -1, drm_dd_minor: -1 };
    assert_eq!(call(&f, ioctls::SET_VERSION, &mut bad), errno(drm::EINVAL));
}

// --------------------------------------------------------- KMS resources

#[test]
fn getresources_two_pass() {
    let _r = rig();
    let f = open_file();
    let mut r = CardRes::default();
    assert_eq!(call(&f, ioctls::MODE_GETRESOURCES, &mut r), 0);
    assert_eq!((r.count_crtcs, r.count_connectors, r.count_encoders, r.count_fbs), (1, 1, 1, 0));
    assert_eq!((r.min_width, r.max_width, r.min_height, r.max_height), (0, W, 0, H));
    let crtcs = umem_alloc(4);
    let conns = umem_alloc(4);
    let encs = umem_alloc(4);
    let mut r2 = CardRes {
        count_crtcs: 1,
        crtc_id_ptr: crtcs,
        count_connectors: 1,
        connector_id_ptr: conns,
        count_encoders: 1,
        encoder_id_ptr: encs,
        ..Default::default()
    };
    assert_eq!(call(&f, ioctls::MODE_GETRESOURCES, &mut r2), 0);
    assert_eq!(read_u32s(crtcs, 1), [CRTC_ID]);
    assert_eq!(read_u32s(conns, 1), [CONNECTOR_ID]);
    assert_eq!(read_u32s(encs, 1), [ENCODER_ID]);
    // A too-small array is left alone but the real count comes back.
    let tiny = umem_alloc(4);
    umem_write(tiny, &[0xEE; 4]);
    let mut r3 = CardRes { count_fbs: 0, fb_id_ptr: tiny, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETRESOURCES, &mut r3), 0);
    assert_eq!(umem_read(tiny, 4), [0xEE; 4]);
}

#[test]
fn getconnector_encoder_and_unknown_ids() {
    let _r = rig();
    let f = open_file();
    let mut c = GetConnector { connector_id: CONNECTOR_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETCONNECTOR, &mut c), 0);
    assert_eq!((c.count_modes, c.count_encoders), (1, 1));
    assert_eq!(c.count_props, 3, "legacy client: DPMS, EDID, non-desktop");
    assert_eq!(c.connection, MODE_CONNECTED);
    assert_eq!(c.connector_type, MODE_CONNECTOR_VIRTUAL);
    assert_eq!((c.mm_width, c.mm_height), (17, 13)); // 64x48 px @ 96 dpi
    assert_eq!(c.encoder_id, 0, "nothing scans out yet");
    let mode = native_mode(&f);
    assert_eq!((mode.hdisplay, mode.vdisplay), (W as u16, H as u16));

    let mut bad = GetConnector { connector_id: 999, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETCONNECTOR, &mut bad), errno(drm::ENOENT));

    let mut e = GetEncoder { encoder_id: ENCODER_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETENCODER, &mut e), 0);
    assert_eq!((e.encoder_type, e.possible_crtcs, e.crtc_id), (MODE_ENCODER_VIRTUAL, 1, 0));
    let mut be = GetEncoder { encoder_id: 5, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETENCODER, &mut be), errno(drm::ENOENT));
}

#[test]
fn atomic_clients_see_atomic_properties() {
    let _r = rig();
    let f = open_file();
    let mut s = SetClientCap { capability: CLIENT_CAP_ATOMIC, value: 1 };
    assert_eq!(call(&f, ioctls::SET_CLIENT_CAP, &mut s), 0);
    let mut c = GetConnector { connector_id: CONNECTOR_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETCONNECTOR, &mut c), 0);
    assert_eq!(c.count_props, 4, "CRTC_ID shows up for atomic clients");

    let mut o = ObjGetProperties { obj_id: PLANE_ID, obj_type: OBJECT_PLANE, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_OBJ_GETPROPERTIES, &mut o), 0);
    assert_eq!(o.count_props, 11);
    let mut o2 = ObjGetProperties { obj_id: CRTC_ID, obj_type: OBJECT_CRTC, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_OBJ_GETPROPERTIES, &mut o2), 0);
    assert_eq!(o2.count_props, 2);
    // The legacy view of the same plane has only the immutable `type`.
    let g = open_file();
    let mut ol = ObjGetProperties { obj_id: PLANE_ID, obj_type: OBJECT_PLANE, ..Default::default() };
    assert_eq!(call(&g, ioctls::MODE_OBJ_GETPROPERTIES, &mut ol), 0);
    assert_eq!(ol.count_props, 1);
    let mut wrong = ObjGetProperties { obj_id: 12345, obj_type: OBJECT_PLANE, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_OBJ_GETPROPERTIES, &mut wrong), errno(drm::ENOENT));
}

#[test]
fn getproperty_enum_and_range_shapes() {
    let _r = rig();
    let f = open_file();
    // DPMS: an enum with four named values.
    let mut p = GetProperty { prop_id: drm::kms::P_DPMS, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETPROPERTY, &mut p), 0);
    assert_eq!(p.flags, PROP_ENUM);
    assert_eq!(&p.name[..4], b"DPMS");
    assert_eq!((p.count_values, p.count_enum_blobs), (4, 4));
    let evals = umem_alloc(4 * 8);
    let eenums = umem_alloc(4 * 40);
    let mut p2 = GetProperty {
        prop_id: drm::kms::P_DPMS,
        count_values: 4,
        values_ptr: evals,
        count_enum_blobs: 4,
        enum_blob_ptr: eenums,
        ..Default::default()
    };
    assert_eq!(call(&f, ioctls::MODE_GETPROPERTY, &mut p2), 0);
    assert_eq!(read_u64s(evals, 4), [0, 1, 2, 3]);
    let off = umem_read(eenums + 3 * 40, 40);
    assert_eq!(u64::from_le_bytes(off[0..8].try_into().unwrap()), 3);
    assert_eq!(&off[8..11], b"Off");
    // CRTC_ID of the connector: an atomic object reference to a CRTC.
    let mut q = GetProperty { prop_id: drm::kms::P_CONN_CRTC_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETPROPERTY, &mut q), 0);
    assert_eq!(q.flags, PROP_ATOMIC | PROP_OBJECT);
    assert_eq!(q.count_values, 1);
    let mut nope = GetProperty { prop_id: 4242, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETPROPERTY, &mut nope), errno(drm::ENOENT));
}

#[test]
fn plane_is_hidden_from_legacy_clients() {
    let _r = rig();
    let f = open_file();
    let mut pr = GetPlaneRes::default();
    assert_eq!(call(&f, ioctls::MODE_GETPLANERESOURCES, &mut pr), 0);
    assert_eq!(pr.count_planes, 0);
    let mut gp = GetPlane { plane_id: PLANE_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETPLANE, &mut gp), errno(drm::ENOENT));

    let mut s = SetClientCap { capability: CLIENT_CAP_UNIVERSAL_PLANES, value: 1 };
    assert_eq!(call(&f, ioctls::SET_CLIENT_CAP, &mut s), 0);
    let mut pr = GetPlaneRes::default();
    assert_eq!(call(&f, ioctls::MODE_GETPLANERESOURCES, &mut pr), 0);
    assert_eq!(pr.count_planes, 1);
    let fmts = umem_alloc(8);
    let mut gp = GetPlane { plane_id: PLANE_ID, count_format_types: 2, format_type_ptr: fmts, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETPLANE, &mut gp), 0);
    assert_eq!(gp.possible_crtcs, 1);
    assert_eq!(read_u32s(fmts, 2), [FORMAT_XRGB8888, FORMAT_ARGB8888]);
}

// ---------------------------------------------------- dumb buffers & GEM

#[test]
fn dumb_buffer_lifecycle_and_no_page_leak() {
    let _r = rig();
    let before = LIVE_FRAMES.load(Ordering::SeqCst);
    {
        let f = open_file();
        let mut cd = CreateDumb { width: 100, height: 30, bpp: 32, ..Default::default() };
        assert_eq!(call(&f, ioctls::MODE_CREATE_DUMB, &mut cd), 0);
        assert_eq!(cd.pitch, 448); // 400 bytes aligned up to 64
        assert_eq!(cd.size, 4096 * 4); // 448*30 = 13440 -> 4 pages
        assert!(cd.handle != 0);
        let mut md = MapDumb { handle: cd.handle, ..Default::default() };
        assert_eq!(call(&f, ioctls::MODE_MAP_DUMB, &mut md), 0);
        assert!(md.offset != 0 && md.offset & 0xFFF == 0);
        // The offset resolves, for this file, to the object...
        let (gem, inner) = drm::mmap_lookup(&f, md.offset).expect("lookup");
        assert_eq!((gem.size, inner), (4096 * 4, 0));
        assert!(drm::mmap_lookup(&f, md.offset + (5 << 12)).is_none()); // past the end
        // ...but another open cannot map it.
        let g = open_file();
        assert!(drm::mmap_lookup(&g, md.offset).is_none());
        let mut bad = MapDumb { handle: 77, ..Default::default() };
        assert_eq!(call(&f, ioctls::MODE_MAP_DUMB, &mut bad), errno(drm::ENOENT));
        let mut dd = DestroyDumb { handle: cd.handle };
        assert_eq!(call(&f, ioctls::MODE_DESTROY_DUMB, &mut dd), 0);
        assert_eq!(call(&f, ioctls::MODE_DESTROY_DUMB, &mut dd), errno(drm::EINVAL));
    }
    assert_eq!(LIVE_FRAMES.load(Ordering::SeqCst), before, "every frame went back");
}

#[test]
fn create_dumb_rejects_nonsense() {
    let _r = rig();
    let f = open_file();
    for (w, h, bpp) in [(0, 10, 32), (10, 0, 32), (10, 10, 0), (10, 10, 7), (100_000, 10, 32), (4096, 8192, 32)] {
        let mut cd = CreateDumb { width: w, height: h, bpp, ..Default::default() };
        assert_eq!(call(&f, ioctls::MODE_CREATE_DUMB, &mut cd), errno(drm::EINVAL), "{w}x{h}x{bpp}");
    }
    let mut cd = CreateDumb { width: 8, height: 8, bpp: 32, flags: 1, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_CREATE_DUMB, &mut cd), errno(drm::EINVAL));
}

#[test]
fn a_mapped_buffer_outlives_its_handle() {
    // The refcount contract mmap relies on: a user mapping holds one `cow`
    // reference per page; closing the handle must not free a mapped frame.
    let _r = rig();
    let before = LIVE_FRAMES.load(Ordering::SeqCst);
    let f = open_file();
    let mut cd = CreateDumb { width: 64, height: 16, bpp: 32, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_CREATE_DUMB, &mut cd), 0);
    let gem = drm::handle_lookup(&f, cd.handle).unwrap();
    let phys0 = gem.page_phys(0).unwrap();
    assert!(mem::cow::track(phys0)); // what mmap_drm does per page
    drop(gem);
    let mut dd = DestroyDumb { handle: cd.handle };
    assert_eq!(call(&f, ioctls::MODE_DESTROY_DUMB, &mut dd), 0);
    // Page 0 is still alive (mapped), the others are gone.
    assert_eq!(LIVE_FRAMES.load(Ordering::SeqCst), before + 1);
    // munmap: the last owner frees it.
    assert!(mem::cow::release(phys0));
    mem::pmm::free_page(phys0);
    assert_eq!(LIVE_FRAMES.load(Ordering::SeqCst), before);
}

#[test]
fn gem_flink_and_open_share_one_object() {
    let _r = rig();
    let a = open_file();
    let b = open_file();
    let mut cd = CreateDumb { width: 16, height: 16, bpp: 32, ..Default::default() };
    assert_eq!(call(&a, ioctls::MODE_CREATE_DUMB, &mut cd), 0);
    let mut fl = GemFlink { handle: cd.handle, name: 0 };
    assert_eq!(call(&a, ioctls::GEM_FLINK, &mut fl), 0);
    assert!(fl.name != 0);
    let mut op = GemOpen { name: fl.name, ..Default::default() };
    assert_eq!(call(&b, ioctls::GEM_OPEN, &mut op), 0);
    assert_eq!(op.size, cd.size);
    let ga = drm::handle_lookup(&a, cd.handle).unwrap();
    let gb = drm::handle_lookup(&b, op.handle).unwrap();
    assert!(Rc::ptr_eq(&ga, &gb));
    let mut bad = GemOpen { name: 0x7777, ..Default::default() };
    assert_eq!(call(&b, ioctls::GEM_OPEN, &mut bad), errno(drm::ENOENT));
    let mut gc = GemClose { handle: op.handle, pad: 0 };
    assert_eq!(call(&b, ioctls::GEM_CLOSE, &mut gc), 0);
    assert_eq!(call(&b, ioctls::GEM_CLOSE, &mut gc), errno(drm::EINVAL));
}

#[test]
fn prime_is_not_offered() {
    let _r = rig();
    let f = open_file();
    let mut p = PrimeHandle::default();
    assert_eq!(call(&f, ioctls::PRIME_HANDLE_TO_FD, &mut p), errno(drm::ENOSYS));
}

// ------------------------------------------------------------ framebuffers

#[test]
fn addfb2_validation() {
    let _r = rig();
    let f = open_file();
    let mut cd = CreateDumb { width: W, height: H, bpp: 32, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_CREATE_DUMB, &mut cd), 0);
    let good = |mut mutate: Box<dyn FnMut(&mut FbCmd2)>| {
        let mut fb = FbCmd2 { width: W, height: H, pixel_format: FORMAT_XRGB8888, ..Default::default() };
        fb.handles[0] = cd.handle;
        fb.pitches[0] = cd.pitch;
        mutate(&mut fb);
        call(&f, ioctls::MODE_ADDFB2, &mut fb)
    };
    assert_eq!(good(Box::new(|_| {})), 0);
    assert_eq!(good(Box::new(|fb| fb.pixel_format = FORMAT_RGB565)), errno(drm::EINVAL));
    assert_eq!(good(Box::new(|fb| fb.width = 0)), errno(drm::EINVAL));
    assert_eq!(good(Box::new(|fb| fb.width = W + 1)), errno(drm::EINVAL)); // beyond scanout size
    assert_eq!(good(Box::new(|fb| fb.pitches[0] = 100)), errno(drm::EINVAL)); // < width*4
    assert_eq!(good(Box::new(|fb| fb.offsets[0] = 4096)), errno(drm::EINVAL)); // object too small
    assert_eq!(good(Box::new(|fb| fb.handles[0] = 9999)), errno(drm::ENOENT));
    assert_eq!(good(Box::new(|fb| fb.handles[1] = cd.handle)), errno(drm::EINVAL)); // 1-plane format
    assert_eq!(good(Box::new(|fb| fb.flags = MODE_FB_MODIFIERS)), errno(drm::EINVAL));
    assert_eq!(good(Box::new(|fb| fb.flags = 0x40)), errno(drm::EINVAL));
}

#[test]
fn legacy_addfb_depth_mapping_and_getfb() {
    let _r = rig();
    let f = open_file();
    let mut cd = CreateDumb { width: W, height: H, bpp: 32, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_CREATE_DUMB, &mut cd), 0);
    let mut fb = FbCmd { width: W, height: H, pitch: cd.pitch, bpp: 32, depth: 24, handle: cd.handle, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_ADDFB, &mut fb), 0);
    let id = fb.fb_id;
    let mut bad = FbCmd { width: W, height: H, pitch: cd.pitch, bpp: 16, depth: 16, handle: cd.handle, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_ADDFB, &mut bad), errno(drm::EINVAL));
    let mut g = FbCmd { fb_id: id, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETFB, &mut g), 0);
    assert_eq!((g.width, g.height, g.pitch, g.bpp, g.depth), (W, H, cd.pitch, 32, 24));
    assert!(g.handle != 0 && g.handle != cd.handle, "GETFB returns a fresh handle");
    // GETRESOURCES lists the file's framebuffers.
    let ids = umem_alloc(8);
    let mut r = CardRes { count_fbs: 2, fb_id_ptr: ids, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETRESOURCES, &mut r), 0);
    assert_eq!(r.count_fbs, 1);
    assert_eq!(read_u32s(ids, 1), [id]);
    // Another file does not see it and cannot remove it.
    let other = open_file();
    let mut r2 = CardRes::default();
    assert_eq!(call(&other, ioctls::MODE_GETRESOURCES, &mut r2), 0);
    assert_eq!(r2.count_fbs, 0);
    let mut rm = id;
    let a = umem_alloc(4);
    umem_write(a, &rm.to_le_bytes());
    assert_eq!(drm::ioctl(&other, ioctls::MODE_RMFB, a), errno(drm::ENOENT));
    assert_eq!(drm::ioctl(&f, ioctls::MODE_RMFB, a), 0);
    assert_eq!(drm::ioctl(&f, ioctls::MODE_RMFB, a), errno(drm::ENOENT));
}

// --------------------------------------------------------- mode setting

#[test]
fn setcrtc_puts_pixels_on_the_panel_and_takes_the_screen() {
    let r = rig();
    let f = open_file();
    let (_, fb) = dumb_fb(&f, 0x0012_3456);
    assert!(!drm::scanout_owned());
    assert_eq!(setcrtc(&f, fb), 0);
    assert!(drm::scanout_owned(), "the console lets go of the screen");
    assert_eq!(r.px(0, 0), 0x0012_3456);
    assert_eq!(r.px(W - 1, H - 1), 0x0012_3456);
    // GETCRTC reports it back.
    let mut c = Crtc { crtc_id: CRTC_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETCRTC, &mut c), 0);
    assert_eq!((c.fb_id, c.mode_valid, c.gamma_size), (fb, 1, 0));
    assert_eq!(c.mode.hdisplay, W as u16);
    // The connector and encoder now show as bound.
    let mut gc = GetConnector { connector_id: CONNECTOR_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETCONNECTOR, &mut gc), 0);
    assert_eq!(gc.encoder_id, ENCODER_ID);
}

#[test]
fn setcrtc_error_paths() {
    let _r = rig();
    let f = open_file();
    let (_, fb) = dumb_fb(&f, 1);
    let mode = native_mode(&f);
    let conns = umem_u32s(&[CONNECTOR_ID]);
    let mk = |fb_id: u32, mode_valid: u32, n: u32, crtc: u32, m: ModeInfo| Crtc {
        crtc_id: crtc,
        fb_id,
        mode_valid,
        mode: m,
        count_connectors: n,
        set_connectors_ptr: conns,
        ..Default::default()
    };
    assert_eq!(call(&f, ioctls::MODE_SETCRTC, &mut mk(fb, 1, 1, 999, mode)), errno(drm::ENOENT));
    assert_eq!(call(&f, ioctls::MODE_SETCRTC, &mut mk(9999, 1, 1, CRTC_ID, mode)), errno(drm::ENOENT));
    assert_eq!(call(&f, ioctls::MODE_SETCRTC, &mut mk(fb, 1, 0, CRTC_ID, mode)), errno(drm::EINVAL)); // no connector
    let mut wrong = mode;
    wrong.hdisplay += 8;
    assert_eq!(call(&f, ioctls::MODE_SETCRTC, &mut mk(fb, 1, 1, CRTC_ID, wrong)), errno(drm::EINVAL));
    let badconn = umem_u32s(&[77]);
    let mut c = mk(fb, 1, 1, CRTC_ID, mode);
    c.set_connectors_ptr = badconn;
    assert_eq!(call(&f, ioctls::MODE_SETCRTC, &mut c), errno(drm::ENOENT));
    // x offset that leaves less than a screen of pixels: no room.
    let mut off = mk(fb, 1, 1, CRTC_ID, mode);
    off.x = 8;
    assert_eq!(call(&f, ioctls::MODE_SETCRTC, &mut off), errno(drm::ENOSPC));
    let mut huge = mk(fb, 1, 1, CRTC_ID, mode);
    huge.x = 0x1_0000;
    assert_eq!(call(&f, ioctls::MODE_SETCRTC, &mut huge), errno(drm::kms::ERANGE));
}

#[test]
fn disabling_the_crtc_gives_the_console_back() {
    let _r = rig();
    let f = open_file();
    let (_, fb) = dumb_fb(&f, 5);
    assert_eq!(setcrtc(&f, fb), 0);
    let before = REPAINTS.load(Ordering::SeqCst);
    let mut off = Crtc { crtc_id: CRTC_ID, ..Default::default() }; // mode_valid 0, fb 0, no connectors
    assert_eq!(call(&f, ioctls::MODE_SETCRTC, &mut off), 0);
    assert!(!drm::scanout_owned());
    assert_eq!(REPAINTS.load(Ordering::SeqCst), before + 1);
    let mut c = Crtc { crtc_id: CRTC_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETCRTC, &mut c), 0);
    assert_eq!((c.fb_id, c.mode_valid), (0, 0));
}

#[test]
fn page_flip_swaps_buffers_and_delivers_an_event() {
    let r = rig();
    let f = open_file();
    let (_, a) = dumb_fb(&f, 0x00AA_0000);
    let (_, b) = dumb_fb(&f, 0x0000_BB00);
    // Flipping before anything is on screen: -EBUSY (nothing to flip from).
    let mut early = CrtcPageFlip { crtc_id: CRTC_ID, fb_id: a, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_PAGE_FLIP, &mut early), errno(drm::EBUSY));
    assert_eq!(setcrtc(&f, a), 0);
    assert_eq!(r.px(3, 3), 0x00AA_0000);

    set_clock(2000);
    let mut pf = CrtcPageFlip { crtc_id: CRTC_ID, fb_id: b, flags: MODE_PAGE_FLIP_EVENT, user_data: 0xC0FFEE, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_PAGE_FLIP, &mut pf), 0);
    assert_eq!(r.px(3, 3), 0x0000_BB00, "the new frame is on the panel");
    // The completion event is not readable until the next vblank (16.7 ms)...
    assert!(!drm::readable(&f));
    assert_eq!(drm::read(&f, umem_alloc(32), 32, true), errno(drm::EAGAIN));
    advance(17);
    assert!(drm::readable(&f));
    let ev = read_events(&f, true);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].base.type_, EVENT_FLIP_COMPLETE);
    assert_eq!(ev[0].base.length, 32);
    assert_eq!(ev[0].user_data, 0xC0FFEE);
    assert_eq!(ev[0].crtc_id, CRTC_ID);
    assert!(ev[0].tv_sec >= 2);
    assert!(!drm::readable(&f), "consumed");

    // Without the EVENT flag: no event at all.
    let mut pf2 = CrtcPageFlip { crtc_id: CRTC_ID, fb_id: a, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_PAGE_FLIP, &mut pf2), 0);
    advance(50);
    assert!(!drm::readable(&f));
    assert_eq!(r.px(3, 3), 0x00AA_0000);
}

#[test]
fn page_flip_argument_checks() {
    let _r = rig();
    let f = open_file();
    let (_, a) = dumb_fb(&f, 1);
    assert_eq!(setcrtc(&f, a), 0);
    let flip = |mutate: &dyn Fn(&mut CrtcPageFlip)| {
        let mut pf = CrtcPageFlip { crtc_id: CRTC_ID, fb_id: a, ..Default::default() };
        mutate(&mut pf);
        call(&f, ioctls::MODE_PAGE_FLIP, &mut pf)
    };
    assert_eq!(flip(&|p| p.flags = 0x80), errno(drm::EINVAL));
    assert_eq!(flip(&|p| p.flags = MODE_PAGE_FLIP_ASYNC), errno(drm::EINVAL)); // cap is 0
    assert_eq!(flip(&|p| p.reserved = 1), errno(drm::EINVAL));
    assert_eq!(flip(&|p| p.crtc_id = 3), errno(drm::ENOENT));
    assert_eq!(flip(&|p| p.fb_id = 555), errno(drm::ENOENT));
}

#[test]
fn dirtyfb_copies_only_the_damaged_rectangle() {
    let r = rig();
    let f = open_file();
    let (handle, fb) = dumb_fb(&f, 0x0011_1111);
    assert_eq!(setcrtc(&f, fb), 0);
    // The client (Xorg's shadow flush) paints into its mapping...
    let gem = drm::handle_lookup(&f, handle).unwrap();
    let rowbytes = PITCH as usize;
    for y in 0..H as usize {
        for x in 0..W as usize {
            let off = y * rowbytes + x * 4;
            let page = gem.page_phys(off / 4096).unwrap() as *mut u8;
            // SAFETY: inside one host page.
            unsafe { (page.add(off % 4096) as *mut u32).write(0x0099_9999) };
        }
    }
    // ...and nothing changes on screen until DIRTYFB.
    assert_eq!(r.px(10, 10), 0x0011_1111);
    let clips = umem_alloc(8);
    let rect = ClipRect { x1: 8, y1: 4, x2: 16, y2: 12 };
    umem_write(clips, as_bytes(&rect));
    let mut d = FbDirtyCmd { fb_id: fb, num_clips: 1, clips_ptr: clips, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_DIRTYFB, &mut d), 0);
    assert_eq!(r.px(8, 4), 0x0099_9999);
    assert_eq!(r.px(15, 11), 0x0099_9999);
    assert_eq!(r.px(16, 11), 0x0011_1111, "just outside the clip");
    assert_eq!(r.px(7, 4), 0x0011_1111);
    assert_eq!(r.px(8, 12), 0x0011_1111);
    // No clips = the whole buffer.
    let mut all = FbDirtyCmd { fb_id: fb, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_DIRTYFB, &mut all), 0);
    assert_eq!(r.px(0, 0), 0x0099_9999);
    assert_eq!(r.px(W - 1, H - 1), 0x0099_9999);
    // Errors: unknown fb, clips without a pointer, too many clips, bad flags.
    let mut bad = FbDirtyCmd { fb_id: 4242, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_DIRTYFB, &mut bad), errno(drm::ENOENT));
    let mut np = FbDirtyCmd { fb_id: fb, num_clips: 1, clips_ptr: 0, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_DIRTYFB, &mut np), errno(drm::EINVAL));
    let mut many = FbDirtyCmd { fb_id: fb, num_clips: 1000, clips_ptr: clips, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_DIRTYFB, &mut many), errno(drm::EINVAL));
    let mut fl = FbDirtyCmd { fb_id: fb, flags: 0x10, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_DIRTYFB, &mut fl), errno(drm::EINVAL));
}

#[test]
fn dirtyfb_of_a_hidden_buffer_is_a_no_op() {
    let r = rig();
    let f = open_file();
    let (_, shown) = dumb_fb(&f, 0x0022_2222);
    let (_, hidden) = dumb_fb(&f, 0x00EE_EEEE);
    assert_eq!(setcrtc(&f, shown), 0);
    let mut d = FbDirtyCmd { fb_id: hidden, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_DIRTYFB, &mut d), 0);
    assert_eq!(r.px(0, 0), 0x0022_2222);
}

#[test]
fn removing_the_scanned_out_fb_turns_the_crtc_off() {
    let _r = rig();
    let f = open_file();
    let (_, fb) = dumb_fb(&f, 3);
    assert_eq!(setcrtc(&f, fb), 0);
    let before = REPAINTS.load(Ordering::SeqCst);
    let a = umem_alloc(4);
    umem_write(a, &fb.to_le_bytes());
    assert_eq!(drm::ioctl(&f, ioctls::MODE_RMFB, a), 0);
    assert!(!drm::scanout_owned());
    assert_eq!(REPAINTS.load(Ordering::SeqCst), before + 1);
    let mut c = Crtc { crtc_id: CRTC_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETCRTC, &mut c), 0);
    assert_eq!((c.fb_id, c.mode_valid), (0, 0));
}

#[test]
fn closing_the_file_releases_fbs_master_and_screen() {
    let _r = rig();
    let before_frames = LIVE_FRAMES.load(Ordering::SeqCst);
    {
        let f = open_file();
        let (_, fb) = dumb_fb(&f, 9);
        assert_eq!(setcrtc(&f, fb), 0);
        assert!(drm::scanout_owned());
        let rep = REPAINTS.load(Ordering::SeqCst);
        drop(f); // what exit() / the last close does
        assert!(!drm::scanout_owned());
        assert_eq!(REPAINTS.load(Ordering::SeqCst), rep + 1, "console repainted exactly once");
    }
    assert_eq!(LIVE_FRAMES.load(Ordering::SeqCst), before_frames, "no frame leaked");
    let g = open_file();
    assert!(g.st.borrow().master);
    let mut c = Crtc { crtc_id: CRTC_ID, ..Default::default() };
    assert_eq!(call(&g, ioctls::MODE_GETCRTC, &mut c), 0);
    assert_eq!((c.fb_id, c.mode_valid), (0, 0));
}

#[test]
fn cursor_and_gamma_are_absent_the_way_linux_says_so() {
    let _r = rig();
    let f = open_file();
    let mut cur = Cursor { flags: MODE_CURSOR_BO, crtc_id: CRTC_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_CURSOR, &mut cur), errno(drm::ENXIO));
    let mut cur0 = Cursor { flags: 0, crtc_id: CRTC_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_CURSOR, &mut cur0), errno(drm::EINVAL));
    let mut lut = CrtcLut { crtc_id: CRTC_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETGAMMA, &mut lut), errno(drm::ENOSYS));
}

// ------------------------------------------------- DPMS and properties

#[test]
fn dpms_off_deactivates_and_on_repaints() {
    let r = rig();
    let f = open_file();
    let (handle, fb) = dumb_fb(&f, 0x0044_4444);
    assert_eq!(setcrtc(&f, fb), 0);
    let mut off = ConnectorSetProperty { value: DPMS_OFF, prop_id: drm::kms::P_DPMS, connector_id: CONNECTOR_ID };
    assert_eq!(call(&f, ioctls::MODE_SETPROPERTY, &mut off), 0);
    let mut o = ObjGetProperties { obj_id: CONNECTOR_ID, obj_type: OBJECT_CONNECTOR, ..Default::default() };
    let ids = umem_alloc(3 * 4);
    let vals = umem_alloc(3 * 8);
    o.count_props = 3;
    o.props_ptr = ids;
    o.prop_values_ptr = vals;
    assert_eq!(call(&f, ioctls::MODE_OBJ_GETPROPERTIES, &mut o), 0);
    assert_eq!(read_u32s(ids, 3)[0], drm::kms::P_DPMS);
    assert_eq!(read_u64s(vals, 3)[0], DPMS_OFF);
    // The mode stays set (GETCRTC mode_valid), the CRTC is just inactive;
    // a DIRTYFB while off copies nothing.
    let gem = drm::handle_lookup(&f, handle).unwrap();
    unsafe { (gem.page_phys(0).unwrap() as *mut u32).write(0x00FF_FFFF) };
    let mut d = FbDirtyCmd { fb_id: fb, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_DIRTYFB, &mut d), 0);
    assert_eq!(r.px(0, 0), 0x0044_4444);
    let mut on = ConnectorSetProperty { value: DPMS_ON, prop_id: drm::kms::P_DPMS, connector_id: CONNECTOR_ID };
    assert_eq!(call(&f, ioctls::MODE_SETPROPERTY, &mut on), 0);
    assert_eq!(r.px(0, 0), 0x00FF_FFFF, "turning DPMS on repaints from the plane's buffer");
    // Invalid values and immutable properties.
    let mut bad = ConnectorSetProperty { value: 9, prop_id: drm::kms::P_DPMS, connector_id: CONNECTOR_ID };
    assert_eq!(call(&f, ioctls::MODE_SETPROPERTY, &mut bad), errno(drm::EINVAL));
    let mut imm = ConnectorSetProperty { value: 1, prop_id: drm::kms::P_NON_DESKTOP, connector_id: CONNECTOR_ID };
    assert_eq!(call(&f, ioctls::MODE_SETPROPERTY, &mut imm), errno(drm::EINVAL));
    let mut nc = ConnectorSetProperty { value: 0, prop_id: drm::kms::P_DPMS, connector_id: 3 };
    assert_eq!(call(&f, ioctls::MODE_SETPROPERTY, &mut nc), errno(drm::ENOENT));
}

#[test]
fn property_blobs() {
    let _r = rig();
    let f = open_file();
    let data = umem_alloc(5);
    umem_write(data, b"hello");
    let mut cb = CreateBlob { data, length: 5, blob_id: 0 };
    assert_eq!(call(&f, ioctls::MODE_CREATEPROPBLOB, &mut cb), 0);
    assert!(cb.blob_id != 0);
    let out = umem_alloc(8);
    // drm_mode_getblob copies only when the length matches exactly.
    let mut gb = GetBlob { blob_id: cb.blob_id, length: 5, data: out };
    assert_eq!(call(&f, ioctls::MODE_GETPROPBLOB, &mut gb), 0);
    assert_eq!(umem_read(out, 5), b"hello");
    let mut sz = GetBlob { blob_id: cb.blob_id, length: 0, data: 0 };
    assert_eq!(call(&f, ioctls::MODE_GETPROPBLOB, &mut sz), 0);
    assert_eq!(sz.length, 5);
    // Ownership: another file cannot destroy it; the owner can, once.
    let other = open_file();
    let mut db = DestroyBlob { blob_id: cb.blob_id };
    assert_eq!(call(&other, ioctls::MODE_DESTROYPROPBLOB, &mut db), errno(drm::EINVAL));
    assert_eq!(call(&f, ioctls::MODE_DESTROYPROPBLOB, &mut db), 0);
    assert_eq!(call(&f, ioctls::MODE_DESTROYPROPBLOB, &mut db), errno(drm::EINVAL));
    let mut gone = GetBlob { blob_id: cb.blob_id, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETPROPBLOB, &mut gone), errno(drm::ENOENT));
    let mut zero = CreateBlob { data, length: 0, blob_id: 0 };
    assert_eq!(call(&f, ioctls::MODE_CREATEPROPBLOB, &mut zero), errno(drm::EINVAL));
    let mut big = CreateBlob { data, length: 0x20000, blob_id: 0 };
    assert_eq!(call(&f, ioctls::MODE_CREATEPROPBLOB, &mut big), errno(drm::EINVAL));
}

// ---------------------------------------------------------------- atomic

fn atomic_commit(f: &Rc<DrmFile>, flags: u32, user_data: u64, reqs: &[(u32, &[(u32, u64)])]) -> i64 {
    let objs: Vec<u32> = reqs.iter().map(|r| r.0).collect();
    let counts: Vec<u32> = reqs.iter().map(|r| r.1.len() as u32).collect();
    let props: Vec<u32> = reqs.iter().flat_map(|r| r.1.iter().map(|p| p.0)).collect();
    let vals: Vec<u64> = reqs.iter().flat_map(|r| r.1.iter().map(|p| p.1)).collect();
    let va = umem_alloc(vals.len().max(1) * 8);
    let mut vb = Vec::new();
    for v in &vals {
        vb.extend_from_slice(&v.to_le_bytes());
    }
    if !vb.is_empty() {
        umem_write(va, &vb);
    }
    let mut a = Atomic {
        flags,
        count_objs: objs.len() as u32,
        objs_ptr: umem_u32s(&objs),
        count_props_ptr: umem_u32s(&counts),
        props_ptr: umem_u32s(&props),
        prop_values_ptr: va,
        user_data,
        ..Default::default()
    };
    call(f, ioctls::MODE_ATOMIC, &mut a)
}

fn atomic_file() -> Rc<DrmFile> {
    let f = open_file();
    let mut s = SetClientCap { capability: CLIENT_CAP_ATOMIC, value: 1 };
    assert_eq!(call(&f, ioctls::SET_CLIENT_CAP, &mut s), 0);
    f
}

fn mode_blob(f: &Rc<DrmFile>) -> u32 {
    let mode = native_mode(f);
    let a = umem_alloc(core::mem::size_of::<ModeInfo>());
    umem_write(a, as_bytes(&mode));
    let mut cb = CreateBlob { data: a, length: core::mem::size_of::<ModeInfo>() as u32, blob_id: 0 };
    assert_eq!(call(f, ioctls::MODE_CREATEPROPBLOB, &mut cb), 0);
    cb.blob_id
}

use drm::kms::*;

#[test]
fn atomic_requires_the_client_cap() {
    let _r = rig();
    let f = open_file();
    assert_eq!(atomic_commit(&f, 0, 0, &[]), errno(drm::EINVAL));
}

#[test]
fn atomic_modeset_commit_test_only_and_event() {
    let r = rig();
    let f = atomic_file();
    let (_, fb) = dumb_fb(&f, 0x0077_7777);
    let blob = mode_blob(&f);
    let (w16, h16) = ((W as u64) << 16, (H as u64) << 16);
    let reqs: [(u32, &[(u32, u64)]); 3] = [
        (CONNECTOR_ID, &[(P_CONN_CRTC_ID, CRTC_ID as u64)]),
        (CRTC_ID, &[(P_MODE_ID, blob as u64), (P_ACTIVE, 1)]),
        (
            PLANE_ID,
            &[
                (P_FB_ID, fb as u64),
                (P_PLANE_CRTC_ID, CRTC_ID as u64),
                (P_SRC_X, 0),
                (P_SRC_Y, 0),
                (P_SRC_W, w16),
                (P_SRC_H, h16),
                (P_CRTC_X, 0),
                (P_CRTC_Y, 0),
                (P_CRTC_W, W as u64),
                (P_CRTC_H, H as u64),
            ],
        ),
    ];
    // A modeset without ALLOW_MODESET is refused...
    assert_eq!(atomic_commit(&f, 0, 0, &reqs), errno(drm::EINVAL));
    // ...TEST_ONLY validates without touching the screen...
    assert_eq!(atomic_commit(&f, MODE_ATOMIC_TEST_ONLY | MODE_ATOMIC_ALLOW_MODESET, 0, &reqs), 0);
    assert_eq!(r.px(0, 0), 0);
    assert!(!drm::scanout_owned());
    // ...and TEST_ONLY + EVENT is a contradiction.
    assert_eq!(
        atomic_commit(&f, MODE_ATOMIC_TEST_ONLY | MODE_PAGE_FLIP_EVENT | MODE_ATOMIC_ALLOW_MODESET, 0, &reqs),
        errno(drm::EINVAL)
    );
    // The real commit.
    set_clock(5000);
    assert_eq!(atomic_commit(&f, MODE_ATOMIC_ALLOW_MODESET | MODE_PAGE_FLIP_EVENT, 0xABCD, &reqs), 0);
    assert!(drm::scanout_owned());
    assert_eq!(r.px(5, 5), 0x0077_7777);
    advance(20);
    let ev = read_events(&f, true);
    assert_eq!(ev.len(), 1);
    assert_eq!((ev[0].base.type_, ev[0].user_data), (EVENT_FLIP_COMPLETE, 0xABCD));

    // Legacy readers agree with the atomic state.
    let mut c = Crtc { crtc_id: CRTC_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETCRTC, &mut c), 0);
    assert_eq!((c.fb_id, c.mode_valid), (fb, 1));

    // A pure flip (only FB_ID) needs no modeset.
    let (_, fb2) = dumb_fb(&f, 0x0033_3333);
    assert_eq!(atomic_commit(&f, MODE_PAGE_FLIP_EVENT, 7, &[(PLANE_ID, &[(P_FB_ID, fb2 as u64)])]), 0);
    assert_eq!(r.px(5, 5), 0x0033_3333);
}

#[test]
fn atomic_check_rejects_impossible_states() {
    let _r = rig();
    let f = atomic_file();
    let (_, fb) = dumb_fb(&f, 1);
    let blob = mode_blob(&f);
    let (w16, h16) = ((W as u64) << 16, (H as u64) << 16);
    let m = MODE_ATOMIC_ALLOW_MODESET | MODE_ATOMIC_TEST_ONLY;
    let plane = |extra: &[(u32, u64)]| -> Vec<(u32, u64)> {
        let mut v = vec![
            (P_FB_ID, fb as u64),
            (P_PLANE_CRTC_ID, CRTC_ID as u64),
            (P_SRC_X, 0),
            (P_SRC_Y, 0),
            (P_SRC_W, w16),
            (P_SRC_H, h16),
            (P_CRTC_X, 0),
            (P_CRTC_Y, 0),
            (P_CRTC_W, W as u64),
            (P_CRTC_H, H as u64),
        ];
        for e in extra {
            v.retain(|x| x.0 != e.0);
            v.push(*e);
        }
        v
    };
    let conn: &[(u32, u64)] = &[(P_CONN_CRTC_ID, CRTC_ID as u64)];
    let crtc: &[(u32, u64)] = &[(P_MODE_ID, blob as u64), (P_ACTIVE, 1)];
    let try_it = |p: Vec<(u32, u64)>| atomic_commit(&f, m, 0, &[(CONNECTOR_ID, conn), (CRTC_ID, crtc), (PLANE_ID, &p)]);
    assert_eq!(try_it(plane(&[])), 0, "the reference state is valid");
    assert_eq!(try_it(plane(&[(P_SRC_W, w16 / 2)])), errno(drm::kms::ERANGE), "no scaling");
    assert_eq!(try_it(plane(&[(P_SRC_W, w16 + 0x8000)])), errno(drm::kms::ERANGE), "no fractional source");
    assert_eq!(try_it(plane(&[(P_CRTC_X, 4)])), errno(drm::EINVAL), "no positioning");
    assert_eq!(try_it(plane(&[(P_SRC_X, 8 << 16)])), errno(drm::ENOSPC), "window outside the fb");
    assert_eq!(try_it(plane(&[(P_FB_ID, 999)])), errno(drm::ENOENT));
    assert_eq!(try_it(plane(&[(P_PLANE_CRTC_ID, 0)])), errno(drm::EINVAL), "fb without CRTC");
    // CRTC enabled but no connector assigned.
    assert_eq!(
        atomic_commit(&f, m, 0, &[(CRTC_ID, crtc), (PLANE_ID, &plane(&[]))]),
        errno(drm::EINVAL)
    );
    // ACTIVE without a mode.
    assert_eq!(atomic_commit(&f, m, 0, &[(CONNECTOR_ID, conn), (CRTC_ID, &[(P_ACTIVE, 1)])]), errno(drm::EINVAL));
    // Unknown property / object / immutable / legacy-only properties.
    assert_eq!(atomic_commit(&f, m, 0, &[(CRTC_ID, &[(999, 1)])]), errno(drm::ENOENT));
    assert_eq!(atomic_commit(&f, m, 0, &[(7777, &[(P_ACTIVE, 1)])]), errno(drm::ENOENT));
    assert_eq!(atomic_commit(&f, m, 0, &[(PLANE_ID, &[(P_PLANE_TYPE, 0)])]), errno(drm::EINVAL));
    assert_eq!(atomic_commit(&f, m, 0, &[(CONNECTOR_ID, &[(P_DPMS, 3)])]), errno(drm::EINVAL));
    assert_eq!(atomic_commit(&f, m, 0, &[(CRTC_ID, &[(P_FB_ID, 3)])]), errno(drm::EINVAL), "prop of another object");
    assert_eq!(atomic_commit(&f, m, 0, &[(CRTC_ID, &[(P_MODE_ID, 31337)])]), errno(drm::ENOENT));
    // Flag validation.
    assert_eq!(atomic_commit(&f, 0x8000_0000, 0, &[]), errno(drm::EINVAL));
    assert_eq!(atomic_commit(&f, MODE_PAGE_FLIP_ASYNC, 0, &[]), errno(drm::EINVAL));
    // A bad mode blob (wrong size) is refused.
    let wrong = {
        let mut m2 = native_mode(&f);
        m2.hdisplay += 8;
        let a = umem_alloc(core::mem::size_of::<ModeInfo>());
        umem_write(a, as_bytes(&m2));
        let mut cb = CreateBlob { data: a, length: core::mem::size_of::<ModeInfo>() as u32, blob_id: 0 };
        assert_eq!(call(&f, ioctls::MODE_CREATEPROPBLOB, &mut cb), 0);
        cb.blob_id
    };
    assert_eq!(
        atomic_commit(&f, m, 0, &[(CONNECTOR_ID, conn), (CRTC_ID, &[(P_MODE_ID, wrong as u64), (P_ACTIVE, 1)])]),
        errno(drm::EINVAL)
    );
}

#[test]
fn atomic_disable_returns_the_screen() {
    let _r = rig();
    let f = atomic_file();
    let (_, fb) = dumb_fb(&f, 6);
    assert_eq!(setcrtc(&f, fb), 0);
    let rep = REPAINTS.load(Ordering::SeqCst);
    let flags = MODE_ATOMIC_ALLOW_MODESET;
    let r = atomic_commit(
        &f,
        flags,
        0,
        &[
            (PLANE_ID, &[(P_FB_ID, 0), (P_PLANE_CRTC_ID, 0)]),
            (CRTC_ID, &[(P_ACTIVE, 0), (P_MODE_ID, 0)]),
            (CONNECTOR_ID, &[(P_CONN_CRTC_ID, 0)]),
        ],
    );
    assert_eq!(r, 0);
    assert!(!drm::scanout_owned());
    assert_eq!(REPAINTS.load(Ordering::SeqCst), rep + 1);
}

#[test]
fn legacy_setplane_follows_the_same_rules() {
    let r = rig();
    let f = open_file();
    let (_, fb) = dumb_fb(&f, 0x0055_5555);
    assert_eq!(setcrtc(&f, fb), 0);
    let (_, fb2) = dumb_fb(&f, 0x0066_6666);
    let mut sp = SetPlane {
        plane_id: PLANE_ID,
        crtc_id: CRTC_ID,
        fb_id: fb2,
        crtc_w: W,
        crtc_h: H,
        src_w: W << 16,
        src_h: H << 16,
        ..Default::default()
    };
    assert_eq!(call(&f, ioctls::MODE_SETPLANE, &mut sp), 0);
    assert_eq!(r.px(1, 1), 0x0066_6666);
    let mut half = sp;
    half.crtc_w = W / 2;
    assert_eq!(call(&f, ioctls::MODE_SETPLANE, &mut half), errno(drm::kms::ERANGE));
    let mut bad = sp;
    bad.plane_id = 9;
    assert_eq!(call(&f, ioctls::MODE_SETPLANE, &mut bad), errno(drm::ENOENT));
    let mut fl = sp;
    fl.flags = 1;
    assert_eq!(call(&f, ioctls::MODE_SETPLANE, &mut fl), errno(drm::EINVAL));
}

// ------------------------------------------------------------- vblank

#[test]
fn wait_vblank_blocks_until_the_next_period() {
    let _r = rig();
    let f = open_file();
    set_clock(10_000);
    let mut v = WaitVblank { type_: VBLANK_RELATIVE, sequence: 1, ..Default::default() };
    assert_eq!(call(&f, ioctls::WAIT_VBLANK, &mut v), 0);
    // 10_000 ms = vblank #600; waiting one period reaches #601 at ~10_017 ms.
    assert_eq!(v.sequence, 601);
    assert!(CLOCK_MS.with(|c| c.get()) >= 10_017);
    assert_eq!(v.a, 10, "tval_sec");
    // Relative 0 returns the current vblank at once.
    let mut now = WaitVblank { type_: VBLANK_RELATIVE, sequence: 0, ..Default::default() };
    let t0 = CLOCK_MS.with(|c| c.get());
    assert_eq!(call(&f, ioctls::WAIT_VBLANK, &mut now), 0);
    assert_eq!(CLOCK_MS.with(|c| c.get()), t0);
    // Other pipes / unknown flags / signal delivery are refused.
    let mut hi = WaitVblank { type_: 1 << 1, ..Default::default() };
    assert_eq!(call(&f, ioctls::WAIT_VBLANK, &mut hi), errno(drm::EINVAL));
    let mut sec = WaitVblank { type_: VBLANK_SECONDARY, ..Default::default() };
    assert_eq!(call(&f, ioctls::WAIT_VBLANK, &mut sec), errno(drm::EINVAL));
    let mut sig = WaitVblank { type_: VBLANK_SIGNAL, ..Default::default() };
    assert_eq!(call(&f, ioctls::WAIT_VBLANK, &mut sig), errno(drm::EINVAL));
    let mut junk = WaitVblank { type_: 0x0000_0100, ..Default::default() };
    assert_eq!(call(&f, ioctls::WAIT_VBLANK, &mut junk), errno(drm::EINVAL));
}

#[test]
fn wait_vblank_event_form_queues_and_returns_immediately() {
    let _r = rig();
    let f = open_file();
    set_clock(20_000);
    let mut v = WaitVblank { type_: VBLANK_RELATIVE | VBLANK_EVENT, sequence: 2, a: 0x1234, ..Default::default() };
    let t0 = CLOCK_MS.with(|c| c.get());
    assert_eq!(call(&f, ioctls::WAIT_VBLANK, &mut v), 0);
    assert_eq!(CLOCK_MS.with(|c| c.get()), t0, "did not block");
    assert!(!drm::readable(&f));
    advance(40);
    let ev = read_events(&f, true);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].base.type_, EVENT_VBLANK);
    assert_eq!(ev[0].user_data, 0x1234);
    assert_eq!(ev[0].sequence as u64, 20_000 * 60 / 1000 + 2);
}

#[test]
fn read_semantics() {
    let _r = rig();
    let f = open_file();
    let (_, a) = dumb_fb(&f, 1);
    let (_, b) = dumb_fb(&f, 2);
    assert_eq!(setcrtc(&f, a), 0);
    assert_eq!(drm::read(&f, umem_alloc(64), 64, true), errno(drm::EAGAIN));
    for fb in [b, a, b] {
        let mut pf = CrtcPageFlip { crtc_id: CRTC_ID, fb_id: fb, flags: MODE_PAGE_FLIP_EVENT, user_data: fb as u64, ..Default::default() };
        assert_eq!(call(&f, ioctls::MODE_PAGE_FLIP, &mut pf), 0);
    }
    advance(20);
    // A buffer smaller than one event is -EINVAL and keeps the events.
    assert_eq!(drm::read(&f, umem_alloc(31), 31, true), errno(drm::EINVAL));
    // Room for two events: exactly two come out, whole, in order.
    let buf = umem_alloc(80);
    assert_eq!(drm::read(&f, buf, 80, true), 64);
    // The third is still queued.
    assert_eq!(read_events(&f, true).len(), 1);
    assert_eq!(drm::read(&f, umem_alloc(64), 64, true), errno(drm::EAGAIN));
}

#[test]
fn blocking_read_waits_for_the_event() {
    let _r = rig();
    let f = open_file();
    let (_, a) = dumb_fb(&f, 1);
    let (_, b) = dumb_fb(&f, 2);
    assert_eq!(setcrtc(&f, a), 0);
    set_clock(30_000);
    let mut pf = CrtcPageFlip { crtc_id: CRTC_ID, fb_id: b, flags: MODE_PAGE_FLIP_EVENT, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_PAGE_FLIP, &mut pf), 0);
    let t0 = CLOCK_MS.with(|c| c.get());
    let ev = read_events(&f, false); // blocks (the stub sleep advances the clock)
    assert_eq!(ev.len(), 1);
    assert!(CLOCK_MS.with(|c| c.get()) > t0);
}

// -------------------------------------------------- the Xorg start-up replay

#[test]
fn xorg_modesetting_startup_sequence() {
    // The calls xf86-video-modesetting makes from probe to the first frame,
    // in order, through the real dispatcher.
    let r = rig();
    let f = open_file();
    // drmSetInterfaceVersion + drmGetVersion + drmGetBusid.
    let mut sv = SetVersion { drm_di_major: 1, drm_di_minor: 4, drm_dd_major: -1, drm_dd_minor: -1 };
    assert_eq!(call(&f, ioctls::SET_VERSION, &mut sv), 0);
    let mut uq = Unique::default();
    assert_eq!(call(&f, ioctls::GET_UNIQUE, &mut uq), 0);
    assert_eq!(uq.unique_len as usize, drm::UNIQUE.len());
    // drmGetCap for the things probe() looks at.
    for c in [CAP_DUMB_BUFFER, CAP_DUMB_PREFERRED_DEPTH, CAP_DUMB_PREFER_SHADOW, CAP_TIMESTAMP_MONOTONIC] {
        let mut g = GetCap { capability: c, value: 0 };
        assert_eq!(call(&f, ioctls::GET_CAP, &mut g), 0);
    }
    // Universal planes (probe asks; atomic is opt-in) and ASPECT_RATIO.
    for (c, v) in [(CLIENT_CAP_UNIVERSAL_PLANES, 1), (CLIENT_CAP_ASPECT_RATIO, 1)] {
        let mut s = SetClientCap { capability: c, value: v };
        assert_eq!(call(&f, ioctls::SET_CLIENT_CAP, &mut s), 0);
    }
    // drmModeGetResources: size, then fetch.
    let mut res = CardRes::default();
    assert_eq!(call(&f, ioctls::MODE_GETRESOURCES, &mut res), 0);
    // drmModeGetConnector: probe pass (counts) then fetch pass.
    let mut c1 = GetConnector { connector_id: CONNECTOR_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETCONNECTOR, &mut c1), 0);
    let enc = umem_alloc(4 * c1.count_encoders as usize);
    let modes = umem_alloc(core::mem::size_of::<ModeInfo>() * c1.count_modes as usize);
    let props = umem_alloc(4 * c1.count_props as usize);
    let pvals = umem_alloc(8 * c1.count_props as usize);
    let mut c2 = GetConnector {
        connector_id: CONNECTOR_ID,
        count_encoders: c1.count_encoders,
        encoders_ptr: enc,
        count_modes: c1.count_modes,
        modes_ptr: modes,
        count_props: c1.count_props,
        props_ptr: props,
        prop_values_ptr: pvals,
        ..Default::default()
    };
    assert_eq!(call(&f, ioctls::MODE_GETCONNECTOR, &mut c2), 0);
    // Each property: GETPROPERTY by id (xf86 reads names to find DPMS, EDID).
    let mut names = Vec::new();
    for id in read_u32s(props, c2.count_props as usize) {
        let mut p = GetProperty { prop_id: id, ..Default::default() };
        assert_eq!(call(&f, ioctls::MODE_GETPROPERTY, &mut p), 0);
        names.push(String::from_utf8_lossy(&p.name).trim_end_matches('\0').to_string());
    }
    assert!(names.contains(&"DPMS".to_string()) && names.contains(&"EDID".to_string()));
    // drmModeGetEncoder, drmModeGetCrtc (the saved mode).
    let mut e = GetEncoder { encoder_id: read_u32s(enc, 1)[0], ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETENCODER, &mut e), 0);
    let mut sc = Crtc { crtc_id: CRTC_ID, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_GETCRTC, &mut sc), 0);
    // Screen creation: dumb buffer, mmap offset, framebuffer.
    let (handle, fb) = dumb_fb(&f, 0);
    let mut md = MapDumb { handle, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_MAP_DUMB, &mut md), 0);
    assert!(drm::mmap_lookup(&f, md.offset).is_some());
    // drmModeSetCrtc with the preferred mode, then the shadow flush.
    assert_eq!(setcrtc(&f, fb), 0);
    let gem = drm::handle_lookup(&f, handle).unwrap();
    unsafe { (gem.page_phys(0).unwrap() as *mut u32).write(0x00DE_ADBE) };
    let mut d = FbDirtyCmd { fb_id: fb, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_DIRTYFB, &mut d), 0);
    assert_eq!(r.px(0, 0), 0x00DE_ADBE);
    // Software cursor: the hardware one is refused (-ENXIO) and Xorg moves on.
    let mut cur = Cursor { flags: MODE_CURSOR_MOVE, crtc_id: CRTC_ID, x: 5, y: 5, ..Default::default() };
    assert_eq!(call(&f, ioctls::MODE_CURSOR, &mut cur), errno(drm::ENXIO));
    // Screensaver: DPMS Off / On.
    for v in [DPMS_OFF, DPMS_ON] {
        let mut p = ConnectorSetProperty { value: v, prop_id: P_DPMS, connector_id: CONNECTOR_ID };
        assert_eq!(call(&f, ioctls::MODE_SETPROPERTY, &mut p), 0);
    }
    // Server exit: drop master, close -> console is back.
    assert_eq!(drm::ioctl(&f, ioctls::DROP_MASTER, 0), 0);
    let rep = REPAINTS.load(Ordering::SeqCst);
    drop(f);
    assert!(!drm::scanout_owned());
    assert_eq!(REPAINTS.load(Ordering::SeqCst), rep + 1);
}
