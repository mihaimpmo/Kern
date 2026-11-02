//! Global Descriptor Table and Task State Segment.
//!
//! The selector layout is dictated by `syscall`/`sysret`:
//!   0x08 kernel code, 0x10 kernel data, 0x18 user data, 0x20 user code, 0x28 TSS.
//!
//! Each CPU needs its own TSS (its own kernel stack and IST stacks), and a
//! TSS descriptor is marked busy once a CPU loads it, so each CPU also gets
//! its own GDT: see `CpuTables`. Until those exist (they need the heap), the
//! boot CPU runs on `BOOT_GDT`, which has the same segments but no TSS.

use core::arch::asm;
use core::mem::size_of;

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;
const TSS_SEL: u16 = 0x28;

/// IST slots (1-based, as stored in the IDT).
pub const IST_DOUBLE_FAULT: u8 = 1;
pub const IST_NMI: u8 = 2;
pub const IST_MACHINE_CHECK: u8 = 3;
pub const IST_COUNT: usize = 3;

// Segment descriptors with the "accessed" bit already set, so the CPU never
// writes to them (BOOT_GDT lives in read-only memory).
const KERNEL_CODE: u64 = 0x00af_9b00_0000_ffff; // 64-bit, DPL 0
const KERNEL_DATA: u64 = 0x00cf_9300_0000_ffff;
const USER_DATA: u64 = 0x00cf_f300_0000_ffff; // DPL 3
const USER_CODE: u64 = 0x00af_fb00_0000_ffff; // 64-bit, DPL 3

static BOOT_GDT: [u64; 5] = [0, KERNEL_CODE, KERNEL_DATA, USER_DATA, USER_CODE];

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

/// One CPU's GDT and TSS.
#[repr(C, align(16))]
pub struct CpuTables {
    gdt: [u64; 7],
    tss: Tss,
}

impl CpuTables {
    /// `ist_tops` are the tops of the stacks for IST slots 1..=IST_COUNT.
    pub fn new(ist_tops: [u64; IST_COUNT]) -> Self {
        let mut ist = [0; 7];
        ist[..IST_COUNT].copy_from_slice(&ist_tops);
        CpuTables {
            gdt: [0, KERNEL_CODE, KERNEL_DATA, USER_DATA, USER_CODE, 0, 0],
            tss: Tss {
                _reserved0: 0,
                rsp: [0; 3],
                _reserved1: 0,
                ist,
                _reserved2: 0,
                _reserved3: 0,
                iomap_base: size_of::<Tss>() as u16, // no I/O permission bitmap
            },
        }
    }

    /// Load these tables on the calling CPU. They must stay where they are
    /// (and alive) from now on, hence `'static`.
    pub fn load(&'static mut self) {
        let base = core::ptr::addr_of!(self.tss) as u64;
        let limit = (size_of::<Tss>() - 1) as u64;
        self.gdt[5] = (limit & 0xffff)
            | ((base & 0xff_ffff) << 16)
            | (0x89 << 40) // present, 64-bit available TSS
            | (((limit >> 16) & 0xf) << 48)
            | (((base >> 24) & 0xff) << 56);
        self.gdt[6] = base >> 32;
        unsafe {
            load_gdt(self.gdt.as_ptr(), self.gdt.len());
            asm!("ltr {:x}", in(reg) TSS_SEL, options(nostack, preserves_flags));
        }
    }

    /// Stack the CPU switches to when an interrupt arrives from user mode.
    pub fn set_kernel_stack(&mut self, top: u64) {
        unsafe { core::ptr::addr_of_mut!(self.tss.rsp[0]).write_unaligned(top) };
    }
}

/// First thing at boot: our segments, so the IDT's selectors mean something.
pub fn load_boot_gdt() {
    unsafe { load_gdt(BOOT_GDT.as_ptr(), BOOT_GDT.len()) };
}

/// lgdt, then reload every segment register with our selectors.
unsafe fn load_gdt(gdt: *const u64, entries: usize) {
    let ptr = DescriptorPointer { limit: (entries * 8 - 1) as u16, base: gdt as u64 };
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
        ptr = in(reg) &ptr,
        kcs = const KERNEL_CS as u64,
        kds = const KERNEL_DS as u64,
        tmp = out(reg) _,
    );
}
