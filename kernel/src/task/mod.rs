//! Tasks and the preemptive round-robin scheduler, shared by all CPUs.
//!
//! Every task (kernel thread or user process) has its own kernel stack.
//! Switching tasks = saving callee-saved registers on the old kernel stack,
//! swapping stack pointers, and popping them from the new one. Each CPU has
//! an idle task (pid 0, not in the task table) that runs when nothing else
//! is ready; ready tasks wait in one queue that every CPU takes work from.
//!
//! Locking: all scheduler state lives in `SCHED`, and the lock is held
//! across `switch_context`. The task switching away takes it; the task that
//! resumes releases it, by dropping the guard it held when it last switched
//! away, or in `task_start` if it has never run. So "mark the old task
//! ready/blocked" and "stop running on its stack" are a single step as far as
//! other CPUs can tell: none of them can pick a task up while its registers
//! are still being saved.

mod elf;
pub mod exec;

use crate::arch::fpu::FpuState;
use crate::arch::{cpu, percpu};
use crate::fs::FdTable;
use crate::mm::uspace::{FaultResult, UserSpace};
use crate::mm::{kstack::KernelStack, vmm};
use crate::sync::{SpinLock, SpinLockGuard};
use alloc::{boxed::Box, collections::BTreeMap, collections::VecDeque, string::String, sync::Arc, vec::Vec};
use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

pub type Pid = u64;

const TIME_SLICE_MS: u32 = 10;

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
    /// CPU the task is running on, while `state == Running`.
    pub cpu: Option<usize>,
    exit_code: i64,
    rsp: u64,
    kstack: Option<KernelStack>,
    /// What the user process owns (None for kernel threads). Locked
    /// separately from the scheduler.
    process: Option<Arc<Process>>,
    /// Page tables to load for this task: the space's, or the kernel's.
    pml4: u64,
    /// Saved x87/SSE registers, for user tasks (the kernel doesn't use them).
    fpu: Option<Box<FpuState>>,
    wake_at: u64,
    user_entry: u64,
    user_stack: u64,
}

impl Task {
    fn new(pid: Pid, name: &str, parent: Option<Pid>) -> Task {
        Task {
            pid,
            name: String::from(name),
            state: State::Ready,
            parent,
            cpu: None,
            exit_code: 0,
            rsp: 0,
            kstack: None,
            process: None,
            pml4: vmm::kernel_pml4(),
            fpu: None,
            wake_at: 0,
            user_entry: 0,
            user_stack: 0,
        }
    }
}

/// A user process's resources, as opposed to its scheduling state.
pub struct Process {
    pub space: SpinLock<UserSpace>,
    pub files: SpinLock<FdTable>,
    /// Canonical absolute path of the working directory.
    pub cwd: SpinLock<String>,
}

/// What a new process starts with besides its program.
pub struct Inherit {
    pub files: FdTable,
    pub cwd: String,
}

/// What the scheduler knows about one CPU.
struct Cpu {
    /// The task running there: in `tasks`, or this CPU's `idle`. Both are
    /// boxed, so the pointer stays valid while the task exists, and a task
    /// is only removed from `tasks` once it can't be current anywhere.
    current: *mut Task,
    idle: Box<Task>,
    slice: u32,
}

struct Scheduler {
    tasks: BTreeMap<Pid, Box<Task>>,
    ready: VecDeque<Pid>,
    sleeping: Vec<Pid>,
    zombies: Vec<Pid>, // exited, resources not yet freed
    cpus: Vec<Cpu>,    // indexed by percpu::index()
    next_pid: Pid,
}

// The raw `current` pointers are only used with the lock held.
unsafe impl Send for Scheduler {}

static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler {
    tasks: BTreeMap::new(),
    ready: VecDeque::new(),
    sleeping: Vec::new(),
    zombies: Vec::new(),
    cpus: Vec::new(),
    next_pid: 1,
});
static TICKS: AtomicU64 = AtomicU64::new(0);
static CHILD_EXITED: WaitQueue = WaitQueue::new();

type SchedGuard = SpinLockGuard<'static, Scheduler>;

impl Scheduler {
    fn this_cpu(&mut self) -> &mut Cpu {
        let index = percpu::index();
        &mut self.cpus[index]
    }

    fn current(&mut self) -> &mut Task {
        unsafe { &mut *self.this_cpu().current }
    }

