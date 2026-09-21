//! The kernel this cell runs, and the three things the adapter needs from
//! it: a way to reach it, a real clock, and a switch.
//!
//! # Some of this is not called yet, on purpose
//!
//! `install`, `mark_started` and the timer service are reached by the
//! cell's `main` only once a real `esp-radio` can be linked, and it cannot
//! be: the published radio pins `esp-hal ~1.1.0` against this family's
//! `=1.2.1`, and `xtensa-lx-rt` is a `links` crate. Until that is settled
//! `main` is a placeholder, so these read as dead. They are allowed rather
//! than deleted because deleting them would lose the half of the seam that
//! already type-checks -- and the allowance is narrow and says why, so it
//! stops being true the moment the cell is finished.
//!
//! # The switch is where our scheduler meets our port
//!
//! `Kernel::switch_context` decides *who* runs — the same fixed-priority
//! scheduler the conformance corpus proves against C FreeRTOS. The Xtensa
//! port enacts it. They meet in [`Software0`], which asks the kernel to
//! choose and then swaps the trap frame for the chosen task's context.
//!
//! # The clock is NOT sim time
//!
//! Every other Kairos cell on this chip runs `SimPort`, where time is
//! critical-section exits. The radio needs a real microsecond clock —
//! `usleep`, `usleep_until` and `now` are in its trait — so this cell runs
//! [`XtensaPort`] with a hardware tick and reads `esp_hal::time::Instant`
//! for the microsecond figure.

#![allow(dead_code, reason = "main is a placeholder until the radio can link")]

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use esp_hal::time::{Duration, Instant};
use esp_hal::timer::PeriodicTimer;
use esp_hal::timer::systimer::SystemTimer;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::handle::TaskHandle;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Trace};
use rusty_rtos_kernel_core::{list_slots_for, lists_for, Kernel};
use rusty_rtos_port_xtensa::{clear_switch_request, switch_context, Context, XtensaPort};

/// Priorities: idle at 0, the radio's tasks between, the timer service at
/// the top but one.
pub const MAX_PRIORITIES: u8 = 8;

/// How many tasks, including idle, the timer daemon and the radio's own.
pub const MAX_TASKS: usize = 16;

const QUEUES: usize = 48;
const SLOTS: usize = 64;
const TIMERS: usize = 4;
const GROUPS: usize = 2;

/// The configuration this cell runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct RadioConfig;

impl Config for RadioConfig {
    type Tick = Bits32;
    /// 1 kHz, so one tick is 1,000 µs. The radio asks for sub-millisecond
    /// sleeps; the adapter rounds them UP, and that rounding is this
    /// constant's consequence. Raising it costs interrupts.
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = MAX_PRIORITIES;
    const MINIMAL_STACK_SIZE: usize = 512;
    const MAX_TASK_NAME_LEN: usize = 12;
    const TIMER_TASK_PRIORITY: u8 = 1;
    const TIMER_TASK_STACK_DEPTH: usize = 512;
    const TIMER_QUEUE_LENGTH: usize = 8;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
    const MAX_TASKS: usize = MAX_TASKS;
    const MAX_QUEUES: usize = QUEUES;
}

