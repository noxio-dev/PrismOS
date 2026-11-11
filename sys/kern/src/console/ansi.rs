// SPDX-License-Identifier: GPL-3.0-or-later
//
// ANSI/VT102 escape-sequence interpreter for the console screens: the
// state machine turns a character stream into `Action`s the screens can
// carry out (cursor moves, erases, attributes).  What it does not know it
// swallows -- an unknown sequence must never print as garbage, which is
// exactly what made vi and the colors unreadable before this module.
//
// The subset is the one the console's users actually emit: busybox vi
// (ESC[7m / ESC[m / ESC[K / ESC[J / ESC[r;cH / ESC[H), ash's line editing
// (ESC[C/D/K and friends), printf's SGR colors, and the few two-character
// escapes (ESC 7/8/D/M/E/c).  Serial output never comes through here: a
// real terminal applies its own escapes, the kernel only interprets what
// lands on its own screens.

/// The standard 16-color palette (the VGA text hardware's own defaults),
/// as RGB the framebuffer console maps SGR colors into.
pub const PALETTE16: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), (0xAA, 0x00, 0x00), (0x00, 0xAA, 0x00), (0xAA, 0xAA, 0x00),
    (0x00, 0x00, 0xAA), (0xAA, 0x00, 0xAA), (0x00, 0xAA, 0xAA), (0xAA, 0xAA, 0xAA),
    (0x55, 0x55, 0x55), (0xFF, 0x55, 0x55), (0x55, 0xFF, 0x55), (0xFF, 0xFF, 0x55),
    (0x55, 0x55, 0xFF), (0xFF, 0x55, 0xFF), (0x55, 0xFF, 0xFF), (0xFF, 0xFF, 0xFF),
];

/// The running text attributes, in SGR terms: two palette indices (the
/// bright half riding the high bit) and reverse video.
#[derive(Clone, Copy)]
pub struct Sgr {
    pub fg: u8,
    pub bg: u8,
    pub reverse: bool,
}

/// What the screens boot with (the kernel log's white on black).
pub const SGR_BOOT: Sgr = Sgr { fg: 15, bg: 0, reverse: false };
/// What `ESC[m` returns to -- light gray on black, the classic default.
pub const SGR_RESET: Sgr = Sgr { fg: 7, bg: 0, reverse: false };

/// The VGA text attribute byte for `s`: low nibble the foreground, high
/// nibble the background, reverse video swapping the two.
pub fn vga_attr(s: Sgr) -> u8 {
    let (fg, bg) = if s.reverse { (s.bg, s.fg) } else { (s.fg, s.bg) };
    ((bg & 7) << 4) | (fg & 15)
}

/// One screen operation the interpreter finished parsing.
#[derive(Clone, Copy)]
pub enum Action {
    /// Attributes for the glyphs printed from now on.
    Sgr(Sgr),
    /// Absolute cursor position, 0-based (CSI H / f).
    Cup(usize, usize),
    /// Relative cursor move (CSI A/B/C/D), (drow, dcol).
    Move(isize, isize),
    /// Absolute column (CSI G / `) or row (CSI d).
    Col(usize),
    Row(usize),
    /// Erase in line / display (CSI K / J), the VT100 parameter 0/1/2.
    EraseLine(u8),
    EraseDisplay(u8),
    /// Save / restore the cursor position (CSI s / u, ESC 7 / 8).
    SaveCursor,
    RestoreCursor,
    /// Hardware cursor visibility (CSI ?25h / ?25l).
    HideCursor,
    ShowCursor,
    /// Index (ESC D): one line down, scrolling at the bottom edge.
    Index,
    /// Reverse index (ESC M): one line up, scrolling at the top edge.
    ReverseIndex,
    /// NEL (ESC E): carriage return plus line feed.
    Nel,
    /// Full reset (ESC c): attributes home and everything erased.
    Reset,
    /// SU (CSI S): scroll the whole screen up n lines.
    ScrollUp(usize),
    /// DECSTBM (CSI t;b r): the scrolling region, (top, bottom) in the
    /// 1-based VT spelling the screens clamp into their own grid -- so
    /// `None` (no parameters, ESC[r) is just (0, "as many as there are").
    Stbm(Option<(usize, usize)>),
}

/// What `feed` makes of one character.
pub enum Out {
    /// A ground character the caller prints the usual way (including the
    /// newlines, tabs and backspaces the screens already understand).
    Print(char),
    /// Consumed by the sequence machinery -- the screen shows nothing.
    Swallow,
    /// A finished action to apply.
    Act(Action),
}

