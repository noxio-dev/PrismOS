// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side test of the DRM / KMS userspace ABI and the scanout blit: the
// real drm/uapi.rs and drm/scanout.rs, with the kernel's framebuffer, GEM
// objects and console replaced by plain memory.
//
//   rustc --edition 2021 --test tests/drm_host.rs -o /tmp/drm_host \
//     && /tmp/drm_host --test-threads=1
//
// The ioctl numbers below are *literals*, taken from Linux's x86-64 headers
// (what libdrm really passes to ioctl(2)), not computed through the code
// under test: a mistake in the `_IOC` encoder or in a struct size cannot
// cancel itself out.

#![allow(dead_code, unused_imports)]

extern crate alloc;

#[path = "../sys/kern/src/drm/uapi.rs"]
pub mod uapi_impl;
#[path = "../sys/kern/src/drm/scanout.rs"]
pub mod scanout_impl;

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

mod console {
    use std::sync::atomic::{AtomicUsize, Ordering};
    pub static REPAINTS: AtomicUsize = AtomicUsize::new(0);
    pub fn repaint() {
        REPAINTS.fetch_add(1, Ordering::SeqCst);
    }
}

mod drm {
    pub use crate::scanout_impl as scanout;
    pub use crate::uapi_impl as uapi;

    pub mod gem {
        /// A GEM object reduced to its bytes.
        pub struct Gem {
            pub id: u32,
            pub size: usize,
            pub data: Vec<u8>,
        }
        impl Gem {
            pub fn read(&self, off: usize, out: &mut [u8]) -> bool {
                let Some(end) = off.checked_add(out.len()) else { return false };
                if end > self.data.len() {
                    return false;
                }
                out.copy_from_slice(&self.data[off..end]);
                true
            }
        }
    }
}

use boot::Framebuffer;
use drm::gem::Gem;
use drm::scanout::{self, Blit, Conv, Native};
use drm::uapi::*;
use std::rc::Rc;

// ------------------------------------------------------------ ioctl ABI

#[test]
fn ioctl_numbers_match_linux_x86_64() {
    // Core (drm.h).
    assert_eq!(ioctls::VERSION, 0xC040_6400);
    assert_eq!(ioctls::GET_UNIQUE, 0xC010_6401);
    assert_eq!(ioctls::GET_MAGIC, 0x8004_6402);
    assert_eq!(ioctls::SET_VERSION, 0xC010_6407);
    assert_eq!(ioctls::GEM_CLOSE, 0x4008_6409);
    assert_eq!(ioctls::GEM_FLINK, 0xC008_640A);
    assert_eq!(ioctls::GEM_OPEN, 0xC010_640B);
    assert_eq!(ioctls::GET_CAP, 0xC010_640C);
    assert_eq!(ioctls::SET_CLIENT_CAP, 0x4010_640D);
    assert_eq!(ioctls::AUTH_MAGIC, 0x4004_6411);
    assert_eq!(ioctls::SET_MASTER, 0x0000_641E);
    assert_eq!(ioctls::DROP_MASTER, 0x0000_641F);
    assert_eq!(ioctls::WAIT_VBLANK, 0xC018_643A);
    assert_eq!(ioctls::PRIME_HANDLE_TO_FD, 0xC00C_642D);
    assert_eq!(ioctls::PRIME_FD_TO_HANDLE, 0xC00C_642E);
    // KMS (drm_mode.h).
    assert_eq!(ioctls::MODE_GETRESOURCES, 0xC040_64A0);
    assert_eq!(ioctls::MODE_GETCRTC, 0xC068_64A1);
    assert_eq!(ioctls::MODE_SETCRTC, 0xC068_64A2);
    assert_eq!(ioctls::MODE_CURSOR, 0xC01C_64A3);
    assert_eq!(ioctls::MODE_GETENCODER, 0xC014_64A6);
    assert_eq!(ioctls::MODE_GETCONNECTOR, 0xC050_64A7);
    assert_eq!(ioctls::MODE_GETPROPERTY, 0xC040_64AA);
    assert_eq!(ioctls::MODE_SETPROPERTY, 0xC010_64AB);
    assert_eq!(ioctls::MODE_GETPROPBLOB, 0xC010_64AC);
    assert_eq!(ioctls::MODE_GETFB, 0xC01C_64AD);
    assert_eq!(ioctls::MODE_ADDFB, 0xC01C_64AE);
    assert_eq!(ioctls::MODE_RMFB, 0xC004_64AF);
    assert_eq!(ioctls::MODE_PAGE_FLIP, 0xC018_64B0);
    assert_eq!(ioctls::MODE_DIRTYFB, 0xC018_64B1);
    assert_eq!(ioctls::MODE_CREATE_DUMB, 0xC020_64B2);
    assert_eq!(ioctls::MODE_MAP_DUMB, 0xC010_64B3);
    assert_eq!(ioctls::MODE_DESTROY_DUMB, 0xC004_64B4);
    assert_eq!(ioctls::MODE_GETPLANERESOURCES, 0xC010_64B5);
    assert_eq!(ioctls::MODE_GETPLANE, 0xC020_64B6);
    assert_eq!(ioctls::MODE_SETPLANE, 0xC030_64B7);
    assert_eq!(ioctls::MODE_ADDFB2, 0xC068_64B8);
    assert_eq!(ioctls::MODE_OBJ_GETPROPERTIES, 0xC020_64B9);
    assert_eq!(ioctls::MODE_OBJ_SETPROPERTY, 0xC018_64BA);
    assert_eq!(ioctls::MODE_CURSOR2, 0xC024_64BB);
    assert_eq!(ioctls::MODE_ATOMIC, 0xC038_64BC);
    assert_eq!(ioctls::MODE_CREATEPROPBLOB, 0xC010_64BD);
    assert_eq!(ioctls::MODE_DESTROYPROPBLOB, 0xC004_64BE);
}

