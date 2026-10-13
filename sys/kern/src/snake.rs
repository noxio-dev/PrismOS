
 // SPDX-License-Identifier: GPL-3.0-or-later
 // Copyright (c) 2026 noxio-dev

 //! Text-mode Snake for PrismOS.

 const W: usize = 32;
 const H: usize = 16;
 const CELLS: usize = W * H;
 const TICK_MS: u64 = 140;

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
             (Self::Up, Self::Down)
                 | (Self::Down, Self::Up)
                 | (Self::Left, Self::Right)
                 | (Self::Right, Self::Left)
         )
     }
 }

 pub fn run() {
     let mut snake = [0usize; CELLS];
     let mut len = 4usize;

     // Initial snake, facing right.
     let start_x = W / 2;
     let start_y = H / 2;
     for i in 0..len {
         snake[i] = (start_y * W) + start_x - i;
     }

     let mut dir = Dir::Right;
     let mut next_dir = dir;
     let mut food = (3 * W) + 8;
     let mut score = 0usize;
     let mut paused = false;
     let mut dead = false;
     let mut seed = crate::pit::uptime_ms() as usize;

     crate::console::clear();
     crate::kprintln!("Starting PrismOS Snake...");
     crate::kprintln!("Controls: ZQSD / WASD, P pause, ESC quit");
     crate::kprintln!("");

     while !dead {
         // Consume available keyboard characters without waiting for input.
         while let Some(c) = crate::kbd::pop_char() {
             let requested = match c {
                 b'z' | b'Z' | b'w' | b'W' => Some(Dir::Up),
                 b's' | b'S' => Some(Dir::Down),
                 b'q' | b'Q' | b'a' | b'A' => Some(Dir::Left),
                 b'd' | b'D' => Some(Dir::Right),
                 b'p' | b'P' => {
                     paused = !paused;
                     None
                 }
                 0x1b => {
                     crate::console::clear();
                     crate::kprintln!("Snake: returning to shell.");
                     return;
                 }
                 _ => None,
             };

             if let Some(d) = requested {
                 if !d.opposite(dir) {
                     next_dir = d;
                 }
             }
         }

         if paused {
             draw(&snake, len, food, score, true);
             wait_for_tick();
             continue;
         }

         dir = next_dir;
         let head = snake[0];
         let x = head % W;
         let y = head / W;

         let (nx, ny) = match dir {
             Dir::Up => {
                 if y == 0 {
                     dead = true;
                     break;
                 }
                 (x, y - 1)
             }
             Dir::Down => {
                 if y + 1 >= H {
                     dead = true;
                     break;
                 }
                 (x, y + 1)
             }
             Dir::Left => {
                 if x == 0 {
                     dead = true;
                     break;
                 }
                 (x - 1, y)
             }
             Dir::Right => {
                 if x + 1 >= W {
                     dead = true;
                     break;
                 }
                 (x + 1, y)
             }
         };

         let new_head = ny * W + nx;
         let eating = new_head == food;

         // The tail moves away on a normal step, so exclude it from collision
         // checks unless the snake is growing.
         let collision_len = if eating { len } else { len.saturating_sub(1) };
         for &segment in snake.iter().take(collision_len) {
             if segment == new_head {
                 dead = true;
                 break;
             }
         }

         if dead {
             break;
         }

         if eating {
             if len < CELLS {
                 len += 1;
             }
             score += 1;
         }

         for i in (1..len).rev() {
             snake[i] = snake[i - 1];
         }
         snake[0] = new_head;

         if eating {
             if len == CELLS {
                 draw(&snake, len, food, score, false);
                 crate::kprintln!("");
                 crate::kprintln!("You filled the board! You win!");
                 break;
             }

             // Small deterministic pseudo-random search for an empty cell.
             seed = seed
                 .wrapping_mul(1_664_525)
                 .wrapping_add(1_013_904_223)
                 .wrapping_add(crate::pit::uptime_ms() as usize);

             let mut candidate = seed % CELLS;
             for _ in 0..CELLS {
                 let occupied = snake.iter().take(len).any(|&s| s == candidate);
                 if !occupied {
                     food = candidate;
                     break;
                 }
                 candidate = (candidate + 1) % CELLS;
             }
         }

         draw(&snake, len, food, score, false);
         wait_for_tick();
     }

     if dead {
         draw(&snake, len, food, score, false);
         crate::kprintln!("");
         crate::kprintln!("Game over! Score: {}", score);
     }

     crate::kprintln!("Press any key to return to the shell...");
     loop {
         if crate::kbd::pop_char().is_some() {
             break;
         }
         // Wait for an interrupt instead of spinning continuously.
         unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
     }

     crate::console::clear();
 }

 fn draw(snake: &[usize; CELLS], len: usize, food: usize, score: usize, paused: bool) {
     crate::console::clear();

     crate::kprintln!("=== PrismOS Snake ===  Score: {}", score);
     crate::kprintln!("Controls: ZQSD/WASD | P pause | ESC quit");
     if paused {
         crate::kprintln!("             PAUSED");
     } else {
         crate::kprintln!("");
     }

     for y in 0..H {
         crate::kprint!("#");
         for x in 0..W {
             let pos = y * W + x;
             let ch = if snake[0] == pos {
                 '@'
             } else if snake.iter().take(len).any(|&s| s == pos) {
                 'o'
             } else if food == pos {
                 '*'
             } else {
                 ' '
             };
             crate::kprint!("{}", ch);
         }
         crate::kprintln!("#");
     }

     crate::kprint!("+");
     for _ in 0..W {
         crate::kprint!("-");
     }
     crate::kprintln!("+");
 }

 fn wait_for_tick() {
     let start = crate::pit::uptime_ms();
     while crate::pit::uptime_ms().wrapping_sub(start) < TICK_MS {
         // HLT is safe only if interrupts are enabled and a timer IRQ can wake
         // the CPU. PrismOS's PIT/interrupt setup should satisfy that contract.
         unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
     }
 }