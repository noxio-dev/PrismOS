// SPDX-License-Identifier: GPL-3.0-or-later
//
// Host-side test of the keyboard decoder: the real hw/keymap.rs and
// hw/kbd.rs, with the port I/O replaced by a scripted controller.
//
//   rustc --edition 2021 --test tests/keymap_host.rs -o /tmp/keymap_host \
//     && /tmp/keymap_host --test-threads=1
//
// (kbd.rs is `include!`d so the tests can reach its private state.)

#![allow(dead_code, unused_imports)]

/// The kernel's port I/O, replaced: bytes "typed" by the test come out of
/// port 0x60 and everything written to the ports is logged.
mod arch {
    pub mod io {
        use std::collections::VecDeque;
        use std::sync::Mutex;

        pub static INPUT: Mutex<VecDeque<u8>> = Mutex::new(VecDeque::new());
        pub static WRITES: Mutex<Vec<(u16, u8)>> = Mutex::new(Vec::new());

        pub unsafe fn inb(port: u16) -> u8 {
            let q = INPUT.lock().unwrap();
            match port {
                0x64 => {
                    if q.is_empty() {
                        0x00
                    } else {
                        0x01
                    }
                }
                0x60 => {
                    drop(q);
                    INPUT.lock().unwrap().pop_front().unwrap_or(0)
                }
                _ => 0,
            }
        }
        pub unsafe fn outb(port: u16, v: u8) {
            WRITES.lock().unwrap().push((port, v));
        }
        pub fn io_wait() {}

        pub struct IrqGuard;
        impl IrqGuard {
            pub fn new() -> IrqGuard {
                IrqGuard
            }
        }
    }
}

#[path = "../sys/kern/src/hw/keymap.rs"]
pub mod keymap;

mod hw {
    pub use crate::keymap;
    pub mod kbd {
        include!("../sys/kern/src/hw/kbd.rs");

        #[cfg(test)]
        mod tests {
            use super::*;
            use crate::arch::io::{INPUT, WRITES};
            use crate::keymap::*;
            use std::sync::Mutex;

            static LOCK: Mutex<()> = Mutex::new(());

            fn reset(layout: &str) -> std::sync::MutexGuard<'static, ()> {
                let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
                INPUT.lock().unwrap().clear();
                WRITES.lock().unwrap().clear();
                while pop_char().is_some() {}
                assert!(kd_set_mode(K_UNICODE));
                assert!(set_layout(layout));
                assert!(kd_set_kbled(LED_NUM as u64));
                // pretend the keyboard answered at init, LEDs in sync
                LED_CAPABLE.store(true, Ordering::Release);
                unsafe {
                    let k = &mut *addr_of_mut!(KBD);
                    k.led_state = 0;
                    k.led_applied = LED_NUM;
                    k.led_override = 0xFF;
                    k.set2 = false;
                    k.ext = false;
                    k.skip = 0;
                }
                g
            }

            /// Type raw controller bytes; return what the console reads.
            fn typed(bytes: &[u8]) -> Vec<u8> {
                INPUT.lock().unwrap().extend(bytes.iter().copied());
                while !INPUT.lock().unwrap().is_empty() {
                    on_irq();
                }
                let mut v = Vec::new();
                while let Some(b) = pop_char() {
                    v.push(b);
                }
                v
            }

            fn s(v: Vec<u8>) -> String {
                String::from_utf8(v).unwrap()
            }

            #[test]
            fn plain_shift_caps() {
                let _g = reset("us");
                assert_eq!(s(typed(&[0x1E, 0x9E])), "a");
                assert_eq!(s(typed(&[0x2A, 0x1E, 0x9E, 0xAA])), "A");
                // Caps Lock on: letters flip, digits do not
                assert_eq!(s(typed(&[0x3A, 0xBA])), "");
                assert_eq!(leds() & LED_CAP, LED_CAP);
                assert_eq!(s(typed(&[0x1E, 0x9E, 0x02, 0x82])), "A1");
                // Caps + Shift: lowercase again
                assert_eq!(s(typed(&[0x2A, 0x1E, 0x9E, 0xAA])), "a");
                // Caps off
                typed(&[0x3A, 0xBA]);
                assert_eq!(leds() & LED_CAP, 0);
                assert_eq!(s(typed(&[0x1E, 0x9E])), "a");
            }

            #[test]
            fn caps_autorepeat_toggles_once() {
                let _g = reset("us");
                // hardware typematic: several makes, one break
                typed(&[0x3A, 0x3A, 0x3A, 0x3A, 0xBA]);
                assert_eq!(leds() & LED_CAP, LED_CAP);
            }

