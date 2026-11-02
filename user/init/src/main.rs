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
  history         recent commands (Up/Down recall them)
  clear           clear the screen";

const PROMPT: &str = "\x1b[92mkern\x1b[0m> ";
const LINE_MAX: usize = 128;
const HISTORY_SIZE: usize = 16;

/// The last HISTORY_SIZE command lines, oldest first.
struct History {
    lines: [[u8; LINE_MAX]; HISTORY_SIZE],
    lens: [usize; HISTORY_SIZE],
    pushed: usize, // total ever added; the ring holds the last HISTORY_SIZE
}

impl History {
    fn new() -> Self {
        History { lines: [[0; LINE_MAX]; HISTORY_SIZE], lens: [0; HISTORY_SIZE], pushed: 0 }
    }

    fn len(&self) -> usize {
        self.pushed.min(HISTORY_SIZE)
    }

    /// Entry `i`, where 0 is the oldest one still kept.
    fn get(&self, i: usize) -> &[u8] {
        let slot = (self.pushed - self.len() + i) % HISTORY_SIZE;
        &self.lines[slot][..self.lens[slot]]
    }

    fn push(&mut self, line: &[u8]) {
        if line.is_empty() || (self.len() > 0 && self.get(self.len() - 1) == line) {
            return;
        }
        let slot = self.pushed % HISTORY_SIZE;
        self.lines[slot][..line.len()].copy_from_slice(line);
        self.lens[slot] = line.len();
        self.pushed += 1;
    }
}

#[no_mangle]
fn main() -> i32 {
    println!("\x1b[96mWelcome to Kern.\x1b[0m Type 'help' for commands.\n");
    let mut line = [0u8; LINE_MAX];
    let mut history = History::new();
    loop {
        print!("{}", PROMPT);
        let len = read_line(&mut line, &history);
        let Ok(text) = core::str::from_utf8(&line[..len]) else { continue };
        let text = text.trim();
        history.push(text.as_bytes());
        run(text, &history);
        while let Some((pid, code)) = ulib::try_wait() {
            println!("[{}] done (exit code {})", pid, code);
        }
    }
}

/// Where we are inside an ANSI escape sequence sent by the keyboard driver.
enum Escape {
    None,
    Esc,
    Csi,
}

/// Line editing: echo typed keys, handle backspace, Up/Down for history,
/// stop at Enter.
fn read_line(buf: &mut [u8; LINE_MAX], history: &History) -> usize {
    let mut len = 0;
    let mut key = [0u8; 16];
    let mut escape = Escape::None;
    // Position in the history; history.len() means the line being typed,
    // which is kept in `draft` while browsing older entries.
    let mut pos = history.len();
    let mut draft = [0u8; LINE_MAX];
    let mut draft_len = 0;
    loop {
        let n = ulib::read(&mut key).max(0) as usize;
        for &c in &key[..n] {
            match escape {
                Escape::Esc if c == b'[' => {
                    escape = Escape::Csi;
                    continue;
                }
                Escape::Esc => escape = Escape::None, // lone Esc: handle `c` normally
                Escape::Csi => {
                    escape = Escape::None;
                    let target = match c {
                        b'A' if pos > 0 => pos - 1,
                        b'B' if pos < history.len() => pos + 1,
                        _ => continue, // other keys (left/right) are ignored
                    };
                    if pos == history.len() {
                        draft[..len].copy_from_slice(&buf[..len]);
                        draft_len = len;
                    }
                    pos = target;
                    let text = if pos == history.len() { &draft[..draft_len] } else { history.get(pos) };
                    buf[..text.len()].copy_from_slice(text);
                    len = text.len();
                    // Redraw the whole line: back to column 0, clear, prompt, text.
                    print!("\r\x1b[K{}", PROMPT);
                    ulib::write(1, &buf[..len]);
                    continue;
                }
                Escape::None => {}
            }
            match c {
                b'\n' | b'\r' => {
                    println!();
                    return len;
                }
                0x08 | 0x7f => {
                    if len > 0 {
                        len -= 1;
                        print!("\x08 \x08");
                    }
                }
                0x1b => escape = Escape::Esc,
                0x20..=0x7e if len < buf.len() => {
                    buf[len] = c;
                    len += 1;
                    ulib::write(1, &[c]);
                }
                _ => {}
            }
        }
    }
}

fn run(line: &str, history: &History) {
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
        "history" => {
            for i in 0..history.len() {
                let entry = core::str::from_utf8(history.get(i)).unwrap_or("?");
                println!("{:>4}  {}", history.pushed - history.len() + i + 1, entry);
            }
        }
        "init" => println!("init is already running"),
        program => {
            let background = arg == "&";
            let pid = ulib::spawn(program);
            if pid == -2 {
                println!("{}: command not found (try 'help' or 'ls')", program);
                return;
            }
            if pid < 0 {
                println!("{}: could not start (error {})", program, pid);
                return;
            }
            if background {
                println!("[{}] started in background", pid);
            } else {
                match ulib::wait(pid as u64) {
                    Some(0) => {}
                    Some(code) => println!("[{}] exited with code {}", pid, code),
                    None => println!("[{}] could not wait for it", pid),
                }
            }
        }
    }
}
