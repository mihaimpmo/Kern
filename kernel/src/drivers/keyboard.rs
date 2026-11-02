//! PS/2 keyboard through the i8042 controller (US layout, scancode set 1).
//! Laptop built-in keyboards are almost always wired this way, even today.
//! USB keyboards need an xHCI + HID driver, which this kernel doesn't have yet;
//! many firmwares emulate them as PS/2 when booted in legacy BIOS mode.

use crate::acpi::Madt;
use crate::arch::{apic, cpu::{inb, outb}, interrupts::VECTOR_KEYBOARD};
use crate::sync::SpinLock;
use crate::task::{self, signal, Interrupted, KillTarget, Pid, WaitQueue};
use core::sync::atomic::{AtomicU64, Ordering};

const CTRL_C: u8 = 3;

/// Process group that Ctrl+C interrupts (0: none, the key is delivered as
/// byte 3 instead). Set by the shell around each foreground job.
static FOREGROUND: AtomicU64 = AtomicU64::new(0);

pub fn set_foreground(group: Pid) {
    FOREGROUND.store(group, Ordering::Relaxed);
}

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const COMMAND: u16 = 0x64;

struct Ring {
    buf: [u8; 256],
    head: usize,
    tail: usize,
}

impl Ring {
    fn push(&mut self, b: u8) {
        let next = (self.head + 1) % self.buf.len();
        if next != self.tail {
            self.buf[self.head] = b;
            self.head = next;
        }
    }
    fn pop(&mut self) -> Option<u8> {
        if self.head == self.tail {
            return None;
        }
        let b = self.buf[self.tail];
        self.tail = (self.tail + 1) % self.buf.len();
        Some(b)
    }
}

struct Modifiers {
    shift: bool,
    ctrl: bool,
    caps: bool,
    extended: bool,
}

static BUFFER: SpinLock<Ring> = SpinLock::new(Ring { buf: [0; 256], head: 0, tail: 0 });
/// Only the interrupt handler uses this; the lock just makes that explicit.
static MODS: SpinLock<Modifiers> = SpinLock::new(Modifiers { shift: false, ctrl: false, caps: false, extended: false });
static READERS: WaitQueue = WaitQueue::new();

fn wait_input_clear() -> bool {
    (0..100_000).any(|_| unsafe { inb(STATUS) } & 2 == 0)
}

fn wait_output_full() -> bool {
    (0..100_000).any(|_| unsafe { inb(STATUS) } & 1 != 0)
}

fn command(cmd: u8) {
    wait_input_clear();
    unsafe { outb(COMMAND, cmd) };
}

fn write_data(b: u8) {
    wait_input_clear();
    unsafe { outb(DATA, b) };
}

fn read_data() -> Option<u8> {
    wait_output_full().then(|| unsafe { inb(DATA) })
}

pub fn init(madt: &Madt) -> bool {
    // A missing controller reads back as 0xFF.
    if unsafe { inb(STATUS) } == 0xFF {
        return false;
    }
    command(0xAD); // disable first port
    command(0xA7); // disable second port
    for _ in 0..64 {
        if unsafe { inb(STATUS) } & 1 == 0 {
            break;
        }
        unsafe { inb(DATA) };
    }

    command(0x20); // read configuration byte
    let Some(mut config) = read_data() else { return false };
    config &= !0b11; // IRQs off while we set up
    config &= !(1 << 4); // keyboard clock on (0xAD above turned it off)
    config |= 1 << 6; // translate to scancode set 1
    command(0x60);
    write_data(config);

    command(0xAE); // enable first port
    write_data(0xF4); // keyboard: enable scanning
    let _ = read_data(); // ACK (0xFA), if any

    config |= 1; // IRQ1 on
    command(0x60);
    write_data(config);

    apic::route_isa_irq(madt, 1, VECTOR_KEYBOARD as u8)
}

const NORMAL: &[u8; 58] = b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
const SHIFTED: &[u8; 58] = b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";

pub fn handle_interrupt() {
    let code = unsafe { inb(DATA) };
    let mut mods = MODS.lock();

    if code == 0xE0 {
        mods.extended = true;
        return;
    }
    let extended = core::mem::replace(&mut mods.extended, false);
    let released = code & 0x80 != 0;
    let key = code & 0x7F;

    match key {
        0x2A | 0x36 => mods.shift = !released,
        0x1D => mods.ctrl = !released,
        0x3A if !released => mods.caps = !mods.caps,
        _ if released => {}
        _ if extended => {
            // Arrow keys -> ANSI escape sequences.
            let seq: &[u8] = match key {
                0x48 => b"\x1b[A",
                0x50 => b"\x1b[B",
                0x4D => b"\x1b[C",
                0x4B => b"\x1b[D",
                _ => b"",
            };
            drop(mods);
            push_bytes(seq);
        }
        _ if (key as usize) < NORMAL.len() => {
            let mut c = if mods.shift { SHIFTED[key as usize] } else { NORMAL[key as usize] };
            if mods.caps && c.is_ascii_alphabetic() {
                c ^= 0x20;
            }
            if mods.ctrl && c.is_ascii_alphabetic() {
                c = c.to_ascii_lowercase() - b'a' + 1; // Ctrl+C = 3, ...
            }
            drop(mods);
            if c == CTRL_C {
                let group = FOREGROUND.load(Ordering::Relaxed);
                if group != 0 {
                    // A job is running in the foreground: interrupt it rather
                    // than give it the key.
                    let _ = task::kill(KillTarget::Group(group), signal::SIGINT);
                    return;
                }
            }
            if c != 0 {
                push_bytes(&[c]);
            }
        }
        _ => {}
    }
}

fn push_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    {
        let mut buf = BUFFER.lock();
        for &b in bytes {
            buf.push(b);
        }
    }
    READERS.wake_all();
}

/// Read up to `out.len()` bytes, blocking until at least one is available
/// or a signal arrives.
pub fn read(out: &mut [u8]) -> Result<usize, Interrupted> {
    if out.is_empty() {
        return Ok(0);
    }
    let mut buf = BUFFER.lock();
    loop {
        let mut n = 0;
        while n < out.len() {
            match buf.pop() {
                Some(b) => {
                    out[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        if n > 0 {
            return Ok(n);
        }
        buf = READERS.wait_interruptible(buf)?;
    }
}
