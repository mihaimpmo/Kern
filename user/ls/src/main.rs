//! ls [path...]: list directories (the working directory by default).
//! Directories are shown with a trailing '/', devices with a trailing '*'.

#![no_std]
#![no_main]

use ulib::fs::{self, Kind};
use ulib::println;

#[no_mangle]
fn main() -> i32 {
    let many = ulib::args().len() > 2;
    let mut failed = false;
    let mut show = |path: &str| {
        if let Err(e) = list(path, many) {
            println!("ls: {}: {}", path, e);
            failed = true;
        }
    };
    if ulib::args().len() == 1 {
        show(".");
    } else {
        ulib::args().skip(1).for_each(show);
    }
    failed as i32
}

fn suffix(kind: Kind) -> &'static str {
    match kind {
        Kind::Dir => "/",
        Kind::Device => "*",
        Kind::File => "",
    }
}

fn list(path: &str, heading: bool) -> Result<(), ulib::Errno> {
    let stat = fs::stat(path)?;
    if stat.kind != Kind::Dir {
        println!("{}{}", path, suffix(stat.kind));
        return Ok(());
    }
    if heading {
        println!("{}:", path);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        println!("{}{}", entry.name(), suffix(entry.kind));
    }
    Ok(())
}
