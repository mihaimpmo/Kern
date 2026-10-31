//! Programs handed to us by the bootloader (listed in limine.conf).
//! This stands in for a filesystem until one exists.

use crate::sync::SpinLock;
use alloc::{string::String, vec::Vec};

struct Module {
    name: String,
    data: &'static [u8],
}

static MODULES: SpinLock<Vec<Module>> = SpinLock::new(Vec::new());

/// "/boot/bin/hello.elf" -> "hello"
fn short_name(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.strip_suffix(".elf").unwrap_or(base)
}

pub fn register(path: &str, data: &'static [u8]) {
    MODULES.lock().push(Module { name: String::from(short_name(path)), data });
}

pub fn find(name: &str) -> Option<&'static [u8]> {
    MODULES.lock().iter().find(|m| m.name == name).map(|m| m.data)
}

pub fn names() -> Vec<String> {
    MODULES.lock().iter().map(|m| m.name.clone()).collect()
}
