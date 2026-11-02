//! Memory system calls (see mm/uspace.rs for the model).

use super::{with_user_space, SysResult, EINVAL};
use crate::mm::uspace::Prot;

const MAP_PRIVATE: u64 = 0x02;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;

pub fn brk(addr: u64) -> SysResult {
    with_user_space(|s| s.brk(addr))
}

/// Anonymous private memory only: `flags` must be MAP_PRIVATE |
/// MAP_ANONYMOUS, optionally with MAP_FIXED.
pub fn mmap(addr: u64, len: u64, prot: u64, flags: u64) -> SysResult {
    let prot = Prot::from_bits(prot).ok_or(EINVAL)?;
    let required = MAP_PRIVATE | MAP_ANONYMOUS;
    if flags & !(required | MAP_FIXED) != 0 || flags & required != required {
        return Err(EINVAL);
    }
    let fixed = (flags & MAP_FIXED != 0).then_some(addr);
    Ok(with_user_space(|s| s.mmap(fixed, len, prot))??)
}

pub fn munmap(addr: u64, len: u64) -> SysResult {
    with_user_space(|s| s.munmap(addr, len))??;
    Ok(0)
}

pub fn mprotect(addr: u64, len: u64, prot: u64) -> SysResult {
    let prot = Prot::from_bits(prot).ok_or(EINVAL)?;
    with_user_space(|s| s.mprotect(addr, len, prot))??;
    Ok(0)
}
