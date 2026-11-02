//! Building a new process image: ELF segments, a stack, and the System V
//! process-entry block on top of the stack.
//!
//! At entry `rsp` is 16-byte aligned and points at:
//!
//!   argc
//!   argv[0] .. argv[argc - 1], NULL     pointers to NUL-terminated strings
//!   NULL                                 (empty environment)
//!   AT_NULL, 0                           (empty auxiliary vector)
//!   ...the strings themselves, near the top of the stack

use super::elf;
use crate::mm::{vmm, vmm::AddressSpace};
use alloc::vec::Vec;

pub const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_f000;
const USER_STACK_SIZE: u64 = 64 * 1024;

/// Most arguments a program can be started with, and their total size.
pub const MAX_ARGS: usize = 64;
pub const MAX_ARG_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    NoMemory,
    ArgsTooLong,
    /// The file is not a static x86-64 executable we can load; says why.
    BadElf(&'static str),
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            LoadError::NoMemory => f.write_str("out of memory"),
            LoadError::ArgsTooLong => f.write_str("argument list too long"),
            LoadError::BadElf(why) => f.write_str(why),
        }
    }
}

pub struct Image {
    pub space: AddressSpace,
    pub entry: u64,
    pub stack: u64,
}

pub fn load(elf_image: &[u8], args: &[&[u8]]) -> Result<Image, LoadError> {
    if args.len() > MAX_ARGS || args.iter().map(|a| a.len() + 1).sum::<usize>() > MAX_ARG_BYTES {
        return Err(LoadError::ArgsTooLong);
    }
    let space = AddressSpace::new().ok_or(LoadError::NoMemory)?;
    let entry = elf::load(&space, elf_image)?;
    let stack_flags = vmm::flags::WRITABLE | vmm::flags::NO_EXECUTE;
    space
        .map_anonymous(USER_STACK_TOP - USER_STACK_SIZE, USER_STACK_SIZE, stack_flags)
        .map_err(|_| LoadError::NoMemory)?;
    let stack = write_entry_block(&space, args).map_err(|_| LoadError::NoMemory)?;
    Ok(Image { space, entry, stack })
}

fn write_entry_block(space: &AddressSpace, args: &[&[u8]]) -> Result<u64, vmm::MapError> {
    // Strings first, from the top down.
    let mut sp = USER_STACK_TOP;
    let mut pointers = Vec::with_capacity(args.len());
    for arg in args.iter().rev() {
        sp -= arg.len() as u64 + 1;
        space.write(sp, arg)?;
        space.write(sp + arg.len() as u64, &[0])?;
        pointers.push(sp);
    }
    pointers.reverse();

    // argc, argv[], NULL, envp NULL, AT_NULL pair.
    let mut block: Vec<u64> = Vec::with_capacity(args.len() + 5);
    block.push(args.len() as u64);
    block.extend_from_slice(&pointers);
    block.extend_from_slice(&[0, 0, 0, 0]);

    sp = (sp - block.len() as u64 * 8) & !0xf;
    let bytes: Vec<u8> = block.iter().flat_map(|w| w.to_le_bytes()).collect();
    space.write(sp, &bytes)?;
    Ok(sp)
}
