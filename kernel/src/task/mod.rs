//! Tasks and the preemptive round-robin scheduler.
//!
//! Every task (kernel thread or user process) has its own kernel stack.
//! Switching tasks = saving callee-saved registers on the old kernel stack,
//! swapping stack pointers, and popping them from the new one.
//!
//! Locking model: the scheduler state is only touched with interrupts
//! disabled. That is sufficient on one CPU; SMP will need a real lock here.

pub mod elf;

use crate::arch::{cpu, gdt, percpu};
use crate::mm::{kstack::KernelStack, vmm, vmm::AddressSpace};
use crate::sync::{IrqGuard, Racy};
use alloc::{boxed::Box, collections::BTreeMap, collections::VecDeque, string::String, vec::Vec};
use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

pub type Pid = u64;

const TIME_SLICE_MS: u32 = 10;
pub const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_f000;
const USER_STACK_SIZE: u64 = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Ready,
    Running,
    Blocked,
    Sleeping,
    Zombie,
}

pub struct Task {
    pub pid: Pid,
    pub name: String,
    pub state: State,
    pub parent: Option<Pid>,
    exit_code: i64,
    rsp: u64,
    kstack: Option<KernelStack>,
    space: Option<AddressSpace>,
    wake_at: u64,
    user_entry: u64,
    user_stack: u64,
}

struct Scheduler {
    tasks: BTreeMap<Pid, Box<Task>>,
    ready: VecDeque<Pid>,
    sleeping: Vec<Pid>,
    zombies: Vec<Pid>, // exited, resources not yet freed
    current: Pid,
    next_pid: Pid,
    slice: u32,
}

static SCHED: Racy<Option<Scheduler>> = Racy::new(None);
static TICKS: AtomicU64 = AtomicU64::new(0);
static CHILD_EXITED: WaitQueue = WaitQueue::new();

fn sched() -> &'static mut Scheduler {
    debug_assert!(!cpu::interrupts_enabled());
    unsafe { (*SCHED.get()).as_mut().expect("scheduler not initialised") }
}

// ------------------------------------------------------------ context switch

extern "C" {
    fn switch_context(old_rsp: *mut u64, new_rsp: u64);
    fn task_trampoline();
    fn enter_user(rip: u64, rsp: u64) -> !;
}

global_asm!(
    r#"
.section .text
.global switch_context
switch_context:
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15
    mov [rdi], rsp
    mov rsp, rsi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

// First code a new task runs: r12 = entry function, r13 = argument.
.global task_trampoline
task_trampoline:
    and rsp, -16
    mov rdi, r13
    mov rsi, r12
    call task_start
    ud2

// Drop to ring 3 at rip = rdi with stack rsp = rsi.
.global enter_user
enter_user:
    cli
    swapgs
    push 0x1b          // SS  = user data
    push rsi           // RSP
    push 0x202         // RFLAGS: interrupts on
    push 0x23          // CS  = user code
    push rdi           // RIP
    xor eax, eax
    xor ebx, ebx
    xor ecx, ecx
    xor edx, edx
    xor esi, esi
    xor edi, edi
    xor ebp, ebp
    xor r8d, r8d
    xor r9d, r9d
    xor r10d, r10d
    xor r11d, r11d
    xor r12d, r12d
    xor r13d, r13d
    xor r14d, r14d
    xor r15d, r15d
    iretq
"#
);

#[no_mangle]
extern "C" fn task_start(arg: u64, entry: u64) -> ! {
    cpu::sti();
    let f: fn(u64) = unsafe { core::mem::transmute(entry as usize) };
    f(arg);
    exit_current(0);
}

/// Lay out a fresh kernel stack so that `switch_context` "returns" into
/// `task_trampoline` with the entry point and argument in r12/r13.
fn prepare_stack(top: u64, entry: fn(u64), arg: u64) -> u64 {
    let rsp = top - 8 * 8;
    let frame = rsp as *mut u64;
    unsafe {
        frame.add(0).write(0); // r15
        frame.add(1).write(0); // r14
        frame.add(2).write(arg); // r13
        frame.add(3).write(entry as usize as u64); // r12
        frame.add(4).write(0); // rbx
        frame.add(5).write(0); // rbp
        frame.add(6).write(task_trampoline as *const () as u64); // return address
        frame.add(7).write(0);
    }
    rsp
}

// ---------------------------------------------------------------- lifecycle

pub fn init() {
    let idle = Box::new(Task {
        pid: 0,
        name: String::from("idle"),
        state: State::Running,
        parent: None,
        exit_code: 0,
        rsp: 0,
        kstack: None, // runs on the boot stack
        space: None,
        wake_at: 0,
        user_entry: 0,
        user_stack: 0,
    });
    let mut tasks = BTreeMap::new();
    tasks.insert(0, idle);
    unsafe {
        *SCHED.get() = Some(Scheduler {
            tasks,
            ready: VecDeque::new(),
            sleeping: Vec::new(),
            zombies: Vec::new(),
            current: 0,
            next_pid: 1,
            slice: TIME_SLICE_MS,
        });
    }
}

