//! **Tickless idle on real hardware: fewer wakeups, the same schedule.**
//!
//! `kairos power idle` measured the prize over the conformance corpus and
//! `kairos power diff` demonstrated that suppressing the tick heartbeat does
//! not move the schedule -- but both read *stored traces*. The kernel's own
//! tickless tests then proved the mechanism against a `SleepyPort` that
//! returns a number. Nothing so far has reprogrammed a timer.
//!
//! This cell does. It runs the same scenario twice -- once with
//! `Config::USE_TICKLESS_IDLE` false, once with it true -- on a Cortex-M3
//! whose SysTick is genuinely stopped, reloaded for a long interval, and
//! waited on with `wfi`.
//!
//! # The three numbers
//!
//! | number | what it is | what it must do |
//! |---|---|---|
//! | **wakeups** | SysTick exceptions actually taken, counted in the handler | **collapse** |
//! | **order digest** | FNV-1a over the projected events: kind and task, no clock | **not move** |
//! | **logical ticks** | what the kernel thinks the time is when the run ends | **stay in a named band** |
//!
//! Energy is the thing a customer cares about and QEMU cannot measure it.
//! Wakeups can be counted exactly, they are what energy is proportional to
//! on a part whose idle current is dominated by leaving `wfi`, and they are
//! deterministic -- so they are the right proxy here and the honest one to
//! quote. The *energy* claim belongs to a board and a shunt; this cell makes
//! the mechanism claim, which is the one that can be proved without one.
//!
//! # Why a digest and not a trace
//!
//! A raw trace cannot gate this: `TASK_INCREMENT_TICK` is 15.2% of the
//! conformance corpus by line and suppressing it is the whole point, so the
//! two arms MUST differ. [`rusty_rtos_core::trace::Scheduling`] is the
//! projection that strips exactly the three suppressible events and passes
//! everything else -- every switch, every list move, every queue and timer
//! operation, with its tick stamp. Hashing that gives one `u64` per arm, and
//! equality of those two `u64`s is the claim.
//!
//! # ... and why the digest drops the tick stamp, which the simulator keeps
//!
//! `kairos power diff` compares whole projected LINES, tick stamp included,
//! and gets 18/18 invariant. **That gate does not transfer to silicon, and
//! this cell is how we found out.** In the oracle traces time is
//! critical-section exits, so a tick stamp is a count of work done and is
//! invariant by construction. On a Cortex-M3 a tick stamp is a reading of a
//! free-running counter, so it also records how long the *work* took -- and
//! the two arms do not take the same time, because one of them spends the
//! idle windows in `wfi` instead of servicing 400 interrupts.
//!
//! The measurement that settled it: editing this file twice -- once to add a
//! second hash, once to add a pinned constant and two checks -- moved the
//! logical tick count each time, in BOTH arms, and moved the timed digest
//! with it. Across those runs the control read 402, 401 and 400 ticks and
//! the tickless arm read 400, 400 and 401, with 0 or 1 wakeups. The order
//! digest never moved at all.
//!
//! That is the whole argument in one sentence: **a quantity the instrument's
//! own cost can move is not a schedule.** The tick stamp is such a quantity
//! on silicon and is not one in the simulator, which is why the same
//! projection gates one and not the other.
//!
//! So the **order** digest is the claim: every projected event's kind, and
//! for the events that name a task, which task. A switch to the wrong task
//! changes it; a lost wake changes it; a run that is a microsecond slower
//! does not. That is the poison test `rusty_rtos_core`'s projection tests
//! run, reproduced here on an ARMv7-M.
//!
//! The order digest alone would miss one real failure, though: a wake that
//! arrives in the right ORDER but N ticks LATE -- which is exactly what a
//! port that oversleeps produces. So the logical tick count is gated too,
//! against a band this scenario's arithmetic fixes. Order says nothing was
//! lost or reordered; the tick band says nothing was late.
//!
//! # Running it
//!
//! ```text
//! cargo run --release                        # the plain arm
//! cargo run --release --features tickless    # the suppressing arm
//! ```
//!
//! Each arm gates itself -- see `finish` -- and prints its digest. The
//! cross-arm claim is that the two digests are equal, which is one `diff`.
//!
//! # What this cell does NOT claim
//!
//! * **Not an energy number.** QEMU has no power model. Wakeups are a proxy,
//!   named as one.
//! * **Not a policy.** The sleep length is `expected_idle_time` less nothing
//!   at all -- the fixed policy FreeRTOS ships. Choosing a margin, or fitting
//!   one, is later work and is not covered by any differential.
//! * **Not the ESP32-S3.** The Xtensa port has no tick-driven kernel cell
//!   yet, and light sleep there reports nothing about how long it lasted, so
//!   the port must measure elapsed time itself. Different mechanism, same
//!   two numbers.
//!
//! # Unsafe
//!
//! As the sibling cells: the kernel is a `static` reached from task context
//! and from `PendSV`. `with_kernel` takes it with interrupts masked, and
//! `PendSV` is the lowest-priority exception, so no task runs while it holds
//! it.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use cortex_m_rt::{entry, exception};
use cortex_m_semihosting::{debug, hprintln};
use panic_semihosting as _;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::handle::TaskHandle;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Scheduling, Trace};
use rusty_rtos_kernel_core::{Kernel, Stall, items_for, lists_for};
use rusty_rtos_port_cortex_m::{
    CURRENT_SP_SLOT, CortexMPort, init_stack, set_scheduler, start_first_task, start_tick,
};

