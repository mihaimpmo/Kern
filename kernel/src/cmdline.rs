//! Kernel command line (the `cmdline:` line in limine.conf): space-separated
//! words, either `key=value` or bare flags.
//!
//!   init=<program>   program to start as pid 1 (default: init)
//!   qemu-exit        when pid 1 exits, end QEMU with its exit code
//!                    (needs `-device isa-debug-exit,iobase=0xf4,iosize=4`)

use crate::boot;

fn words() -> impl Iterator<Item = &'static str> {
    boot::CMDLINE.response().map_or("", |r| r.cmdline()).split_ascii_whitespace()
}

pub fn get(key: &str) -> Option<&'static str> {
    words().find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
}

pub fn flag(name: &str) -> bool {
    words().any(|w| w == name)
}