/// Sentinel parameter slot: "no parameter arrived".
const ABSENT: u32 = u32::MAX;

/// One parser.  The state is per-call-chain in practice (the console
/// paths are the only feeders), so a single instance is shared by both
/// console write paths -- they write the same screens anyway.
pub struct Parser {
    /// 0 ground, 1 after ESC, 2 in CSI, 3 in OSC, 4 OSC's ESC, 5 the
    /// single designator char after an ESC's intermediates.
    st: u8,
    vals: [u32; 8],
    n: usize,
    private: bool,
    sgr: Sgr,
}

impl Parser {
    pub const fn new() -> Self {
        Parser {
            st: 0,
            vals: [ABSENT; 8],
            n: 0,
            private: false,
            sgr: SGR_BOOT,
        }
    }

    pub fn feed(&mut self, ch: char) -> Out {
        match self.st {
            0 => self.ground(ch),
            1 => self.after_esc(ch),
            2 => self.csi(ch),
            3 => self.osc(ch),
            4 => match ch {
                '\\' => {
                    self.st = 0;
                    Out::Swallow
                }
                _ => {
                    self.st = 0;
                    self.feed(ch)
                }
            },
            _ => {
                self.st = 0;
                Out::Swallow
            }
        }
    }

    fn ground(&mut self, ch: char) -> Out {
        match ch {
            '\x1b' => {
                self.st = 1;
                Out::Swallow
            }
            // The controls the screens already carry out go through.  BEL
            // and DEL are noise here -- the line editor's failed-completion
            // beep and vi's error bell ring on the serial line (a real
            // terminal acts on them), on our own screens they only ever
            // printed as '?' -- so they are swallowed.
            '\n' | '\r' | '\t' | '\x08' => Out::Print(ch),
            '\x07' | '\x7f' => Out::Swallow,
            c if (c as u32) < 0x20 => Out::Print('?'),
            c => Out::Print(c),
        }
    }

    fn after_esc(&mut self, ch: char) -> Out {
        match ch {
            '[' => {
                self.st = 2;
                self.vals = [ABSENT; 8];
                self.n = 0;
                self.private = false;
                Out::Swallow
            }
            ']' => {
                self.st = 3;
                Out::Swallow
            }
            // ESC ( B and friends: one intermediate then one designator.
            c if ('\u{20}'..='\u{2F}').contains(&c) => {
                self.st = 5;
                Out::Swallow
            }
            '7' => {
                self.st = 0;
                Out::Act(Action::SaveCursor)
            }
            '8' => {
                self.st = 0;
                Out::Act(Action::RestoreCursor)
            }
            'D' => {
                self.st = 0;
                Out::Act(Action::Index)
            }
            'M' => {
                self.st = 0;
                Out::Act(Action::ReverseIndex)
            }
            'E' => {
                self.st = 0;
                Out::Act(Action::Nel)
            }
            'c' => {
                self.st = 0;
                Out::Act(Action::Reset)
            }
            '=' | '>' => {
                self.st = 0;
                Out::Swallow
            }
            // C0 controls execute even mid-escape (ECMA 48).
            c if (c as u32) < 0x20 => {
                self.st = 0;
                self.ground(ch)
            }
            _ => {
                self.st = 0;
                Out::Swallow
            }
        }
    }

    fn csi(&mut self, ch: char) -> Out {
        if let Some(d) = ch.to_digit(10) {
            let i = self.n.min(7);
            self.vals[i] = if self.vals[i] == ABSENT {
                d
            } else {
                self.vals[i].saturating_mul(10).saturating_add(d).min(32_767)
            };
            return Out::Swallow;
        }
        match ch {
            ';' => {
                self.n = (self.n + 1).min(7);
                Out::Swallow
            }
            // Sub-parameters (SGR 38:5:196's colons) fold into the slot.
            ':' => Out::Swallow,
            '?' => {
                self.private = true;
                Out::Swallow
            }
            // Intermediate bytes before the final: stay open, the final
            // still terminates the sequence.
            c if ('\u{20}'..='\u{2F}').contains(&c) => Out::Swallow,
            c if ('\u{40}'..='\u{7E}').contains(&c) => {
                self.st = 0;
                self.finish(c)
            }
            // C0 and anything else: swallowed without closing, the way a
            // real terminal executes them inline.
            _ => Out::Swallow,
        }
    }

