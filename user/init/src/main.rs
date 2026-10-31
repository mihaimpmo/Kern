//! init: the first user program. A small interactive shell.

#![no_std]
#![no_main]

use ulib::{print, println};

const HELP: &str = "\
commands:
  help            this text
  ls              list programs
  <program>       run a program and wait for it
  <program> &     run a program in the background
  ps              list tasks
  mem             memory usage
  uptime          time since boot
  echo <text>     print text
  sleep <ms>      sleep for a while
  clear           clear the screen";

#[no_mangle]
fn main() -> i32 {
    println!("\x1b[96mWelcome to Kern.\x1b[0m Type 'help' for commands.\n");
    let mut line = [0u8; 128];
    loop {
        print!("\x1b[92mkern\x1b[0m> ");
        let len = read_line(&mut line);
        let Ok(text) = core::str::from_utf8(&line[..len]) else { continue };
        run(text.trim());
        while let Some((pid, code)) = ulib::try_wait() {
            println!("[{}] done (exit code {})", pid, code);
        }
    }
}

/// Line editing: echo typed keys, handle backspace, stop at Enter.
fn read_line(buf: &mut [u8]) -> usize {
    let mut len = 0;
    let mut key = [0u8; 16];
    loop {
        let n = ulib::read(&mut key);
        let mut i = 0;
        while i < n as usize {
            match key[i] {
                b'\n' | b'\r' => {
                    println!();
                    return len;
                }
                0x08 | 0x7f => {
                    if len > 0 {
                        len -= 1;
                        print!("\x08");
                    }
                }
                0x1b => i += 2, // skip arrow-key escape sequences
                c @ 0x20..=0x7e if len < buf.len() => {
                    buf[len] = c;
                    len += 1;
                    ulib::write(1, &[c]);
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn run(line: &str) {
    let (cmd, arg) = match line.split_once(' ') {
        Some((c, a)) => (c, a.trim()),
        None => (line, ""),
    };
    match cmd {
        "" => {}
        "help" => println!("{}", HELP),
        "echo" => println!("{}", arg),
        "clear" => print!("\x1b[2J\x1b[H"),
        "uptime" => {
            let ms = ulib::uptime_ms();
            println!("up {}.{:03} s", ms / 1000, ms % 1000);
        }
        "mem" => {
            let info = ulib::sysinfo();
            let used = info.total_memory - info.free_memory;
            println!(
                "memory: {} MiB used / {} MiB total ({} MiB free)",
                used >> 20,
                info.total_memory >> 20,
                info.free_memory >> 20
            );
        }
        "sleep" => match arg.parse::<u64>() {
            Ok(ms) => ulib::sleep_ms(ms),
            Err(_) => println!("usage: sleep <ms>"),
        },
        "ls" => {
            let mut buf = [0u8; 512];
            let n = ulib::programs(&mut buf).max(0) as usize;
            ulib::write(1, &buf[..n]);
        }
        "ps" => {
            let mut buf = [0u8; 2048];
            let n = ulib::ps(&mut buf).max(0) as usize;
            ulib::write(1, &buf[..n]);
        }
        "init" => println!("init is already running"),
        program => {
            let background = arg == "&";
            let pid = ulib::spawn(program);
            if pid < 0 {
                println!("{}: command not found (try 'help' or 'ls')", program);
                return;
            }
            if background {
                println!("[{}] started in background", pid);
            } else {
                let code = ulib::wait(pid as u64);
                if code != 0 {
                    println!("[{}] exited with code {}", pid, code);
                }
            }
        }
    }
}