#[test]
fn ioc_fields_decode() {
    let c = ioctls::MODE_SETCRTC;
    assert_eq!(ioc_type(c), b'd' as u32);
    assert_eq!(ioc_nr(c), 0xA2);
    assert_eq!(ioc_size(c), 104);
    assert_eq!(ioc_dir(c), IOC_READ | IOC_WRITE);
    assert_eq!(ioc_dir(ioctls::SET_MASTER), IOC_NONE);
    assert_eq!(ioc_dir(ioctls::GET_MAGIC), IOC_READ);
    assert_eq!(ioc_dir(ioctls::GEM_CLOSE), IOC_WRITE);
}

#[test]
fn struct_sizes_match_linux_lp64() {
    use core::mem::size_of;
    assert_eq!(size_of::<ModeInfo>(), 68);
    assert_eq!(size_of::<Crtc>(), 104);
    assert_eq!(size_of::<CardRes>(), 64);
    assert_eq!(size_of::<GetConnector>(), 80);
    assert_eq!(size_of::<GetEncoder>(), 20);
    assert_eq!(size_of::<GetProperty>(), 64);
    assert_eq!(size_of::<PropertyEnum>(), 40);
    assert_eq!(size_of::<FbCmd>(), 28);
    assert_eq!(size_of::<FbCmd2>(), 104);
    assert_eq!(size_of::<CreateDumb>(), 32);
    assert_eq!(size_of::<MapDumb>(), 16);
    assert_eq!(size_of::<CrtcPageFlip>(), 24);
    assert_eq!(size_of::<GetPlane>(), 32);
    assert_eq!(size_of::<SetPlane>(), 48);
    assert_eq!(size_of::<Atomic>(), 56);
    assert_eq!(size_of::<WaitVblank>(), 24);
    assert_eq!(size_of::<EventVblank>(), 32);
    assert_eq!(size_of::<Version>(), 64);
}

#[test]
fn fourcc_codes() {
    assert_eq!(FORMAT_XRGB8888, 0x3432_5258); // 'X' 'R' '2' '4'
    assert_eq!(FORMAT_ARGB8888, 0x3432_5241); // 'A' 'R' '2' '4'
    assert_eq!(FORMAT_XBGR8888, 0x3432_4258);
    assert_eq!(FORMAT_RGB565, 0x3631_4752);
}

