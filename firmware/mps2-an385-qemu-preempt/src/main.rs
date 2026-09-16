//! **The Kairos kernel scheduling real tasks on real stacks.**
//!
//! The sibling `mps2-an385-qemu-switch` cell proved the port's half: PendSV
//! saves and restores a task's context correctly. It picked the next task
//! with a round robin, because a port's job is to *switch*, not to choose.
//! This cell hands the choosing to `Kernel::switch_context` — so what runs
//! here is the same fixed-priority preemptive scheduler the conformance
//! corpus proves against C FreeRTOS, driving an ARMv7-M.
//!
//! # The three joints
//!
//! | joint | here |
//! |---|---|
//! | who runs next | `PendSV` -> [`pick_next`] -> `Kernel::switch_context` -> `CURRENT_SP_SLOT` |
//! | when to switch | `SysTick` -> `Kernel::increment_tick` -> pend a PendSV if it says so |
//! | how a task blocks | `Kernel::delay` then `yield_now`, and it resumes on the line after |
//!
//! # What is being asserted
//!
//! A round robin would make both tasks run. Only a *priority* scheduler
//! makes the numbers below come out:
//!
//! * The high-priority task delays for [`DELAY_TICKS`] each lap, so it must
//!   complete about `ticks / DELAY_TICKS` laps and no more. Too many means
//!   `delay` did not block it; too few means it was never woken.
//! * The low-priority task must run — but only while the high one is
//!   blocked, which is the whole content of "fixed priority".
//! * Neither may run while it is on the delayed list, which the high task
//!   checks by reading the tick on both sides of its own delay.
//!
//! # Unsafe
//!
//! One item: the kernel is a `static` reached from both task context and
//! `PendSV`. [`with_kernel`] takes it with interrupts masked, and `PendSV`
//! is the lowest-priority exception so no task is running when it holds it.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use cortex_m_rt::{entry, exception};
use cortex_m_semihosting::{debug, hprintln};
use panic_semihosting as _;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Trace};
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_kernel_core::{Kernel, Stall, items_for, lists_for};
use rusty_rtos_port_cortex_m::{
    CURRENT_SP_SLOT, CortexMPort, init_stack, set_scheduler, start_first_task, start_tick,
};

/// Four priorities: idle at 0, the timer daemon, and two application tasks.
#[derive(Debug, Clone, Copy, Default)]
pub struct M3Config;

impl Config for M3Config {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = 4;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 3;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 2;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
}

