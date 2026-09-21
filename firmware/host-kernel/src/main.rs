//! **The Kairos kernel scheduling real tasks on real OS stacks.**
//!
//! This is `mps2-an385-qemu-kernel`'s experiment, moved off the chip: the
//! same fixed-priority preemptive scheduler, the same three assertions,
//! driving OS threads instead of a Cortex-M3. It is the host port's kill
//! test, and it is the thing that had to exist before an unmodified C demo
//! file could run on a laptop — a C task blocks, so a C task needs a
//! stack, and until `rusty_rtos_port-host` there was nowhere on a host to
//! put one.
//!
//! "firmware" is this repo's word for a CELL: one project, one target, one
//! claim. The target here happens to be the machine you are reading this
//! on.
//!
//! # The three joints, and where they went
//!
//! | joint | on the M3 | here |
//! |---|---|---|
//! | who runs next | `PendSV` -> `Kernel::switch_context` -> `CURRENT_SP_SLOT` | [`pick_next`] -> `Kernel::switch_context` -> `CURRENT` |
//! | when to switch | `SysTick` | a tick thread |
//! | how a task blocks | `Kernel::delay` then `pend_switch`, resuming on the line after | the same call, and the same line |
//!
//! # What is being asserted
//!
//! A round robin would make both tasks run. Only a *priority* scheduler
//! makes these numbers come out:
//!
//! * The high-priority task delays [`DELAY_TICKS`] each lap, so it must
//!   complete about `ticks / DELAY_TICKS` laps and no more. Too many means
//!   `delay` did not block it; too few means it was never woken.
//! * The low-priority task must run — but only while the high one is
//!   blocked, which is the whole content of "fixed priority".
//! * Neither may run while it is on the delayed list, which the high task
//!   checks by reading the tick on both sides of its own delay.
//!
//! And one the M3 cell could not ask, because on a chip it is free:
//!
//! * **Exactly one task is on the CPU at a time.** Here every task is a
//!   real OS thread that the OS would happily run in parallel, so the
//!   single run permit is doing real work and a bug in it is a data race
//!   rather than a wrong number. Each task stamps its own index into a
//!   shared cell on entry and checks it on exit; a mismatch means two
//!   threads were inside at once.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Trace};
use rusty_rtos_kernel_core::{list_slots_for, lists_for, Kernel};
use rusty_rtos_port_host::{
    init_task, pend_switch, set_scheduler, start_first_task, HostPort, Ticker, CURRENT, PREEMPTIVE,
};

/// Four priorities: idle at 0, the timer daemon, and two application
/// tasks. The same shape as the M3 cell, so the two are comparable.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostConfig;