/// A trace that keeps nothing: this cell asserts radio behaviour, not a
/// trace, and a formatter inside a critical section would be the instrument
/// becoming the experiment.
#[derive(Debug, Default)]
pub struct NoTrace;
impl Trace for NoTrace {
    fn event(&mut self, _tick: u64, _event: Event<'_>) {}
}

/// The kernel type this cell runs.
pub type K = Kernel<
    RadioConfig,
    XtensaPort,
    NoTrace,
    NoTickHook,
    MAX_TASKS,
    { list_slots_for(MAX_TASKS, TIMERS, lists_for(MAX_PRIORITIES, QUEUES, GROUPS)) },
    { lists_for(MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    2,
    64,
    TIMERS,
    GROUPS,
>;

struct KernelCell(UnsafeCell<Option<K>>);
// SAFETY: every access goes through `with_kernel`, which masks interrupts,
// and there is one core.
unsafe impl Sync for KernelCell {}
static KERNEL: KernelCell = KernelCell(UnsafeCell::new(None));

static STARTED: AtomicBool = AtomicBool::new(false);
/// How many times the switching interrupt has been ENTERED, and how many
/// times it actually swapped. Two numbers, because "the interrupt never
/// fired" and "it fired and declined" are different faults with the same
/// symptom.
static ENTRIES: AtomicU32 = AtomicU32::new(0);
static SWAPS: AtomicU32 = AtomicU32::new(0);
/// Declined because the scheduler chose the same task.
static SAME: AtomicU32 = AtomicU32::new(0);
/// Declined because a chosen task had no context of its own.
static NO_CTX: AtomicU32 = AtomicU32::new(0);
/// The last pair the scheduler chose, as raw indices.
static LAST_FROM: AtomicU32 = AtomicU32::new(9999);
static LAST_TO: AtomicU32 = AtomicU32::new(9999);

/// How many tasks are ready at each priority, and who is current.
///
/// A scheduler that will not switch is either looking at an empty ready list
/// or already running the task it would choose; this says which.
pub fn ready_report() -> heapless::String<96> {
    use core::fmt::Write as _;
    let mut out = heapless::String::new();
    let current = with_kernel(&mut |k: &mut K| Some(k.current())).unwrap_or_default();
    let _ = write!(out, "current={} ready=[", current.index());
    for p in 0..MAX_PRIORITIES {
        let n = with_kernel(&mut |k: &mut K| k.ready_len(p).ok()).unwrap_or(0);
        let _ = write!(out, "{n}");
        if p + 1 < MAX_PRIORITIES {
            let _ = write!(out, ",");
        }
    }
    let _ = write!(out, "]");
    out
}

/// The two switch counters: entries, swaps.
pub fn switch_counts() -> (u32, u32, u32, u32, u32, u32) {
    (
        ENTRIES.load(Ordering::Relaxed),
        SWAPS.load(Ordering::Relaxed),
        SAME.load(Ordering::Relaxed),
        NO_CTX.load(Ordering::Relaxed),
        LAST_FROM.load(Ordering::Relaxed),
        LAST_TO.load(Ordering::Relaxed),
    )
}

/// Whether the scheduler is up. `SchedulerImplementation::initialized`.
pub fn started() -> bool {
    STARTED.load(Ordering::Acquire)
}

/// Borrow the kernel with interrupts masked.
///
/// The closure must NOT block or yield: the lock is an interrupt mask, and
/// holding it across a switch would run another task with interrupts off.
/// Every blocking path in the adapter takes this once per attempt and drops
/// it before yielding.
pub fn with_kernel<R>(f: &mut dyn FnMut(&mut K) -> Option<R>) -> Option<R> {
    let saved = mask();
    // SAFETY: interrupts are masked and there is one core, so this is the
    // only live borrow.
    let out = unsafe {
        match (*KERNEL.0.get()).as_mut() {
            Some(k) => f(k),
            None => None,
        }
    };
    unmask(saved);
    out
}

#[cfg(target_arch = "xtensa")]
#[inline(always)]
fn mask() -> u32 {
    let ps: u32;
    // SAFETY: reads PS and raises INTLEVEL; touches no memory.
    unsafe {
        core::arch::asm!("rsil {0}, 3", out(reg) ps, options(nostack));
    }
    ps
}

#[cfg(target_arch = "xtensa")]
#[inline(always)]
fn unmask(ps: u32) {
    // SAFETY: `ps` is a state this core was already in.
    unsafe {
        core::arch::asm!("wsr.ps {0}", "rsync", in(reg) ps, options(nostack));
    }
}

#[cfg(not(target_arch = "xtensa"))]
fn mask() -> u32 {
    0
}
#[cfg(not(target_arch = "xtensa"))]
fn unmask(_ps: u32) {}

/// Install the kernel. Once, before anything else touches it.
pub fn install(kernel: K) {
    let saved = mask();
    // SAFETY: interrupts masked, single core, called once from `main`.
    unsafe {
        *KERNEL.0.get() = Some(kernel);
    }
    unmask(saved);
}

/// Mark the scheduler running.
pub fn mark_started() {
    STARTED.store(true, Ordering::Release);
}

/// Microseconds since boot, from the hardware timer rather than sim time.
pub fn now_us() -> u64 {
    Instant::now().duration_since_epoch().as_micros()
}

/// Ask for a switch and let it happen.
///
/// The raise is all this does; the switch occurs when the software
/// interrupt is taken, which is the only context where the machine state is
/// saved. Returning from here means this task has been scheduled again.
pub fn yield_and_switch() {
    rusty_rtos_port_xtensa::yield_now();
}

/// `Software0`: the switch.
///
/// # The kernel no longer decides before this runs
///
/// `Port::COMMITS_SWITCH` is `true` for [`XtensaPort`], so
/// `Kernel::port_yield` raises this exception and leaves `current` alone.
/// That makes this the ONE place a switch is decided and enacted, and the
/// two happen together — which is the whole point.
///
/// It was not always so. The first version let the kernel commit at the
/// yield and tried to reconcile here, keeping a `RUNNING` static for "who is
/// actually loaded". That is a scheduling primitive re-implemented in a
/// second place, which the house has an ADR about, and it did not work: the
/// kernel moved `current` while task code kept running, and a blocking call
/// in that gap parked the wrong task until every ready list was empty.
#[esp_hal::ram]
#[unsafe(export_name = "Software0")]
fn switching_interrupt(trap_frame: &mut Context) {
    ENTRIES.fetch_add(1, Ordering::Relaxed);
    clear_switch_request();
    if !started() {
        return;
    }

    // Decide AND commit, here, in one step.
    let moved = {
        // SAFETY: interrupts are already masked -- this is an interrupt
        // handler -- and there is one core, so this borrow is exclusive.
        let k = unsafe { (*KERNEL.0.get()).as_mut() };
        match k {
            None => None,
            Some(k) => {
                let from = k.current();
                k.switch_context();
                let to = k.current();
                LAST_FROM.store(u32::from(from.index()), Ordering::Relaxed);
                LAST_TO.store(u32::from(to.index()), Ordering::Relaxed);
                if from == to {
                    SAME.fetch_add(1, Ordering::Relaxed);
                    None
                } else {
                    Some((from, to))
                }
            }
        }
    };

    let Some((from, to)) = moved else { return };
    let (Some(out), Some(into)) = (
        crate::adapter::context_of(from),
        crate::adapter::context_of(to),
    ) else {
        // A kernel task with no stack of its own -- idle or the timer
        // daemon.
        NO_CTX.fetch_add(1, Ordering::Relaxed);
        return;
    };

    SWAPS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: both pointers come from live `TaskSlot`s, and `trap_frame` is
    // the frame this handler was handed.
    unsafe { switch_context(Some(out), into, trap_frame) };
}

// -------------------------------------------------------- radio timers --

/// One radio timer.
///
/// The kernel's own software timers dispatch by a `u16` callback index
/// through its daemon, which does not fit a C function pointer handed over
/// at runtime. So the radio's timers are kept here and serviced by a task
/// of this cell's own, built on nothing but `delay` and [`now_us`].
#[derive(Clone, Copy)]
struct RadioTimer {
    used: bool,
    active: bool,
    periodic: bool,
    period_us: u64,
    due_us: u64,
    callback: Option<unsafe extern "C" fn(*mut c_void)>,
    data: *mut c_void,
}

const MAX_RADIO_TIMERS: usize = 16;

static mut TIMERS_TABLE: [RadioTimer; MAX_RADIO_TIMERS] = [RadioTimer {
    used: false,
    active: false,
    periodic: false,
    period_us: 0,
    due_us: 0,
    callback: None,
    data: core::ptr::null_mut(),
}; MAX_RADIO_TIMERS];

static TIMER_SLOTS_USED: AtomicU32 = AtomicU32::new(0);

/// Claim a slot for a radio timer; the index is its identity.
pub fn remember_timer(callback: unsafe extern "C" fn(*mut c_void), data: *mut c_void) -> usize {
    let saved = mask();
    let mut found = usize::MAX;
    // SAFETY: interrupts masked, single core.
    unsafe {
        let table = (&raw mut TIMERS_TABLE).cast::<RadioTimer>();
        for i in 0..MAX_RADIO_TIMERS {
            if !(*table.add(i)).used {
                *table.add(i) = RadioTimer {
                    used: true,
                    active: false,
                    periodic: false,
                    period_us: 0,
                    due_us: 0,
                    callback: Some(callback),
                    data,
                };
                found = i;
                break;
            }
        }
    }
    unmask(saved);
    if found != usize::MAX {
        TIMER_SLOTS_USED.fetch_add(1, Ordering::Relaxed);
    }
    found
}

/// Release a slot.
pub fn forget_timer(index: usize) {
    if index >= MAX_RADIO_TIMERS {
        return;
    }
    let saved = mask();
    // SAFETY: as `remember_timer`.
    unsafe {
        let table = (&raw mut TIMERS_TABLE).cast::<RadioTimer>();
        (*table.add(index)).used = false;
        (*table.add(index)).active = false;
    }
    unmask(saved);
}

/// Arm a slot.
pub fn arm_timer(index: usize, timeout_us: u64, periodic: bool) {
    if index >= MAX_RADIO_TIMERS {
        return;
    }
    let saved = mask();
    // SAFETY: as `remember_timer`.
    unsafe {
        let table = (&raw mut TIMERS_TABLE).cast::<RadioTimer>();
        (*table.add(index)).active = true;
        (*table.add(index)).periodic = periodic;
        (*table.add(index)).period_us = timeout_us;
        (*table.add(index)).due_us = now_us().saturating_add(timeout_us);
    }
    unmask(saved);
}

/// Disarm a slot.
pub fn disarm_timer(index: usize) {
    if index >= MAX_RADIO_TIMERS {
        return;
    }
    let saved = mask();
    // SAFETY: as `remember_timer`.
    unsafe {
        let table = (&raw mut TIMERS_TABLE).cast::<RadioTimer>();
        (*table.add(index)).active = false;
    }
    unmask(saved);
}

/// Whether a slot is armed.
pub fn timer_active(index: usize) -> bool {
    if index >= MAX_RADIO_TIMERS {
        return false;
    }
    let saved = mask();
    // SAFETY: as `remember_timer`.
    let out = unsafe {
        let table = (&raw const TIMERS_TABLE).cast::<RadioTimer>();
        (*table.add(index)).active
    };
    unmask(saved);
    out
}

/// Fire every timer that is due, and answer how many.
///
/// Called by the cell's timer-service task. The callback runs OUTSIDE the
/// mask: a radio callback may take semaphores, and holding an interrupt
/// mask across that would deadlock the first time one blocked.
pub fn service_timers() -> u32 {
    let mut fired = 0;
    let now = now_us();
    for i in 0..MAX_RADIO_TIMERS {
        let saved = mask();
        // SAFETY: as `remember_timer`.
        let due = unsafe {
            let table = (&raw mut TIMERS_TABLE).cast::<RadioTimer>();
            let t = *table.add(i);
            if t.used && t.active && now >= t.due_us {
                if t.periodic {
                    (*table.add(i)).due_us = now.saturating_add(t.period_us);
                } else {
                    (*table.add(i)).active = false;
                }
                t.callback.map(|cb| (cb, t.data))
            } else {
                None
            }
        };
        unmask(saved);
        if let Some((cb, data)) = due {
            // SAFETY: the pointer came from `TimerImplementation::create`,
            // and the radio keeps it valid until it deletes the timer.
            unsafe { cb(data) };
            fired += 1;
        }
    }
    fired
}

/// Bring the kernel up and make `main` a task the scheduler can switch away
/// from.
///
/// # Why `main` needs a task of its own
///
/// The switch swaps the trap frame for the CHOSEN task's context, and finds
/// that context by the kernel's task index. If `main` were not itself a
/// kernel task with a registered slot, the scheduler would have nowhere to
/// save it — and `switch_context` would refuse the switch rather than
/// discard it, so the workers would never get the CPU and the cell would
/// hang with every counter at zero.
///
/// Its context starts zeroed. The first switch AWAY from `main` fills it in,
/// which is the same trick the `xiao-s3-switch` cell plays with its own
/// context, and the reason neither needs to capture a frame by hand.
/// The 1 kHz tick, kept alive for the life of the program.
static mut TICKER: Option<PeriodicTimer<'static, esp_hal::Blocking>> = None;

/// The tick interrupt: count it, and let the kernel say whether that woke
/// somebody worth switching to.
///
/// Without this the kernel has no time at all: `delay` never expires and a
/// blocking call with a timeout waits for ever. The first run after the
/// switch was fixed hung here — not because the switch was wrong, but
/// because blocking had finally become real and nothing was left to end it.
#[esp_hal::ram]
extern "C" fn on_tick() {
    // SAFETY: set once in `boot` before interrupts are enabled, and only
    // ever read here.
    unsafe {
        if let Some(t) = (*(&raw mut TICKER)).as_mut() {
            t.clear_interrupt();
        }
    }
    let want = {
        // SAFETY: an interrupt handler on a single core; interrupts at this
        // level are masked while it runs.
        let k = unsafe { (*KERNEL.0.get()).as_mut() };
        match k {
            None => false,
            Some(k) => {
                k.port().note_tick();
                k.increment_tick()
            }
        }
    };
    if want {
        rusty_rtos_port_xtensa::yield_now();
    }
}

pub fn boot() -> bool {
    let kernel = match K::new(XtensaPort::new(), NoTrace) {
        Ok(k) => k,
        Err(_) => return false,
    };
    install(kernel);

    // `main` runs below the workers so that blocking it hands them the CPU.
    let Some(main_task) = with_kernel(&mut |k: &mut K| k.create_task("main", 2).ok()) else {
        return false;
    };
    if with_kernel(&mut |k: &mut K| k.start_scheduler().ok()).is_none() {
        return false;
    }
    crate::adapter::register_main(main_task);
    // Who the kernel thinks is running, and who `main` is. If these differ,
    // every block below parks the wrong task.
    let current = with_kernel(&mut |k: &mut K| Some(k.current())).unwrap_or_default();
    esp_println::println!(
        "RADIO boot main_index={} current_index={} ready_at_2={:?}",
        main_task.index(),
        current.index(),
        with_kernel(&mut |k: &mut K| k.ready_len(2).ok())
    );

    rusty_rtos_port_xtensa::enable_switching();
    mark_started();
    true
}

/// Start the 1 kHz tick. Separate from [`boot`] because it needs a
/// peripheral, and `boot` is called before `main` has one to give.
pub fn start_tick(systimer: esp_hal::peripherals::SYSTIMER<'static>) -> bool {
    let alarm = SystemTimer::new(systimer).alarm0;
    let mut timer = PeriodicTimer::new(alarm);
    timer.set_interrupt_handler(esp_hal::interrupt::InterruptHandler::new(
        on_tick,
        esp_hal::interrupt::Priority::Priority1,
    ));
    if timer.start(Duration::from_millis(1)).is_err() {
        return false;
    }
    // SAFETY: `main` calls this once, before any task runs.
    unsafe {
        TICKER = Some(timer);
    }
    true
}
