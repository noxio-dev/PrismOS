// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev

//! Snake for PrismOS.
//!
//! The rules live in `Game`, a plain state machine with no I/O.  Two front
//! ends draw it: a pixel-art one on the double-buffered framebuffer (UEFI),
//! and a text one for VGA text mode (BIOS).  Nothing is printed through the
//! console while the pixel version runs, so the board never scrolls, never
//! flickers and never goes through the serial line.

use alloc::format;

use crate::console::gfx::Surface;

const W: usize = 32;
const H: usize = 20;
const CELLS: usize = W * H;
const START_TICK_MS: u64 = 150;
const MIN_TICK_MS: u64 = 60;
/// Direction changes buffered between two steps, so two quick key presses
/// (a U-turn made of two quarter turns) are not lost.
const QUEUE: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    fn opposite(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Up, Self::Down) | (Self::Down, Self::Up) | (Self::Left, Self::Right) | (Self::Right, Self::Left)
        )
    }

    fn delta(self) -> (i32, i32) {
        match self {
            Self::Up => (0, -1),
            Self::Down => (0, 1),
            Self::Left => (-1, 0),
            Self::Right => (1, 0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Playing,
    Paused,
    Dead,
    Won,
}

struct Game {
    /// Cell indices (y * W + x), body[0] is the head.
    body: [u16; CELLS],
    len: usize,
    occupied: [bool; CELLS],
    dir: Dir,
    queue: [Dir; QUEUE],
    qlen: usize,
    food: usize,
    score: usize,
    best: usize,
    state: State,
    seed: u32,
}

impl Game {
    fn new(best: usize, seed: u32) -> Game {
        let mut g = Game {
            body: [0; CELLS],
            len: 4,
            occupied: [false; CELLS],
            dir: Dir::Right,
            queue: [Dir::Right; QUEUE],
            qlen: 0,
            food: 0,
            score: 0,
            best,
            state: State::Playing,
            seed: seed | 1,
        };
        let (sx, sy) = (W / 2, H / 2);
        for i in 0..g.len {
            let cell = sy * W + sx - i;
            g.body[i] = cell as u16;
            g.occupied[cell] = true;
        }
        g.place_food();
        g
    }

    fn rand(&mut self) -> u32 {
        // xorshift32: plenty for picking a food cell.
        let mut x = self.seed;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.seed = x;
        x
    }

    fn place_food(&mut self) {
        let mut c = self.rand() as usize % CELLS;
        for _ in 0..CELLS {
            if !self.occupied[c] {
                self.food = c;
                return;
            }
            c = (c + 1) % CELLS;
        }
    }

    /// Queue a turn.  Turns are checked against the last queued direction,
    /// so "up, left" works in one step interval but a 180 degree turn never.
    fn turn(&mut self, d: Dir) {
        let last = if self.qlen > 0 { self.queue[self.qlen - 1] } else { self.dir };
        if d == last || d.opposite(last) || self.qlen >= QUEUE {
            return;
        }
        self.queue[self.qlen] = d;
        self.qlen += 1;
    }

    /// Milliseconds between two steps: it gets faster as the score grows.
    fn tick_ms(&self) -> u64 {
        START_TICK_MS.saturating_sub(self.score as u64 * 4).max(MIN_TICK_MS)
    }

    /// Advance the snake by one cell.
    fn step(&mut self) {
        if self.state != State::Playing {
            return;
        }
        if self.qlen > 0 {
            self.dir = self.queue[0];
            self.queue.copy_within(1..QUEUE, 0);
            self.qlen -= 1;
        }
        let head = self.body[0] as usize;
        let (x, y) = ((head % W) as i32, (head / W) as i32);
        let (dx, dy) = self.dir.delta();
        let (nx, ny) = (x + dx, y + dy);
        if nx < 0 || ny < 0 || nx >= W as i32 || ny >= H as i32 {
            return self.die();
        }
        let new = ny as usize * W + nx as usize;
        let eating = new == self.food;
        let tail = self.body[self.len - 1] as usize;
        // The tail cell is free again this step, unless the snake grows.
        if self.occupied[new] && !(new == tail && !eating) {
            return self.die();
        }
        if eating {
            self.len += 1;
        } else {
            self.occupied[tail] = false;
        }
        for i in (1..self.len).rev() {
            self.body[i] = self.body[i - 1];
        }
        self.body[0] = new as u16;
        self.occupied[new] = true;
        if eating {
            self.score += 1;
            self.best = self.best.max(self.score);
            if self.len == CELLS {
                self.state = State::Won;
            } else {
                self.place_food();
            }
        }
    }

    fn die(&mut self) {
        self.best = self.best.max(self.score);
        self.state = State::Dead;
    }
}

enum Action {
    Turn(Dir),
    Pause,
    Retry,
    Quit,
}

fn decode(c: u8) -> Option<Action> {
    Some(match c {
        b'z' | b'Z' | b'w' | b'W' => Action::Turn(Dir::Up),
        b's' | b'S' => Action::Turn(Dir::Down),
        b'q' | b'Q' | b'a' | b'A' => Action::Turn(Dir::Left),
        b'd' | b'D' => Action::Turn(Dir::Right),
        b'p' | b'P' | b' ' => Action::Pause,
        b'r' | b'R' | b'\n' | b'\r' => Action::Retry,
        0x1b => Action::Quit,
        _ => return None,
    })
}

pub fn run() {
    while crate::hw::kbd::pop_char().is_some() {}
    let g = match crate::console::with_surface(|s| (s.w as i32, s.h as i32)) {
        Some((w, h)) => run_gfx(w, h),
        None => run_text(),
    };
    crate::console::clear();
    crate::kprintln!("Snake: score {} (best {}).", g.score, g.best);
}

/// What a key press does to the game; returns (redraw needed, quit).
fn handle(game: &mut Game, c: u8, next_step: &mut u64) -> (bool, bool) {
    let now = crate::hw::pit::uptime_ms();
    match decode(c) {
        Some(Action::Quit) => (false, true),
        Some(Action::Turn(d)) => {
            if game.state == State::Playing {
                game.turn(d);
            }
            (false, false)
        }
        Some(Action::Pause) => {
            match game.state {
                State::Playing => game.state = State::Paused,
                State::Paused => {
                    game.state = State::Playing;
                    *next_step = now + game.tick_ms();
                }
                _ => {}
            }
            (true, false)
        }
        Some(Action::Retry) => {
            if matches!(game.state, State::Dead | State::Won) {
                *game = Game::new(game.best, now as u32 ^ game.seed);
                *next_step = now + game.tick_ms();
                return (true, false);
            }
            (false, false)
        }
        None => (false, false),
    }
}

/// Shared loop: poll the keyboard, step on schedule, redraw on change.
fn game_loop(mut draw: impl FnMut(&Game)) -> Game {
    let mut game = Game::new(0, crate::hw::pit::uptime_ms() as u32 ^ 0x9E37_79B9);
    let mut next_step = crate::hw::pit::uptime_ms() + game.tick_ms();
    draw(&game);
    loop {
        let mut dirty = false;
        while let Some(c) = crate::hw::kbd::pop_char() {
            let (redraw, quit) = handle(&mut game, c, &mut next_step);
            if quit {
                return game;
            }
            dirty |= redraw;
        }
        let now = crate::hw::pit::uptime_ms();
        if game.state == State::Playing && now >= next_step {
            game.step();
            // No catch-up burst after a long stall.
            next_step = (next_step + game.tick_ms()).max(now + 1);
            dirty = true;
        }
        if dirty {
            draw(&game);
        }
        // Sleep until the next interrupt (timer tick or key press).
        crate::arch::io::hlt();
    }
}

// ------------------------------------------------------------ pixel front end

struct Layout {
    cell: i32,
    bx: i32,
    by: i32,
    bw: i32,
    bh: i32,
    text: usize,
    /// Pixel rows of the title line and of the score line.
    title_y: i32,
    stat_y: i32,
}

fn layout(w: i32, h: i32) -> Layout {
    let text = (h as usize / 540).clamp(1, 3);
    let th = 16 * text as i32;
    let (title_y, stat_y) = (12, 12 + th + 8);
    let top = stat_y + th + 20;
    let avail_w = w - 32;
    let avail_h = h - top - 24;
    let cell = (avail_w / W as i32).min(avail_h / H as i32).clamp(4, 64);
    let (bw, bh) = (cell * W as i32, cell * H as i32);
    Layout { cell, bx: (w - bw) / 2, by: top + ((avail_h - bh) / 2).max(0), bw, bh, text, title_y, stat_y }
}

struct Palette {
    bg: u32,
    cell_a: u32,
    cell_b: u32,
    wall: u32,
    text: u32,
    dim: u32,
    food: u32,
    food_hi: u32,
    eye: u32,
    box_bg: u32,
}

impl Palette {
    fn new(s: &Surface) -> Palette {
        Palette {
            bg: s.rgb(10, 12, 20),
            cell_a: s.rgb(20, 24, 36),
            cell_b: s.rgb(25, 30, 44),
            wall: s.rgb(96, 108, 150),
            text: s.rgb(232, 236, 246),
            dim: s.rgb(128, 138, 164),
            food: s.rgb(255, 72, 84),
            food_hi: s.rgb(255, 170, 170),
            eye: s.rgb(8, 24, 12),
            box_bg: s.rgb(6, 8, 14),
        }
    }

    /// Body colour: bright at the head, darker towards the tail.
    fn body(&self, s: &Surface, i: usize, len: usize) -> u32 {
        let t = (i * 255 / len.max(1)) as i32;
        let lerp = |a: i32, b: i32| (a + (b - a) * t / 255) as u8;
        s.rgb(lerp(110, 22), lerp(250, 120), lerp(130, 70))
    }
}

fn draw_static(s: &mut Surface, lay: &Layout, pal: &Palette) {
    s.clear(pal.bg);
    s.text(lay.bx, lay.title_y, "PRISM SNAKE", lay.text, pal.text, pal.bg);
    let hint = "ZQSD / WASD move   P pause   R retry   ESC quit";
    let hw = Surface::text_width(hint, lay.text) as i32;
    s.text(lay.bx + lay.bw - hw, lay.title_y, hint, lay.text, pal.dim, pal.bg);
    s.stroke_rect(lay.bx - 6, lay.by - 6, lay.bw + 12, lay.bh + 12, 4, pal.wall);
}

fn draw_overlay(s: &mut Surface, lay: &Layout, pal: &Palette, title: &str, sub: &str, hint: &str) {
    let sc = lay.text;
    let lines = [(title, sc + 1, pal.text), (sub, sc, pal.dim), (hint, sc, pal.dim)];
    let mut width = 0;
    let mut height = 12;
    for (t, scale, _) in lines {
        width = width.max(Surface::text_width(t, scale) as i32);
        height += 16 * scale as i32 + 10;
    }
    let (bw, bh) = (width + 48, height + 12);
    let (x, y) = (lay.bx + (lay.bw - bw) / 2, lay.by + (lay.bh - bh) / 2);
    s.fill_rect(x, y, bw, bh, pal.box_bg);
    s.stroke_rect(x, y, bw, bh, 3, pal.wall);
    let mut ty = y + 18;
    for (t, scale, col) in lines {
        let tw = Surface::text_width(t, scale) as i32;
        s.text(x + (bw - tw) / 2, ty, t, scale, col, pal.box_bg);
        ty += 16 * scale as i32 + 10;
    }
}

fn draw_board(s: &mut Surface, lay: &Layout, pal: &Palette, g: &Game) {
    let c = lay.cell;
    // Score line, erased first because its width changes with the digits.
    let th = 16 * lay.text as i32;
    let status = format!("Score {}    Best {}    Speed {}%", g.score, g.best, 15000 / g.tick_ms());
    s.fill_rect(lay.bx, lay.stat_y, lay.bw, th, pal.bg);
    s.text(lay.bx, lay.stat_y, &status, lay.text, pal.text, pal.bg);

    // Checkerboard floor.
    for y in 0..H {
        for x in 0..W {
            let col = if (x + y) % 2 == 0 { pal.cell_a } else { pal.cell_b };
            s.fill_rect(lay.bx + x as i32 * c, lay.by + y as i32 * c, c, c, col);
        }
    }

    // Food: a rounded octagon with a highlight.
    let (fx, fy) = ((g.food % W) as i32, (g.food / W) as i32);
    let (cx, cy) = (lay.bx + fx * c + c / 2, lay.by + fy * c + c / 2);
    let r = (c * 38 / 100).max(2);
    let k = (r * 42 / 100).max(1);
    s.fill_convex(
        &[(cx - k, cy - r), (cx + k, cy - r), (cx + r, cy - k), (cx + r, cy + k), (cx + k, cy + r), (cx - k, cy + r), (cx - r, cy + k), (cx - r, cy - k)],
        pal.food,
    );
    let hi = (c / 6).max(1);
    s.fill_rect(cx - r / 2, cy - r / 2, hi, hi, pal.food_hi);

    // Body, tail first so the head ends up on top.
    for i in (0..g.len).rev() {
        let cell = g.body[i] as usize;
        let (x, y) = ((cell % W) as i32 * c + lay.bx, (cell / W) as i32 * c + lay.by);
        let col = pal.body(s, i, g.len);
        let inset = if i == 0 { 0 } else { 1 };
        s.fill_rect(x + inset, y + inset, c - 2 * inset, c - 2 * inset, col);
    }

    // Eyes, on the side the head is facing.
    let head = g.body[0] as usize;
    let (hx, hy) = ((head % W) as i32 * c + lay.bx, (head / W) as i32 * c + lay.by);
    let e = (c / 6).max(2);
    let (ex, ey) = match g.dir {
        Dir::Up => ([c / 4 - e / 2, c * 3 / 4 - e / 2], [c / 5, c / 5]),
        Dir::Down => ([c / 4 - e / 2, c * 3 / 4 - e / 2], [c * 4 / 5 - e, c * 4 / 5 - e]),
        Dir::Left => ([c / 5, c / 5], [c / 4 - e / 2, c * 3 / 4 - e / 2]),
        Dir::Right => ([c * 4 / 5 - e, c * 4 / 5 - e], [c / 4 - e / 2, c * 3 / 4 - e / 2]),
    };
    for i in 0..2 {
        s.fill_rect(hx + ex[i], hy + ey[i], e, e, pal.eye);
    }

    match g.state {
        State::Paused => draw_overlay(s, lay, pal, "PAUSED", "", "P: resume    ESC: quit"),
        State::Dead => {
            let sub = format!("Score {}   Best {}", g.score, g.best);
            draw_overlay(s, lay, pal, "GAME OVER", &sub, "R: play again    ESC: quit");
        }
        State::Won => {
            let sub = format!("You filled the board: {}", g.score);
            draw_overlay(s, lay, pal, "YOU WIN!", &sub, "R: play again    ESC: quit");
        }
        State::Playing => {}
    }
}

fn run_gfx(w: i32, h: i32) -> Game {
    let lay = layout(w, h);
    let mut first = true;
    game_loop(|g| {
        crate::console::with_surface(|s| {
            let pal = Palette::new(s);
            if first {
                draw_static(s, &lay, &pal);
                first = false;
            }
            draw_board(s, &lay, &pal, g);
        });
    })
}

// ------------------------------------------------------------- text front end

fn run_text() -> Game {
    crate::console::clear();
    game_loop(draw_text)
}

/// Redraw the board in place, always the same 25 lines of the same width, so
/// nothing needs clearing between frames.
fn draw_text(g: &Game) {
    crate::console::home();
    let status = match g.state {
        State::Playing => "",
        State::Paused => "PAUSED - P: resume",
        State::Dead => "GAME OVER - R: play again, ESC: quit",
        State::Won => "YOU WIN! - R: play again, ESC: quit",
    };
    crate::kprintln!("{:<60}", format!("=== PrismOS Snake ===  Score: {}  Best: {}", g.score, g.best));
    crate::kprintln!("{:<60}", "ZQSD/WASD move | P pause | R retry | ESC quit");
    crate::kprintln!("{:<60}", status);
    crate::kprint!("+");
    for _ in 0..W {
        crate::kprint!("-");
    }
    crate::kprintln!("+");
    for y in 0..H {
        crate::kprint!("|");
        for x in 0..W {
            let pos = y * W + x;
            let ch = if g.body[0] as usize == pos {
                '@'
            } else if g.occupied[pos] {
                'o'
            } else if g.food == pos {
                '*'
            } else {
                ' '
            };
            crate::kprint!("{}", ch);
        }
        crate::kprintln!("|");
    }
    // Last line without a newline: printing it must not scroll the screen.
    crate::kprint!("+");
    for _ in 0..W {
        crate::kprint!("-");
    }
    crate::kprint!("+");
}