#[test]
fn object_types_and_flags() {
    assert_eq!(OBJECT_CRTC, 0xcccc_cccc);
    assert_eq!(OBJECT_CONNECTOR, 0xc0c0_c0c0);
    assert_eq!(OBJECT_ENCODER, 0xe0e0_e0e0);
    assert_eq!(OBJECT_FB, 0xfbfb_fbfb);
    assert_eq!(OBJECT_BLOB, 0xbbbb_bbbb);
    assert_eq!(OBJECT_PLANE, 0xeeee_eeee);
    assert_eq!(PROP_OBJECT, 0x40);
    assert_eq!(PROP_SIGNED_RANGE, 0x80);
    assert_eq!(PROP_ATOMIC, 0x8000_0000);
    assert_eq!(MODE_PAGE_FLIP_EVENT, 1);
    assert_eq!(MODE_ATOMIC_TEST_ONLY, 0x100);
    assert_eq!(MODE_ATOMIC_ALLOW_MODESET, 0x400);
    assert_eq!(EVENT_VBLANK, 1);
    assert_eq!(EVENT_FLIP_COMPLETE, 2);
}

// -------------------------------------------------- byte-level layouts

#[test]
fn from_bytes_zero_extends_and_to_bytes_truncates() {
    // An old libdrm passing only the first 8 bytes of drm_mode_create_dumb.
    let mut raw = [0u8; 8];
    raw[0..4].copy_from_slice(&600u32.to_le_bytes()); // height
    raw[4..8].copy_from_slice(&800u32.to_le_bytes()); // width
    let c: CreateDumb = from_bytes(&raw);
    assert_eq!((c.height, c.width, c.bpp, c.handle, c.size), (600, 800, 0, 0, 0));

    let out = CreateDumb { height: 1, width: 2, bpp: 32, flags: 0, handle: 7, pitch: 4096, size: 0x1234_5678_9ABC };
    let mut small = [0xAAu8; 20];
    to_bytes(&out, &mut small);
    assert_eq!(&small[16..20], &7u32.to_le_bytes()); // handle is at offset 16
}

#[test]
fn wire_offsets_of_the_crtc_struct() {
    let mut c = Crtc::default();
    c.crtc_id = 41;
    c.fb_id = 100;
    c.mode_valid = 1;
    c.mode.hdisplay = 1920;
    let b = as_bytes(&c);
    assert_eq!(&b[12..16], &41u32.to_le_bytes());
    assert_eq!(&b[16..20], &100u32.to_le_bytes());
    assert_eq!(&b[32..36], &1u32.to_le_bytes());
    assert_eq!(&b[36 + 4..36 + 6], &1920u16.to_le_bytes()); // mode.hdisplay
}

#[test]
fn event_wire_format() {
    let ev = EventVblank {
        base: Event { type_: EVENT_FLIP_COMPLETE, length: 32 },
        user_data: 0xDEAD_BEEF_0000_0001,
        tv_sec: 12,
        tv_usec: 345,
        sequence: 6,
        crtc_id: 41,
    };
    let b = as_bytes(&ev);
    assert_eq!(b.len(), 32);
    assert_eq!(&b[0..4], &2u32.to_le_bytes());
    assert_eq!(&b[4..8], &32u32.to_le_bytes());
    assert_eq!(&b[8..16], &0xDEAD_BEEF_0000_0001u64.to_le_bytes());
    assert_eq!(&b[16..20], &12u32.to_le_bytes());
    assert_eq!(&b[28..32], &41u32.to_le_bytes());
}

// ------------------------------------------------------------- scanout

#[test]
fn native_mode_is_a_sane_60hz_mode() {
    let m = scanout::native_mode(1920, 1080);
    assert_eq!((m.hdisplay, m.vdisplay, m.vrefresh), (1920, 1080, 60));
    assert!(m.hsync_start > m.hdisplay && m.hsync_end > m.hsync_start && m.htotal > m.hsync_end);
    assert!(m.vsync_start > m.vdisplay && m.vsync_end > m.vsync_start && m.vtotal > m.vsync_end);
    // clock (kHz) * 1000 == htotal * vtotal * refresh, up to integer division
    let want = m.htotal as u64 * m.vtotal as u64 * 60 / 1000;
    assert_eq!(m.clock as u64, want);
    assert_eq!(&m.name[..9], b"1920x1080");
    assert_eq!(m.name[9], 0);
    assert!(m.type_ & MODE_TYPE_PREFERRED != 0 && m.type_ & MODE_TYPE_DRIVER != 0);
    let small = scanout::native_mode(640, 480);
    assert_eq!(&small.name[..7], b"640x480");
}

#[test]
fn physical_size_is_96_dpi() {
    assert_eq!(scanout::size_mm(1920), 508);
    assert_eq!(scanout::size_mm(1080), 286);
}