    fn make_ready(&mut self, pid: Pid) {
        if let Some(t) = self.tasks.get_mut(&pid) {
            t.state = State::Ready;
            self.ready.push_back(pid);
        }
    }
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
    // We were switched to with the scheduler lock held, and have no guard
    // of our own to drop (see the module comment).
    unsafe { SCHED.force_unlock() };
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

/// Register the calling CPU with the scheduler; its boot thread becomes its
/// idle task. CPUs must call this in index order (the boot CPU first).
pub fn add_cpu() {
    let mut s = SCHED.lock();
    assert_eq!(s.cpus.len(), percpu::index(), "CPUs registered out of order");
    let mut idle = Box::new(Task::new(0, "idle", None)); // runs on the stack it was started on
    idle.state = State::Running;
    idle.cpu = Some(percpu::index());
    let current: *mut Task = &mut *idle;
    s.cpus.push(Cpu { current, idle, slice: TIME_SLICE_MS });
}

fn add_task(name: &str, parent: Option<Pid>, process: Option<Process>, entry: fn(u64), arg: u64) -> Option<Pid> {
    let kstack = KernelStack::new()?;
    let rsp = prepare_stack(kstack.top(), entry, arg);
    let mut s = SCHED.lock();
    let pid = s.next_pid;
    s.next_pid += 1;
    let mut task = Box::new(Task::new(pid, name, parent));
    task.rsp = rsp;
    task.kstack = Some(kstack);
    if let Some(process) = process {
        task.pml4 = process.space.lock().pml4();
        task.process = Some(Arc::new(process));
        task.fpu = Some(FpuState::new());
    }
    s.tasks.insert(pid, task);
    Some(pid)
}

/// Start a kernel thread running `entry(arg)`.
#[allow(dead_code)]
pub fn spawn_kernel(name: &str, entry: fn(u64), arg: u64) -> Option<Pid> {
    let pid = add_task(name, None, None, entry, arg)?;
    SCHED.lock().make_ready(pid);
    Some(pid)
}

/// Load an ELF executable into a new address space and start it in ring 3
/// with the given arguments (`args[0]` is conventionally the program name).
fn spawn_user(
    name: &str,
    image: &[u8],
    args: &[&[u8]],
    parent: Option<Pid>,
    inherit: Inherit,
) -> Result<Pid, exec::LoadError> {
    let image = exec::load(image, args)?;
    let process = Process {
        space: SpinLock::new(image.space),
        files: SpinLock::new(inherit.files),
        cwd: SpinLock::new(inherit.cwd),
    };
    let pid = add_task(name, parent, Some(process), user_task_entry, 0).ok_or(exec::LoadError::NoMemory)?;
    let mut s = SCHED.lock();
    let t = s.tasks.get_mut(&pid).unwrap();
    t.user_entry = image.entry;
    t.user_stack = image.stack;
    s.make_ready(pid);
    Ok(pid)
}

#[derive(Clone, Copy, Debug)]
pub enum SpawnError {
    Fs(crate::fs::FsError),
    Load(exec::LoadError),
}

impl core::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            SpawnError::Fs(e) => write!(f, "{:?}", e),
            SpawnError::Load(e) => write!(f, "{}", e),
        }
    }
}

/// Start the program at `path`, resolved against `inherit.cwd`. A bare name
/// (no '/') means /bin/<name>. The task is named after the last component.
pub fn spawn_program(path: &str, args: &[&[u8]], parent: Option<Pid>, inherit: Inherit) -> Result<Pid, SpawnError> {
    let full = if path.contains('/') { String::from(path) } else { alloc::format!("/bin/{}", path) };
    let inode = crate::fs::lookup(&inherit.cwd, &full).map_err(SpawnError::Fs)?;
    let image = crate::fs::read_file(&inode).map_err(SpawnError::Fs)?;
    let name = full.rsplit('/').next().unwrap_or(&full);
    spawn_user(name, &image, args, parent, inherit).map_err(SpawnError::Load)
}

fn user_task_entry(_: u64) {
    let (rip, rsp) = {
        let mut s = SCHED.lock();
        let t = s.current();
        (t.user_entry, t.user_stack)
    };
    unsafe { enter_user(rip, rsp) };
}

/// Terminate the running task. Its memory is freed by the scheduler once
/// we have switched away from its stack.
pub fn exit_current(code: i64) -> ! {
    // Close our files now, outside the scheduler lock: closing can wake
    // other tasks (the reader at the other end of a pipe, say).
    if let Some(process) = current_process() {
        let files = process.files.lock().take_all();
        drop(files);
    }
    let mut s = SCHED.lock();
    let pid = s.current().pid;
    assert!(pid != 0, "idle task tried to exit");
    if pid == 1 {
        drop(s);
        crate::power::init_exited(code);
    }
    // Orphan our children. A child still running, or exited but not yet
    // reaped, is removed by `reap` once it has no parent. A child that was
    // already reaped is only waiting for us to collect its exit code, which
    // will now never happen, so drop its entry here.
    let s_ref = &mut *s;
    let zombies = &s_ref.zombies;
    s_ref.tasks.retain(|_, t| {
        if t.parent != Some(pid) {
            return true;
        }
        if t.state == State::Zombie && !zombies.contains(&t.pid) {
            return false;
        }
        t.parent = None;
        true
    });
    let t = s.current();
    t.state = State::Zombie;
    t.exit_code = code;
    s.zombies.push(pid);
    CHILD_EXITED.wake_all_locked(&mut s);
    schedule(&mut s);
    unreachable!("zombie task was scheduled");
}

