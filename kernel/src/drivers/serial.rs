//! 16550 UART on COM1. Most laptops have no serial port; in that case the
//! loopback test fails and output silently goes to the screen only.
//!
//! Output is buffered: `write_byte` only appends to a ring, and the timer
//! interrupt moves bytes into the UART's transmit FIFO whenever it is empty.
//! At 115200 baud the port takes ~87 µs per byte, so writing synchronously
//! (with the console lock held, i.e. interrupts off) would stall the timer
//! for milliseconds on real hardware. Until the timer runs (`start_buffering`)
//! and again after a panic (`panic_mode`), writes go straight to the port so
//! nothing is lost if the kernel stops early.

use crate::arch::cpu::{inb, outb};
use crate::sync::SpinLock;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const COM1: u16 = 0x3F8;
const LSR_THR_EMPTY: u8 = 0x20;
const RING_SIZE: usize = 16 * 1024;

static PRESENT: AtomicBool = AtomicBool::new(false);
static BUFFERED: AtomicBool = AtomicBool::new(false);
/// Bytes the UART accepts at once when its transmitter is empty: 16 with a
/// working FIFO (16550A), otherwise 1.
static FIFO_DEPTH: AtomicUsize = AtomicUsize::new(1);

struct Ring {
    buf: [u8; RING_SIZE],
    head: usize, // next byte to send
    len: usize,
}

impl Ring {
    fn push(&mut self, b: u8) {
        self.buf[(self.head + self.len) % RING_SIZE] = b;
        self.len += 1;
    }
    fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let b = self.buf[self.head];
        self.head = (self.head + 1) % RING_SIZE;
        self.len -= 1;
        Some(b)
    }
}

static RING: SpinLock<Ring> = SpinLock::new(Ring { buf: [0; RING_SIZE], head: 0, len: 0 });

pub fn init() {
    unsafe {
        outb(COM1 + 1, 0x00); // no interrupts
        outb(COM1 + 3, 0x80); // DLAB on
        outb(COM1, 0x01); // 115200 baud
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8N1
        outb(COM1 + 2, 0xC7); // FIFO
        outb(COM1 + 4, 0x1E); // loopback mode for the self-test
        outb(COM1, 0xAE);
        if inb(COM1) != 0xAE {
            return;
        }
        outb(COM1 + 4, 0x0F); // normal operation
        // IIR bits 6-7 read 11 only if the FIFO really got enabled.
        if inb(COM1 + 2) & 0xC0 == 0xC0 {
            FIFO_DEPTH.store(16, Ordering::Relaxed);
        }
    }
    PRESENT.store(true, Ordering::Relaxed);
}

fn thr_empty() -> bool {
    unsafe { inb(COM1 + 5) & LSR_THR_EMPTY != 0 }
}

/// Send one byte, waiting (bounded) for the transmitter.
fn write_sync(b: u8) {
    let mut spins = 0;
    while !thr_empty() && spins < 100_000 {
        spins += 1;
    }
    unsafe { outb(COM1, b) };
}

pub fn write_byte(b: u8) {
    if !PRESENT.load(Ordering::Relaxed) {
        return;
    }
    if !BUFFERED.load(Ordering::Relaxed) {
        write_sync(b);
        return;
    }
    let mut ring = RING.lock();
    if ring.len == RING_SIZE {
        // Full: make room the slow way. Callers that can sleep should check
        // `has_room` first so this stays rare.
        if let Some(old) = ring.pop() {
            write_sync(old);
        }
    }
    ring.push(b);
}

/// True if `n` more bytes fit without falling back to synchronous output.
pub fn has_room(n: usize) -> bool {
    !BUFFERED.load(Ordering::Relaxed) || RING.lock().len + n <= RING_SIZE
}

/// Called from the timer interrupt: if the transmitter is idle, give it one
/// FIFO's worth of bytes, without waiting. 16 bytes per 1 ms tick is faster
/// than the 11.5 bytes/ms the line carries, so the port stays busy. Only one
/// batch per tick: emulated UARTs report "empty" at once, and each port write
/// can be slow there, so looping would stretch the tick handler.
pub fn poll() {
    if !BUFFERED.load(Ordering::Relaxed) {
        return;
    }
    let mut ring = RING.lock();
    if ring.len == 0 || !thr_empty() {
        return;
    }
    for _ in 0..FIFO_DEPTH.load(Ordering::Relaxed) {
        match ring.pop() {
            Some(b) => unsafe { outb(COM1, b) },
            None => break,
        }
    }
}

/// Switch to buffered output. Call once the timer interrupt is running.
pub fn start_buffering() {
    if PRESENT.load(Ordering::Relaxed) {
        BUFFERED.store(true, Ordering::Relaxed);
    }
}

/// Panic path: flush whatever is queued and write synchronously from now on,
/// since the timer will never drain the ring again.
pub fn panic_mode() {
    if !BUFFERED.swap(false, Ordering::Relaxed) {
        return;
    }
    unsafe { RING.force_unlock() };
    let mut ring = RING.lock();
    while let Some(b) = ring.pop() {
        write_sync(b);
    }
}
