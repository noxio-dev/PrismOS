// SPDX-License-Identifier: GPL-3.0-or-later
//
// Keyboard translation, Linux console style.
//
// This file is pure logic (no hardware, no allocation, only `core`), so it
// is also compiled on the host by tests/keymap_host.rs and tools/mkkmap.rs.
//
// The model is the one of drivers/tty/vt/keyboard.c, so that BusyBox's
// `loadkmap` / `dumpkmap` (KDSKBENT / KDGKBENT) and every ".bkmap" file made
// by `loadkeys --bkeymap` work unchanged:
//
//   * a *keycode* (Linux numbering: set-1 scancode, E0-prefixed keys
//     remapped, see `keycode_of`) indexes a table of 16-bit *keysyms*;
//   * which table is used is the *modifier state*: bit 0 Shift, 1 AltGr,
//     2 Control, 3 Alt, 4 ShiftL, 5 ShiftR, 6 CtrlL, 7 CtrlR (up to 256
//     tables);
//   * a keysym is (type << 8) | value: KT_LATIN is a Latin-1 character,
//     KT_FN a function-key string, KT_PAD a keypad key, KT_SPEC the locks,
//     ...  Unicode above U+00FF is written `cp ^ 0xF000`.
//
// Lock state lives in `Engine::leds` (bit 0 Scroll, 1 Num, 2 Caps -- the
// very bits of the PS/2 "set LEDs" command), so the LEDs and the decoder
// can never disagree.

#![allow(dead_code)]

pub const NR_KEYS: usize = 128;
pub const MAX_MAPS: usize = 256;

// ------------------------------------------------------------------ keysyms

pub const KT_LATIN: u16 = 0;
pub const KT_FN: u16 = 1;
pub const KT_SPEC: u16 = 2;
pub const KT_PAD: u16 = 3;
pub const KT_DEAD: u16 = 4;
pub const KT_CONS: u16 = 5;
pub const KT_CUR: u16 = 6;
pub const KT_SHIFT: u16 = 7;
pub const KT_META: u16 = 8;
pub const KT_ASCII: u16 = 9;
pub const KT_LOCK: u16 = 10;
pub const KT_LETTER: u16 = 11;
pub const KT_SLOCK: u16 = 12;
/// First type number that means "this is Unicode, `value ^ 0xF000`".
pub const NR_TYPES: u16 = 15;

pub const fn k(t: u16, v: u16) -> u16 {
    (t << 8) | v
}

pub const K_HOLE: u16 = k(KT_SPEC, 0);
pub const K_ENTER: u16 = k(KT_SPEC, 1);
pub const K_CAPS: u16 = k(KT_SPEC, 7);
pub const K_NUM: u16 = k(KT_SPEC, 8);
pub const K_HOLD: u16 = k(KT_SPEC, 9);
pub const K_CAPSON: u16 = k(KT_SPEC, 13);
pub const K_NOSUCHMAP: u16 = k(KT_SPEC, 0xFF);

pub const K_SHIFT: u16 = k(KT_SHIFT, 0);
pub const K_ALTGR: u16 = k(KT_SHIFT, 1);
pub const K_CTRL: u16 = k(KT_SHIFT, 2);
pub const K_ALT: u16 = k(KT_SHIFT, 3);

pub const K_DOWN: u16 = k(KT_CUR, 0);
pub const K_LEFT: u16 = k(KT_CUR, 1);
pub const K_RIGHT: u16 = k(KT_CUR, 2);
pub const K_UP: u16 = k(KT_CUR, 3);

// Keypad values (KT_PAD).
pub const P_PLUS: u16 = 10;
pub const P_MINUS: u16 = 11;
pub const P_STAR: u16 = 12;
pub const P_SLASH: u16 = 13;
pub const P_ENTER: u16 = 14;
pub const P_COMMA: u16 = 15;
pub const P_DOT: u16 = 16;

// Function-key table indexes (KT_FN), Linux numbering.
pub const F_FIND: u16 = 20;
pub const F_INSERT: u16 = 21;
pub const F_REMOVE: u16 = 22;
pub const F_SELECT: u16 = 23;
pub const F_PGUP: u16 = 24;
pub const F_PGDN: u16 = 25;