/// Whether this build suppresses ticks. The ONLY difference between the two
/// arms, and it is read in exactly one place that matters.
const TICKLESS: bool = cfg!(feature = "tickless");

/// Four priorities: idle at 0, the worker at 2, the timer daemon at 3.
#[derive(Debug, Clone, Copy, Default)]
pub struct CellConfig;

impl Config for CellConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = 4;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 3;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 2;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;

    const USE_TICKLESS_IDLE: bool = TICKLESS;
    /// `configEXPECTED_IDLE_TIME_BEFORE_SLEEP`. Two ticks is the default and
    /// the floor `kairos power idle --min-sleep` uses, so the cell and the
    /// probe agree about what counts as a sleepable window.
    const EXPECTED_IDLE_TIME_BEFORE_SLEEP: u64 = 2;
}

// -------------------------------------------------------------- the digest --

/// FNV-1a over the schedule, which is what both arms must agree about.
///
/// It is deliberately NOT a trace: a trace of the tickless arm is *supposed*
/// to be shorter. This folds the projection, so the heartbeat is already
/// gone before a byte reaches it.
#[derive(Debug)]
struct Digest {
    /// The order the schedule happened in: every event's kind and task, and
    /// nothing about when.
    order: u64,
    /// The same, with each event's tick stamp folded in as well.
    timed: u64,
    events: u64,
    switches: u64,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

impl Digest {
    const fn new() -> Self {
        Self {
            order: FNV_OFFSET,
            timed: FNV_OFFSET,
            events: 0,
            switches: 0,
        }
    }

    /// Fold into BOTH digests: this byte is part of the schedule itself.
    fn fold_byte(&mut self, byte: u8) {
        self.order = (self.order ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
        self.timed = (self.timed ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
    }

    /// Fold into the timed digest ONLY: this byte is a clock reading.
    fn fold_when(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.timed = (self.timed ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
        }
    }

    fn fold_u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.fold_byte(byte);
        }
    }

    fn fold_str(&mut self, text: &str) {
        for byte in text.as_bytes() {
            self.fold_byte(*byte);
        }
    }
}

impl Trace for Digest {
    /// The digest hashes the event's KIND, not the task's name, so the kernel
    /// need not look one up. That also keeps it independent of
    /// `MAX_TASK_NAME_LEN` and of how names are stored.
    const WANTS_NAMES: bool = false;

