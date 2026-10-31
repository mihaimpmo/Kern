//! Synchronisation primitives.
//!
//! The kernel currently runs on a single CPU, but `SpinLock` is written to be
//! SMP-safe: it uses an atomic flag *and* disables interrupts while held, so an
//! interrupt handler can never deadlock against the code it interrupted.

use crate::arch::cpu;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), data: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let irq = IrqGuard::new();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinLockGuard { lock: self, _irq: irq }
    }

    #[allow(dead_code)]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let irq = IrqGuard::new();
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinLockGuard { lock: self, _irq: irq })
    }

    /// Used only by the panic handler to get at the console no matter what.
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    _irq: IrqGuard,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        // `_irq` is dropped after this and restores the interrupt flag.
    }
}

/// Disables interrupts for its lifetime, then restores the previous state.
pub struct IrqGuard {
    was_enabled: bool,
}

impl IrqGuard {
    pub fn new() -> Self {
        let was_enabled = cpu::interrupts_enabled();
        cpu::cli();
        Self { was_enabled }
    }
}

impl Drop for IrqGuard {
    fn drop(&mut self) {
        if self.was_enabled {
            cpu::sti();
        }
    }
}

/// A raw global cell. Callers are responsible for synchronisation
/// (usually: interrupts disabled on a single CPU).
pub struct Racy<T>(UnsafeCell<T>);

unsafe impl<T> Sync for Racy<T> {}

impl<T> Racy<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }
    pub fn get(&self) -> *mut T {
        self.0.get()
    }
}