/// LED / lock bits (the PS/2 0xED mask).
pub const LED_SCR: u8 = 1;
pub const LED_NUM: u8 = 2;
pub const LED_CAP: u8 = 4;

/// KDSKBMODE values.
pub const K_RAW: u8 = 0;
pub const K_XLATE: u8 = 1;
pub const K_MEDIUMRAW: u8 = 2;
pub const K_UNICODE: u8 = 3;
pub const K_OFF: u8 = 4;

/// What each function key sends (Linux's defkeymap.c_shipped, TERM=linux).
const FUNC: [&[u8]; 30] = [
    b"\x1b[[A",
    b"\x1b[[B",
    b"\x1b[[C",
    b"\x1b[[D",
    b"\x1b[[E",
    b"\x1b[17~",
    b"\x1b[18~",
    b"\x1b[19~",
    b"\x1b[20~",
    b"\x1b[21~",
    b"\x1b[23~",
    b"\x1b[24~",
    b"\x1b[25~",
    b"\x1b[26~",
    b"\x1b[28~",
    b"\x1b[29~",
    b"\x1b[31~",
    b"\x1b[32~",
    b"\x1b[33~",
    b"\x1b[34~",
    b"\x1b[1~", // Find (Home)
    b"\x1b[2~", // Insert
    b"\x1b[3~", // Remove (Delete)
    b"\x1b[4~", // Select (End)
    b"\x1b[5~", // Prior (Page Up)
    b"\x1b[6~", // Next (Page Down)
    b"\x1b[M",
    b"\x1b[M",
    b"\x1b[M",
    b"\x1b[P", // Pause
];

/// Keypad characters with Num Lock on (Linux's pad_chars; Enter is '\r' here
/// like the main Enter key: the tty's ICRNL turns it into '\n' in canonical
/// mode, and raw-mode programs such as nano see the CR they expect).
const PAD_CHARS: &[u8; 21] = b"0123456789+-*/\r,.?()#";

/// Dead-key accents, standalone form (Linux's ret_diacr).
const ACCENT: [char; 6] = ['`', '\'', '^', '~', '"', ','];

/// (dead key, base letter, result).
const COMPOSE: &[(u8, char, char)] = &[
    (0, 'a', 'à'), (0, 'e', 'è'), (0, 'i', 'ì'), (0, 'o', 'ò'), (0, 'u', 'ù'),
    (0, 'A', 'À'), (0, 'E', 'È'), (0, 'I', 'Ì'), (0, 'O', 'Ò'), (0, 'U', 'Ù'),
    (1, 'a', 'á'), (1, 'e', 'é'), (1, 'i', 'í'), (1, 'o', 'ó'), (1, 'u', 'ú'), (1, 'y', 'ý'),
    (1, 'A', 'Á'), (1, 'E', 'É'), (1, 'I', 'Í'), (1, 'O', 'Ó'), (1, 'U', 'Ú'), (1, 'Y', 'Ý'),
    (2, 'a', 'â'), (2, 'e', 'ê'), (2, 'i', 'î'), (2, 'o', 'ô'), (2, 'u', 'û'),
    (2, 'A', 'Â'), (2, 'E', 'Ê'), (2, 'I', 'Î'), (2, 'O', 'Ô'), (2, 'U', 'Û'),
    (3, 'a', 'ã'), (3, 'n', 'ñ'), (3, 'o', 'õ'),
    (3, 'A', 'Ã'), (3, 'N', 'Ñ'), (3, 'O', 'Õ'),
    (4, 'a', 'ä'), (4, 'e', 'ë'), (4, 'i', 'ï'), (4, 'o', 'ö'), (4, 'u', 'ü'), (4, 'y', 'ÿ'),
    (4, 'A', 'Ä'), (4, 'E', 'Ë'), (4, 'I', 'Ï'), (4, 'O', 'Ö'), (4, 'U', 'Ü'),
    (5, 'c', 'ç'), (5, 'C', 'Ç'),
];

fn compose(dead: u8, c: char) -> Option<char> {
    let mut i = 0;
    while i < COMPOSE.len() {
        let (d, b, r) = COMPOSE[i];
        if d == dead && b == c {
            return Some(r);
        }
        i += 1;
    }
    None
}

// ------------------------------------------------------------------ keymaps

