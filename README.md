# Kern

A small monolithic x86-64 kernel written in Rust. It boots on real PCs (UEFI or legacy BIOS) through the [Limine](https://github.com/limine-bootloader/limine) bootloader, runs programs in user mode with memory protection, and switches between them preemptively.

```
[ ok ] GDT, TSS and IDT loaded
[ ok ] memory: 2001 MiB usable, 1995 MiB free
[ ok ] x APIC, 1 I/O APIC(s), 4 CPU(s) found (using 1)
[ ok ] timer: 1000 Hz (APIC bus ~1001 MHz)
[ ok ] scheduler and system calls ready
[ ok ] PS/2 keyboard
[ ok ] started init (pid 1)

kern> hello &
[2] started in background
kern> crash
[kernel] process 3 killed: Page fault at 0x400012 (address 0xffffffff80000000)
```

## What it has

| Area | Implementation |
|---|---|
| Boot | Limine protocol (base revision 5), one hybrid ISO for UEFI + BIOS |
| CPU setup | GDT, TSS, IDT, 256 interrupt stubs, separate stacks (IST) for double fault / NMI / machine check |
| Physical memory | Free-list frame allocator built from the firmware memory map |
| Virtual memory | 4-level paging, one address space per process, shared kernel half, NX |
| Kernel heap | Grows on demand (`Vec`, `Box`, `String` all work in the kernel) |
| Kernel stacks | 32 KiB each, with an unmapped guard page below to catch overflows |
| Interrupts | Local APIC (xAPIC and x2APIC) + I/O APIC, routes taken from ACPI MADT |
| Timer | APIC timer calibrated against the PIT, 1000 Hz |
| Scheduler | Preemptive round-robin, 10 ms time slices, sleep, wait queues, idle task |
| User mode | Ring 3 processes loaded from ELF files, `syscall`/`sysret` |
| Syscalls | exit, write, read, getpid, sleep, yield, uptime, spawn, wait, try_wait, ps, sysinfo, list programs |
| Faults | A crashing program is killed; the kernel keeps running |
| Drivers | Framebuffer text console (with ANSI colours), PS/2 keyboard, serial port |
| User space | `ulib` (mini standard library), a shell (`init`), demo programs |

## Project layout

```
kern/
├── Makefile            builds everything into kern.iso
├── limine.conf         boot menu + which programs to load
├── kernel/
│   ├── build.rs        generates interrupt stubs, passes linker script
│   ├── linker.ld       kernel placed at 0xffffffff80000000
│   └── src/
│       ├── main.rs     kmain: boot sequence
│       ├── boot.rs     Limine requests/responses
│       ├── console.rs  framebuffer + serial console, kprintln!
│       ├── acpi.rs     finds the MADT (CPUs, interrupt controllers)
│       ├── syscall.rs  syscall entry (assembly) + every system call
│       ├── modules.rs  programs provided by the bootloader
│       ├── sync.rs     SpinLock, IrqGuard
│       ├── arch/       cpu, gdt, idt, interrupts, apic, percpu
│       ├── mm/         pmm (frames), vmm (page tables), heap, kstack
│       ├── task/       scheduler, context switch, ELF loader
│       └── drivers/    keyboard, serial
└── user/
    ├── linker.ld       user programs start at 0x400000
    ├── ulib/           syscalls, print!/println!, _start, panic handler
    ├── init/           the shell
    ├── hello/          prints with pauses (run several with `hello &`)
    ├── spin/           hogs the CPU to show preemption
    └── crash/          writes to kernel memory to show protection
```

## Building

You need Linux (WSL works on Windows), plus:

```sh
# Rust, with the standard library source
rustup component add rust-src
# Debian/Ubuntu
sudo apt install build-essential git xorriso
# optional, to test in a VM
sudo apt install qemu-system-x86 ovmf
```

Then:

```sh
make            # -> kern.iso
make run        # test in QEMU, legacy BIOS
make run-uefi   # test in QEMU, UEFI
```

The Makefile sets `RUSTC_BOOTSTRAP=1` so a stable Rust toolchain can rebuild `core`/`alloc` for the bare-metal target. With a nightly toolchain that variable is unnecessary.

## Booting on a real computer

1. Write `kern.iso` to a USB stick. It erases the stick.
   - Windows: **Rufus**, and when asked choose **"Write in DD Image mode"**. balenaEtcher also works.
   - Linux: `sudo dd if=kern.iso of=/dev/sdX bs=4M status=progress && sync`
2. In the firmware setup, **turn off Secure Boot**. Limine isn't signed with Microsoft's key.
3. Boot from the USB stick. On Lenovo IdeaPads: press **F12** at power-on, or use the **Novo** button, then choose the USB drive.

Nothing is written to the internal disk; the kernel has no disk driver yet.

### What to expect on real hardware

- **Keyboard:** laptop built-in keyboards are PS/2 internally and work. A USB keyboard usually won't, since there is no USB driver yet. Some firmwares emulate USB keyboards as PS/2 in BIOS (CSM) mode.
- **Screen:** the console runs at the resolution the firmware chose; text is scaled up on screens ≥1600 px wide.
- **CPU:** only the first core runs; the others are detected but stay parked.
- **Untested path:** if the firmware leaves the CPU in x2APIC mode (common on recent machines), the kernel uses the x2APIC MSR interface. QEMU here couldn't emulate that mode, so this is the first thing to suspect if the boot stops right after the "memory" line.
- If anything goes wrong, the kernel prints a red `KERNEL PANIC` with the file, line and register dump. A photo of that screen is enough to debug it.

## Extending it

**Add a user program:** copy `user/hello`, add it to `members` in `user/Cargo.toml`, `PROGRAMS` in the Makefile, and a `module_path` line in `limine.conf`. The shell can then run it by name.

**Add a system call:** add a number and a `sys_*` function in `kernel/src/syscall.rs`, and a wrapper in `user/ulib/src/lib.rs`. Always validate user pointers with `user_slice` / `user_slice_mut`.

**Add a kernel thread:** `task::spawn_kernel("name", function, arg)`.

**Add an interrupt:** pick a vector in `arch/interrupts.rs`, handle it in `interrupt_dispatch`, and route the device's IRQ with `apic::route_isa_irq` (or MSI for PCI devices).

### Suggested next steps, roughly in order

1. **SMP:** start the other cores (Limine MP request), per-CPU run queues, real locks in the scheduler.
2. **PCI enumeration** through ACPI MCFG; it's the gateway to every modern device.
3. **Storage:** an NVMe or AHCI driver, then a VFS layer and a FAT32 filesystem, so programs can be loaded from disk instead of as boot modules.
4. **User memory:** `mmap`/`brk` syscalls, then a heap allocator in `ulib`.
5. **`fork`/`exec`, file descriptors, pipes**, enough to port a real libc later.
6. **USB:** xHCI + HID keyboard.
7. Reclaim bootloader memory, a buddy allocator for contiguous frames (needed for DMA).

## Design notes

- **Monolithic:** drivers run inside the kernel, as in Linux.
- **Address space:** user space is `0x0`–`0x7fff_ffff_ffff`; the kernel half (`0xffff_8000_...` and up) is shared by all processes and not accessible from ring 3.
- **Locking:** `SpinLock` disables interrupts while held, so interrupt handlers can't deadlock against the code they interrupted. The scheduler is protected by disabling interrupts, which is correct for one CPU and must become a real lock for SMP.
- **Process exit:** a process can't free the stack it's running on, so exit marks it a zombie and the scheduler frees its memory on the next switch. The exit code is kept until the parent collects it with `wait`.
