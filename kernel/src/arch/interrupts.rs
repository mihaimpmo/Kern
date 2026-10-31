//! Common interrupt entry path and dispatch.

use super::{apic, cpu};
use crate::{drivers::keyboard, task};
use core::arch::global_asm;

pub const VECTOR_TIMER: u64 = 0x20;
pub const VECTOR_KEYBOARD: u64 = 0x21;
pub const VECTOR_LEGACY_PIC_BASE: u64 = 0xE0; // masked 8259 PIC, remapped out of the way
pub const VECTOR_SPURIOUS: u64 = 0xFF;

global_asm!(include_str!(concat!(env!("OUT_DIR"), "/isr_stubs.s")));

// Every stub pushes [error code], vector, then jumps here.
// Stack at entry: vector, error, rip, cs, rflags, rsp, ss.
global_asm!(
    r#"
.section .text
.global isr_common
isr_common:
    test qword ptr [rsp + 24], 3     // came from user mode?
    jz 3f
    swapgs
3:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    cld
    mov rdi, rsp
    call interrupt_dispatch
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    test qword ptr [rsp + 24], 3
    jz 4f
    swapgs
4:
    add rsp, 16                      // drop vector + error code
    iretq
"#
);

#[repr(C)]
#[derive(Debug)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl InterruptFrame {
    pub fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }
}

const EXCEPTIONS: [&str; 32] = [
    "Divide error", "Debug", "NMI", "Breakpoint", "Overflow", "Bound range exceeded",
    "Invalid opcode", "Device not available", "Double fault", "Coprocessor segment overrun",
    "Invalid TSS", "Segment not present", "Stack-segment fault", "General protection fault",
    "Page fault", "Reserved", "x87 floating-point error", "Alignment check", "Machine check",
    "SIMD floating-point error", "Virtualization exception", "Control protection exception",
    "Reserved", "Reserved", "Reserved", "Reserved", "Reserved", "Reserved",
    "Hypervisor injection exception", "VMM communication exception", "Security exception", "Reserved",
];

#[no_mangle]
extern "C" fn interrupt_dispatch(frame: &mut InterruptFrame) {
    match frame.vector {
        0..=31 => exception(frame),
        VECTOR_TIMER => {
            apic::eoi();
            task::timer_tick();
        }
        VECTOR_KEYBOARD => {
            keyboard::handle_interrupt();
            apic::eoi();
        }
        VECTOR_SPURIOUS => {} // spurious: no EOI
        v if (VECTOR_LEGACY_PIC_BASE..VECTOR_LEGACY_PIC_BASE + 16).contains(&v) => {} // stray 8259 IRQ
        v => {
            kprintln!("[irq] unexpected vector {:#x}", v);
            apic::eoi();
        }
    }
}

fn exception(frame: &mut InterruptFrame) {
    let name = EXCEPTIONS[frame.vector as usize];
    let cr2 = cpu::read_cr2();

    if frame.from_user() {
        // A user program faulted: kill it, the rest of the system keeps running.
        let pid = task::current_pid();
        if frame.vector == 14 {
            kprintln!(
                "\x1b[91m[kernel] process {} killed: {} at {:#x} (address {:#x})\x1b[0m",
                pid, name, frame.rip, cr2
            );
        } else {
            kprintln!("\x1b[91m[kernel] process {} killed: {} at {:#x}\x1b[0m", pid, name, frame.rip);
        }
        task::exit_current(-(128 + frame.vector as i64));
    }

    if frame.vector == 14 {
        panic!(
            "{} in kernel at {:#x}\n  address {:#x}, error {:#x} ({})\n{:#x?}",
            name, frame.rip, cr2, frame.error, page_fault_reason(frame.error), frame
        );
    }
    panic!("{} in kernel at {:#x} (error {:#x})\n{:#x?}", name, frame.rip, frame.error, frame);
}

fn page_fault_reason(err: u64) -> &'static str {
    match (err & 1 != 0, err & 2 != 0, err & 16 != 0) {
        (_, _, true) => "instruction fetch",
        (false, false, _) => "read of unmapped page",
        (false, true, _) => "write to unmapped page",
        (true, true, _) => "write to read-only page",
        (true, false, _) => "protection violation on read",
    }
}
