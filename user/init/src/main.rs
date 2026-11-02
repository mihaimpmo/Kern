//! init: the first user program, and the shell.
//!
//! A line is a pipeline: commands separated by `|`, each a list of words.
//! The first command may read from a file (`< file`), the last may write to
//! one (`> file`, `>> file` to append), and a trailing `&` runs the whole
//! pipeline in the background. A word that isn't a builtin is a program: a
//! path, or a name in /bin.
//!
//! Each pipeline runs as its own process group. While it runs in the
//! foreground the console sends Ctrl+C to that group (SIGINT); at the
//! prompt, Ctrl+C just abandons the line.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};
use ulib::fs::{self, flags, Fd};
use ulib::{print, println, Errno, ExitStatus, Pid};

const HELP: &str = "\
builtins:
  help                this text
  cd [dir]            change directory (default /)
  pwd                 print the working directory
  mem                 memory usage
  uptime              time since boot
  history             recent commands (Up/Down recall them)
  clear               clear the screen
programs (in /bin; try `ls /bin`):
  echo, cat, ls, mkdir, rm, wc, ps, kill, sleep, date, hello, spin, crash, selftest
running:
  cmd args            run and wait; Ctrl+C interrupts it
  cmd args &          run in the background
  a | b | c           pipe each command's output into the next
  cmd < in > out      read from / write to files; >> appends";

const HISTORY_SIZE: usize = 100;
const CTRL_C: u8 = 3;

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
            Ok(Some(pipeline)) => run(&pipeline, &history),
            Ok(None) => {}
            Err(why) => println!("syntax error: {}", why),
        }
        while let Some((pid, status)) = ulib::try_wait() {
            println!("[{}] done ({})", pid, status);
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
                CTRL_C => {
                    // Nothing runs in the foreground at the prompt, so the
                    // key arrives here: abandon the line.
                    println!("^C");
                    return String::new();
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

struct Pipeline<'a> {
    /// The words of each command, in order.
    commands: Vec<Vec<&'a str>>,
    stdin: Option<&'a str>,
    /// File, and whether to append.
    stdout: Option<(&'a str, bool)>,
    background: bool,
}

/// None for an empty line.
fn parse(line: &str) -> Result<Option<Pipeline<'_>>, &'static str> {
    let mut p = Pipeline { commands: Vec::new(), stdin: None, stdout: None, background: false };
    for (i, part) in line.split('|').enumerate() {
        if i > 0 && p.stdout.is_some() {
            return Err("only the last command can write to a file");
        }
        if p.background {
            return Err("& must come last");
        }
        let mut words = Vec::new();
        let mut tokens = part.split_ascii_whitespace().peekable();
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
                p.background = true;
                continue;
            } else {
                words.push(token);
                continue;
            };
            let file = if rest.is_empty() { tokens.next().ok_or("missing file name after redirection")? } else { rest };
            if op == "<" {
                if i > 0 {
                    return Err("only the first command can read from a file");
                }
                p.stdin = Some(file);
            } else {
                p.stdout = Some((file, op == ">>"));
            }
        }
        if words.len() > ulib::MAX_ARGS {
            return Err("too many words");
        }
        p.commands.push(words);
    }
    if p.commands.iter().all(|c| c.is_empty()) && p.stdin.is_none() && p.stdout.is_none() && !p.background {
        return Ok(None);
    }
    if p.commands.iter().any(|c| c.is_empty()) {
        return Err("empty command");
    }
    Ok(Some(p))
}

// --------------------------------------------------------------- running

/// Points fds 0/1 elsewhere while it lives; puts them back when dropped.
/// Builtins print through fd 1 like programs do, and a spawned child gets
/// its own copy of the descriptors, so this works for both.
struct FdSwap {
    saved: Vec<(Fd, Fd)>, // (target fd, saved original)
}

impl FdSwap {
    fn new() -> FdSwap {
        FdSwap { saved: Vec::new() }
    }

    /// Make `target` refer to `fd`'s open file.
    fn point(&mut self, target: Fd, fd: Fd) -> Result<(), Errno> {
        // Close-on-spawn: the saved copy must not leak into children.
        let saved = fs::dup(target, flags::CLOSE_ON_SPAWN)?;
        let _ = fs::dup2(fd, target);
        self.saved.push((target, saved));
        Ok(())
    }
}

impl Drop for FdSwap {
    fn drop(&mut self) {
        for &(target, saved) in self.saved.iter().rev() {
            let _ = fs::dup2(saved, target);
            let _ = fs::close(saved);
        }
    }
}

