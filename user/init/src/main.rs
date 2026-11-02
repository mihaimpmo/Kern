//! init: the first user program, and the shell.
//!
//! A line is words separated by spaces, optionally with redirections
//! (`< file`, `> file`, `>> file`) and a trailing `&` to run in the
//! background. A word that isn't a builtin is a program: a path, or a name
//! in /bin.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};
use ulib::fs::{self, flags};
use ulib::{print, println, Errno};

const HELP: &str = "\
builtins:
  help                this text
  cd [dir]            change directory (default /)
  pwd                 print the working directory
  echo <text>         print text
  ps                  list tasks
  mem                 memory usage
  uptime              time since boot
  sleep <ms>          sleep for a while
  history             recent commands (Up/Down recall them)
  clear               clear the screen
programs (in /bin; try `ls /bin`):
  <program> [args]    run it and wait for it
  <program> [args] &  run it in the background
redirection:
  cmd < in > out      read from / write to files; >> appends";

const HISTORY_SIZE: usize = 100;

#[no_mangle]
fn main() -> i32 {
    println!("\x1b[96mWelcome to Kern.\x1b[0m Type 'help' for commands.\n");
    let mut history: Vec<String> = Vec::new();
    loop {
        let line = read_line(&prompt(), &history);
        let line = line.trim();
        if !line.is_empty() && history.last().map_or(true, |last| last != line) {
            if history.len() == HISTORY_SIZE {
                history.remove(0);
            }
            history.push(String::from(line));
        }
        match parse(line) {
            Ok(Some(command)) => run(&command, &history),
            Ok(None) => {}
            Err(why) => println!("syntax error: {}", why),
        }
        while let Some((pid, code)) = ulib::try_wait() {
            println!("[{}] done (exit code {})", pid, code);
        }
    }
}

fn prompt() -> String {
    let mut buf = [0u8; 256];
    let cwd = fs::getcwd(&mut buf).unwrap_or("?");
    format!("\x1b[92mkern\x1b[0m:\x1b[94m{}\x1b[0m> ", cwd)
}

// ---------------------------------------------------------------- input

/// Where we are inside an ANSI escape sequence sent by the keyboard driver.
enum Escape {
    None,
    Esc,
    Csi,
}

/// Line editing: echo typed keys, handle backspace, Up/Down for history,
/// stop at Enter.
fn read_line(prompt: &str, history: &[String]) -> String {
    print!("{}", prompt);
    let mut line = String::new();
    // Position in the history; history.len() means the line being typed,
    // which is kept in `draft` while browsing older entries.
    let mut pos = history.len();
    let mut draft = String::new();
    let mut escape = Escape::None;
    let mut keys = [0u8; 16];
    loop {
        let n = ulib::read(ulib::STDIN, &mut keys).unwrap_or(0);
        for &c in &keys[..n] {
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
                        draft = line.clone();
                    }
                    pos = target;
                    line = if pos == history.len() { draft.clone() } else { history[pos].clone() };
                    // Redraw the whole line: back to column 0, clear, prompt, text.
                    print!("\r\x1b[K{}{}", prompt, line);
                    continue;
                }
                Escape::None => {}
            }
            match c {
                b'\n' | b'\r' => {
                    println!();
                    return line;
                }
                0x08 | 0x7f => {
                    if line.pop().is_some() {
                        print!("\x08 \x08");
                    }
                }
                0x1b => escape = Escape::Esc,
                0x20..=0x7e => {
                    line.push(c as char);
                    print!("{}", c as char);
                }
                _ => {}
            }
        }
    }
}

// --------------------------------------------------------------- parsing

struct Command<'a> {
    words: Vec<&'a str>,
    stdin: Option<&'a str>,
    /// File, and whether to append.
    stdout: Option<(&'a str, bool)>,
    background: bool,
}

/// None for an empty line.
fn parse(line: &str) -> Result<Option<Command<'_>>, &'static str> {
    let mut command = Command { words: Vec::new(), stdin: None, stdout: None, background: false };
    let mut tokens = line.split_ascii_whitespace().peekable();
    while let Some(token) = tokens.next() {
        // `> file` and `>file` both work.
        let (op, rest) = if let Some(rest) = token.strip_prefix(">>") {
            (">>", rest)
        } else if let Some(rest) = token.strip_prefix('>') {
            (">", rest)
        } else if let Some(rest) = token.strip_prefix('<') {
            ("<", rest)
        } else if token == "&" {
            if tokens.peek().is_some() {
                return Err("& must come last");
            }
            command.background = true;
            continue;
        } else {
            command.words.push(token);
            continue;
        };
        let file = if rest.is_empty() { tokens.next().ok_or("missing file name after redirection")? } else { rest };
        match op {
            "<" => command.stdin = Some(file),
            _ => command.stdout = Some((file, op == ">>")),
        }
    }
    if command.words.is_empty() {
        return if command.stdin.is_some() || command.stdout.is_some() { Err("nothing to run") } else { Ok(None) };
    }
    if command.words.len() > ulib::MAX_ARGS {
        return Err("too many words");
    }
    Ok(Some(command))
}

