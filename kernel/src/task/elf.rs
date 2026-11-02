//! ELF64 loader for user programs (static executables, no dynamic linking).

use crate::mm::{phys_to_virt, vmm, vmm::AddressSpace, PAGE_SIZE};
use core::ptr::read_unaligned;

const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

#[derive(Clone, Copy)]
#[repr(C)]
struct Header {
    ident: [u8; 16],
    kind: u16,
    machine: u16,
    version: u32,
    entry: u64,
    phoff: u64,
    shoff: u64,
    flags: u32,
    ehsize: u16,
    phentsize: u16,
    phnum: u16,
    shentsize: u16,
    shnum: u16,
    shstrndx: u16,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct ProgramHeader {
    kind: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    paddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

/// Map and copy all loadable segments; returns the entry point.
pub fn load(space: &AddressSpace, image: &[u8]) -> Result<u64, &'static str> {
    if image.len() < core::mem::size_of::<Header>() {
        return Err("file too small");
    }
    let h: Header = unsafe { read_unaligned(image.as_ptr() as *const Header) };
    if &h.ident[..4] != b"\x7fELF" || h.ident[4] != 2 || h.ident[5] != 1 {
        return Err("not a 64-bit little-endian ELF");
    }
    if h.machine != 0x3E || h.kind != 2 {
        return Err("not an x86-64 static executable");
    }
    if h.entry >= vmm::USER_END {
        return Err("entry point outside user space");
    }
    let ph_size = core::mem::size_of::<ProgramHeader>();
    if h.phnum != 0 && (h.phentsize as usize) < ph_size {
        return Err("bad program header table");
    }

    // All offsets and sizes come from the file, so every sum is checked.
    let in_image = |offset: u64, len: u64| offset.checked_add(len).map_or(false, |end| end <= image.len() as u64);
    for i in 0..h.phnum as u64 {
        let off = (h.phentsize as u64).checked_mul(i).and_then(|o| o.checked_add(h.phoff));
        let off = match off {
            Some(off) if in_image(off, ph_size as u64) => off as usize,
            _ => return Err("bad program header table"),
        };
        let ph: ProgramHeader = unsafe { read_unaligned(image.as_ptr().add(off) as *const ProgramHeader) };
        if ph.kind != PT_LOAD || ph.memsz == 0 {
            continue;
        }
        let end = ph.vaddr.checked_add(ph.memsz).ok_or("segment overflows")?;
        // Page 0 stays unmapped so null pointers in user programs fault.
        if ph.vaddr < PAGE_SIZE || end > vmm::USER_END || ph.filesz > ph.memsz || !in_image(ph.offset, ph.filesz) {
            return Err("bad segment");
        }

        let mut flags = 0;
        if ph.flags & PF_W != 0 {
            flags |= vmm::flags::WRITABLE;
        }
        if ph.flags & PF_X == 0 {
            flags |= vmm::flags::NO_EXECUTE;
        }
        space.map_anonymous(ph.vaddr, ph.memsz, flags).map_err(|_| "out of memory")?;

        // Copy file contents page by page through the HHDM (the new address
        // space is not active, so we can't just memcpy to the user address).
        let mut copied = 0;
        while copied < ph.filesz {
            let va = ph.vaddr + copied;
            let in_page = (PAGE_SIZE - (va % PAGE_SIZE)).min(ph.filesz - copied);
            let (pa, _) = vmm::translate(space.pml4, va).ok_or("mapping vanished")?;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    image.as_ptr().add((ph.offset + copied) as usize),
                    phys_to_virt(pa) as *mut u8,
                    in_page as usize,
                );
            }
            copied += in_page;
        }
    }
    Ok(h.entry)
}