/// Open the pipeline's redirection files: (stdin, stdout). Close-on-spawn,
/// so only the command they're meant for gets them (as fd 0 or 1).
fn open_redirections(p: &Pipeline) -> Result<(Option<Fd>, Option<Fd>), String> {
    let stdin = match p.stdin {
        Some(path) => Some(fs::open(path, flags::READ_ONLY | flags::CLOSE_ON_SPAWN).map_err(|e| format!("{}: {}", path, e))?),
        None => None,
    };
    let stdout = match p.stdout {
        Some((path, append)) => {
            let mode = flags::WRITE_ONLY | flags::CREATE | flags::CLOSE_ON_SPAWN | if append { flags::APPEND } else { flags::TRUNCATE };
            match fs::open(path, mode) {
                Ok(fd) => Some(fd),
                Err(e) => {
                    if let Some(fd) = stdin {
                        let _ = fs::close(fd);
                    }
                    return Err(format!("{}: {}", path, e));
                }
            }
        }
        None => None,
    };
    Ok((stdin, stdout))
}

fn run(p: &Pipeline, history: &[String]) {
    let (stdin, stdout) = match open_redirections(p) {
        Ok(fds) => fds,
        Err(why) => {
            println!("{}", why);
            return;
        }
    };
    if p.commands.len() == 1 && is_builtin(p.commands[0][0]) {
        let mut swap = FdSwap::new();
        let ok = stdin.map_or(Ok(()), |fd| swap.point(ulib::STDIN, fd)).and(stdout.map_or(Ok(()), |fd| swap.point(ulib::STDOUT, fd)));
        if ok.is_ok() {
            builtin(&p.commands[0], history);
        }
        drop(swap);
    } else if let Some(name) = p.commands.iter().map(|c| c[0]).find(|&w| is_builtin(w)) {
        println!("{}: a builtin can't be part of a pipeline", name);
    } else {
        run_programs(p, stdin, stdout);
    }
    for fd in [stdin, stdout].into_iter().flatten() {
        let _ = fs::close(fd);
    }
}

fn is_builtin(word: &str) -> bool {
    matches!(word, "help" | "cd" | "pwd" | "mem" | "uptime" | "history" | "clear" | "init")
}

fn builtin(words: &[&str], history: &[String]) {
    let args = &words[1..];
    match words[0] {
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
        "history" => {
            for (i, line) in history.iter().enumerate() {
                println!("{:>4}  {}", i + 1, line);
            }
        }
        "init" => println!("init is already running"),
        other => println!("{}: not a builtin", other),
    }
}

/// Start every command of the pipeline, connected by pipes, in one new
/// process group; then wait for them unless it runs in the background.
fn run_programs(p: &Pipeline, stdin: Option<Fd>, stdout: Option<Fd>) {
    let mut pids: Vec<Pid> = Vec::new();
    let mut input = stdin; // what the next command reads from
    let mut owned_input = false; // whether `input` is a pipe end we must close
    let last = p.commands.len() - 1;
    for (i, words) in p.commands.iter().enumerate() {
        let (next_input, output) = if i < last {
            match fs::pipe(flags::CLOSE_ON_SPAWN) {
                Ok((r, w)) => (Some(r), Some(w)),
                Err(e) => {
                    println!("pipe: {}", e);
                    break;
                }
            }
        } else {
            (None, stdout)
        };
        let mut swap = FdSwap::new();
        let mut ok = true;
        if let Some(fd) = input {
            ok &= swap.point(ulib::STDIN, fd).is_ok();
        }
        if let Some(fd) = output {
            ok &= swap.point(ulib::STDOUT, fd).is_ok();
        }
        let spawned = if ok { ulib::spawn(words[0], &words[1..]) } else { Err(Errno::EMFILE) };
        drop(swap);
        // The child has its own copies; ours would keep the pipes open.
        if owned_input {
            let _ = fs::close(input.unwrap());
        }
        if i < last {
            let _ = fs::close(output.unwrap());
        }
        input = next_input;
        owned_input = next_input.is_some();
        match spawned {
            Ok(pid) => {
                let _ = ulib::setpgid(pid, pids.first().copied().unwrap_or(pid));
                pids.push(pid);
            }
            Err(Errno::ENOENT) => println!("{}: command not found (try 'help' or 'ls /bin')", words[0]),
            Err(e) => println!("{}: could not start: {}", words[0], e),
        }
    }
    if owned_input {
        let _ = fs::close(input.unwrap());
    }
    let Some(&group) = pids.first() else { return };
    if p.background {
        println!("[{}] started in background", group);
        return;
    }
    ulib::set_foreground(group);
    let mut last_status = None;
    for &pid in &pids {
        last_status = ulib::wait(pid).ok();
    }
    ulib::set_foreground(0);
    match last_status {
        Some(ExitStatus::Exited(0)) | None => {}
        Some(ExitStatus::Killed(sig)) if sig == ulib::signal::SIGINT => println!("^C"),
        Some(status) => println!("[{}] {}", pids[pids.len() - 1], status),
    }
}