            #[test]
            fn led_handshake() {
                let _g = reset("us");
                WRITES.lock().unwrap().clear();
                typed(&[0x3A]); // Caps Lock down
                assert_eq!(*WRITES.lock().unwrap(), vec![(0x60, 0xED)]);
                typed(&[0xFA]); // ACK of the command -> mask follows
                assert_eq!(*WRITES.lock().unwrap(), vec![(0x60, 0xED), (0x60, LED_NUM | LED_CAP)]);
                typed(&[0xFA]); // ACK of the mask -> done
                assert_eq!(unsafe { (*addr_of_mut!(KBD)).led_state }, 0);
                assert_eq!(unsafe { (*addr_of_mut!(KBD)).led_applied }, LED_NUM | LED_CAP);
                // The break code of Caps Lock does not start another update.
                typed(&[0xBA]);
                assert_eq!(WRITES.lock().unwrap().len(), 2);
            }

            #[test]
            fn led_resend_and_change_during_update() {
                let _g = reset("us");
                WRITES.lock().unwrap().clear();
                typed(&[0x3A, 0xBA]); // caps on: 0xED sent
                typed(&[0xFE]); // keyboard asks to resend
                assert_eq!(WRITES.lock().unwrap().len(), 2);
                assert_eq!(WRITES.lock().unwrap()[1], (0x60, 0xED));
                // Num Lock pressed while the update is in flight: no new command yet
                typed(&[0x45, 0xC5]);
                assert_eq!(WRITES.lock().unwrap().len(), 2);
                typed(&[0xFA]); // ACK cmd -> mask with both changes (num off, caps on)
                assert_eq!(*WRITES.lock().unwrap().last().unwrap(), (0x60, LED_CAP));
                typed(&[0xFA]); // ACK mask: in sync, nothing more to send
                assert_eq!(unsafe { (*addr_of_mut!(KBD)).led_state }, 0);
            }

            #[test]
            fn keypad_and_numlock() {
                let _g = reset("us");
                // Num Lock on (boot default): digits
                assert_eq!(s(typed(&[0x47, 0xC7, 0x52, 0xD2])), "70");
                assert_eq!(s(typed(&[0x4E, 0xCE, 0x37, 0xB7])), "+*");
                assert_eq!(s(typed(&[0xE0, 0x35, 0xE0, 0xB5, 0xE0, 0x1C, 0xE0, 0x9C])), "/\r");
                assert_eq!(s(typed(&[0x53, 0xD3])), ".");
                // Num Lock off: navigation
                typed(&[0x45, 0xC5]);
                assert_eq!(leds() & LED_NUM, 0);
                assert_eq!(s(typed(&[0x47, 0xC7])), "\x1b[1~"); // Home
                assert_eq!(s(typed(&[0x48, 0xC8])), "\x1b[A"); // up
                assert_eq!(s(typed(&[0x50, 0xD0])), "\x1b[B");
                assert_eq!(s(typed(&[0x4B, 0xCB])), "\x1b[D");
                assert_eq!(s(typed(&[0x4D, 0xCD])), "\x1b[C");
                assert_eq!(s(typed(&[0x4F, 0xCF])), "\x1b[4~"); // End
                assert_eq!(s(typed(&[0x49, 0xC9])), "\x1b[5~"); // PgUp
                assert_eq!(s(typed(&[0x51, 0xD1])), "\x1b[6~"); // PgDn
                assert_eq!(s(typed(&[0x52, 0xD2])), "\x1b[2~"); // Ins
                assert_eq!(s(typed(&[0x53, 0xD3])), "\x1b[3~"); // Del
                // + - * / Enter keep working with Num Lock off
                assert_eq!(s(typed(&[0x4E, 0xCE])), "+");
                // Num Lock back on
                typed(&[0x45, 0xC5]);
                assert_eq!(s(typed(&[0x48, 0xC8])), "8");
            }

            #[test]
            fn navigation_cluster_and_fake_shift() {
                let _g = reset("us");
                // E0 2A is the fake shift sent around Insert/Delete/arrows with Num Lock on
                let seq = [0xE0, 0x2A, 0xE0, 0x48, 0xE0, 0xC8, 0xE0, 0xAA];
                assert_eq!(s(typed(&seq)), "\x1b[A");
                assert_eq!(modifiers().0, false);
                assert_eq!(s(typed(&[0xE0, 0x47, 0xE0, 0xC7])), "\x1b[1~");
                assert_eq!(s(typed(&[0xE0, 0x53, 0xE0, 0xD3])), "\x1b[3~");
            }

