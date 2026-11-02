//! kill [-SIGNAL] <pid>...: send a signal (default SIGTERM). A negative pid
//! means a process group; the signal is a name (TERM, SIGINT) or a number.

#![no_std]
#![no_main]

use ulib::{println, signal};

#[no_mangle]
fn main() -> i32 {
    let mut args = ulib::args().skip(1).peekable();
    let mut sig = signal::SIGTERM;
    if let Some(first) = args.peek().and_then(|a| a.strip_prefix('-')) {
        match signal::parse(first) {
            Some(s) => sig = s,
            None => {
                println!("kill: unknown signal {}", first);
                return 2;
            }
        }
        args.next();
    }
    if args.peek().is_none() {
        println!("usage: kill [-SIGNAL] <pid>...");
        return 2;
    }
    let mut failed = false;
    for arg in args {
        let result = arg.parse::<i64>().map_err(|_| ulib::Errno::EINVAL).and_then(|pid| ulib::kill(pid, sig));
        if let Err(e) = result {
            println!("kill: {}: {}", arg, e);
            failed = true;
        }
    }
    failed as i32
}