fn add_task(name: &str, parent: Option<Pid>, space: Option<AddressSpace>, entry: fn(u64), arg: u64) -> Option<Pid> {
    let kstack = KernelStack::new()?;
    let rsp = prepare_stack(kstack.top(), entry, arg);
    let _irq = IrqGuard::new();
    let s = sched();
    let pid = s.next_pid;
    s.next_pid += 1;
    s.tasks.insert(
        pid,
        Box::new(Task {
            pid,
            name: String::from(name),
            state: State::Ready,
            parent,
            exit_code: 0,
            rsp,
            kstack: Some(kstack),
            space,
            wake_at: 0,
            user_entry: 0,
            user_stack: 0,
        }),
    );
    Some(pid)
}

fn make_ready(pid: Pid) {
    let _irq = IrqGuard::new();
    sched().ready.push_back(pid);
}

/// Start a kernel thread running `entry(arg)`.
#[allow(dead_code)]
pub fn spawn_kernel(name: &str, entry: fn(u64), arg: u64) -> Option<Pid> {
    let pid = add_task(name, None, None, entry, arg)?;
    make_ready(pid);
    Some(pid)
}

/// Load an ELF executable into a new address space and start it in ring 3.
pub fn spawn_user(name: &str, image: &[u8], parent: Option<Pid>) -> Result<Pid, &'static str> {
    let space = AddressSpace::new().ok_or("out of memory")?;
    let entry = elf::load(&space, image)?;
    space
        .map_anonymous(
            USER_STACK_TOP - USER_STACK_SIZE,
            USER_STACK_SIZE,
            vmm::flags::WRITABLE | vmm::flags::NO_EXECUTE,
        )
        .map_err(|_| "out of memory")?;

    let pid = add_task(name, parent, Some(space), user_task_entry, 0).ok_or("out of memory")?;
    {
        let _irq = IrqGuard::new();
        let t = sched().tasks.get_mut(&pid).unwrap();
        t.user_entry = entry;
        t.user_stack = USER_STACK_TOP - 8; // as if `_start` had been called
    }
    make_ready(pid);
    Ok(pid)
}

fn user_task_entry(_: u64) {
    let (rip, rsp) = {
        let _irq = IrqGuard::new();
        let s = sched();
        let t = &s.tasks[&s.current];
        (t.user_entry, t.user_stack)
    };
    unsafe { enter_user(rip, rsp) };
}

/// Terminate the running task. Its memory is freed by the scheduler once
/// we have switched away from its stack.
pub fn exit_current(code: i64) -> ! {
    cpu::cli();
    let s = sched();
    let pid = s.current;
    assert!(pid != 0, "idle task tried to exit");
    // Orphan our children. A child still running, or exited but not yet
    // reaped, is removed by `reap` once it has no parent. A child that was
    // already reaped is only waiting for us to collect its exit code, which
    // will now never happen, so drop its entry here.
    let zombies = &s.zombies;
    s.tasks.retain(|_, t| {
        if t.parent != Some(pid) {
            return true;
        }
        if t.state == State::Zombie && !zombies.contains(&t.pid) {
            return false;
        }
        t.parent = None;
        true
    });
    let t = s.tasks.get_mut(&pid).unwrap();
    t.state = State::Zombie;
    t.exit_code = code;
    s.zombies.push(pid);
    wake_all(&CHILD_EXITED);
    schedule();
    unreachable!("zombie task was scheduled");
}

/// Wait for a child process to exit and return its exit code.
pub fn wait(pid: Pid) -> Option<i64> {
    loop {
        let _irq = IrqGuard::new();
        let s = sched();
        let me = s.current;
        let t = s.tasks.get_mut(&pid)?;
        if t.parent != Some(me) {
            return None;
        }
        if t.state == State::Zombie {
            let code = t.exit_code;
            t.parent = None; // the reaper removes it once its resources are freed
            if !s.zombies.contains(&pid) {
                s.tasks.remove(&pid);
            }
            return Some(code);
        }
        block_on(&CHILD_EXITED);
    }
}

/// Collect any one exited child without blocking: Some((pid, exit code)).
pub fn try_wait_any() -> Option<(Pid, i64)> {
    let _irq = IrqGuard::new();
    let s = sched();
    let me = s.current;
    let pid = s
        .tasks
        .values()
        .find(|t| t.parent == Some(me) && t.state == State::Zombie)
        .map(|t| t.pid)?;
    let t = s.tasks.get_mut(&pid).unwrap();
    let code = t.exit_code;
    t.parent = None;
    if !s.zombies.contains(&pid) {
        s.tasks.remove(&pid);
    }
    Some((pid, code))
}

/// Free kernel stacks and address spaces of exited tasks (never the current one).
fn reap(s: &mut Scheduler) {
    let current = s.current;
    let mut i = 0;
    while i < s.zombies.len() {
        let pid = s.zombies[i];
        if pid == current {
            i += 1;
            continue;
        }
        s.zombies.swap_remove(i);
        let parent = {
            let t = s.tasks.get_mut(&pid).unwrap();
            t.kstack = None; // unmaps and frees the kernel stack
            t.space = None; // frees every page the process owned
            t.parent
        };
        let orphan = parent.map_or(true, |p| !s_has(&s.tasks, p));
        if orphan {
            s.tasks.remove(&pid);
        }
    }
}