// --------------------------------------------------------------- running

/// Points stdin/stdout at files while it lives; restores them when dropped.
/// Builtins print through fd 1 like programs do, and a spawned child gets
/// its own copy of the descriptors, so this works for both.
struct Redirect {
    saved: Vec<(fs::Fd, fs::Fd)>, // (target fd, saved original)
}

impl Redirect {
    fn apply(command: &Command) -> Result<Redirect, String> {
        let mut redirect = Redirect { saved: Vec::new() };
        if let Some(path) = command.stdin {
            redirect.point(ulib::STDIN, path, flags::READ_ONLY)?;
        }
        if let Some((path, append)) = command.stdout {
            let mode = flags::WRITE_ONLY | flags::CREATE | if append { flags::APPEND } else { flags::TRUNCATE };
            redirect.point(ulib::STDOUT, path, mode)?;
        }
        Ok(redirect)
    }

    fn point(&mut self, target: fs::Fd, path: &str, mode: u64) -> Result<(), String> {
        let file = fs::open(path, mode).map_err(|e| format!("{}: {}", path, e))?;
        let saved = fs::dup(target).map_err(|e| format!("{}: {}", path, e))?;
        let _ = fs::dup2(file, target);
        let _ = fs::close(file);
        self.saved.push((target, saved));
        Ok(())
    }
}

impl Drop for Redirect {
    fn drop(&mut self) {
        for &(target, saved) in self.saved.iter().rev() {
            let _ = fs::dup2(saved, target);
            let _ = fs::close(saved);
        }
    }
}

fn run(command: &Command, history: &[String]) {
    let redirect = match Redirect::apply(command) {
        Ok(r) => r,
        Err(why) => {
            println!("{}", why);
            return;
        }
    };
    let (cmd, args) = (command.words[0], &command.words[1..]);
    match cmd {
        "help" => println!("{}", HELP),
        "cd" => {
            let dir = args.first().copied().unwrap_or("/");
            if let Err(e) = fs::chdir(dir) {
                println!("cd: {}: {}", dir, e);
            }
        }
        "pwd" => {
            let mut buf = [0u8; 4096];
            println!("{}", fs::getcwd(&mut buf).unwrap_or("?"));
        }
        "echo" => println!("{}", args.join(" ")),
        "clear" => print!("\x1b[2J\x1b[H"),
        "uptime" => {
            let ms = ulib::uptime_ms();
            println!("up {}.{:03} s", ms / 1000, ms % 1000);
        }
        "mem" => {
            let info = ulib::sysinfo();
            let used = info.total_memory - info.free_memory;
            println!(
                "memory: {} MiB used / {} MiB total ({} MiB free), {} CPU(s)",
                used >> 20,
                info.total_memory >> 20,
                info.free_memory >> 20,
                info.cpus
            );
        }
        "sleep" => match args {
            [ms] if ms.parse::<u64>().is_ok() => ulib::sleep_ms(ms.parse().unwrap()),
            _ => println!("usage: sleep <ms>"),
        },
        "ps" => {
            let mut buf = [0u8; 4096];
            let n = ulib::ps(&mut buf).unwrap_or(0);
            let _ = ulib::write_all(ulib::STDOUT, &buf[..n]);
        }
        "history" => {
            for (i, line) in history.iter().enumerate() {
                println!("{:>4}  {}", i + 1, line);
            }
        }
        "init" => println!("init is already running"),
        program => {
            let spawned = ulib::spawn(program, args);
            drop(redirect); // the child has its own copy of the descriptors
            match spawned {
                Err(Errno::ENOENT) => println!("{}: command not found (try 'help' or 'ls /bin')", program),
                Err(e) => println!("{}: could not start: {}", program, e),
                Ok(pid) if command.background => println!("[{}] started in background", pid),
                Ok(pid) => match ulib::wait(pid) {
                    Ok(0) => {}
                    Ok(code) => println!("[{}] exited with code {}", pid, code),
                    Err(e) => println!("[{}] could not wait for it: {}", pid, e),
                },
            }
        }
    }
}