            #[test]
            fn function_keys() {
                let _g = reset("us");
                assert_eq!(s(typed(&[0x3B, 0xBB])), "\x1b[[A"); // F1
                assert_eq!(s(typed(&[0x3F, 0xBF])), "\x1b[[E"); // F5
                assert_eq!(s(typed(&[0x40, 0xC0])), "\x1b[17~"); // F6
                assert_eq!(s(typed(&[0x44, 0xC4])), "\x1b[21~"); // F10
                assert_eq!(s(typed(&[0x57, 0xD7])), "\x1b[23~"); // F11
                assert_eq!(s(typed(&[0x58, 0xD8])), "\x1b[24~"); // F12
            }

            #[test]
            fn pause_is_swallowed() {
                let _g = reset("us");
                assert_eq!(s(typed(&[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5, 0x1E, 0x9E])), "a");
                assert_eq!(leds() & LED_NUM, LED_NUM); // the 0x45 inside is not Num Lock
            }

            #[test]
            fn control_alt_enter_backspace() {
                let _g = reset("us");
                assert_eq!(typed(&[0x1D, 0x2E, 0xAE, 0x9D]), vec![3]); // Ctrl-C
                assert_eq!(typed(&[0x1D, 0x2B, 0xAB, 0x9D]), vec![0x1C]); // Ctrl-\
                assert_eq!(typed(&[0x1D, 0x1A, 0x9A, 0x9D]), vec![0x1B]); // Ctrl-[
                assert_eq!(typed(&[0xE0, 0x1D, 0x16, 0x96, 0xE0, 0x9D]), vec![0x15]); // right ctrl + u
                assert_eq!(typed(&[0x38, 0x2D, 0xAD, 0xB8]), vec![0x1B, b'x']); // Alt-x = ESC x
                assert_eq!(typed(&[0x1C, 0x9C]), vec![b'\r']);
                assert_eq!(typed(&[0x0E, 0x8E]), vec![0x7F]);
                assert_eq!(typed(&[0x0F, 0x8F]), vec![b'\t']);
                assert_eq!(typed(&[0x01, 0x81]), vec![0x1B]);
                assert_eq!(typed(&[0x39, 0xB9]), vec![b' ']);
                // modifiers released: back to normal
                assert_eq!(s(typed(&[0x2E, 0xAE])), "c");
            }

            #[test]
            fn french_layout() {
                let _g = reset("fr");
                assert_eq!(layout_name(), "fr");
                assert_eq!(s(typed(&[0x10, 0x90])), "a"); // AZERTY: key Q gives a
                assert_eq!(s(typed(&[0x03, 0x83])), "é");
                assert_eq!(s(typed(&[0x2A, 0x03, 0x83, 0xAA])), "2");
                assert_eq!(s(typed(&[0xE0, 0x38, 0x09, 0x89, 0xE0, 0xB8])), "\\"); // AltGr+8
                assert_eq!(s(typed(&[0xE0, 0x38, 0x0B, 0x8B, 0xE0, 0xB8])), "@"); // AltGr+0
                assert_eq!(s(typed(&[0xE0, 0x38, 0x12, 0x92, 0xE0, 0xB8])), "€"); // AltGr+E (U+20AC)
                assert_eq!(typed(&[0xE0, 0x38, 0x12, 0x92, 0xE0, 0xB8]), vec![0xE2, 0x82, 0xAC]);
                assert_eq!(s(typed(&[0x56, 0xD6])), "<");
                // Caps Lock: letters flip, é does not become 2
                typed(&[0x3A, 0xBA]);
                assert_eq!(s(typed(&[0x10, 0x90, 0x03, 0x83])), "Aé");
                // Ctrl on the AZERTY 'q' key (which makes 'a') is Ctrl-A
                typed(&[0x3A, 0xBA]);
                assert_eq!(typed(&[0x1D, 0x10, 0x90, 0x9D]), vec![1]);
            }

            #[test]
            fn xlate_mode_is_latin1() {
                let _g = reset("fr");
                assert!(kd_set_mode(K_XLATE));
                assert_eq!(typed(&[0x03, 0x83]), vec![0xE9]);
                assert_eq!(typed(&[0xE0, 0x38, 0x12, 0x92, 0xE0, 0xB8]), vec![]); // euro: not Latin-1
                assert_eq!(kd_get_mode(), K_XLATE);
            }

