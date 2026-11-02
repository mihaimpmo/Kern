//! Kern: a small monolithic x86-64 kernel.
//!
//! Boot order: serial -> framebuffer console -> GDT/IDT -> SMEP/SMAP/UMIP ->
//! physical memory -> paging -> heap -> ACPI/APIC -> timer -> syscalls ->
//! scheduler -> keyboard -> first user program (`init`) -> buffered serial
//! output -> idle loop.

#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
mod console;
mod acpi;
mod arch;
mod boot;
mod cmdline;
mod drivers;
mod mm;
mod modules;
mod power;
mod sync;
mod syscall;
mod task;

use arch::{apic, cpu, gdt, idt, percpu};
use core::panic::PanicInfo;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[no_mangle]
extern "C" fn kmain() -> ! {
    drivers::serial::init();
    if !boot::BASE_REVISION.is_supported() {
        cpu::halt_forever(); // bootloader too old for this kernel
    }

    let hhdm = boot::HHDM.response().expect("no HHDM from bootloader");
    mm::set_hhdm_offset(hhdm.offset);
    console::init(boot::FRAMEBUFFER.response().and_then(|r| r.first()));

    kprintln!("\x1b[96mKern\x1b[0m kernel v{} (x86-64)", VERSION);
    if let Some(info) = boot::BOOTLOADER_INFO.response() {
        kprintln!("  booted by {} {}", info.name(), info.version());
    }
    let mut brand = [0u8; 48];
    kprintln!("  cpu: {}", cpu::brand_string(&mut brand));
    if let Some((w, h, _)) = console::size() {
        kprintln!("  display: {}x{}", w, h);
    }

    gdt::init();
    idt::init();
    ok("GDT, TSS and IDT loaded");

    let prot = cpu::enable_protections();
    let flag = |on: bool, name: &'static str| if on { name } else { "" };
    if prot.smep || prot.smap || prot.umip {
        ok(format_args!(
            "user-memory protection:{}{}{}",
            flag(prot.smep, " SMEP"),
            flag(prot.smap, " SMAP"),
            flag(prot.umip, " UMIP")
        ));
    } else {
        kprintln!("\x1b[93m[warn]\x1b[0m CPU has no SMEP/SMAP/UMIP; relying on syscall pointer checks only");
    }

    let memmap = boot::MEMMAP.response().expect("no memory map from bootloader");
    mm::pmm::init(memmap);
    mm::vmm::init();
    mm::heap::init();
    console::enable_shadow_buffer();
    let (free, total) = mm::pmm::stats();
    ok(format_args!("memory: {} MiB usable, {} MiB free", total >> 20, free >> 20));

    let rsdp = boot::RSDP.response().expect("no ACPI RSDP (is this a UEFI/ACPI machine?)");
    let madt = acpi::parse_madt(rsdp.address).expect("ACPI MADT not found");
    apic::init(&madt);
    ok(format_args!(
        "{} APIC, {} I/O APIC(s), {} CPU(s) found (using 1)",
        if apic::is_x2apic() { "x2" } else { "x" },
        madt.ioapics.len(),
        madt.cpu_count
    ));

    let ticks = apic::start_timer();
    ok(format_args!("timer: 1000 Hz (APIC bus ~{} MHz)", ticks * 16 / 1000));

    percpu::init();
    syscall::init();
    task::init();
    ok("scheduler and system calls ready");

    if drivers::keyboard::init(&madt) {
        ok("PS/2 keyboard");
    } else {
        kprintln!("\x1b[93m[warn]\x1b[0m no PS/2 keyboard found (USB keyboards are not supported yet)");
    }

    if let Some(mods) = boot::MODULES.response() {
        for m in mods.modules() {
            modules::register(m.path(), m.data());
        }
    }
    let names = modules::names();
    ok(format_args!("{} program(s) loaded by the bootloader", names.len()));

    let init = cmdline::get("init").unwrap_or("init");
    match modules::find(init) {
        Some(image) => match task::spawn_user(init, image, &[init.as_bytes()], None) {
            Ok(pid) => ok(format_args!("started {} (pid {})", init, pid)),
            Err(e) => kprintln!("\x1b[91m[fail]\x1b[0m could not start {}: {}", init, e),
        },
        None => kprintln!("\x1b[93m[warn]\x1b[0m no program named {}; add it in limine.conf", init),
    }
    kprintln!();

    // Interrupts come on in the idle loop, so the timer can drain buffered
    // serial output from here on. Until now it was written synchronously, so
    // a boot that hangs still shows everything up to the hang.
    drivers::serial::start_buffering();

    // This boot thread becomes the idle task.
    task::idle();
}

fn ok(msg: impl core::fmt::Display) {
    kprintln!("\x1b[92m[ ok ]\x1b[0m {}", msg);
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    cpu::cli();
    drivers::serial::stop_buffering();
    console::_panic_print(format_args!("\n\x1b[91m*** KERNEL PANIC ***\x1b[0m\n"));
    if let Some(loc) = info.location() {
        console::_panic_print(format_args!("at {}:{}\n", loc.file(), loc.line()));
    }
    console::_panic_print(format_args!("{}\n", info.message()));
    cpu::halt_forever();
}
