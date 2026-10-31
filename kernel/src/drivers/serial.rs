//! 16550 UART on COM1. Most laptops have no serial port; in that case the
//! loopback test fails and output silently goes to the screen only.

use crate::arch::cpu::{inb, outb};
use core::sync::atomic::{AtomicBool, Ordering};

const COM1: u16 = 0x3F8;
static PRESENT: AtomicBool = AtomicBool::new(false);

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
    }
    PRESENT.store(true, Ordering::Relaxed);
}

pub fn write_byte(b: u8) {
    if !PRESENT.load(Ordering::Relaxed) {
        return;
    }
    unsafe {
        let mut spins = 0;
        while inb(COM1 + 5) & 0x20 == 0 && spins < 100_000 {
            spins += 1;
        }
        outb(COM1, b);
    }
}