#[test]
fn pixel_conversion() {
    // XRGB8888 -> RGB565: 0xFF8040
    let px = 0x00FF_8040;
    assert_eq!(scanout::convert_pixel(px, (11, 5), (5, 6), (0, 5)), 0xF800 | (0x20 << 5) | 0x08);
    // -> RGBX8888 (red at bit 0)
    assert_eq!(scanout::convert_pixel(px, (0, 8), (8, 8), (16, 8)), 0x0040_80FF);
    // identity layout
    assert_eq!(scanout::convert_pixel(px, (16, 8), (8, 8), (0, 8)), px);
}

fn fb_over(buf: &mut Vec<u8>, w: u32, h: u32, pitch: u32, shifts: (u8, u8, u8)) -> Native {
    Native::new(Framebuffer {
        addr: buf.as_mut_ptr() as u64,
        width: w,
        height: h,
        pitch,
        bpp: 32,
        red_shift: shifts.0,
        red_bits: 8,
        green_shift: shifts.1,
        green_bits: 8,
        blue_shift: shifts.2,
        blue_bits: 8,
        reserved: 0,
    })
    .expect("drivable")
}

fn gem_of(pixels: &[u32], pitch_bytes: usize, rows: usize) -> Rc<Gem> {
    let mut data = vec![0u8; pitch_bytes * rows];
    for (i, p) in pixels.iter().enumerate() {
        data[i * 4..i * 4 + 4].copy_from_slice(&p.to_le_bytes());
    }
    Rc::new(Gem { id: 1, size: data.len(), data })
}

fn px(buf: &[u8], pitch: usize, x: usize, y: usize) -> u32 {
    let o = y * pitch + x * 4;
    u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]])
}

#[test]
fn native_rejects_undrivable_panels() {
    let mut buf = vec![0u8; 4096];
    let mut fb = fb_over(&mut buf, 8, 8, 32, (16, 8, 0)).fb;
    fb.bpp = 16;
    assert!(Native::new(fb).is_none());
    fb.bpp = 32;
    fb.pitch = 16; // narrower than a row
    assert!(Native::new(fb).is_none());
    fb.pitch = 32;
    fb.addr = 0;
    assert!(Native::new(fb).is_none());
}

// The driver refuses panels under 8x8, so every test panel is 8x8: pitch 40
// (8 pixels = 32 bytes, plus 8 bytes of row padding that must stay untouched)
// unless a test says otherwise.

#[test]
fn blit_copy_path_honours_pitches_and_offsets() {
    let mut panel = vec![0xEEu8; 40 * 8];
    let nat = fb_over(&mut panel, 8, 8, 40, (16, 8, 0));
    assert_eq!(nat.conv, Conv::Copy);
    // Source 6x3 pixels, pitch 32 bytes; pixel (x,y) = 0x1000*y + x.
    let mut src = vec![0u32; 8 * 3];
    for y in 0..3 {
        for x in 0..6 {
            src[y * 8 + x] = 0x1000 * y as u32 + x as u32;
        }
    }
    let gem = gem_of(&src, 32, 3);
    // Show the window starting at source pixel (2,1), 4x2, at panel (0,1).
    let job = Blit { gem, offset: 0, pitch: 32, sx: 2, sy: 1, dx: 0, dy: 1, w: 4, h: 2 };
    assert!(scanout::blit(&nat, &job));
    assert_eq!(px(&panel, 40, 0, 1), 0x1000 + 2);
    assert_eq!(px(&panel, 40, 3, 1), 0x1000 + 5);
    assert_eq!(px(&panel, 40, 0, 2), 0x2000 + 2);
    assert_eq!(px(&panel, 40, 3, 2), 0x2000 + 5);
    // Untouched: row 0, pixels right of the window, and the row padding.
    assert_eq!(px(&panel, 40, 0, 0), 0xEEEE_EEEE);
    assert_eq!(px(&panel, 40, 4, 1), 0xEEEE_EEEE);
    for y in 0..8 {
        assert!(panel[y * 40 + 32..y * 40 + 40].iter().all(|&b| b == 0xEE), "padding of row {y}");
    }
}

