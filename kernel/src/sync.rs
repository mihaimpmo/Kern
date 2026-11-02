//! Synchronisation primitives: `SpinLock` (interrupts off, never sleeps),
//! `Mutex` (sleeps, interrupts on), `BootCell` (written only during boot).
//!
//! `SpinLock` uses an atomic flag *and* disables interrupts on the local CPU
//! while held, so it protects data from other CPUs and from interrupt
//! handlers on this one: a handler can never spin on a lock held by the code
//! it interrupted.

use crate::arch::cpu;
use core::cell::UnsafeCell;
use core::mem::ManuallyDrop;
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
        self.lock_with(IrqGuard::new())
    }

    /// Lock, reusing an `IrqGuard` the caller already holds (see
    /// `SpinLockGuard::release`), so the original interrupt state is restored
    /// when the new guard is dropped.
    pub fn lock_with(&self, irq: IrqGuard) -> SpinLockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self, _irq: irq }
    }

    /// Unlock without a guard.
    ///
    /// # Safety
    /// The lock must be held, and its guard must never be dropped (it was
    /// leaked, or lives on a stack that will never run again). Used by the
    /// panic handler and by the scheduler, whose lock is released by the task
    /// that runs next rather than by the one that took it.
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    _irq: IrqGuard,
}

impl<'a, T> SpinLockGuard<'a, T> {
    /// Unlock but keep interrupts disabled: returns the lock (to take it
    /// again later) and the guard that restores the original interrupt state.
    /// This is what lets a wait queue release a lock and go to sleep without
    /// an interrupt slipping in between.
    pub fn release(self) -> (&'a SpinLock<T>, IrqGuard) {
        let this = ManuallyDrop::new(self);
        this.lock.locked.store(false, Ordering::Release);
        // SAFETY: `this` is never dropped, so `_irq` is moved out exactly once.
        let irq = unsafe { core::ptr::read(&this._irq) };
        (this.lock, irq)
    }
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

/// A global written only during early boot, while a single CPU runs with
/// interrupts off, and read-only afterwards (e.g. the IDT).
pub struct BootCell<T>(UnsafeCell<T>);

unsafe impl<T: Sync> Sync for BootCell<T> {}

impl<T> BootCell<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    /// # Safety
    /// Only during single-threaded early boot, with no other reference alive.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self) -> &mut T {
        &mut *self.0.get()
    }

    pub fn as_ptr(&self) -> *const T {
        self.0.get()
    }
}

/// A lock that sleeps instead of spinning, for data held across long
/// operations (copying file contents) or across calls that may block.
/// Interrupts stay enabled while it is held, so it must never be taken in an
/// interrupt handler.
pub struct Mutex<T> {
    locked: SpinLock<bool>,
    waiters: crate::task::WaitQueue,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: SpinLock::new(false), waiters: crate::task::WaitQueue::new(), data: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        let mut locked = self.locked.lock();
        while *locked {
            locked = self.waiters.wait(locked);
        }
        *locked = true;
        MutexGuard { mutex: self }
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        *self.mutex.locked.lock() = false;
        self.mutex.waiters.wake_all();
    }
}
