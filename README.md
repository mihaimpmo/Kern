# Kern

A small monolithic x86-64 kernel written in Rust. It boots on real PCs (UEFI or legacy BIOS) through the [Limine](https://github.com/limine-bootloader/limine) bootloader, runs programs in user mode with memory protection, and switches between them preemptively.

```
[ ok ] GDT, TSS and IDT loaded
[ ok ] memory: 2001 MiB usable, 1995 MiB free
[ ok ] x APIC, 1 I/O APIC(s), 4 CPU(s) found
[ ok ] timer: 1000 Hz (APIC bus ~1001 MHz)
[ ok ] scheduler and system calls ready
[ ok ] 4 CPU(s) online
[ ok ] PS/2 keyboard
[ ok ] filesystem: 9 file(s) from the initramfs
[ ok ] started init (pid 1)

kern:/> hello &
[2] started in background
kern:/> crash
[kernel] process 3 killed: Page fault at 0x400012 (address 0xffffffff80000000)
```

## What it has

| Area | Implementation |
|---|---|
| Boot | Limine protocol (base revision 5), one hybrid ISO for UEFI + BIOS |
| CPU setup | GDT, TSS, IDT, 256 interrupt stubs, separate stacks (IST) for double fault / NMI / machine check |
| Hardening | SMEP, SMAP and UMIP when the CPU has them; user memory is only touched through checked copies |
| Physical memory | Free-list frame allocator built from the firmware memory map |
| Virtual memory | 4-level paging, one address space per process, shared kernel half, NX |
| Kernel heap | Grows on demand (`Vec`, `Box`, `String` all work in the kernel) |
| Kernel stacks | 32 KiB each, with an unmapped guard page below to catch overflows |
| Interrupts | Local APIC (xAPIC and x2APIC) + I/O APIC, routes taken from ACPI MADT |
| Timer | APIC timer calibrated against the PIT, 1000 Hz |
| Multi-core | All CPUs run the kernel (Limine MP request), per-CPU GDT/TSS/idle task, panics stop every CPU |
| Scheduler | Preemptive round-robin across all CPUs, 10 ms time slices, sleep, wait queues |
| User mode | Ring 3 processes loaded from ELF files, `syscall`/`sysret`, arguments (argv) |
| Floating point | SSE in user programs, register state saved per task |
| Files | In-memory VFS (`/bin` from an initramfs, `/tmp`, `/dev`), file descriptors, working directory, redirection |
| User memory | Demand-paged regions: `brk`, `mmap`/`munmap`/`mprotect`, 8 MiB stack; `Vec`/`String`/`Box` in user programs |
| Syscalls | 28: processes (spawn with argv, wait, ...), memory (brk, mmap, ...), files (open, read, write, lseek, stat, readdir, mkdir, unlink, chdir, dup2, ...) |
| Faults | A crashing program is killed; the kernel keeps running |
| Drivers | Framebuffer text console (with ANSI colours), PS/2 keyboard, buffered serial port |
| User space | `ulib` (mini standard library), a shell (`init`) with history and redirection, `ls`, `cat`, `mkdir`, `rm`, demo programs |

## Project layout

```
kern/
├── Makefile            builds everything into kern.iso
├── limine.conf         boot menu; loads the kernel and the initramfs
├── kernel/
│   ├── build.rs        generates interrupt stubs, passes linker script
│   ├── linker.ld       kernel placed at 0xffffffff80000000
│   └── src/
│       ├── main.rs     kmain: boot sequence
│       ├── boot.rs     Limine requests/responses
│       ├── console.rs  framebuffer + serial console, kprintln!
│       ├── acpi.rs     finds the MADT (CPUs, interrupt controllers)
│       ├── syscall/    syscall entry (assembly), dispatch, file/process/memory calls
│       ├── fs/         VFS, tmpfs, devices, initramfs
│       ├── sync.rs     SpinLock, IrqGuard
│       ├── arch/       cpu, gdt, idt, interrupts, apic, percpu, smp
│       ├── mm/         pmm (frames), vmm (page tables), uspace (process memory), heap, kstack
│       ├── task/       scheduler, context switch, ELF loader
│       └── drivers/    keyboard, serial
├── user/
│   ├── linker.ld       user programs start at 0x400000
│   ├── ulib/           syscalls, print!/println!, args, heap allocator, _start, panic handler
│   ├── init/           the shell
│   ├── ls/ cat/ mkdir/ rm/  file utilities
│   ├── hello/          prints with pauses (run several with `hello &`)
│   ├── spin/           hogs the CPU to show preemption
│   ├── crash/          writes to kernel memory to show protection
│   └── selftest/       kernel checks run by `make test`
└── tests/run.py        boots the test ISOs in QEMU and checks the results
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
make test       # automated tests in QEMU (needs python3)
```

The Makefile sets `RUSTC_BOOTSTRAP=1` so a stable Rust toolchain can rebuild `core`/`alloc` for the bare-metal targets. With a nightly toolchain that variable is unnecessary. The kernel is built for `x86_64-unknown-none`; user programs for `user/x86_64-kern-user.json`, the same target with SSE floating point enabled.

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
- **CPU:** every core runs (up to 64).
- **Untested path:** if the firmware leaves the CPU in x2APIC mode (common on recent machines), the kernel uses the x2APIC MSR interface. QEMU here couldn't emulate that mode, so this is the first thing to suspect if the boot stops right after the "memory" line.
- If anything goes wrong, the kernel prints a red `KERNEL PANIC` with the file, line and register dump. A photo of that screen is enough to debug it.

## Extending it

**Add a user program:** copy `user/hello`, then add it to `members` in `user/Cargo.toml` and `PROGRAMS` in the Makefile. It ends up in `/bin`, and the shell can run it by name, with arguments (`ulib::args()`).

**Add a system call:** add a number and a `sys_*` function in `kernel/src/syscall.rs`, and a wrapper in `user/ulib/src/lib.rs`. Only touch user memory through `copy_from_user` / `copy_to_user`; with SMAP on, anything else faults. Add a check for it to `user/selftest` and run `make test`.

**Add a kernel thread:** `task::spawn_kernel("name", function, arg)`.

**Add an interrupt:** pick a vector in `arch/interrupts.rs`, handle it in `interrupt_dispatch`, and route the device's IRQ with `apic::route_isa_irq` (or MSI for PCI devices).

### Suggested next steps, roughly in order

1. **SMP, part 2:** per-CPU run queues instead of one shared queue, wake-up IPIs, TLB shootdown.
2. **PCI enumeration** through ACPI MCFG; it's the gateway to every modern device.
3. **Storage:** an NVMe or AHCI driver and a FAT32 filesystem under the VFS, so files survive a reboot.
4. **Pipes, signals (Ctrl+C), `exec`**, then a real libc port.
5. **USB:** xHCI + HID keyboard.
6. Reclaim bootloader memory, a buddy allocator for contiguous frames (needed for DMA).

## Design notes

- **Monolithic:** drivers run inside the kernel, as in Linux.
- **Address space:** user space is `0x0`–`0x7fff_ffff_ffff`; the kernel half (`0xffff_8000_...` and up) is shared by all processes and not accessible from ring 3.
- **Locking:** `SpinLock` disables interrupts while held, so interrupt handlers can't deadlock against the code they interrupted. The scheduler is one spinlock around a shared ready queue, held across the context switch and released by the task that resumes.
- **Process exit:** a process can't free the stack it's running on, so exit marks it a zombie and the scheduler frees its memory on the next switch. The exit code is kept until the parent collects it with `wait`.
