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
use crate::mm::uspace::{MemError, Prot, UserSpace, STACK_BOTTOM, STACK_TOP};
use alloc::vec::Vec;

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
    pub space: UserSpace,
    pub entry: u64,
    pub stack: u64,
}

pub fn load(elf_image: &[u8], args: &[&[u8]]) -> Result<Image, LoadError> {
    if args.len() > MAX_ARGS || args.iter().map(|a| a.len() + 1).sum::<usize>() > MAX_ARG_BYTES {
        return Err(LoadError::ArgsTooLong);
    }
    let mut space = UserSpace::new().ok_or(LoadError::NoMemory)?;
    let program = elf::load(&mut space, elf_image)?;
    space.set_heap_start(program.end);
    space.add_region(STACK_BOTTOM, STACK_TOP, Prot::RW).map_err(|_| LoadError::NoMemory)?;
    let stack = write_entry_block(&mut space, args).map_err(|_| LoadError::NoMemory)?;
    Ok(Image { space, entry: program.entry, stack })
}

fn write_entry_block(space: &mut UserSpace, args: &[&[u8]]) -> Result<u64, MemError> {
    // Strings first, from the top down.
    let mut sp = STACK_TOP;
    let mut pointers = Vec::with_capacity(args.len());
    for arg in args.iter().rev() {
        sp -= arg.len() as u64 + 1;
        space.load(sp, arg)?;
        space.load(sp + arg.len() as u64, &[0])?;
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
    space.load(sp, &bytes)?;
    Ok(sp)
}
