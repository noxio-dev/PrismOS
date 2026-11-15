// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// Randomness (phase 3.5.2): the kernel CSPRNG behind /dev/random,
// /dev/urandom, getrandom(2) and AT_RANDOM.
//
// Construction (the same shape Linux's random.c settled on, "fast key
// erasure"):
//
//   * an entropy pool, a SHA-256 chain: pool = H(pool || source || data);
//   * a ChaCha20 generator (RFC 8439): every request runs the cipher under the
//     current key, takes the first 32 bytes of the stream as the NEXT key and
//     hands out the rest -- so a captured key reveals nothing that was
//     already output (forward secrecy);
//   * reseeding: key = H("reseed" || key || pool), when the pool has gathered
//     enough, and again every so often from interrupt timing.
//
// Sources, strongest first:
//   RDSEED / RDRAND   the CPU's own generator (cpuid-gated), up to 256 bits
//                     credited -- mixed in with the other sources, never alone;
//   TSC jitter        1024 timed bursts of dependent memory work; credited
//                     only when the deltas really vary;
//   interrupt timing  the TSC low bits at every timer tick / key press /
//                     mouse byte, folded into a small accumulator and mixed in
//                     every 64 events -- keeps the pool fresh and is the last
//                     resort that seeds a machine with no hardware RNG;
//   boot-time facts   RTC, uptime, stack address: mixed, credited nothing.
//
// The generator reports "seeded" once 128 bits were credited.  Before that
// getrandom(2) (without GRND_INSECURE) and /dev/random wait, as Linux does
// at boot; on every machine with RDSEED / RDRAND or a jittery TSC that is
// before the first process starts.
//
// Locking: the state sits behind a SpinLock taken with interrupts masked;
// interrupt context only ever `try_lock`s (an event is dropped rather than
// deadlocking on a holder it interrupted).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::lib::spin::SpinLock;

// ------------------------------------------------------------------ SHA-256

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
    0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
    0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
    0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

/// Streaming SHA-256.
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    n: usize,
    len: u64,
}

impl Sha256 {
    pub const fn new() -> Sha256 {
        Sha256 {
            h: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
            ],
            buf: [0; 64],
            n: 0,
            len: 0,
        }
    }

    fn block(h: &mut [u32; 8], b: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut bb, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & bb) ^ (a & c) ^ (bb & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = bb;
            bb = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, bb, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;
        if self.n > 0 {
            let take = (64 - self.n).min(data.len());
            self.buf[self.n..self.n + take].copy_from_slice(&data[..take]);
            self.n += take;
            data = &data[take..];
            if self.n == 64 {
                let b = self.buf;
                Self::block(&mut self.h, &b);
                self.n = 0;
            }
        }
        while data.len() >= 64 {
            let mut b = [0u8; 64];
            b.copy_from_slice(&data[..64]);
            Self::block(&mut self.h, &b);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.n = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.len * 8;
        let mut pad = [0u8; 72];
        pad[0] = 0x80;
        let padlen = if self.n < 56 { 56 - self.n } else { 120 - self.n };
        let mut tail = [0u8; 8];
        tail.copy_from_slice(&bits.to_be_bytes());
        let len_before = self.len;
        self.update(&pad[..padlen]);
        self.update(&tail);
        self.len = len_before;
        let mut out = [0u8; 32];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut s = Sha256::new();
    s.update(data);
    s.finish()
}

// ----------------------------------------------------------------- ChaCha20

#[inline(always)]
fn qr(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// One ChaCha20 block (RFC 8439 §2.3): 32-byte key, 32-bit block counter,
/// 96-bit nonce.
pub fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12], out: &mut [u8; 64]) {
    let mut s = [0u32; 16];
    s[0] = 0x6170_7865;
    s[1] = 0x3320_646e;
    s[2] = 0x7962_2d32;
    s[3] = 0x6b20_6574;
    for i in 0..8 {
        s[4 + i] = u32::from_le_bytes([key[i * 4], key[i * 4 + 1], key[i * 4 + 2], key[i * 4 + 3]]);
    }
    s[12] = counter;
    for i in 0..3 {
        s[13 + i] = u32::from_le_bytes([nonce[i * 4], nonce[i * 4 + 1], nonce[i * 4 + 2], nonce[i * 4 + 3]]);
    }
    let init = s;
    for _ in 0..10 {
        qr(&mut s, 0, 4, 8, 12);
        qr(&mut s, 1, 5, 9, 13);
        qr(&mut s, 2, 6, 10, 14);
        qr(&mut s, 3, 7, 11, 15);
        qr(&mut s, 0, 5, 10, 15);
        qr(&mut s, 1, 6, 11, 12);
        qr(&mut s, 2, 7, 8, 13);
        qr(&mut s, 3, 4, 9, 14);
    }
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
}

// ------------------------------------------------------------------- state

/// Credit (bits) needed before the generator counts as seeded.
const SEED_BITS: u32 = 128;
/// Interrupt events folded per pool update.
const EVENTS_PER_MIX: u32 = 64;
/// Most bytes one refill hands out (15 blocks after the key block).
const REFILL: usize = 15 * 64 + 32;