    fn osc(&mut self, ch: char) -> Out {
        match ch {
            '\x07' => {
                self.st = 0;
                Out::Swallow
            }
            '\x1b' => {
                self.st = 4;
                Out::Swallow
            }
            _ => Out::Swallow,
        }
    }

    fn param(&self, i: usize) -> Option<u32> {
        match self.vals[i.min(7)] {
            ABSENT => None,
            v => Some(v),
        }
    }

    /// Parameter `i` with `dflt` for absent, and 0 promoted to `dflt`
    /// (the VT100 rule for cursor distances).
    fn num(&self, i: usize, dflt: u32) -> u32 {
        self.param(i).filter(|&v| v != 0).unwrap_or(dflt).max(1)
    }

    fn finish(&mut self, f: char) -> Out {
        match f {
            'H' | 'f' => {
                let row = self.num(0, 1).min(65_535) as usize;
                let col = self.num(1, 1).min(65_535) as usize;
                Out::Act(Action::Cup(row - 1, col - 1))
            }
            'A' => Out::Act(Action::Move(-(self.num(0, 1) as isize), 0)),
            'B' => Out::Act(Action::Move(self.num(0, 1) as isize, 0)),
            'C' => Out::Act(Action::Move(0, self.num(0, 1) as isize)),
            'D' => Out::Act(Action::Move(0, -(self.num(0, 1) as isize))),
            'G' | '`' => Out::Act(Action::Col(self.num(0, 1) as usize - 1)),
            'd' => Out::Act(Action::Row(self.num(0, 1) as usize - 1)),
            'J' => Out::Act(Action::EraseDisplay(self.param(0).unwrap_or(0).min(2) as u8)),
            'K' => Out::Act(Action::EraseLine(self.param(0).unwrap_or(0).min(2) as u8)),
            'm' => Out::Act(Action::Sgr(self.sgr_params())),
            's' => Out::Act(Action::SaveCursor),
            'u' => Out::Act(Action::RestoreCursor),
            'h' | 'l' if self.private => {
                // The one private mode the screens can honour: the
                // hardware cursor's visibility.  The rest are swallowed.
                if self.num(0, 0) == 25 {
                    if f == 'h' {
                        Out::Act(Action::ShowCursor)
                    } else {
                        Out::Act(Action::HideCursor)
                    }
                } else {
                    Out::Swallow
                }
            }
            'S' => Out::Act(Action::ScrollUp(self.num(0, 1) as usize)),
            'r' => {
                // DECSTBM: the scrolling region.  1-based top;bottom, each
                // defaulting to its edge (top 1, bottom "the screen's own
                // last row" -- the screens clamp).  The screens ignore a
                // region of fewer than two lines, like xterm.
                let t = self.param(0).map(|v| (v.max(1) as usize) - 1);
                let b = self.param(1).map(|v| v as usize);
                Out::Act(Action::Stbm(match (t, b) {
                    (None, None) => None,
                    (t, b) => Some((t.unwrap_or(0), b.unwrap_or(65_535))),
                }))
            }
            // DSR, DA, IL/DL, ECH, DCH, window ops: known but not
            // implemented -- swallowed, never printed.
            _ => Out::Swallow,
        }
    }

    /// Fold the collected SGR parameters into the running attribute.
    fn sgr_params(&mut self) -> Sgr {
        let mut s = self.sgr;
        let mut any = false;
        for i in 0..8 {
            let Some(v) = self.param(i) else { continue };
            any = true;
            match v {
                0 => s = SGR_RESET,
                // Bold rides the palette's bright half; 22 back down.
                1 => s.fg |= 8,
                2 | 21 | 22 => s.fg &= 7,
                7 => s.reverse = true,
                27 => s.reverse = false,
                30..=37 => s.fg = (v - 30) as u8,
                39 => s.fg = 7,
                40..=47 => s.bg = (v - 40) as u8,
                49 => s.bg = 0,
                90..=97 => s.fg = (v - 90 + 8) as u8,
                100..=107 => s.bg = (v - 100 + 8) as u8,
                // Italic, underline, blink, 256/RGB colors, strikes: no
                // hardware meaning here -- ignored, sequence still clean.
                _ => {}
            }
        }
        if !any {
            // Bare ESC[m is a reset.
            s = SGR_RESET;
        }
        self.sgr = s;
        s
    }
}