impl Config for HostConfig {
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

const TASKS: usize = 7;
const QUEUES: usize = 2;
const SLOTS: usize = 8;
const TIMERS: usize = 1;
const GROUPS: usize = 1;

type K = Kernel<
    HostConfig,
    HostPort,
    NoTrace,
    NoTickHook,
    TASKS,
    { list_slots_for(TASKS, TIMERS, lists_for(HostConfig::MAX_PRIORITIES, QUEUES, GROUPS)) },
    { lists_for(HostConfig::MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    1,
    8,
    TIMERS,
    GROUPS,
>;

/// The kernel, reached from every task thread and from the tick thread.
///
/// An `UnsafeCell` rather than a `Mutex` on purpose: the port's critical
/// section is the lock, exactly as on a chip, and wrapping a second one
/// round it would hide which of the two is actually excluding whom.
struct KernelCell(std::cell::UnsafeCell<Option<K>>);
// SAFETY: every access goes through `with_kernel`, which takes the port's
// critical section -- the same lock the tick thread takes before it
// touches anything, and the same lock that makes freezing a task thread
// safe. One task runs at a time by construction of the run permit.
unsafe impl Sync for KernelCell {}
static KERNEL: KernelCell = KernelCell(std::cell::UnsafeCell::new(None));

static PORT: HostPort = HostPort::new();

/// Borrow the kernel inside the port's critical section.
fn with_kernel<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    use rusty_rtos_core::port::Port as _;
    PORT.enter_critical();
    // SAFETY: the critical section is held, so the tick thread is not
    // inside the kernel and no other task thread holds the run permit.
    let slot = unsafe { &mut *KERNEL.0.get() };
    let out = slot.as_mut().map(f);
    PORT.exit_critical();
    out
}

/// As `with_kernel`, for a caller that already holds the critical section.
fn with_kernel_locked<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    // SAFETY: the caller holds the port's critical section; see
    // `with_kernel`.
    let slot = unsafe { &mut *KERNEL.0.get() };
    slot.as_mut().map(f)
}

static LAPS_HI: AtomicU32 = AtomicU32::new(0);
static LAPS_LO: AtomicU32 = AtomicU32::new(0);
/// A SECOND task at the low task's priority, which also never yields.
///
/// Fixed-priority scheduling says nothing about what happens between two
/// READY tasks of the SAME priority — that is a round-robin the kernel
/// owes them, and `taskSELECT_HIGHEST_PRIORITY_TASK` in the C provides it
/// by advancing an index through the ready list rather than always taking
/// the head.
///
/// It is worth a task of its own because the failure is silent and looks
/// like somebody else's bug: a demo whose tasks sit at priority 0 beside a
/// busier demo's priority-0 tasks simply stops, and the obvious reading is
/// starvation-by-design rather than a scheduler that never rotates.
static LAPS_LO2: AtomicU32 = AtomicU32::new(0);
static RAN_WHILE_DELAYED: AtomicU32 = AtomicU32::new(0);
static SWITCHES: AtomicU32 = AtomicU32::new(0);
static OVERRAN: AtomicU32 = AtomicU32::new(0);

/// How many times a task found somebody else's index in [`ON_CPU`] while
/// it believed it was the one running. Any value but zero is two threads
/// on the CPU at once.
static OVERLAPS: AtomicU32 = AtomicU32::new(0);
/// Who last claimed the CPU.
static ON_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
/// How many ticks the tick thread delivered.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// How long the high-priority task sleeps each lap.
const DELAY_TICKS: u64 = 10;
/// How long the run lasts.
const RUN_TICKS: u64 = 400;
/// When the low-priority task gives up waiting. Generous, and only reached
/// if the scheduling joint is broken.
const DEADLINE_TICKS: u64 = RUN_TICKS * 3;

/// The port asks who is next; the kernel answers. This is the whole joint.
extern "C" fn pick_next() {
    let next = with_kernel_locked(|k| {
        k.switch_context();
        k.current()
    });
    if let Some(handle) = next {
        let to = usize::from(handle.index());
        let n = SWITCHES.fetch_add(1, Ordering::Relaxed);
        // `KAIROS_HOST_TRACE=1` prints the first switches with the
        // round-robin cursor beside them. A task that is ready and never
        // chosen is either not in the list the scheduler looks at or the
        // cursor is not moving, and this is what tells them apart.
        if n < 24 && option_env!("KAIROS_HOST_TRACE").is_some() {
            eprintln!(
                "  switch {n}: -> slot {to}  cursor@1={}  list@1={:?}",
                with_kernel_locked(|k| k.ready_cursor(1)).unwrap_or(0),
                {
                    let mut items = [0u16; 6];
                    let used = with_kernel_locked(|k| k.ready_items(1, &mut items)).unwrap_or(0);
                    items.get(..used).map(<[u16]>::to_vec)
                }
            );
        }
        CURRENT.store(to, Ordering::SeqCst);
    }
}

/// One tick of simulated time, from the tick thread, with the critical
/// section already held and the running task frozen.
extern "C" fn on_tick() -> bool {
    TICKS.fetch_add(1, Ordering::Relaxed);
    with_kernel_locked(|k| k.increment_tick()).unwrap_or(false)
}

/// Claim the CPU, and say so if somebody else still had it.
///
/// On a chip this check is unnecessary: there is one CPU and a task cannot
/// be on it twice. Here every task is an OS thread the operating system
/// would run in parallel given the chance, so "one at a time" is a
/// PROPERTY OF THIS PORT rather than of the hardware, and it is worth
/// asserting directly instead of inferring it from a lap count.
fn claim(me: usize) {
    let was = ON_CPU.swap(me, Ordering::SeqCst);
    if was != me && was != usize::MAX && CURRENT.load(Ordering::SeqCst) != me {
        OVERLAPS.fetch_add(1, Ordering::Relaxed);
    }
}

/// The high-priority task: work, then sleep, and check it really slept.
extern "C" fn task_hi(index: usize) -> ! {
    loop {
        claim(index);
        LAPS_HI.fetch_add(1, Ordering::Relaxed);
        let before = with_kernel(|k| k.tick_count()).unwrap_or(0);
        // Ask the kernel to block us, then give the CPU up. We resume on
        // the line after, with this thread's stack exactly as we left it
        // -- which is the entire point of a port that has stacks.
        let _ = with_kernel(|k| k.delay(DELAY_TICKS));
        pend_switch();
        claim(index);
        let after = with_kernel(|k| k.tick_count()).unwrap_or(0);
        // A task that was really on the delayed list cannot come back
        // before its time. Anything less means `delay` did not block.
        if after.wrapping_sub(before) < DELAY_TICKS {
            RAN_WHILE_DELAYED.fetch_add(1, Ordering::Relaxed);
        }
        if after >= RUN_TICKS {
            finish();
        }
    }
}

/// The low-priority task, and it **never yields and never blocks**.
///
/// This is deliberate and it is the cell's sharpest assertion. It is the
/// shape of `integer.c` and `flop.c` in the standard demos: with
/// `configUSE_PREEMPTION` set they do arithmetic forever and rely on being
/// taken off the CPU. A cooperative host port runs this task and nothing
/// else, ever -- the high task never gets another lap, the deadline
/// passes, and the failure looks like a broken `delay` rather than a
/// missing port feature.
///
/// So the only thing that can make the high task's lap count come out is
/// the tick thread freezing this one where it stands.
extern "C" fn task_lo2(index: usize) -> ! {
    loop {
        claim(index);
        LAPS_LO2.fetch_add(1, Ordering::Relaxed);
        if with_kernel(|k| k.tick_count()).unwrap_or(0) >= DEADLINE_TICKS {
            OVERRAN.fetch_add(1, Ordering::Relaxed);
            finish();
        }
    }
}

extern "C" fn task_lo(index: usize) -> ! {
    loop {
        claim(index);
        LAPS_LO.fetch_add(1, Ordering::Relaxed);
        let now = with_kernel(|k| k.tick_count()).unwrap_or(0);
        if now >= DEADLINE_TICKS {
            OVERRAN.fetch_add(1, Ordering::Relaxed);
            finish();
        }
    }
}

/// The idle task. Priority 0, so it runs only when nothing else is ready.
extern "C" fn task_idle(_: usize) -> ! {
    loop {
        pend_switch();
    }
}

/// The timer daemon. Nothing in this cell creates a timer, so it parks.
extern "C" fn task_timer(_: usize) -> ! {
    loop {
        let _ = with_kernel(|k| k.delay(1));
        pend_switch();
    }
}

/// Report, then leave. Called from whichever task reaches the end first.
fn finish() -> ! {
    let laps_hi = LAPS_HI.load(Ordering::Relaxed);
    let laps_lo = LAPS_LO.load(Ordering::Relaxed);
    let ticks = with_kernel(|k| k.tick_count()).unwrap_or(0);
    let expected = ticks / DELAY_TICKS;

    println!();
    println!("=== the Kairos kernel on OS threads, with real stacks ===");
    println!("port    rusty_rtos_port-host {}", rusty_rtos_port_host::VERSION);
    println!("preempt {PREEMPTIVE} (this platform can take a task off the CPU)");
    println!();
    println!("ticks           {ticks}");
    println!("switches        {switches}", switches = SWITCHES.load(Ordering::Relaxed));
    println!("high-task laps  {laps_hi}  (expected about {expected})");
    println!("low-task laps   {laps_lo}");
    println!();

    let (t, wanted, unfrozen, switched) = rusty_rtos_port_host::tick_stats();
    println!("ticks {t}: {wanted} wanted a switch, {unfrozen} could not freeze, {switched} switched");
    println!("ready-list lengths by priority:");
    for prio in 0..HostConfig::MAX_PRIORITIES {
        println!(
            "    priority {prio}: {} task(s)",
            with_kernel(|k| k.ready_len(prio)).and_then(|r| r.ok()).unwrap_or(0)
        );
    }
    println!("what the kernel thinks of each task:");
    for i in 0..TASKS {
        let Some(Some(h)) = with_kernel(|k| k.task_at(i)) else {
            continue;
        };
        let name = with_kernel(|k| k.name_of(h)).and_then(|r| r.ok());
        let state = with_kernel(|k| k.state_of(h)).and_then(|r| r.ok());
        let prio = with_kernel(|k| k.priority_of(Some(h))).and_then(|r| r.ok());
        println!(
            "    slot {i}: {:<6} priority {prio:?}  {state:?}",
            name.as_ref().map_or("?", rusty_rtos_kernel_core::name::Name::as_str)
        );
    }
    println!("permits handed out, per slot:");
    for i in 0..TASKS {
        let g = rusty_rtos_port_host::grants_to(i);
        if g > 0 {
            println!("    slot {i}: {g}");
        }
    }
    let mut failed = 0u32;
    let mut check = |ok: bool, what: &str| {
        if ok {
            println!("      ok    {what}");
        } else {
            failed += 1;
            println!("      FAIL  {what}");
        }
    };

    check(
        OVERRAN.load(Ordering::Relaxed) == 0,
        "the run ended on the high task's own schedule, not on the deadline",
    );
    check(
        SWITCHES.load(Ordering::Relaxed) > 0,
        "the kernel chose a task at least once",
    );
    check(laps_hi > 0, "the high-priority task ran");
    check(
        laps_lo > 0,
        "the low-priority task ran, so it was not starved out",
    );
    check(
        RAN_WHILE_DELAYED.load(Ordering::Relaxed) == 0,
        "the high-priority task never resumed before its delay expired",
    );
    // Allow one lap of slack at each end: the first lap happens before the
    // first tick and the last is cut short by `finish`.
    let close = laps_hi as u64 <= expected + 2 && laps_hi as u64 + 2 >= expected;
    check(
        close,
        "its lap count is ticks/delay, so `delay` blocked it for exactly as long as it asked",
    );
    let laps_lo2 = LAPS_LO2.load(Ordering::Relaxed);
    println!("second low-task laps  {laps_lo2}");
    check(
        laps_lo2 > 0,
        "the SECOND task at the low task's priority ran at all -- equal priorities rotate",
    );
    // Neither may be starved by the other. A tenth of its twin's turns is
    // generous; a scheduler that never rotates gives one of them ZERO.
    let (a, b) = (u64::from(laps_lo), u64::from(laps_lo2));
    check(
        a.min(b) * 10 >= a.max(b),
        "the two equal-priority tasks got comparable shares, not one starving the other",
    );
    check(
        OVERLAPS.load(Ordering::Relaxed) == 0,
        "exactly one task was on the CPU at a time, though each is a real OS thread",
    );
    check(
        close && laps_lo > 0,
        "a task that NEVER yields was taken off the CPU -- the port preempts",
    );

    println!();
    if failed == 0 {
        println!("RESULT: PASS -- the Kairos scheduler drove real tasks on OS threads");
        std::process::exit(0);
    }
    println!("RESULT: FAIL -- {failed} check(s) failed");
    std::process::exit(1);
}

fn main() {
    let kernel = match K::new(HostPort::new(), NoTrace) {
        Ok(k) => k,
        Err(e) => {
            println!("kernel refused the geometry: {e:?}");
            std::process::exit(1);
        }
    };
    // SAFETY: nothing else has been started, so no thread can be inside.
    unsafe {
        *KERNEL.0.get() = Some(kernel);
    }

    let Some(Ok(hi)) = with_kernel(|k| k.create_task("hi", 2)) else {
        println!("could not create the high task");
        std::process::exit(1);
    };
    let Some(Ok(lo)) = with_kernel(|k| k.create_task("lo", 1)) else {
        println!("could not create the low task");
        std::process::exit(1);
    };
    let Some(Ok(lo2)) = with_kernel(|k| k.create_task("lo2", 1)) else {
        println!("could not create the second low task");
        std::process::exit(1);
    };
    let Some(Ok(started)) = with_kernel(|k| k.start_scheduler()) else {
        println!("the kernel refused to start");
        std::process::exit(1);
    };

    init_task(usize::from(hi.index()), task_hi);
    init_task(usize::from(lo.index()), task_lo);
    init_task(usize::from(lo2.index()), task_lo2);
    init_task(usize::from(started.idle.index()), task_idle);
    init_task(usize::from(started.timer.index()), task_timer);

    let Some(first) = with_kernel(|k| k.current()) else {
        println!("the kernel named no first task");
        std::process::exit(1);
    };

    set_scheduler(pick_next);
    // One millisecond, because `TICK_RATE_HZ` is 1000 and the two must
    // mean the same duration -- the same agreement the C ABI cell's
    // `FreeRTOSConfig.h` and its Rust `Config` have to keep.
    Ticker::new(Duration::from_millis(1), on_tick).spawn();

    println!("starting the first task ({})...", usize::from(first.index()));
    start_first_task(usize::from(first.index()));
}