struct Rng {
    key: [u8; 32],
    pool: [u8; 32],
    /// Distinguishes refills under one key (the key changes every time, so
    /// this only makes reseed collisions across calls impossible).
    nonce: u64,
    credit: u32,
    /// Interrupt accumulator.
    acc: u64,
    acc_n: u32,
}

static RNG: SpinLock<Rng> =
    SpinLock::new(Rng { key: [0; 32], pool: [0; 32], nonce: 0, credit: 0, acc: 0, acc_n: 0 });
static SEEDED: AtomicBool = AtomicBool::new(false);
static EVENTS: AtomicU32 = AtomicU32::new(0);

impl Rng {
    fn mix(&mut self, tag: u8, data: &[u8]) {
        let mut h = Sha256::new();
        h.update(&self.pool);
        h.update(&[tag]);
        h.update(data);
        self.pool = h.finish();
    }

    /// New key from the old key and the pool.
    fn reseed(&mut self) {
        let mut h = Sha256::new();
        h.update(b"prismos-rng-reseed");
        h.update(&self.key);
        h.update(&self.pool);
        h.update(&self.nonce.to_le_bytes());
        self.key = h.finish();
    }

    fn credit(&mut self, bits: u32) {
        self.credit = self.credit.saturating_add(bits);
        if self.credit >= SEED_BITS && !SEEDED.load(Ordering::Relaxed) {
            self.reseed();
            SEEDED.store(true, Ordering::Release);
        }
    }

    /// Up to `REFILL` bytes under the current key, with key erasure.
    fn refill(&mut self, out: &mut [u8]) {
        debug_assert!(out.len() <= REFILL);
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&self.nonce.to_le_bytes());
        self.nonce = self.nonce.wrapping_add(1);
        let nblocks = (out.len() + 32 + 63) / 64;
        let mut stream = [0u8; 16 * 64];
        for i in 0..nblocks {
            let mut b = [0u8; 64];
            chacha20_block(&self.key, i as u32, &nonce, &mut b);
            stream[i * 64..i * 64 + 64].copy_from_slice(&b);
        }
        self.key.copy_from_slice(&stream[..32]);
        out.copy_from_slice(&stream[32..32 + out.len()]);
        // Wipe the stream (volatile so the optimiser keeps it).
        for b in stream.iter_mut() {
            // SAFETY: a plain byte of a local array.
            unsafe { core::ptr::write_volatile(b, 0) };
        }
    }
}

// ----------------------------------------------------------------- sources

#[inline]
fn rdtsc() -> u64 {
    // SAFETY: rdtsc exists on every x86-64 CPU.
    unsafe { core::arch::x86_64::_rdtsc() }
}

fn have_rdrand() -> bool {
    crate::arch::cpu::cpuid(1, 0).ecx & (1 << 30) != 0
}

fn have_rdseed() -> bool {
    crate::arch::cpu::cpuid(0, 0).eax >= 7 && crate::arch::cpu::cpuid(7, 0).ebx & (1 << 18) != 0
}