#[test]
fn blit_clips_to_the_panel_and_to_the_buffer() {
    let gem = gem_of(&[7u32; 64], 32, 8);
    // Wider and taller than the panel: clipped to 8x8.
    let mut panel = vec![0u8; 32 * 8];
    let nat = fb_over(&mut panel, 8, 8, 32, (16, 8, 0));
    let job = Blit { gem: gem.clone(), offset: 0, pitch: 32, sx: 0, sy: 0, dx: 0, dy: 0, w: 100, h: 100 };
    assert!(scanout::blit(&nat, &job));
    assert_eq!(px(&panel, 32, 7, 7), 7);
    // Entirely outside the panel: a no-op, no panic.
    let mut panel2 = vec![0u8; 32 * 8];
    let nat2 = fb_over(&mut panel2, 8, 8, 32, (16, 8, 0));
    let job = Blit { gem: gem.clone(), offset: 0, pitch: 32, sx: 0, sy: 0, dx: 9, dy: 0, w: 4, h: 2 };
    assert!(scanout::blit(&nat2, &job));
    assert!(panel2.iter().all(|&b| b == 0));
    // A window starting inside but overhanging: only the visible part lands.
    let mut panel3 = vec![0u8; 32 * 8];
    let nat3 = fb_over(&mut panel3, 8, 8, 32, (16, 8, 0));
    let job = Blit { gem: gem.clone(), offset: 0, pitch: 32, sx: 0, sy: 0, dx: 6, dy: 6, w: 8, h: 8 };
    assert!(scanout::blit(&nat3, &job));
    assert_eq!(px(&panel3, 32, 7, 7), 7);
    assert_eq!(px(&panel3, 32, 5, 7), 0);
    // A buffer too small for the request: missing rows are skipped, never
    // read out of bounds.
    let short = gem_of(&[9u32; 8], 32, 1);
    let mut panel4 = vec![0u8; 32 * 8];
    let nat4 = fb_over(&mut panel4, 8, 8, 32, (16, 8, 0));
    let job = Blit { gem: short, offset: 0, pitch: 32, sx: 0, sy: 0, dx: 0, dy: 0, w: 8, h: 8 };
    assert!(scanout::blit(&nat4, &job));
    assert_eq!(px(&panel4, 32, 0, 0), 9);
    assert_eq!(px(&panel4, 32, 0, 1), 0);
}

#[test]
fn blit_converts_for_a_non_bgra_panel() {
    // A panel whose red channel is the low byte (RGBX).
    let mut panel = vec![0u8; 32 * 8];
    let nat = fb_over(&mut panel, 8, 8, 32, (0, 8, 16));
    assert!(matches!(nat.conv, Conv::Shift { .. }));
    let gem = gem_of(&[0x00FF_8040, 0x0000_0001, 0, 0, 0, 0, 0, 0], 32, 1);
    let job = Blit { gem, offset: 0, pitch: 32, sx: 0, sy: 0, dx: 0, dy: 0, w: 8, h: 1 };
    assert!(scanout::blit(&nat, &job));
    assert_eq!(px(&panel, 32, 0, 0), 0x0040_80FF);
    assert_eq!(px(&panel, 32, 1, 0), 0x0001_0000);
}

#[test]
fn blit_honours_the_gem_offset() {
    // An fb whose pixels start 64 bytes into the object (ADDFB2 offsets[0]).
    let mut panel = vec![0u8; 32 * 8];
    let nat = fb_over(&mut panel, 8, 8, 32, (16, 8, 0));
    let mut data = vec![0u8; 64 + 32];
    for x in 0..8 {
        data[64 + x * 4..64 + x * 4 + 4].copy_from_slice(&(0x10 + x as u32).to_le_bytes());
    }
    let gem = Rc::new(Gem { id: 1, size: data.len(), data });
    let job = Blit { gem, offset: 64, pitch: 32, sx: 0, sy: 0, dx: 0, dy: 0, w: 8, h: 1 };
    assert!(scanout::blit(&nat, &job));
    assert_eq!(px(&panel, 32, 2, 0), 0x12);
    assert_eq!(px(&panel, 32, 7, 0), 0x17);
}

#[test]
fn releasing_the_screen_repaints_the_console_once() {
    use std::sync::atomic::Ordering;
    let before = console::REPAINTS.load(Ordering::SeqCst);
    scanout::set_owned(true);
    assert!(scanout::owned());
    assert_eq!(console::REPAINTS.load(Ordering::SeqCst), before); // taking: no repaint
    scanout::set_owned(false);
    assert!(!scanout::owned());
    assert_eq!(console::REPAINTS.load(Ordering::SeqCst), before + 1);
    scanout::set_owned(false); // already ours: nothing to repaint
    assert_eq!(console::REPAINTS.load(Ordering::SeqCst), before + 1);
}