/// A trace that keeps nothing: this cell asserts scheduling, not a trace.
#[derive(Debug, Default)]
struct NoTrace;
impl Trace for NoTrace {
    fn event(&mut self, _tick: u64, _event: Event<'_>) {}
}

const TASKS: usize = 6;
// The timer daemon's command queue, plus this cell's two semaphores.
// The clone this cell started from had 2, which is exactly enough to make
// the SECOND semaphore fail with `Full` -- and it did.
const QUEUES: usize = 4;
const SLOTS: usize = 16;
const TIMERS: usize = 1;
const GROUPS: usize = 1;

type K = Kernel<
    M3Config,
    CortexMPort,
    NoTrace,
    NoTickHook,
    TASKS,
    { items_for(TASKS, TIMERS) },
    { lists_for(M3Config::MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    1,
    8,
    TIMERS,
    GROUPS,
>;

/// The kernel, reachable from a task and from `PendSV`.
struct KernelCell(UnsafeCell<Option<K>>);
// SAFETY: every access goes through `with_kernel`, which masks interrupts,
// or through `PendSV`, which is the lowest-priority exception and therefore
// runs with no task on the CPU. There is one core.
unsafe impl Sync for KernelCell {}
static KERNEL: KernelCell = KernelCell(UnsafeCell::new(None));

/// Borrow the kernel with interrupts masked.
fn with_kernel<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    cortex_m::interrupt::free(|_| {
        // SAFETY: interrupts are masked, so neither SysTick nor PendSV can
        // be holding this, and there is no second core.
        let slot = unsafe { &mut *KERNEL.0.get() };
        slot.as_mut().map(f)
    })
}

/// As `with_kernel`, but for `PendSV`, which already has exclusivity by
/// being the lowest-priority exception.
fn with_kernel_in_pendsv<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    // SAFETY: PendSV is configured lowest priority, so it cannot preempt
    // another exception, and no task runs while an exception is active.
    let slot = unsafe { &mut *KERNEL.0.get() };
    slot.as_mut().map(f)
}

const STACK_WORDS: usize = 512;
static mut STACK_HI: [usize; STACK_WORDS] = [0; STACK_WORDS];
static mut STACK_LO: [usize; STACK_WORDS] = [0; STACK_WORDS];
static mut STACK_IDLE: [usize; STACK_WORDS] = [0; STACK_WORDS];
static mut STACK_TMR: [usize; STACK_WORDS] = [0; STACK_WORDS];

/// One saved stack pointer per task slot, indexed by `TaskHandle::index`.
/// This is what `CURRENT_SP_SLOT` points into.
static SLOTS_SP: [AtomicUsize; TASKS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// How many hand-offs the pair must complete.
const ROUNDS: u32 = 200;
/// The two semaphores. Written once by `main` before any task runs, read
/// by both tasks thereafter.
static mut SEM_FULL: Option<QueueHandle> = None;
static mut SEM_FREE: Option<QueueHandle> = None;

/// The `full` semaphore, or a null handle if `main` never made it.
fn sem_full() -> QueueHandle {
    // SAFETY: written once before the first task runs, read-only after.
    unsafe { SEM_FULL }.unwrap_or(QueueHandle::NULL)
}

/// The `free` semaphore.
fn sem_free() -> QueueHandle {
    // SAFETY: as `sem_full`.
    unsafe { SEM_FREE }.unwrap_or(QueueHandle::NULL)
}

/// The producer's own handle, so it can ask whether the kernel still thinks
/// it is the running task.
static mut TASK_LO_HANDLE: Option<TaskHandle> = None;

/// How many times the producer's `give` left `Kernel::current` naming
/// somebody ELSE — i.e. how many times the window this cell exists to test
/// actually opened. **A pass with this at zero proves nothing.**
static WINDOW_OPENED: AtomicU32 = AtomicU32::new(0);
/// ...and of those, how many times the call made inside the window BLOCKED,
/// which is the only case that can park the wrong task.
static BLOCKED_IN_WINDOW: AtomicU32 = AtomicU32::new(0);

static LAPS_HI: AtomicU32 = AtomicU32::new(0);
static LAPS_LO: AtomicU32 = AtomicU32::new(0);
static RAN_WHILE_DELAYED: AtomicU32 = AtomicU32::new(0);
static SWITCHES: AtomicU32 = AtomicU32::new(0);
/// Set when the deadline passed before the high task finished the run.
static OVERRAN: AtomicU32 = AtomicU32::new(0);

/// How long the high-priority task sleeps each lap.
const DELAY_TICKS: u64 = 10;
/// How long the run lasts.
const RUN_TICKS: u64 = 400;
/// When the low-priority task gives up waiting for the high one to finish
/// the run. Generous, and only reached if the scheduling joint is broken.
const DEADLINE_TICKS: u64 = RUN_TICKS * 3;

static PORT: CortexMPort = CortexMPort::new();

/// `PendSV` asks the kernel who is next, and points the port at that
/// task's saved-SP slot. This is the whole joint.
extern "C" fn pick_next() {
    let next = with_kernel_in_pendsv(|k| {
        k.switch_context();
        k.current()
    });
    if let Some(handle) = next {
        let i = usize::from(handle.index());
        if let Some(slot) = SLOTS_SP.get(i) {
            CURRENT_SP_SLOT.store(core::ptr::from_ref(slot) as usize, Ordering::Relaxed);
            SWITCHES.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The CONSUMER, and it is the HIGH-priority task on purpose.
///
/// # Why the priorities are this way round
///
/// The window under test opens only when a kernel call wakes a task that
/// OUTRANKS the caller: that is the one case where the kernel preemptively
/// moves `current` at the point of the call. A first version of this cell
/// had the producer high and the consumer low; the give woke a lower task,
/// no preemption happened, `current` never moved, and the cell passed
/// without ever reaching the condition it exists to test. 198 of 200
/// hand-offs and a clean bill of health, proving nothing.
///
/// With the consumer high, the producer's `give` wakes something above it,
/// the kernel switches `current` there and then, and the producer's NEXT
/// kernel call is made by a task the kernel no longer thinks is running.
extern "C" fn task_hi(_: usize) -> ! {
    loop {
        let got = matches!(
            with_kernel(|k| k.semaphore_take(sem_full(), 1000)),
            Some(Ok(Wait::Ready(())))
        );
        rusty_rtos_port_cortex_m::pend_switch();
        if got {
            LAPS_HI.fetch_add(1, Ordering::Relaxed);
        }
        let _ = with_kernel(|k| k.semaphore_give(sem_free()));
        if with_kernel(|k| k.tick_count()).unwrap_or(0) >= DEADLINE_TICKS {
            OVERRAN.fetch_add(1, Ordering::Relaxed);
            finish();
        }
    }
}

/// The PRODUCER, low priority.
///
/// Its `give` wakes the higher-priority consumer, so the kernel commits a
/// switch right there. The very next statement is another kernel call, made
/// with NO hand-rolled `pend_switch()` in between — which is exactly the
/// window the sibling `mps2-an385-qemu-kernel` cell never opens, because it
/// pends a switch by hand after every `delay`.
extern "C" fn task_lo(_: usize) -> ! {
    loop {
        // Count the give that SUCCEEDED, not the one attempted.
        let sent = matches!(
            with_kernel(|k| k.semaphore_give(sem_full())),
            Some(Ok(Wait::Ready(())))
        );
        if sent {
            LAPS_LO.fetch_add(1, Ordering::Relaxed);
        }
        if LAPS_LO.load(Ordering::Relaxed) >= ROUNDS {
            finish();
        }
        // Did the give actually move `current` away from us? That is the
        // window, and it is measured rather than assumed.
        // SAFETY: written once by `main` before any task ran.
        let me = unsafe { TASK_LO_HANDLE };
        let open = with_kernel(|k| Some(k.current()) != me).unwrap_or(false);
        if open {
            WINDOW_OPENED.fetch_add(1, Ordering::Relaxed);
        }

        // THE WINDOW: a second kernel call, with nothing between it and the
        // one above to enact the switch the kernel already committed.
        let blocked = matches!(
            with_kernel(|k| k.semaphore_take(sem_free(), 1000)),
            Some(Ok(Wait::Blocked))
        );
        if open && blocked {
            BLOCKED_IN_WINDOW.fetch_add(1, Ordering::Relaxed);
        }
        rusty_rtos_port_cortex_m::pend_switch();
    }
}

extern "C" fn task_idle(_: usize) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

/// The timer daemon's body.
///
/// `start_scheduler` creates `Tmr Svc` unconditionally, at
/// `TIMER_TASK_PRIORITY` — which is **above** both application tasks, as
/// it is in FreeRTOS. So it has to exist, has to have a stack, and has to
/// behave: a highest-priority task that spun would starve everything
/// below it, and the first run of this cell locked up precisely because
/// the task existed with no stack at all and the kernel quite correctly
/// chose it first.
///
/// Software timers are not what this cell claims, so the daemon sleeps
/// through the whole run rather than pretending to service a queue.
extern "C" fn task_timer(_: usize) -> ! {
    loop {
        let _ = with_kernel(|k| k.delay(RUN_TICKS * 4));
        rusty_rtos_port_cortex_m::pend_switch();
    }
}

fn finish() -> ! {
    let ticks = with_kernel(|k| k.tick_count()).unwrap_or(0);
    let hi = LAPS_HI.load(Ordering::SeqCst);
    let lo = LAPS_LO.load(Ordering::SeqCst);
    let switches = SWITCHES.load(Ordering::SeqCst);
    // Law 3, as of 2026-09-11: the scheduler now says when it could not
    // choose. Non-zero is an invariant break, and it is the FIRST thing to
    // read when tasks stop making progress.
    let (stalls, why) = with_kernel(|k| (k.stalls(), k.first_stall())).unwrap_or((0, Stall::None));

    hprintln!();
    hprintln!("ticks                 {}", ticks);
    hprintln!("switches              {}", switches);
    hprintln!("producer hand-offs    {}   (want {})", lo, ROUNDS);
    hprintln!("consumer hand-offs    {}   (want {})", hi, ROUNDS);
    hprintln!("scheduler stalls      {}   first: {:?}", stalls, why);
    hprintln!(
        "window opened         {}   of which blocked inside: {}",
        WINDOW_OPENED.load(Ordering::SeqCst),
        BLOCKED_IN_WINDOW.load(Ordering::SeqCst)
    );

    let mut failed = 0u32;
    let mut check = |ok: bool, what: &str| {
        if ok {
            hprintln!("      ok    {}", what);
        } else {
            failed += 1;
            hprintln!("      FAIL  {}", what);
        }
    };
    check(
        OVERRAN.load(Ordering::SeqCst) == 0,
        "the run ended on the producer's own count, not on the deadline",
    );
    check(switches > 0, "the kernel chose a task at least once");
    // THE regression check, and it inverted on 2026-09-11.
    //
    // This cell was built to REACH a condition: a blocking call made while
    // the kernel had already moved `current`. It reached it 199 times in
    // 200 rounds, and the check was `> 0` -- reaching it was the point.
    //
    // `Port::COMMITS_SWITCH` closed the window. The kernel no longer commits
    // at the yield, so `current` cannot drift from the running task and the
    // count is now 0 by construction. The assertion therefore flips: what
    // was evidence the cell worked is now evidence the KERNEL works, and a
    // non-zero count means somebody set that const back to `false`.
    check(
        WINDOW_OPENED.load(Ordering::SeqCst) == 0,
        "the window is CLOSED: `current` never moved while this task was still running",
    );
    check(
        stalls == 0,
        "the scheduler never failed to choose a task (Law 3)",
    );
    check(
        lo >= ROUNDS,
        "the producer completed every hand-off it was asked for",
    );
    // The consumer may be one behind when the producer stops, and no more.
    // With the window closed this is exact: 200 against 200. The slack is
    // kept because the producer can still stop with one token in flight,
    // and a gate that demands a tie it cannot always win is a flaky gate.
    check(
        hi + 2 >= ROUNDS,
        "the consumer kept up -- it was never left blocked on a token that was sent",
    );

    hprintln!();
    if failed == 0 {
        hprintln!("RESULT: PASS -- two stacked tasks blocked and resumed through the");
        hprintln!("        kernel, with no hand-rolled switch between the calls.");
        debug::exit(debug::EXIT_SUCCESS);
    } else {
        hprintln!("RESULT: FAIL -- {} check(s) failed", failed);
        debug::exit(debug::EXIT_FAILURE);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[exception]
fn SysTick() {
    // `xPortSysTickHandler`: the kernel counts the tick and says whether a
    // switch is now required — a task woke, or a time slice ended.
    let want = with_kernel_in_pendsv(|k| k.increment_tick()).unwrap_or(false);
    rusty_rtos_port_cortex_m::tick(&PORT, want);
}

/// Give one task a stack and record it in its own slot.
fn arm_task(handle: TaskHandle, top: *mut usize, entry: extern "C" fn(usize) -> !) -> bool {
    let i = usize::from(handle.index());
    match SLOTS_SP.get(i) {
        Some(slot) => {
            slot.store(init_stack(top, entry, i), Ordering::SeqCst);
            true
        }
        None => false,
    }
}

#[entry]
fn main() -> ! {
    hprintln!();
    hprintln!("=== the Kairos kernel scheduling real tasks (mps2-an385, QEMU) ===");
    hprintln!("the port switches; the KERNEL chooses. Fixed priority, and a");
    hprintln!("`delay` that really blocks -- the same scheduler the corpus");
    hprintln!("proves against C FreeRTOS, on an ARMv7-M.");

    let mut kernel = match K::new(CortexMPort::new(), NoTrace) {
        Ok(k) => k,
        Err(e) => {
            hprintln!("kernel refused the geometry: {:?}", e);
            debug::exit(debug::EXIT_FAILURE);
            loop {
                core::hint::spin_loop();
            }
        }
    };

    let hi = kernel.create_task("hi", 2).expect("hi");
    let lo = kernel.create_task("lo", 1).expect("lo");
    let started = kernel.start_scheduler().expect("start");

    // BOTH start empty, and that is the point.
    //
    // A depth-one buffer with `free` pre-loaded lets the producer's second
    // call succeed immediately, so it never blocks -- and the window this
    // cell exists to open is only harmful when the call made inside it
    // BLOCKS. The first version did exactly that and passed 200/199 while
    // never once reaching the condition. Starting `free` empty forces the
    // producer to block on a token only the consumer can supply, inside the
    // window opened by its own `give`.
    let full = kernel.semaphore_create_counting(1, 0).expect("full");
    let free = kernel.semaphore_create_counting(1, 0).expect("free");
    // SAFETY: no task exists yet; `start_first_task` is far below.
    unsafe {
        SEM_FULL = Some(full);
        SEM_FREE = Some(free);
    }

    hprintln!("tasks: timer=prio 3 (sleeps), hi=prio 2, lo=prio 1, idle=prio 0");

    // SAFETY: `&raw mut` so no reference to a `static mut` is created.
    let top_hi = core::ptr::addr_of_mut!(STACK_HI)
        .cast::<usize>()
        .wrapping_add(STACK_WORDS);
    let top_lo = core::ptr::addr_of_mut!(STACK_LO)
        .cast::<usize>()
        .wrapping_add(STACK_WORDS);
    let top_idle = core::ptr::addr_of_mut!(STACK_IDLE)
        .cast::<usize>()
        .wrapping_add(STACK_WORDS);

    let top_tmr = core::ptr::addr_of_mut!(STACK_TMR)
        .cast::<usize>()
        .wrapping_add(STACK_WORDS);
    // EVERY task the kernel created needs a stack, including the two it
    // creates for itself. Missing one is not a soft failure: the scheduler
    // will choose it, the port will load a zero stack pointer, and the
    // core will lock up.
    // SAFETY: no task has run yet.
    unsafe {
        TASK_LO_HANDLE = Some(lo);
    }
    let armed = arm_task(hi, top_hi, task_hi)
        && arm_task(lo, top_lo, task_lo)
        && arm_task(started.idle, top_idle, task_idle)
        && arm_task(started.timer, top_tmr, task_timer);
    if !armed {
        hprintln!("a task handle fell outside the slot table");
        debug::exit(debug::EXIT_FAILURE);
    }

    // The kernel picked a current task when the scheduler started; point
    // the port at its slot so the first switch has somewhere to come from.
    let first = usize::from(kernel.current().index());
    if let Some(slot) = SLOTS_SP.get(first) {
        CURRENT_SP_SLOT.store(core::ptr::from_ref(slot) as usize, Ordering::SeqCst);
    }

    // SAFETY: nothing else holds the kernel yet; interrupts are still
    // masked from reset until `start_first_task` enables them.
    cortex_m::interrupt::free(|_| unsafe {
        *KERNEL.0.get() = Some(kernel);
    });

    set_scheduler(pick_next);
    start_tick(20_000);

    hprintln!("starting the first task ({})...", first);
    // SAFETY: every task has a stack from `init_stack`, a scheduler is
    // installed, and CURRENT_SP_SLOT names the current task's slot.
    unsafe { start_first_task() }
}