fn s_has(tasks: &BTreeMap<Pid, Box<Task>>, pid: Pid) -> bool {
    tasks.get(&pid).map_or(false, |t| t.state != State::Zombie)
}

// ---------------------------------------------------------------- scheduling

/// Pick the next task and switch to it. Must be called with interrupts off.
pub fn schedule() {
    let s = sched();
    reap(s);

    let prev = s.current;
    let prev_running = s.tasks[&prev].state == State::Running;

    let next = loop {
        match s.ready.pop_front() {
            Some(pid) => {
                if s.tasks.get(&pid).map_or(false, |t| t.state == State::Ready) {
                    break pid;
                }
            }
            None => break if prev_running && prev != 0 { prev } else { 0 },
        }
    };

    if next == prev {
        s.tasks.get_mut(&prev).unwrap().state = State::Running;
        s.slice = TIME_SLICE_MS;
        return;
    }

    if prev_running {
        s.tasks.get_mut(&prev).unwrap().state = State::Ready;
        if prev != 0 {
            s.ready.push_back(prev);
        }
    }

    let next_task = s.tasks.get_mut(&next).unwrap();
    next_task.state = State::Running;
    if let Some(stack) = &next_task.kstack {
        gdt::set_kernel_stack(stack.top());
        percpu::set_kernel_stack(stack.top());
    }
    let cr3 = next_task.space.as_ref().map_or(vmm::kernel_pml4(), |a| a.pml4);
    if cpu::read_cr3() & !0xfff != cr3 {
        unsafe { cpu::write_cr3(cr3) };
    }
    let next_rsp = next_task.rsp;
    let prev_rsp: *mut u64 = &mut s.tasks.get_mut(&prev).unwrap().rsp;
    s.current = next;
    s.slice = TIME_SLICE_MS;

    unsafe { switch_context(prev_rsp, next_rsp) };
}

/// Called from the timer interrupt, 1000 times per second.
pub fn timer_tick() {
    let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    let s = sched();

    let mut i = 0;
    while i < s.sleeping.len() {
        let pid = s.sleeping[i];
        match s.tasks.get_mut(&pid) {
            Some(t) if t.state == State::Sleeping && t.wake_at <= now => {
                t.state = State::Ready;
                s.ready.push_back(pid);
                s.sleeping.swap_remove(i);
            }
            Some(t) if t.state == State::Sleeping => i += 1,
            _ => {
                s.sleeping.swap_remove(i);
            }
        }
    }

    s.slice = s.slice.saturating_sub(1);
    if (s.slice == 0 || s.current == 0) && !s.ready.is_empty() {
        schedule();
    }
}

pub fn yield_now() {
    let _irq = IrqGuard::new();
    schedule();
}

pub fn sleep_ms(ms: u64) {
    let _irq = IrqGuard::new();
    let s = sched();
    let pid = s.current;
    let t = s.tasks.get_mut(&pid).unwrap();
    t.wake_at = uptime_ms().saturating_add(ms.max(1));
    t.state = State::Sleeping;
    s.sleeping.push(pid);
    schedule();
}

pub fn uptime_ms() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

pub fn current_pid() -> Pid {
    let _irq = IrqGuard::new();
    sched().current
}

/// (pid, parent, state, name) for every task.
pub fn list() -> Vec<(Pid, Option<Pid>, State, String)> {
    let _irq = IrqGuard::new();
    sched().tasks.values().map(|t| (t.pid, t.parent, t.state, t.name.clone())).collect()
}

/// The idle loop: runs whenever nothing else is ready.
pub fn idle() -> ! {
    loop {
        cpu::sti_hlt();
    }
}

// -------------------------------------------------------------- wait queues

/// A list of tasks sleeping until some event happens.
pub struct WaitQueue {
    waiters: Racy<Vec<Pid>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self { waiters: Racy::new(Vec::new()) }
    }
}

/// Put the current task to sleep on `queue`. Call with interrupts disabled,
/// right after checking the condition you are waiting for.
pub fn block_on(queue: &WaitQueue) {
    let _irq = IrqGuard::new();
    let s = sched();
    let pid = s.current;
    unsafe { (*queue.waiters.get()).push(pid) };
    s.tasks.get_mut(&pid).unwrap().state = State::Blocked;
    schedule();
}

/// Wake every task sleeping on `queue`.
pub fn wake_all(queue: &WaitQueue) {
    let _irq = IrqGuard::new();
    let s = sched();
    let waiters = unsafe { &mut *queue.waiters.get() };
    for pid in waiters.drain(..) {
        if let Some(t) = s.tasks.get_mut(&pid) {
            if t.state == State::Blocked {
                t.state = State::Ready;
                s.ready.push_back(pid);
            }
        }
    }
}
