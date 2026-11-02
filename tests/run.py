#!/usr/bin/env python3
"""Kern test runner (`make test`).

1. selftest: boots the test ISO (pid 1 = `selftest`, `qemu-exit` on the
   kernel command line) on several CPU models and CPU counts. selftest exits with its number
   of failures, which the kernel hands to QEMU's isa-debug-exit device, so
   QEMU's exit status is the verdict: (code << 1) | 1, i.e. 1 means all passed.

2. shell: boots the normal ISO, types commands through the QEMU monitor's
   `sendkey` (the kernel only reads the PS/2 keyboard, not serial) and waits
   for the expected output in the serial log.

usage: run.py KERN_ISO TEST_ISO
"""
import os
import re
import subprocess
import sys
import tempfile
import time

QEMU = ["qemu-system-x86_64", "-M", "q35", "-m", "512M", "-display", "none", "-no-reboot"]
# (CPU model, CPU count). max has SMEP/SMAP/UMIP, qemu64 has none.
CONFIGS = [("qemu64", 1), ("max", 4), ("qemu64", 4)]
BOOT_TIMEOUT = 60
STEP_TIMEOUT = 15

ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def clean(text):
    return ANSI.sub("", text).replace("\r", "")


def read_log(path):
    try:
        with open(path, "rb") as f:
            return clean(f.read().decode("utf-8", "replace"))
    except FileNotFoundError:
        return ""


def machine(cpu, smp):
    """QEMU options for a CPU model and count; several vCPUs run on several
    host threads, so they really run in parallel."""
    return ["-cpu", cpu, "-smp", str(smp)] + (["-accel", "tcg,thread=multi"] if smp > 1 else [])