/// Wait for a child process to exit and return its exit code.
pub fn wait(pid: Pid) -> Option<i64> {
    let mut s = SCHED.lock();
    loop {
        let me = s.current().pid;
        let t = s.tasks.get_mut(&pid)?;
        if t.parent != Some(me) {
            return None;
        }
        if t.state == State::Zombie {
            let code = t.exit_code;
            collect(&mut s, pid);
            return Some(code);
        }
        CHILD_EXITED.block_locked(&mut s);
    }
}

/// Collect any one exited child without blocking: Some((pid, exit code)).
pub fn try_wait_any() -> Option<(Pid, i64)> {
    let mut s = SCHED.lock();
    let me = s.current().pid;
    let (pid, code) = s
        .tasks
        .values()
        .find(|t| t.parent == Some(me) && t.state == State::Zombie)
        .map(|t| (t.pid, t.exit_code))?;
    collect(&mut s, pid);
    Some((pid, code))
}

/// The parent has the exit code of zombie `pid`: forget the task, or let the
/// reaper do so if it still owns resources.
fn collect(s: &mut Scheduler, pid: Pid) {
    if s.zombies.contains(&pid) {
        s.tasks.get_mut(&pid).unwrap().parent = None; // reap removes it
    } else {
        s.tasks.remove(&pid);
    }
}

/// Free kernel stacks and address spaces of exited tasks. Any zombie except
/// this CPU's current task is safe: with the lock held, no other CPU can be
/// running on a zombie's stack (a zombie releases the lock only by switching
/// away from it).
fn reap(s: &mut Scheduler) {
    let current = s.this_cpu().current;
    let mut i = 0;
    while i < s.zombies.len() {
        let pid = s.zombies[i];
        let t = s.tasks.get_mut(&pid).unwrap();
        if core::ptr::eq(&**t, current) {
            i += 1;
            continue;
        }
        s.zombies.swap_remove(i);
        t.kstack = None; // back to the stack pool
        t.process = None; // the last reference: frees every page the process owned
        let parent = t.parent;
        let orphan = parent.map_or(true, |p| s.tasks.get(&p).map_or(true, |t| t.state == State::Zombie));
        if orphan {
            s.tasks.remove(&pid);
        }
    }
}

// ---------------------------------------------------------------- scheduling

/// Pick the next task for this CPU and switch to it. Returns when the
/// calling task is scheduled again, with the lock held again.
fn schedule(s: &mut SchedGuard) {
    reap(s);
    let index = percpu::index();
    let prev_ptr = s.cpus[index].current;
    let prev = unsafe { &mut *prev_ptr };
    let prev_running = prev.state == State::Running;
    let prev_idle = core::ptr::eq(prev_ptr, &*s.cpus[index].idle);

    let next_ptr: *mut Task = loop {
        match s.ready.pop_front() {
            Some(pid) => match s.tasks.get_mut(&pid) {
                Some(t) if t.state == State::Ready => break &mut **t,
                _ => {} // stale entry: the task exited or was woken twice
            },
            None if prev_running && !prev_idle => break prev_ptr,
            None => break &mut *s.cpus[index].idle,
        }
    };
    s.cpus[index].slice = TIME_SLICE_MS;
    if next_ptr == prev_ptr {
        prev.state = State::Running;
        return;
    }

    if prev_running {
        prev.state = State::Ready;
        if !prev_idle {
            s.ready.push_back(prev.pid);
        }
    }
    prev.cpu = None;

    let next = unsafe { &mut *next_ptr };
    next.state = State::Running;
    next.cpu = Some(index);
    if let Some(stack) = &next.kstack {
        percpu::set_kernel_stack(stack.top());
    }
    if cpu::read_cr3() & !0xfff != next.pml4 {
        unsafe { cpu::write_cr3(next.pml4) };
    }
    // The kernel never uses these registers, so swapping them here, before
    // the stack switch, is the same as doing it on the way to user mode.
    if let Some(fpu) = prev.fpu.as_mut() {
        fpu.save();
    }
    if let Some(fpu) = next.fpu.as_ref() {
        fpu.restore();
    }
    s.cpus[index].current = next_ptr;
    let (prev_rsp, next_rsp) = (&mut prev.rsp as *mut u64, next.rsp);
    unsafe { switch_context(prev_rsp, next_rsp) };
}

