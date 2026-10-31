//! Thin wrappers around x86-64 instructions.

use core::arch::asm;

#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
}

#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    value
}

/// Small delay used by legacy hardware (writes to an unused port).
#[inline]
pub unsafe fn io_wait() {
    outb(0x80, 0);
}

#[inline]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    ((hi as u64) << 32) | lo as u64
}

#[inline]
pub unsafe fn wrmsr(msr: u32, value: u64) {
    asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32,
         options(nomem, nostack, preserves_flags));
}

pub mod msr {
    pub const APIC_BASE: u32 = 0x1B;
    pub const EFER: u32 = 0xC000_0080;
    pub const STAR: u32 = 0xC000_0081;
    pub const LSTAR: u32 = 0xC000_0082;
    pub const FMASK: u32 = 0xC000_0084;
    pub const GS_BASE: u32 = 0xC000_0101;
    pub const KERNEL_GS_BASE: u32 = 0xC000_0102;
}

#[inline]
pub fn read_cr2() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub fn read_cr3() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub unsafe fn write_cr3(v: u64) {
    asm!("mov cr3, {}", in(reg) v, options(nostack, preserves_flags));
}

#[inline]
pub unsafe fn invlpg(addr: u64) {
    asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags));
}

#[inline]
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    unsafe { asm!("pushfq", "pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    flags & 0x200 != 0
}

#[inline]
pub fn cli() {
    unsafe { asm!("cli", options(nomem, nostack)) };
}

#[inline]
pub fn sti() {
    unsafe { asm!("sti", options(nomem, nostack)) };
}

/// Enable interrupts and wait for the next one, atomically.
#[inline]
pub fn sti_hlt() {
    unsafe { asm!("sti", "hlt", options(nomem, nostack)) };
}

pub fn halt_forever() -> ! {
    loop {
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

pub struct Cpuid {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

pub fn cpuid(leaf: u32, sub: u32) -> Cpuid {
    #[allow(unused_unsafe)]
    let r = unsafe { core::arch::x86_64::__cpuid_count(leaf, sub) };
    Cpuid { eax: r.eax, ebx: r.ebx, ecx: r.ecx, edx: r.edx }
}

/// CPU brand string, e.g. "AMD Ryzen 7 7840HS w/ Radeon 780M Graphics".
pub fn brand_string(buf: &mut [u8; 48]) -> &str {
    if cpuid(0x8000_0000, 0).eax < 0x8000_0004 {
        return "unknown CPU";
    }
    for (i, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
        let r = cpuid(leaf, 0);
        for (j, reg) in [r.eax, r.ebx, r.ecx, r.edx].iter().enumerate() {
            buf[i * 16 + j * 4..i * 16 + j * 4 + 4].copy_from_slice(&reg.to_le_bytes());
        }
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(48);
    core::str::from_utf8(&buf[..len]).unwrap_or("unknown CPU").trim()
}