def run_selftest(iso, cpu, smp, workdir):
    name = f"{cpu} x{smp}"
    log = os.path.join(workdir, f"selftest-{cpu}-{smp}.log")
    cmd = QEMU + machine(cpu, smp) + ["-cdrom", iso, "-serial", f"file:{log}",
                                      "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]
    try:
        status = subprocess.run(cmd, stdin=subprocess.DEVNULL, timeout=BOOT_TIMEOUT).returncode
    except subprocess.TimeoutExpired:
        status = None
    text = read_log(log)
    summary = re.search(r"selftest: (\d+) checks, (\d+) failed", text)
    online = re.search(r"(\d+) CPU\(s\) online", text)
    passed = status == 1 and summary is not None and summary.group(2) == "0"
    if online is None or int(online.group(1)) != smp:
        print(f"  selftest on {name}: expected {smp} CPU(s) online, log says {online and online.group(1)}")
        passed = False
    if passed:
        print(f"  selftest on {name}: {summary.group(1)} checks passed")
    else:
        why = "timed out" if status is None else f"QEMU exit status {status}"
        print(f"  selftest on {name}: FAILED ({why})")
        failures = [l for l in text.splitlines() if l.startswith("not ok") or "PANIC" in l]
        print("\n".join("    " + l for l in failures) or indent(text))
    return passed


# ---------------------------------------------------------------- shell test

KEYS = {" ": "spc", "&": "shift-7", "-": "minus", ".": "dot", "/": "slash", "_": "shift-minus",
        ">": "shift-dot", "<": "shift-comma", "|": "shift-backslash"}
TOKENS = {"<up>": "up", "<down>": "down", "<esc>": "esc", "<bs>": "backspace", "<ctrl-c>": "ctrl-c"}

# (what to type, regex that must newly appear in the output). Typed text is
# followed by Enter unless it ends in a key token. PROMPT matches any prompt,
# for commands whose effect is checked by a later step.
PROMPT = r"kern:\S*> "
# A step that just waits a moment.
PAUSE = "<pause>"
SHELL_STEPS = [
    ("echo hello   world", r"^hello world$"),
    ("hello 2", r"hello from process \d+ \(2/2\)"),
    ("hello x", r"usage: hello \[count\]"),
    ("crash", r"killed by SIGSEGV"),
    ("no-such-thing", r"no-such-thing: command not found"),
    ("<up><up>", r"kern:/> crash$"),        # history recall redraws the line
    ("", r"killed by SIGSEGV"),              # ...and Enter runs it again
    ("history", r"^\s+\d+\s+no-such-thing$"),
    ("hello 1 &", r"\[\d+\] started in background"),
    ("ps", r"PID\s+PPID\s+STATE\s+CPU\s+NAME"),
    ("echo esc<esc>ok", r"^escok$"),         # a lone Esc doesn't eat keys
    ("ls /bin", r"^selftest$"),
    ("echo some text > /tmp/note", PROMPT),
    ("echo more >> /tmp/note", PROMPT),
    ("cat /tmp/note", r"^some text\nmore$"),
    ("cat < /tmp/note", r"^some text\nmore$"),
    ("mkdir /tmp/dir", PROMPT),
    ("cd /tmp", r"kern:/tmp> "),
    ("pwd", r"^/tmp$"),
    ("ls", r"^dir/\nnote$"),
    ("rm note dir", PROMPT),
    ("cat note", r"cat: note: no such file or directory"),
    ("cd nowhere", r"cd: nowhere: no such file or directory"),
    ("echo piped words | cat", r"^piped words$"),
    ("ls /bin | wc -l", r"^15$"),
    ("echo a b c | cat | wc -w", r"^3$"),
    ("date", r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d UTC$"),
    # Ctrl+C interrupts the foreground job (once it has started: a key
    # pressed before that is just input, as on any terminal)...
    ("sleep 100", r"> sleep 100$"),
    (PAUSE, None),
    ("<ctrl-c>", r"^\^C$"),
    ("cat", r"> cat$"),
    (PAUSE, None),
    ("<ctrl-c>", r"^\^C$"),
    ("echo partial<ctrl-c>", r"echo partial\^C$"),  # ...or abandons the line
    ("spin | wc -l", r"> spin \| wc -l$"),
    (PAUSE, None),
    ("<ctrl-c>", r"^\^C$"),                 # every process in the pipeline
    ("ps | wc -l", r"^4$"),                  # header, init, ps, wc: nothing left over
]


def keys_for(text):
    i = 0
    while i < len(text):
        for token, key in TOKENS.items():
            if text.startswith(token, i):
                yield key
                i += len(token)
                break
        else:
            c = text[i]
            yield "shift-" + c.lower() if c.isupper() else KEYS.get(c, c)
            i += 1


def count(pattern, text):
    return len(re.findall(pattern, text, re.M))


def wait_for(log, pattern, at_least, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if count(pattern, read_log(log)) >= at_least:
            return True
        time.sleep(0.2)
    return False


def run_shell(iso, workdir):
    log = os.path.join(workdir, "shell.log")
    cmd = QEMU + machine("max", 2) + ["-cdrom", iso, "-serial", f"file:{log}", "-monitor", "stdio"]
    qemu = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, text=True)

    def monitor(line):
        qemu.stdin.write(line + "\n")
        qemu.stdin.flush()

    ok = True
    try:
        if not wait_for(log, PROMPT, 1, BOOT_TIMEOUT):
            print("  shell: FAILED (no prompt)")
            print(indent(read_log(log)))
            return False
        for typed, expect in SHELL_STEPS:
            if typed == PAUSE:
                time.sleep(1)
                continue
            seen = count(expect, read_log(log))
            for key in keys_for(typed):
                monitor("sendkey " + key)
                time.sleep(0.05)
            if not any(typed.endswith(t) for t in TOKENS):
                monitor("sendkey ret")
            if not wait_for(log, expect, seen + 1, STEP_TIMEOUT):
                print(f"  shell: FAILED at {typed!r}: no {expect!r}")
                print(indent("\n".join(read_log(log).splitlines()[-15:])))
                ok = False
                break
        if ok:
            print(f"  shell: {sum(t != PAUSE for t, _ in SHELL_STEPS)} steps passed")
    finally:
        monitor("quit")
        try:
            qemu.wait(timeout=10)
        except subprocess.TimeoutExpired:
            qemu.kill()
    return ok


def indent(text):
    return "\n".join("    " + l for l in text.splitlines())


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    kern_iso, test_iso = sys.argv[1:]
    with tempfile.TemporaryDirectory(prefix="kern-test-") as workdir:
        results = [run_selftest(test_iso, cpu, smp, workdir) for cpu, smp in CONFIGS]
        results.append(run_shell(kern_iso, workdir))
    if all(results):
        print("all tests passed")
    else:
        sys.exit("some tests FAILED")


if __name__ == "__main__":
    main()