            #[test]
            fn raw_and_mediumraw_modes() {
                let _g = reset("us");
                assert!(kd_set_mode(K_RAW));
                assert_eq!(typed(&[0xE0, 0x48, 0xE0, 0xC8, 0x1E]), vec![0xE0, 0x48, 0xE0, 0xC8, 0x1E]);
                assert!(kd_set_mode(K_MEDIUMRAW));
                assert_eq!(typed(&[0x1E, 0x9E]), vec![0x1E, 0x9E]);
                assert_eq!(typed(&[0xE0, 0x48, 0xE0, 0xC8]), vec![103, 103 | 0x80]);
                assert!(kd_set_mode(K_OFF));
                assert_eq!(typed(&[0x1E, 0x9E]), vec![]);
                assert!(!kd_set_mode(9));
            }

            #[test]
            fn kdskbent_roundtrip_and_remap() {
                let _g = reset("us");
                assert_eq!(kd_get_entry(0, 30), b'a' as u16 | K_LETTER_FLAG);
                assert_eq!(kd_get_entry(77, 30), K_NOSUCHMAP); // table 77 does not exist
                assert!(kd_set_entry(0, 30, b'z' as u16));
                assert_eq!(layout_name(), "custom");
                assert_eq!(s(typed(&[0x1E, 0x9E])), "z");
                assert!(!kd_set_entry(0, 200, 0)); // bad index
                // a new table appears as a copy of table 0
                assert!(kd_set_entry(40, 31, b'q' as u16));
                assert_eq!(kd_get_entry(40, 30), b'z' as u16);
                assert_eq!(kd_get_entry(40, 31), b'q' as u16);
                // K_NOSUCHMAP at index 0 deletes it
                assert!(kd_set_entry(40, 0, K_NOSUCHMAP));
                assert_eq!(kd_get_entry(40, 31), K_NOSUCHMAP);
            }

            const K_LETTER_FLAG: u16 = (KT_LETTER << 8);

            #[test]
            fn loadkmap_of_a_bkmap_file() {
                // What `loadkmap < fr.bkmap` does: parse, then one KDSKBENT per entry.
                let mut km = Keymap::new();
                build_default(&mut km, &LAYOUT_FR);
                let mut file = vec![0u8; 7 + 256 + 16 * 256];
                let n = km.to_bkmap(&mut file);
                file.truncate(n);
                assert_eq!(&file[..7], b"bkeymap");
                let _g = reset("us");
                let flags = &file[7..7 + 256];
                let mut at = 7 + 256;
                for t in 0..256 {
                    if flags[t] == 1 {
                        for i in 0..128 {
                            let v = u16::from_le_bytes([file[at], file[at + 1]]);
                            at += 2;
                            assert!(kd_set_entry(t as u8, i as u8, v));
                        }
                    }
                }
                assert_eq!(at, file.len());
                assert_eq!(s(typed(&[0x10, 0x90])), "a"); // now AZERTY
                assert_eq!(s(typed(&[0xE0, 0x38, 0x09, 0x89, 0xE0, 0xB8])), "\\");
                // dumpkmap sees the same thing
                for t in 0..16 {
                    for i in 0..128 {
                        assert_eq!(kd_get_entry(t, i), km.get(t as usize, i as usize));
                    }
                }
            }

            #[test]
            fn dead_keys() {
                let _g = reset("us");
                // dead circumflex on the [ key, dead diaeresis on shift+[
                assert!(kd_set_entry(0, 0x1A, k(KT_DEAD, 2)));
                assert_eq!(s(typed(&[0x1A, 0x9A, 0x12, 0x92])), "ê");
                assert_eq!(s(typed(&[0x1A, 0x9A, 0x39, 0xB9])), "^"); // dead + space
                assert_eq!(s(typed(&[0x1A, 0x9A, 0x1A, 0x9A])), "^"); // dead twice
                assert_eq!(s(typed(&[0x1A, 0x9A, 0x31, 0xB1])), "^n"); // no composition
                assert_eq!(s(typed(&[0x1A, 0x9A, 0x2A, 0x12, 0x92, 0xAA])), "Ê");
            }