/// The key tables.  `present[m]` says whether modifier-state table `m`
/// exists (KDGKBENT answers K_NOSUCHMAP for the others).
pub struct Keymap {
    pub maps: [[u16; NR_KEYS]; MAX_MAPS],
    pub present: [bool; MAX_MAPS],
}

impl Keymap {
    pub const fn new() -> Keymap {
        Keymap { maps: [[0; NR_KEYS]; MAX_MAPS], present: [false; MAX_MAPS] }
    }

    /// KDGKBENT.
    pub fn get(&self, table: usize, index: usize) -> u16 {
        if table < MAX_MAPS && index < NR_KEYS && self.present[table] {
            self.maps[table][index]
        } else {
            K_NOSUCHMAP
        }
    }

    /// KDSKBENT.  Like Linux, a missing table springs into existence as a
    /// copy of table 0, and K_NOSUCHMAP at index 0 deletes a table.
    /// False = invalid table / index.
    pub fn set(&mut self, table: usize, index: usize, val: u16) -> bool {
        if table >= MAX_MAPS || index >= NR_KEYS {
            return false;
        }
        if index == 0 && val == K_NOSUCHMAP {
            if table != 0 {
                self.present[table] = false;
            }
            return true;
        }
        if val == K_NOSUCHMAP {
            return false;
        }
        if !self.present[table] {
            self.maps[table] = self.maps[0];
            self.present[table] = true;
        }
        self.maps[table][index] = val;
        true
    }

    /// Serialise in the `loadkeys --bkeymap` / BusyBox `dumpkmap` format:
    /// "bkeymap", 256 present flags, then 128 little-endian u16 per present
    /// table.  Returns the length.
    pub fn to_bkmap(&self, out: &mut [u8]) -> usize {
        let mut n = 0;
        for &b in b"bkeymap" {
            out[n] = b;
            n += 1;
        }
        for m in 0..MAX_MAPS {
            out[n] = self.present[m] as u8;
            n += 1;
        }
        for m in 0..MAX_MAPS {
            if self.present[m] {
                for key in 0..NR_KEYS {
                    let v = self.maps[m][key];
                    out[n] = v as u8;
                    out[n + 1] = (v >> 8) as u8;
                    n += 2;
                }
            }
        }
        n
    }
}

// ------------------------------------------------------------------ layouts

/// One layout: unshifted / shifted / AltGr tables indexed by keycode,
/// holding Unicode characters ('\0' = no character).  Only the source the
/// default keymap is generated from; the live data is the `Keymap`.
pub struct Layout {
    pub name: &'static str,
    pub plain: [char; 128],
    pub shift: [char; 128],
    pub altgr: [char; 128],
}

const fn fill(dst: &mut [char; 128], start: u8, s: &[char]) {
    let mut i = 0;
    while i < s.len() {
        dst[start as usize + i] = s[i];
        i += 1;
    }
}

/// Keys every layout shares: Esc, Backspace, Tab, Enter, Space.
const fn common(t: &mut [char; 128]) {
    t[0x01] = '\x1b';
    t[0x0E] = '\x7f';
    t[0x0F] = '\t';
    t[0x1C] = '\n';
    t[0x39] = ' ';
}

const US_PLAIN: [char; 128] = {
    let mut t = ['\0'; 128];
    common(&mut t);
    fill(&mut t, 0x02, &['1', '2', '3', '4', '5', '6', '7', '8', '9', '0', '-', '=']);
    fill(&mut t, 0x10, &['q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p', '[', ']']);
    fill(&mut t, 0x1E, &['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', ';', '\'']);
    t[0x29] = '`';
    t[0x2B] = '\\';
    fill(&mut t, 0x2C, &['z', 'x', 'c', 'v', 'b', 'n', 'm', ',', '.', '/']);
    t[0x56] = '\\'; // ISO key next to left shift
    t
};

const US_SHIFT: [char; 128] = {
    let mut t = ['\0'; 128];
    common(&mut t);
    fill(&mut t, 0x02, &['!', '@', '#', '$', '%', '^', '&', '*', '(', ')', '_', '+']);
    fill(&mut t, 0x10, &['Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', 'O', 'P', '{', '}']);
    fill(&mut t, 0x1E, &['A', 'S', 'D', 'F', 'G', 'H', 'J', 'K', 'L', ':', '"']);
    t[0x29] = '~';
    t[0x2B] = '|';
    fill(&mut t, 0x2C, &['Z', 'X', 'C', 'V', 'B', 'N', 'M', '<', '>', '?']);
    t[0x56] = '|';
    t
};