    fn event(&mut self, tick: u64, event: Event<'_>) {
        self.events = self.events.saturating_add(1);
        self.fold_when(tick);
        self.fold_str(event.name());
        // WHICH task, for the events where the schedule is the answer to
        // that question. A switch at the right tick to the wrong task is
        // exactly the failure this cell has to be able to see.
        let who = match event {
            Event::TaskSwitchedIn { task, .. } => {
                self.switches = self.switches.saturating_add(1);
                u64::from(task.index())
            }
            Event::TaskSwitchedOut { task, .. }
            | Event::MovedTaskToReadyState { task, .. }
            | Event::TaskDelay { task, .. }
            | Event::TaskSuspend { task, .. }
            | Event::TaskResume { task, .. } => u64::from(task.index()),
            // Not a task-naming event; fold a value no index can take, so
            // the absence is itself hashed.
            _ => u64::MAX,
        };
        self.fold_u64(who);
    }
}

/// The sink the kernel is given: the projection, over the digest.
type Tr = Scheduling<Digest>;

// -------------------------------------------------------------- the kernel --

const TASKS: usize = 4;
/// The timer daemon's command queue, and one spare.
const QUEUES: usize = 2;
const SLOTS: usize = 16;
const TIMERS: usize = 1;
const GROUPS: usize = 1;

type K = Kernel<
    CellConfig,
    CortexMPort,
    Tr,
    NoTickHook,
    TASKS,
    { items_for(TASKS, TIMERS) },
    { lists_for(CellConfig::MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    1,
    8,
    TIMERS,
    GROUPS,
>;

struct KernelCell(UnsafeCell<Option<K>>);
// SAFETY: every access goes through `with_kernel`, which masks interrupts,
// or through `PendSV`, which is the lowest-priority exception and therefore
// runs with no task on the CPU. There is one core.
unsafe impl Sync for KernelCell {}
static KERNEL: KernelCell = KernelCell(UnsafeCell::new(None));

/// Borrow the kernel with interrupts masked.
///
/// **This is also what makes the tickless sleep legal.** The port's
/// `suppress_ticks_and_sleep` reprograms SysTick and executes `wfi`, and it
/// requires PRIMASK to be held so the exception it waits for is never TAKEN
/// -- only pended, so the port can account for it and then clear it. That is
/// the C port's idiom, and the mask it needs is the one this function
/// already takes for a quite different reason.
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
static mut STACK_WORK: [usize; STACK_WORDS] = [0; STACK_WORDS];
static mut STACK_IDLE: [usize; STACK_WORDS] = [0; STACK_WORDS];
static mut STACK_TMR: [usize; STACK_WORDS] = [0; STACK_WORDS];

/// One saved stack pointer per task slot, indexed by `TaskHandle::index`.
static SLOTS_SP: [AtomicUsize; TASKS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

// ------------------------------------------------------------- the numbers --

/// How long the worker sleeps each lap. Long enough that the window is worth
/// suppressing, and short enough that 24 bits of SysTick LOAD hold it: at
/// 20,001 cycles a tick, one programming buys 838 of them.
const DELAY_TICKS: u64 = 20;
/// How many laps the run lasts. `ROUNDS * DELAY_TICKS` is the tick count,
/// and almost all of it is idle.
const ROUNDS: u32 = 20;
/// The SysTick reload, as the sibling preempt cell uses.
const TICK_RELOAD: u32 = 20_000;

/// **The pinned schedule.** Both arms must produce this, and the fact that
/// there is ONE constant for two arms is the cross-arm claim.
///
/// Measured 2026-09-19 on qemu-system-arm 11.1.0, `-cpu cortex-m3 -machine
/// mps2-an385`, release profile. It is a digest of the `Scheduling`
/// projection with the tick stamps left out -- see this file's header for
/// why they are left out, which is the finding this cell exists to have
/// made.
///
/// **Re-pinning.** A kernel or scheduler change that legitimately moves this
/// scenario's schedule moves this number, and both arms move together. Run
/// both, check they agree with each other, and write the new value here. If
/// they do NOT agree with each other there is nothing to pin: the change
/// broke tickless, and that is what this cell is for.
const PINNED_ORDER: u64 = 0x94f2_29d7_a5bd_8f77;

/// **The instrument.** SysTick exceptions actually TAKEN, counted in the
/// handler itself.
///
/// Note it is not `Kernel::tick_count`, and the difference is the whole
/// point: the kernel counts LOGICAL ticks, including the ones
/// `Kernel::step_tick` winds forward without any interrupt happening. This
/// counts the times the core actually left `wfi` for the timer. The two are
/// equal in the plain arm by construction, and the tickless arm is trying to
/// make them differ.
static WAKEUPS: AtomicU32 = AtomicU32::new(0);
static LAPS: AtomicU32 = AtomicU32::new(0);

static PORT: CortexMPort = CortexMPort::new();

/// `PendSV` asks the kernel who is next, and points the port at that task's
/// saved-SP slot.
extern "C" fn pick_next() {
    let next = with_kernel_in_pendsv(|k| {
        k.switch_context();
        k.current()
    });
    if let Some(handle) = next {
        let i = usize::from(handle.index());
        if let Some(slot) = SLOTS_SP.get(i) {
            CURRENT_SP_SLOT.store(core::ptr::from_ref(slot) as usize, Ordering::Relaxed);
        }
    }
}

/// The worker: sleep, wake, count, repeat. Its delay is the only thing
/// keeping the system busy, so `next_unblock_time` is always its wake time
/// and `expected_idle_time` always has something real to report.
extern "C" fn task_work(_: usize) -> ! {
    loop {
        let _ = with_kernel(|k| k.delay(DELAY_TICKS));
        rusty_rtos_port_cortex_m::pend_switch();
        if LAPS.fetch_add(1, Ordering::Relaxed).saturating_add(1) >= ROUNDS {
            finish();
        }
    }
}

/// The idle task, and it is **identical in both arms**.
///
/// `Kernel::idle_suppress_ticks` is `prvIdleTask`'s `configUSE_TICKLESS_IDLE`
/// block. It returns immediately when the const is false, so the plain arm
/// spins here exactly as the sibling cells' idle tasks do, and the
/// suppressing arm sleeps. Nothing else in this function knows which arm it
/// is in -- which is what makes the digest comparison mean something.
///
/// It deliberately does NOT pend a switch of its own. `resume_all` inside
/// `idle_suppress_ticks` pends one when unwinding the pended tick woke
/// somebody, which is the C's `taskYIELD_IF_USING_PREEMPTION`; an
/// unconditional pend here would put idle-to-idle switch events in one arm's
/// digest and not the other's, and the comparison would then fail for a
/// reason that has nothing to do with tickless.
extern "C" fn task_idle(_: usize) -> ! {
    loop {
        let _ = with_kernel(Kernel::idle_suppress_ticks);
    }
}

/// The timer daemon sleeps through the run, as in the sibling cells: a
/// highest-priority task that spun would starve everything below it, and
/// software timers are not what this cell claims.
extern "C" fn task_timer(_: usize) -> ! {
    loop {
        let _ = with_kernel(|k| k.delay(u64::from(ROUNDS) * DELAY_TICKS * 4));
        rusty_rtos_port_cortex_m::pend_switch();
    }
}

#[exception]
fn SysTick() {
    // The instrument, and it is the first thing in the handler: every entry
    // to this function is one wakeup the part paid for.
    WAKEUPS.fetch_add(1, Ordering::Relaxed);
    let want = with_kernel_in_pendsv(|k| k.increment_tick()).unwrap_or(false);
    rusty_rtos_port_cortex_m::tick(&PORT, want);
}

fn finish() -> ! {
    let ticks = with_kernel(|k| k.tick_count()).unwrap_or(0);
    let (stalls, why) = with_kernel(|k| (k.stalls(), k.first_stall())).unwrap_or((0, Stall::None));
    let (order, timed, events, switches) = with_kernel(|k| {
        let d = k.trace().inner();
        (d.order, d.timed, d.events, d.switches)
    })
    .unwrap_or((0, 0, 0, 0));
    let wakeups = u64::from(WAKEUPS.load(Ordering::SeqCst));
    let laps = LAPS.load(Ordering::SeqCst);

    hprintln!();
    hprintln!(
        "arm                   {}",
        if TICKLESS { "TICKLESS" } else { "plain" }
    );
    hprintln!("laps                  {}   (want {})", laps, ROUNDS);
    hprintln!("logical ticks         {}", ticks);
    hprintln!("SysTick wakeups       {}", wakeups);
    hprintln!("scheduler stalls      {}   first: {:?}", stalls, why);
    hprintln!("projected events      {}   switches {}", events, switches);
    hprintln!("SCHEDULE DIGEST       {:016x}   (order: who ran, in what order)", order);
    hprintln!("  ...with tick stamps  {:016x}   (diagnostic -- see the header)", timed);

    let mut failed = 0u32;
    let mut check = |ok: bool, what: &str| {
        if ok {
            hprintln!("      ok    {}", what);
        } else {
            failed = failed.saturating_add(1);
            hprintln!("      FAIL  {}", what);
        }
    };
    check(
        laps >= ROUNDS,
        "the worker completed every lap it was asked for",
    );
    check(
        stalls == 0,
        "the scheduler never failed to choose a task (Law 3)",
    );
    check(switches > 0, "the kernel chose a task at least once");
    check(
        events > 0,
        "the projection saw events -- the digest is not of nothing",
    );
    // THE ORDER CLAIM, and it is pinned rather than merely printed so that a
    // single run gates. One constant serves BOTH arms: that is the cross-arm
    // claim, carried by a number rather than by a promise to run a diff.
    //
    // A kernel change that legitimately moves this scenario's schedule
    // re-pins it -- in both arms at once, and if the two arms then disagree
    // there is no value to pin and the change broke tickless.
    check(
        order == PINNED_ORDER,
        "the schedule is the one this cell pinned, event for event and task for task",
    );

    // THE TIMING CLAIM. The order digest cannot see a wake that arrives in
    // the right sequence but late, which is precisely what an oversleeping
    // port produces -- so bound the clock as well.
    //
    // The floor is arithmetic: `ROUNDS` laps of `DELAY_TICKS` cannot take
    // less. The ceiling is the floor plus a slop for the sub-tick execution
    // drift the paragraph in this file's header measured: a lap whose work
    // crosses a tick boundary pushes its own wake one tick out, and over
    // twenty laps the control arm does that once or twice. Four is that,
    // doubled, and it is nowhere near the tens of ticks an oversleep costs.
    let floor: u64 = u64::from(ROUNDS) * DELAY_TICKS;
    const SLOP: u64 = 4;
    check(
        ticks >= floor && ticks <= floor.saturating_add(SLOP),
        "the clock landed in the band the scenario's arithmetic fixes -- nothing overslept",
    );

    if TICKLESS {
        // THE CLAIM, and it is unfakeable: a logical tick that cost no
        // wakeup can only have come from `step_tick` winding the clock
        // forward over a suppressed interval. Nothing else in the system can
        // advance the kernel's tick without a SysTick exception.
        check(
            wakeups < ticks,
            "ticks passed that cost NO wakeup -- the port really suppressed them",
        );
        // And the prize is worth having. 20-tick windows should cost about
        // one wakeup each; needing more than a quarter would mean the
        // suppression fires but barely.
        check(
            wakeups.saturating_mul(4) < ticks,
            "and it suppressed most of them, not a token few",
        );
    } else {
        // The control. Every tick this arm counted was an interrupt it took,
        // so the tickless arm's number has something to be smaller than.
        check(
            wakeups >= ticks,
            "the control paid one wakeup for every tick, as an untouched SysTick must",
        );
    }

    hprintln!();
    if failed == 0 {
        hprintln!("RESULT: PASS -- compare SCHEDULE DIGEST against the other arm;");
        hprintln!("        equal digests are the claim, the wakeup counts are the prize.");
        debug::exit(debug::EXIT_SUCCESS);
    } else {
        hprintln!("RESULT: FAIL -- {} check(s) failed", failed);
        debug::exit(debug::EXIT_FAILURE);
    }
    loop {
        core::hint::spin_loop();
    }
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
    hprintln!("=== tickless idle on an ARMv7-M: fewer wakeups, the same schedule ===");
    hprintln!(
        "arm: {}",
        if TICKLESS {
            "TICKLESS (USE_TICKLESS_IDLE = true)"
        } else {
            "plain control (USE_TICKLESS_IDLE = false)"
        }
    );

    let mut kernel = match K::new(CortexMPort::new(), Scheduling::new(Digest::new())) {
        Ok(k) => k,
        Err(e) => {
            hprintln!("kernel refused the geometry: {:?}", e);
            debug::exit(debug::EXIT_FAILURE);
            loop {
                core::hint::spin_loop();
            }
        }
    };

    let work = kernel.create_task("work", 2).expect("work");
    let started = kernel.start_scheduler().expect("start");

    // SAFETY: `addr_of_mut!` so no reference to a `static mut` is created.
    let top_work = core::ptr::addr_of_mut!(STACK_WORK)
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
    // will choose it, the port will load a zero stack pointer, and the core
    // will lock up.
    let armed = arm_task(work, top_work, task_work)
        && arm_task(started.idle, top_idle, task_idle)
        && arm_task(started.timer, top_tmr, task_timer);
    if !armed {
        hprintln!("a task handle fell outside the slot table");
        debug::exit(debug::EXIT_FAILURE);
    }

    let first = usize::from(kernel.current().index());
    if let Some(slot) = SLOTS_SP.get(first) {
        CURRENT_SP_SLOT.store(core::ptr::from_ref(slot) as usize, Ordering::SeqCst);
    }

    // SAFETY: nothing else holds the kernel yet; interrupts are still masked
    // from reset until `start_first_task` enables them.
    cortex_m::interrupt::free(|_| unsafe {
        *KERNEL.0.get() = Some(kernel);
    });

    set_scheduler(pick_next);
    // This ALSO tells the port how long a tick is: `suppress_ticks_and_sleep`
    // reads the reload back rather than being told it a second time.
    start_tick(TICK_RELOAD);

    hprintln!(
        "worker delays {} ticks a lap, {} laps -- so the box is idle almost all of it",
        DELAY_TICKS,
        ROUNDS
    );
    hprintln!("starting the first task ({})...", first);
    // SAFETY: every task has a stack from `init_stack`, a scheduler is
    // installed, and CURRENT_SP_SLOT names the current task's slot.
    unsafe { start_first_task() }
}
