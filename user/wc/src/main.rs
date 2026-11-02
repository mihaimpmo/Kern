//! wc [-l] [-w] [-c] [file...]: count lines, words and bytes of files (or
//! standard input). Without options, all three.

#![no_std]
#![no_main]

use ulib::fs::{self, flags, Fd};
use ulib::{print, println, Errno};

#[derive(Default, Clone, Copy)]
struct Counts {
    lines: u64,
    words: u64,
    bytes: u64,
}

#[no_mangle]
fn main() -> i32 {
    let (mut lines, mut words, mut bytes) = (false, false, false);
    let mut files = 0;
    for arg in ulib::args().skip(1) {
        match arg {
            "-l" => lines = true,
            "-w" => words = true,
            "-c" => bytes = true,
            _ if arg.starts_with('-') => {
                println!("usage: wc [-l] [-w] [-c] [file...]");
                return 2;
            }
            _ => files += 1,
        }
    }
    if !(lines || words || bytes) {
        (lines, words, bytes) = (true, true, true);
    }
    let show = |c: Counts, name: Option<&str>| {
        let fields = [(lines, c.lines), (words, c.words), (bytes, c.bytes)];
        let mut first = true;
        for (on, value) in fields {
            if on {
                print!("{}{}", if first { "" } else { " " }, value);
                first = false;
            }
        }
        match name {
            Some(name) => println!(" {}", name),
            None => println!(),
        }
    };
    if files == 0 {
        return match count(ulib::STDIN) {
            Ok(c) => {
                show(c, None);
                0
            }
            Err(e) => {
                println!("wc: {}", e);
                1
            }
        };
    }
    let mut failed = false;
    for path in ulib::args().skip(1).filter(|a| !a.starts_with('-')) {
        let result = fs::open(path, flags::READ_ONLY).and_then(|fd| {
            let c = count(fd);
            let _ = fs::close(fd);
            c
        });
        match result {
            Ok(c) => show(c, Some(path)),
            Err(e) => {
                println!("wc: {}: {}", path, e);
                failed = true;
            }
        }
    }
    failed as i32
}

fn count(fd: Fd) -> Result<Counts, Errno> {
    let mut c = Counts::default();
    let mut in_word = false;
    let mut buf = [0u8; 4096];
    loop {
        let n = ulib::read(fd, &mut buf)?;
        if n == 0 {
            return Ok(c);
        }
        for &b in &buf[..n] {
            c.bytes += 1;
            if b == b'\n' {
                c.lines += 1;
            }
            let space = b.is_ascii_whitespace();
            if !space && !in_word {
                c.words += 1;
            }
            in_word = !space;
        }
    }
}
