// Build script: generates the 256 interrupt entry stubs and passes the linker script.
use std::{env, fmt::Write, fs, path::PathBuf};

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rustc-link-arg=-T{}", dir.join("linker.ld").display());
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=build.rs");

    // CPU exceptions that push an error code themselves.
    let has_error = |v: u32| matches!(v, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30);

    let mut s = String::from(".section .text\n");
    for v in 0..256u32 {
        writeln!(s, ".global isr_stub_{v}\nisr_stub_{v}:").unwrap();
        if !has_error(v) {
            s.push_str("    push 0\n");
        }
        writeln!(s, "    push {v}\n    jmp isr_common").unwrap();
    }
    s.push_str(".section .rodata\n.balign 8\n.global isr_stub_table\nisr_stub_table:\n");
    for v in 0..256u32 {
        writeln!(s, "    .quad isr_stub_{v}").unwrap();
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("isr_stubs.s");
    fs::write(out, s).unwrap();
}