/// Called from the timer interrupt on every CPU, 1000 times per second.
pub fn timer_tick() {
    let mut s = SCHED.lock();
    if percpu::index() == 0 {
        let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
        wake_sleepers(&mut s, now);
    }
    let cpu = s.this_cpu();
    cpu.slice = cpu.slice.saturating_sub(1);
    let want_switch = cpu.slice == 0 || core::ptr::eq(cpu.current, &*cpu.idle);
    if want_switch && !s.ready.is_empty() {
        schedule(&mut s);
    }
}

fn wake_sleepers(s: &mut Scheduler, now: u64) {
    let mut i = 0;
    while i < s.sleeping.len() {
        let pid = s.sleeping[i];
        match s.tasks.get(&pid) {
            Some(t) if t.state == State::Sleeping && t.wake_at > now => i += 1,
            Some(t) if t.state == State::Sleeping => {
                s.sleeping.swap_remove(i);
                s.make_ready(pid);
            }
            _ => {
                s.sleeping.swap_remove(i);
            }
        }
    }
}

pub fn yield_now() {
    schedule(&mut SCHED.lock());
}

pub fn sleep_ms(ms: u64) {
    let mut s = SCHED.lock();
    let t = s.current();
    t.wake_at = uptime_ms().saturating_add(ms.max(1));
    t.state = State::Sleeping;
    let pid = t.pid;
    s.sleeping.push(pid);
    schedule(&mut s);
}

pub fn uptime_ms() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

pub fn current_pid() -> Pid {
    SCHED.lock().current().pid
}

/// The running user process; None for kernel threads. Drop the result
/// before exiting: a reference left on a dead task's stack is never dropped.
pub fn current_process() -> Option<Arc<Process>> {
    SCHED.lock().current().process.clone()
}

/// A page fault from user mode on a page that isn't mapped.
pub fn user_page_fault(addr: u64, write: bool, exec: bool) -> FaultResult {
    match current_process() {
        Some(process) => process.space.lock().handle_fault(addr, write, exec),
        None => FaultResult::Denied,
    }
}

pub struct TaskInfo {
    pub pid: Pid,
    pub parent: Option<Pid>,
    pub state: State,
    pub cpu: Option<usize>,
    pub name: String,
}

/// Every task except the idle tasks.
pub fn list() -> Vec<TaskInfo> {
    let s = SCHED.lock();
    s.tasks
        .values()
        .map(|t| TaskInfo { pid: t.pid, parent: t.parent, state: t.state, cpu: t.cpu, name: t.name.clone() })
        .collect()
}

/// The idle loop: runs whenever nothing else is ready on this CPU.
pub fn idle() -> ! {
    loop {
        cpu::sti_hlt();
    }
}

// -------------------------------------------------------------- wait queues

/// Tasks sleeping until some event happens, used like a condition variable:
/// check the condition with its lock held, then `wait` with that lock's
/// guard. A `wake_all` from another CPU can't slip in between the check and
/// going to sleep, because `wait` releases the lock only once the task is
/// already on the queue.
///
/// Lock order: the caller's lock, then the scheduler, then `waiters`.
pub struct WaitQueue {
    waiters: SpinLock<Vec<Pid>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self { waiters: SpinLock::new(Vec::new()) }
    }

    /// Release `guard`, sleep until woken, then take the lock again. Wakeups
    /// can be spurious: re-check the condition in a loop.
    pub fn wait<'a, T>(&self, guard: SpinLockGuard<'a, T>) -> SpinLockGuard<'a, T> {
        let mut s = SCHED.lock();
        // Queue up *before* releasing the caller's lock: whoever changes the
        // condition next (under that lock) then finds us in `waiters`.
        self.enqueue_current(&mut s);
        let (lock, irq) = guard.release();
        schedule(&mut s);
        drop(s);
        lock.lock_with(irq)
    }

    /// Sleep on the queue when the condition is scheduler state itself.
    fn block_locked(&self, s: &mut SchedGuard) {
        self.enqueue_current(s);
        schedule(s);
    }

    fn enqueue_current(&self, s: &mut Scheduler) {
        let t = s.current();
        t.state = State::Blocked;
        self.waiters.lock().push(t.pid);
    }

    /// Wake every task sleeping on the queue. Don't hold the scheduler lock.
    /// Call it after changing the condition and releasing its lock.
    pub fn wake_all(&self) {
        // Cheap when nobody waits: a waiter queues itself before releasing
        // the condition's lock, so it's either visible here already or will
        // see the new condition itself.
        if self.waiters.lock().is_empty() {
            return;
        }
        self.wake_all_locked(&mut SCHED.lock());
    }

    fn wake_all_locked(&self, s: &mut Scheduler) {
        let waiters = core::mem::take(&mut *self.waiters.lock());
        for pid in waiters {
            if s.tasks.get(&pid).map_or(false, |t| t.state == State::Blocked) {
                s.make_ready(pid);
            }
        }
    }
}