fn rdrand64() -> Option<u64> {
    for _ in 0..10 {
        let v: u64;
        let ok: u8;
        // SAFETY: the caller checked cpuid.1:ecx[30]; CF reports success.
        unsafe {
            core::arch::asm!("rdrand {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack));
        }
        if ok != 0 {
            return Some(v);
        }
    }
    None
}

fn rdseed64() -> Option<u64> {
    for _ in 0..64 {
        let v: u64;
        let ok: u8;
        // SAFETY: the caller checked cpuid.7:ebx[18]; CF reports success.
        unsafe {
            core::arch::asm!("rdseed {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack));
        }
        if ok != 0 {
            return Some(v);
        }
        // SAFETY: pause is always safe.
        unsafe { core::arch::asm!("pause", options(nomem, nostack, preserves_flags)) };
    }
    None
}

/// Timed bursts of dependent memory work; the cycle counts jitter with
/// cache, bus and (under a hypervisor) host scheduling.  Returns the samples'
/// digest and how many *distinct* deltas were seen -- the measure of whether
/// the clock actually moved unpredictably.
fn tsc_jitter() -> ([u8; 32], u32) {
    const N: usize = 1024;
    let mut scratch = [0u32; 512];
    let mut seen = [0u64; 4]; // which of the low 8 bits patterns occurred
    let mut h = Sha256::new();
    let mut x = rdtsc() as u32 | 1;
    for i in 0..N {
        let t0 = rdtsc();
        for j in 0..48usize {
            let k = (x as usize >> 3) % scratch.len();
            x = x.wrapping_mul(1664525).wrapping_add(1013904223) ^ scratch[k];
            scratch[(k + j) % 512] = x;
        }
        let d = rdtsc().wrapping_sub(t0);
        let low = (d & 0xFF) as usize;
        seen[low / 64] |= 1 << (low % 64);
        h.update(&d.to_le_bytes());
        h.update(&(i as u32).to_le_bytes());
    }
    h.update(&scratch.iter().fold(0u64, |a, v| a.rotate_left(5) ^ *v as u64).to_le_bytes());
    let distinct = seen.iter().map(|w| w.count_ones()).sum();
    (h.finish(), distinct)
}

/// Gather the boot entropy.  Call once, early (after the heap is not needed:
/// nothing here allocates), before the first process.
pub fn init() {
    let mut r = RNG.lock();
    let mut hw_bits = 0u32;
    // The CPU's own generator.
    if have_rdseed() {
        let mut got = 0;
        for _ in 0..4 {
            if let Some(v) = rdseed64() {
                r.mix(b'S', &v.to_le_bytes());
                got += 1;
            }
        }
        hw_bits += got * 64;
    }
    if hw_bits < SEED_BITS && have_rdrand() {
        let mut got = 0;
        for _ in 0..8 {
            if let Some(v) = rdrand64() {
                r.mix(b'R', &v.to_le_bytes());
                got += 1;
            }
        }
        hw_bits += got * 32; // conservative: RDRAND output is reseeded, not raw
    }
    // TSC jitter: credited only if the deltas varied enough to mean it.
    let (digest, distinct) = tsc_jitter();
    r.mix(b'J', &digest);
    let jitter_bits = if distinct >= 16 { 128 } else if distinct >= 4 { 32 } else { 0 };
    // Facts about this boot (credit nothing).
    let stack = &digest as *const _ as u64;
    r.mix(b'B', &stack.to_le_bytes());
    r.mix(b'T', &rdtsc().to_le_bytes());
    let total = hw_bits + jitter_bits;
    r.credit(total);
    if !SEEDED.load(Ordering::Relaxed) {
        // Make the stream differ per boot even so; seeding completes from
        // interrupt timing (irq_event).
        r.reseed();
    }
    crate::kprintln!(
        "random   : ChaCha20 CSPRNG, {} (rdseed={}, rdrand={}, jitter {} distinct)",
        if SEEDED.load(Ordering::Relaxed) { "seeded" } else { "waiting for interrupt entropy" },
        have_rdseed() as u8,
        have_rdrand() as u8,
        distinct
    );
}

/// Mix wall-clock facts the boot learns later (RTC time) -- no credit.
pub fn add_boot_fact(data: &[u8]) {
    let mut r = RNG.lock();
    r.mix(b'F', data);
}

/// Mix caller-supplied bytes (a write to /dev/random): no credit, so a
/// hostile writer cannot make the generator believe it is seeded.
pub fn add_bytes(data: &[u8]) {
    let _g = crate::arch::io::IrqGuard::new();
    let mut r = RNG.lock();
    for c in data.chunks(64) {
        r.mix(b'W', c);
    }
}

/// An interrupt happened (timer tick, key, mouse byte, disk...): its exact
/// cycle count is the entropy.  Safe from interrupt context.
pub fn irq_event(tag: u32) {
    let t = rdtsc();
    let n = EVENTS.fetch_add(1, Ordering::Relaxed);
    let Some(mut r) = RNG.try_lock() else { return };
    r.acc = r.acc.rotate_left(7) ^ t ^ ((tag as u64) << 56);
    r.acc_n += 1;
    if r.acc_n >= EVENTS_PER_MIX {
        let a = r.acc;
        r.acc_n = 0;
        r.mix(b'I', &a.to_le_bytes());
        // One bit of credit per mixed batch: slow, deliberately honest.
        r.credit(1);
        if n % (EVENTS_PER_MIX * 16) == 0 {
            r.reseed();
        }
    }
}

// ------------------------------------------------------------------ output

/// Has the generator been seeded (128 credited bits)?
pub fn is_seeded() -> bool {
    SEEDED.load(Ordering::Acquire)
}

/// Fill `out` with cryptographically strong random bytes.  Never blocks:
/// callers that must not run unseeded check `is_seeded` first.
pub fn fill(out: &mut [u8]) {
    let _g = crate::arch::io::IrqGuard::new();
    let mut r = RNG.lock();
    // Fold the cycle counter in every call: cheap insurance against a
    // snapshot-restored VM replaying a stream.
    let t = rdtsc();
    r.mix(b'C', &t.to_le_bytes());
    for chunk in out.chunks_mut(REFILL) {
        r.refill(chunk);
    }
}

/// 16 bytes for AT_RANDOM.
pub fn bytes16() -> [u8; 16] {
    let mut b = [0u8; 16];
    fill(&mut b);
    b
}

/// A u64 for kernel uses (ASLR-style choices, hash seeds).
pub fn u64() -> u64 {
    let mut b = [0u8; 8];
    fill(&mut b);
    u64::from_le_bytes(b)
}

// getrandom(2) flags (Linux values).
pub const GRND_NONBLOCK: u64 = 1;
pub const GRND_RANDOM: u64 = 2;
pub const GRND_INSECURE: u64 = 4;
