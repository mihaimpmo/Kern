//! Global Descriptor Table and Task State Segment.
//!
//! The selector layout is dictated by `syscall`/`sysret`:
//!   0x08 kernel code, 0x10 kernel data, 0x18 user data, 0x20 user code, 0x28 TSS.

use crate::sync::Racy;
use core::arch::asm;
use core::mem::size_of;

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
#[allow(dead_code)]
pub const USER_DS: u16 = 0x18 | 3;
#[allow(dead_code)]
pub const USER_CS: u16 = 0x20 | 3;
const TSS_SEL: u16 = 0x28;

/// IST slots (1-based, as stored in the IDT).
pub const IST_DOUBLE_FAULT: u8 = 1;
pub const IST_NMI: u8 = 2;
pub const IST_MACHINE_CHECK: u8 = 3;

#[repr(C, packed)]
struct Tss {
    _reserved0: u32,
    rsp: [u64; 3],
    _reserved1: u64,
    ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    iomap_base: u16,
}

#[repr(C, packed)]
struct DescriptorPointer {
    limit: u16,
    base: u64,
}

const IST_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct IstStack([u8; IST_STACK_SIZE]);

static IST_STACKS: Racy<[IstStack; 3]> = Racy::new([
    IstStack([0; IST_STACK_SIZE]),
    IstStack([0; IST_STACK_SIZE]),
    IstStack([0; IST_STACK_SIZE]),
]);

static TSS: Racy<Tss> = Racy::new(Tss {
    _reserved0: 0,
    rsp: [0; 3],
    _reserved1: 0,
    ist: [0; 7],
    _reserved2: 0,
    _reserved3: 0,
    iomap_base: size_of::<Tss>() as u16, // no I/O permission bitmap
});

static GDT: Racy<[u64; 7]> = Racy::new([
    0,
    0x00af_9a00_0000_ffff, // kernel code (64-bit, DPL 0)
    0x00cf_9200_0000_ffff, // kernel data
    0x00cf_f200_0000_ffff, // user data (DPL 3)
    0x00af_fa00_0000_ffff, // user code (64-bit, DPL 3)
    0,                     // TSS low  (filled in at runtime)
    0,                     // TSS high
]);

pub fn init() {
    unsafe {
        let tss = TSS.get();
        let stacks = &*IST_STACKS.get();
        for (i, s) in stacks.iter().enumerate() {
            let top = s.0.as_ptr() as u64 + IST_STACK_SIZE as u64;
            core::ptr::addr_of_mut!((*tss).ist[i]).write_unaligned(top);
        }

        let base = tss as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        let gdt = &mut *GDT.get();
        gdt[5] = (limit & 0xffff)
            | ((base & 0xff_ffff) << 16)
            | (0x89 << 40) // present, 64-bit available TSS
            | (((limit >> 16) & 0xf) << 48)
            | (((base >> 24) & 0xff) << 56);
        gdt[6] = base >> 32;

        let ptr = DescriptorPointer { limit: (size_of::<[u64; 7]>() - 1) as u16, base: gdt.as_ptr() as u64 };
        asm!(
            "lgdt [{ptr}]",
            // Reload CS with a far return.
            "push {kcs}",
            "lea {tmp}, [rip + 55f]",
            "push {tmp}",
            "retfq",
            "55:",
            "mov {tmp:x}, {kds}",
            "mov ds, {tmp:x}",
            "mov es, {tmp:x}",
            "mov ss, {tmp:x}",
            "xor {tmp:e}, {tmp:e}",
            "mov fs, {tmp:x}",
            "mov gs, {tmp:x}",
            "mov {tmp:x}, {tss}",
            "ltr {tmp:x}",
            ptr = in(reg) &ptr,
            kcs = const KERNEL_CS as u64,
            kds = const KERNEL_DS as u64,
            tss = const TSS_SEL as u64,
            tmp = out(reg) _,
        );
    }
}

/// Stack the CPU switches to when an interrupt arrives from user mode.
pub fn set_kernel_stack(top: u64) {
    unsafe { core::ptr::addr_of_mut!((*TSS.get()).rsp[0]).write_unaligned(top) };
}