            #[test]
            fn kbled_ioctls() {
                let _g = reset("us");
                assert_eq!(kd_get_kbled() & 7, LED_NUM);
                assert!(!kd_set_kbled(0x88)); // invalid bits
                WRITES.lock().unwrap().clear();
                assert!(kd_set_kbled((LED_NUM | LED_CAP) as u64));
                assert_eq!(*WRITES.lock().unwrap(), vec![(0x60, 0xED)]);
                typed(&[0xFA, 0xFA]);
                assert_eq!(*WRITES.lock().unwrap().last().unwrap(), (0x60, LED_NUM | LED_CAP));
                assert_eq!(leds(), LED_NUM | LED_CAP);
                // KDSETLED overrides what the LEDs show without touching the locks
                kd_set_led(LED_SCR as u64);
                typed(&[0xFA, 0xFA]);
                assert_eq!(kd_get_led(), LED_SCR);
                assert_eq!(leds(), LED_NUM | LED_CAP);
                kd_set_led(0xFF); // back to the lock state
                typed(&[0xFA, 0xFA]);
                assert_eq!(kd_get_led(), LED_NUM | LED_CAP);
            }

            #[test]
            fn queue_overflow_never_splits_a_sequence() {
                let _g = reset("us");
                // fill the ring with F1 presses nobody reads: every key is 4 bytes or nothing
                for _ in 0..200 {
                    INPUT.lock().unwrap().extend([0x3B, 0xBB]);
                    on_irq();
                }
                let mut all = Vec::new();
                while let Some(b) = pop_char() {
                    all.push(b);
                }
                assert_eq!(all.len() % 4, 0);
                for c in all.chunks(4) {
                    assert_eq!(c, b"\x1b[[A");
                }
            }

            #[test]
            fn set2_decoding() {
                let _g = reset("us");
                unsafe { (*addr_of_mut!(KBD)).set2 = true };
                // 'a' = 1C / F0 1C, shift = 12, F7 = 83, up = E0 75, num lock = 77
                assert_eq!(s(typed(&[0x1C, 0xF0, 0x1C])), "a");
                assert_eq!(s(typed(&[0x12, 0x1C, 0xF0, 0x1C, 0xF0, 0x12])), "A");
                assert_eq!(s(typed(&[0x83, 0xF0, 0x83])), "\x1b[18~"); // F7
                assert_eq!(s(typed(&[0xE0, 0x75, 0xE0, 0xF0, 0x75])), "\x1b[A");
                assert_eq!(s(typed(&[0x05, 0xF0, 0x05])), "\x1b[[A"); // F1
                assert_eq!(s(typed(&[0x78, 0xF0, 0x78])), "\x1b[23~"); // F11
                typed(&[0x77, 0xF0, 0x77]);
                assert_eq!(leds() & LED_NUM, 0);
                assert_eq!(s(typed(&[0xE0, 0x71, 0xE0, 0xF0, 0x71])), "\x1b[3~"); // Delete
                assert_eq!(s(typed(&[0xE1, 0x14, 0x77, 0xE1, 0xF0, 0x14, 0xF0, 0x77, 0x1C, 0xF0, 0x1C])), "a");
            }

            #[test]
            fn builtin_maps_have_the_linux_shape() {
                let mut km = Keymap::new();
                build_default(&mut km, &LAYOUT_US);
                assert_eq!(km.get(0, 28), K_ENTER);
                assert_eq!(km.get(0, 1), 0x1B);
                assert_eq!(km.get(0, 14), 0x7F);
                assert_eq!(km.get(0, 58), K_CAPS);
                assert_eq!(km.get(0, 69), K_NUM);
                assert_eq!(km.get(0, 70), K_HOLD);
                assert_eq!(km.get(0, 29), K_CTRL);
                assert_eq!(km.get(15, 97), K_CTRL);
                assert_eq!(km.get(0, 100), K_ALTGR);
                assert_eq!(km.get(1, 2), b'!' as u16);
                assert_eq!(km.get(2, 2), K_HOLE); // US has nothing on AltGr
                assert_eq!(km.get(0, 30), k(KT_LETTER, b'a' as u16));
                assert_eq!(km.get(1, 30), k(KT_LETTER, b'A' as u16));
                assert_eq!(km.get(4, 30), 1); // Ctrl-A
                assert_eq!(km.get(8, 30), k(KT_META, b'a' as u16));
                assert_eq!(km.get(0, 82), k(KT_PAD, 0));
                assert!(km.get(16, 0) == K_NOSUCHMAP);
                let mut fr = Keymap::new();
                build_default(&mut fr, &LAYOUT_FR);
                assert_eq!(fr.get(0, 3), 0xE9); // é, Latin-1
                assert_eq!(fr.get(0, 0x10), k(KT_LETTER, b'a' as u16));
                assert_eq!(fr.get(2, 0x12), 0x20AC ^ 0xF000); // € as Unicode
            }
        }
    }
}
