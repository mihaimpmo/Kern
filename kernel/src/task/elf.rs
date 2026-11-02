//! ELF64 loader for user programs (static executables, no dynamic linking).

use super::exec::LoadError::{self, BadElf};
use crate::mm::uspace::{MemError, Prot, UserSpace, MMAP_BASE};
use crate::mm::{align_down, align_up, vmm, PAGE_SIZE};
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

pub struct Loaded {
    pub entry: u64,
    /// End of the highest segment: the heap starts after it.
    pub end: u64,
}

/// Add a region for every loadable segment and copy in its contents
/// (the zero-filled rest is mapped on first touch).
pub fn load(space: &mut UserSpace, image: &[u8]) -> Result<Loaded, LoadError> {
    if image.len() < core::mem::size_of::<Header>() {
        return Err(BadElf("file too small"));
    }
    let h: Header = unsafe { read_unaligned(image.as_ptr() as *const Header) };
    if &h.ident[..4] != b"\x7fELF" || h.ident[4] != 2 || h.ident[5] != 1 {
        return Err(BadElf("not a 64-bit little-endian ELF"));
    }
    if h.machine != 0x3E || h.kind != 2 {
        return Err(BadElf("not an x86-64 static executable"));
    }
    if h.entry >= vmm::USER_END {
        return Err(BadElf("entry point outside user space"));
    }
    let ph_size = core::mem::size_of::<ProgramHeader>();
    if h.phnum != 0 && (h.phentsize as usize) < ph_size {
        return Err(BadElf("bad program header table"));
    }

    let mut highest = 0;
    // All offsets and sizes come from the file, so every sum is checked.
    let in_image = |offset: u64, len: u64| offset.checked_add(len).map_or(false, |end| end <= image.len() as u64);
    for i in 0..h.phnum as u64 {
        let off = (h.phentsize as u64).checked_mul(i).and_then(|o| o.checked_add(h.phoff));
        let off = match off {
            Some(off) if in_image(off, ph_size as u64) => off as usize,
            _ => return Err(BadElf("bad program header table")),
        };
        let ph: ProgramHeader = unsafe { read_unaligned(image.as_ptr().add(off) as *const ProgramHeader) };
        if ph.kind != PT_LOAD || ph.memsz == 0 {
            continue;
        }
        let end = ph.vaddr.checked_add(ph.memsz).ok_or(BadElf("segment overflows"))?;
        // Page 0 stays unmapped so null pointers in user programs fault, and
        // the mmap area above the heap stays free.
        if ph.vaddr < PAGE_SIZE || end > MMAP_BASE || ph.filesz > ph.memsz || !in_image(ph.offset, ph.filesz) {
            return Err(BadElf("bad segment"));
        }

        let mut prot = Prot::READ;
        if ph.flags & PF_W != 0 {
            prot = prot.union(Prot::WRITE);
        }
        if ph.flags & PF_X != 0 {
            prot = prot.union(Prot::EXEC);
        }
        let region = (align_down(ph.vaddr, PAGE_SIZE), align_up(end, PAGE_SIZE));
        space.add_region(region.0, region.1, prot).map_err(|e| match e {
            MemError::NoMemory => LoadError::NoMemory,
            _ => BadElf("overlapping segments"),
        })?;
        let contents = &image[ph.offset as usize..(ph.offset + ph.filesz) as usize];
        space.load(ph.vaddr, contents).map_err(|_| LoadError::NoMemory)?;
        highest = highest.max(end);
    }
    Ok(Loaded { entry: h.entry, end: highest })
}
