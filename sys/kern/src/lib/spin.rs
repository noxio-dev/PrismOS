// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// A minimal spin lock (phase 1.5 SMP): test-and-set on a u32 with the
// `pause` hint, ticket-free (FIFO fairness is not worth the code yet -- the
// only contended locks are the console and the kernel log, held for
// microseconds).
//
// Lock discipline: a spin lock may be taken from any CPU at any privilege
// context, but NEVER from an interrupt handler that can interrupt the lock
// holder on the SAME CPU (the console lock is taken with interrupts masked
// by the caller for exactly that reason; see console::_print).

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// Safety: the lock guards every access to the data; T itself must be Send
// (the lock hands it out across CPUs).
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(v: T) -> Self {
        SpinLock { locked: AtomicBool::new(false), data: UnsafeCell::new(v) }
    }

    /// Acquire, spinning with `pause` between attempts.
    pub fn lock(&self) -> Guard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // SAFETY: privileged instruction, safe in ring 0.
            unsafe { core::arch::asm!("pause", options(nomem, nostack, preserves_flags)) };
        }
        Guard { lock: self }
    }

    /// Try to acquire without waiting.
    pub fn try_lock(&self) -> Option<Guard<'_, T>> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(Guard { lock: self })
        } else {
            None
        }
    }
}

impl<T: ?Sized> SpinLock<T> {
    /// Whether the lock is currently held (for diagnostics only).
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    fn release(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

pub struct Guard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
}

impl<T: ?Sized> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.release();
    }
}

impl<T: ?Sized> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: we hold the lock.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the lock.
        unsafe { &mut *self.lock.data.get() }
    }
}