const US_ALTGR: [char; 128] = ['\0'; 128];

/// French AZERTY (the standard Windows FR layout).  AltGr layer:
///   é~ " # ' { ( [ - | è` _ \ ç^ à@ ) ] = }
/// so **AltGr+8 is the backslash** (and AltGr+9 = ^, AltGr+0 = @), AltGr+E
/// is the euro sign.  ^ and ¨ are typed as plain characters (no dead keys;
/// a `loadkmap` of a real fr keymap brings the dead keys).
const FR_PLAIN: [char; 128] = {
    let mut t = ['\0'; 128];
    common(&mut t);
    t[0x29] = '²';
    fill(&mut t, 0x02, &['&', 'é', '"', '\'', '(', '-', 'è', '_', 'ç', 'à', ')', '=']);
    fill(&mut t, 0x10, &['a', 'z', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p', '^', '$']);
    fill(&mut t, 0x1E, &['q', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', 'm', 'ù']);
    t[0x2B] = '*';
    fill(&mut t, 0x2C, &['w', 'x', 'c', 'v', 'b', 'n', ',', ';', ':', '!']);
    t[0x56] = '<';
    t
};

const FR_SHIFT: [char; 128] = {
    let mut t = ['\0'; 128];
    common(&mut t);
    fill(&mut t, 0x02, &['1', '2', '3', '4', '5', '6', '7', '8', '9', '0', '°', '+']);
    fill(&mut t, 0x10, &['A', 'Z', 'E', 'R', 'T', 'Y', 'U', 'I', 'O', 'P', '¨', '£']);
    fill(&mut t, 0x1E, &['Q', 'S', 'D', 'F', 'G', 'H', 'J', 'K', 'L', 'M', '%']);
    t[0x2B] = 'µ';
    fill(&mut t, 0x2C, &['W', 'X', 'C', 'V', 'B', 'N', '?', '.', '/', '§']);
    t[0x56] = '>';
    t
};

const FR_ALTGR: [char; 128] = {
    let mut t = ['\0'; 128];
    // 0x03..0x0D: the AltGr layer of the digit row.
    fill(&mut t, 0x03, &['~', '#', '{', '[', '|', '`', '\\', '^', '@', ']', '}']);
    t[0x12] = '€';
    t[0x1B] = '¤';
    t
};

pub static LAYOUT_US: Layout = Layout { name: "us", plain: US_PLAIN, shift: US_SHIFT, altgr: US_ALTGR };
pub static LAYOUT_FR: Layout = Layout { name: "fr", plain: FR_PLAIN, shift: FR_SHIFT, altgr: FR_ALTGR };

// -------------------------------------------------- default keymap generation

/// Keys whose keysym does not depend on the layout nor on the modifiers.
fn fixed_key(kc: usize) -> Option<u16> {
    Some(match kc {
        29 | 97 => K_CTRL,
        42 | 54 => K_SHIFT,
        56 => K_ALT,
        100 => K_ALTGR,
        58 => K_CAPS,
        69 => K_NUM,
        70 => K_HOLD,
        59..=68 => k(KT_FN, (kc - 59) as u16), // F1..F10
        87 => k(KT_FN, 10),                    // F11
        88 => k(KT_FN, 11),                    // F12
        102 => k(KT_FN, F_FIND),               // Home
        103 => K_UP,
        104 => k(KT_FN, F_PGUP),
        105 => K_LEFT,
        106 => K_RIGHT,
        107 => k(KT_FN, F_SELECT), // End
        108 => K_DOWN,
        109 => k(KT_FN, F_PGDN),
        110 => k(KT_FN, F_INSERT),
        111 => k(KT_FN, F_REMOVE), // Delete
        _ => return None,
    })
}

/// The keypad (KT_PAD): what Num Lock turns into digits or cursor keys.
fn pad_key(kc: usize) -> Option<u16> {
    Some(k(
        KT_PAD,
        match kc {
            71 => 7,
            72 => 8,
            73 => 9,
            74 => P_MINUS,
            75 => 4,
            76 => 5,
            77 => 6,
            78 => P_PLUS,
            79 => 1,
            80 => 2,
            81 => 3,
            82 => 0,
            83 => P_DOT,
            55 => P_STAR,
            96 => P_ENTER,
            98 => P_SLASH,
            _ => return None,
        },
    ))
}

/// A character as a keysym: Latin-1 as is, the rest as `cp ^ 0xF000`.
fn char_sym(c: char, letter: bool) -> u16 {
    let cp = c as u32;
    if cp < 0x100 {
        if letter {
            k(KT_LETTER, cp as u16)
        } else {
            cp as u16
        }
    } else if cp <= 0xFFFF {
        (cp as u16) ^ 0xF000
    } else {
        K_HOLE
    }
}

/// What Ctrl turns a character into (Ctrl-C = 3, Ctrl-\ = SIGQUIT...).
fn ctrl_of(c: char) -> char {
    match c {
        'a'..='z' | 'A'..='Z' => ((c as u8 | 0x20) & 0x1F) as char,
        '[' => '\x1b',
        '\\' => '\x1c',
        ']' => '\x1d',
        ' ' => '\0',
        _ => c,
    }
}

fn is_letter_key(plain: char, shift: char) -> bool {
    (plain as u32) < 0x100
        && plain.is_alphabetic()
        && plain != shift
        && shift.to_lowercase().next() == Some(plain)
}

fn latin_or_meta(c: char, alt: bool, letter: bool) -> u16 {
    if c == '\n' && !alt {
        return K_ENTER;
    }
    if alt {
        if (c as u32) < 0x100 {
            k(KT_META, c as u16)
        } else {
            K_HOLE
        }
    } else {
        char_sym(c, letter)
    }
}

/// Fill `km` with the default tables of a layout: the 16 combinations of
/// Shift / AltGr / Ctrl / Alt, like Linux's defkeymap.
pub fn build_default(km: &mut Keymap, l: &Layout) {
    for m in 0..MAX_MAPS {
        km.present[m] = m < 16;
        km.maps[m] = [K_HOLE; NR_KEYS];
    }
    for kc in 1..NR_KEYS {
        let (plain, shift, altgr) = (l.plain[kc], l.shift[kc], l.altgr[kc]);
        let letter = is_letter_key(plain, shift);
        for s in 0..16usize {
            let (sh, ag, ct, alt) = (s & 1 != 0, s & 2 != 0, s & 4 != 0, s & 8 != 0);
            let sym = if let Some(f) = fixed_key(kc) {
                f
            } else if let Some(p) = pad_key(kc) {
                if alt {
                    K_HOLE
                } else {
                    p
                }
            } else {
                // The character this key makes in this Shift/AltGr state.
                let c = if ag {
                    altgr
                } else if sh {
                    shift
                } else {
                    plain
                };
                if c == '\0' {
                    K_HOLE
                } else if ct {
                    // Ctrl works on the unshifted letter; AltGr+Ctrl does nothing.
                    if ag || plain == '\0' {
                        K_HOLE
                    } else {
                        latin_or_meta(ctrl_of(plain), alt, false)
                    }
                } else {
                    latin_or_meta(c, alt, letter && !ag)
                }
            };
            km.maps[s][kc] = sym;
        }
    }
}

// ------------------------------------------------- scancode -> keycode

/// Linux keycode of a set-1 scancode (`sc` without the 0x80 release bit),
/// `ext` when it came after an 0xE0 prefix.  0 = a key we ignore (the
/// "fake shift" pair that brackets Num-Lock'ed navigation keys).
pub fn keycode_of(sc: u8, ext: bool) -> u8 {
    if !ext {
        return if sc >= 0x59 { 0 } else { sc };
    }
    match sc {
        0x1C => 96,  // keypad enter
        0x1D => 97,  // right ctrl
        0x35 => 98,  // keypad slash
        0x37 => 99,  // print screen
        0x38 => 100, // right alt (AltGr)
        0x47 => 102, // home
        0x48 => 103, // up
        0x49 => 104, // page up
        0x4B => 105, // left
        0x4D => 106, // right
        0x4F => 107, // end
        0x50 => 108, // down
        0x51 => 109, // page down
        0x52 => 110, // insert
        0x53 => 111, // delete
        0x5B => 125, // left meta
        0x5C => 126, // right meta
        0x5D => 127, // menu
        _ => 0,      // E0 2A / E0 36 fake shifts, multimedia keys
    }
}

// ------------------------------------------------------------------ engine

/// Bytes a key press produced.
pub struct Out {
    pub buf: [u8; 64],
    pub len: usize,
    utf8: bool,
}

impl Out {
    pub const fn new(utf8: bool) -> Out {
        Out { buf: [0; 64], len: 0, utf8 }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    fn byte(&mut self, b: u8) {
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    fn put(&mut self, s: &[u8]) {
        for &b in s {
            self.byte(b);
        }
    }

    /// A character: UTF-8 in K_UNICODE mode, one Latin-1 byte in K_XLATE
    /// (characters beyond Latin-1 are dropped there).
    fn ch(&mut self, c: char) {
        if self.utf8 {
            let mut tmp = [0u8; 4];
            let s = c.encode_utf8(&mut tmp);
            self.put(s.as_bytes());
        } else if (c as u32) < 0x100 {
            self.byte(c as u32 as u8);
        }
    }
}

pub struct Engine {
    /// Shift keys held, per modifier bit.
    shift_down: [u8; 8],
    /// Modifier bits currently held.
    pub shift_state: u8,
    /// Latched modifiers (KT_LOCK keys).
    pub lockstate: u8,
    /// Scroll / Num / Caps Lock state = the LEDs.
    pub leds: u8,
    /// KDSKBLED's "default" LED state (kept for KDGKBLED).
    pub default_leds: u8,
    key_down: [bool; NR_KEYS],
    /// Keysym each held key had when it went down, so its release undoes
    /// the right thing even if the keymap changed in between.
    down_sym: [u16; NR_KEYS],
    /// Pending dead key + 1 (0 = none).
    dead: u8,
    /// KDSKBMODE.
    pub mode: u8,
}

impl Engine {
    pub const fn new() -> Engine {
        Engine {
            shift_down: [0; 8],
            shift_state: 0,
            lockstate: 0,
            leds: LED_NUM, // Num Lock on at boot, like most BIOSes
            default_leds: LED_NUM,
            key_down: [false; NR_KEYS],
            down_sym: [K_HOLE; NR_KEYS],
            dead: 0,
            mode: K_UNICODE,
        }
    }

    /// Forget held keys (after a keymap reload or a lost release).
    pub fn release_all(&mut self) {
        self.shift_down = [0; 8];
        self.shift_state = 0;
        self.key_down = [false; NR_KEYS];
        self.dead = 0;
    }

    fn utf8(&self) -> bool {
        self.mode != K_XLATE
    }

    pub fn new_out(&self) -> Out {
        Out::new(self.utf8())
    }

    fn sym(km: &Keymap, state: u8, kc: usize) -> u16 {
        if km.present[state as usize] {
            km.maps[state as usize][kc]
        } else {
            K_HOLE
        }
    }

    /// A character from a key, through the pending dead key.
    fn emit(&mut self, out: &mut Out, c: char) {
        if self.dead != 0 {
            let d = self.dead - 1;
            self.dead = 0;
            if c == ' ' {
                out.ch(ACCENT[d as usize]);
                return;
            }
            if let Some(r) = compose(d, c) {
                out.ch(r);
                return;
            }
            out.ch(ACCENT[d as usize]);
        }
        out.ch(c);
    }

    fn func(out: &mut Out, idx: u16) {
        if let Some(s) = FUNC.get(idx as usize) {
            out.put(s);
        }
    }

    fn cursor(out: &mut Out, v: u16) {
        out.put(b"\x1b[");
        out.byte(b"BDCA"[(v & 3) as usize]);
    }

    fn pad(&mut self, out: &mut Out, v: u16) {
        if self.leds & LED_NUM == 0 {
            // Num Lock off: the keypad is the navigation cluster.
            match v {
                P_COMMA | P_DOT => return Self::func(out, F_REMOVE),
                0 => return Self::func(out, F_INSERT),
                1 => return Self::func(out, F_SELECT),
                2 => return Self::cursor(out, 0),
                3 => return Self::func(out, F_PGDN),
                4 => return Self::cursor(out, 1),
                5 => return out.put(b"\x1b[G"),
                6 => return Self::cursor(out, 2),
                7 => return Self::func(out, F_FIND),
                8 => return Self::cursor(out, 3),
                9 => return Self::func(out, F_PGUP),
                _ => {}
            }
        }
        if let Some(&c) = PAD_CHARS.get(v as usize) {
            out.byte(c);
        }
    }

    /// One key event.  `kc` is a Linux keycode (1..127).  Whatever the key
    /// produces lands in `out`; lock changes show in `self.leds`.
    pub fn key(&mut self, km: &Keymap, kc: u8, down: bool, out: &mut Out) {
        let kc = (kc & 0x7F) as usize;
        if kc == 0 {
            return;
        }
        let rep = down && self.key_down[kc];
        self.key_down[kc] = down;

        match self.mode {
            K_MEDIUMRAW => {
                out.byte(kc as u8 | if down { 0 } else { 0x80 });
                return;
            }
            K_OFF | K_RAW => return,
            _ => {}
        }

        let fin = self.shift_state ^ self.lockstate;
        let mut sym = if down { Self::sym(km, fin, kc) } else { self.down_sym[kc] };
        if down {
            self.down_sym[kc] = sym;
        }
        let mut ty = sym >> 8;
        let mut val = sym & 0xFF;

        if ty >= NR_TYPES {
            // Unicode above Latin-1.
            if down {
                if let Some(c) = char::from_u32((sym ^ 0xF000) as u32) {
                    self.emit(out, c);
                }
            }
            return;
        }
        if ty == KT_LETTER {
            ty = KT_LATIN;
            if down && self.leds & LED_CAP != 0 && km.present[(fin ^ 1) as usize] {
                // Caps Lock: letters use the Shift table (flipped by Shift).
                sym = km.maps[(fin ^ 1) as usize][kc];
                val = sym & 0xFF;
            }
        }

        match ty {
            KT_LATIN => {
                if down {
                    self.emit(out, char::from(val as u8));
                }
            }
            KT_FN => {
                if down {
                    Self::func(out, val);
                }
            }
            KT_SPEC => {
                if down {
                    match sym {
                        K_ENTER => self.emit(out, '\r'), // CR like Linux: ICRNL makes it '\n' when cooked
                        K_CAPS if !rep => self.leds ^= LED_CAP,
                        K_NUM if !rep => self.leds ^= LED_NUM,
                        K_HOLD if !rep => self.leds ^= LED_SCR,
                        K_CAPSON if !rep => self.leds |= LED_CAP,
                        _ => {}
                    }
                }
            }
            KT_PAD => {
                if down {
                    self.pad(out, val);
                }
            }
            KT_DEAD => {
                if down && !rep && val < 6 {
                    let new = val as u8 + 1;
                    if self.dead == new {
                        out.ch(ACCENT[val as usize]);
                        self.dead = 0;
                    } else {
                        if self.dead != 0 {
                            out.ch(ACCENT[(self.dead - 1) as usize]);
                        }
                        self.dead = new;
                    }
                }
            }
            KT_CUR => {
                if down {
                    Self::cursor(out, val);
                }
            }
            KT_SHIFT => {
                if rep {
                    return;
                }
                let v = if val == 8 { 0 } else { val as usize }; // Caps_Shift = Shift
                if v < 8 {
                    if down {
                        self.shift_down[v] = self.shift_down[v].saturating_add(1);
                    } else if self.shift_down[v] > 0 {
                        self.shift_down[v] -= 1;
                    }
                    if self.shift_down[v] > 0 {
                        self.shift_state |= 1 << v;
                    } else {
                        self.shift_state &= !(1 << v);
                    }
                }
            }
            KT_META => {
                if down {
                    out.byte(0x1B);
                    self.emit(out, char::from(val as u8));
                }
            }
            KT_LOCK => {
                if down && !rep {
                    self.lockstate ^= 1 << (val & 7);
                }
            }
            _ => {} // KT_CONS, KT_ASCII, KT_SLOCK: not implemented
        }
    }
}
