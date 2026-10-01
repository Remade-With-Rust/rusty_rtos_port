//! The kernel this cell runs, and the three things the radio needs from
//! it: a way to reach it, a real clock, and a switch.
//!
//! Lifted from `xiao-s3-radio`, with two changes that only matter once the
//! real radio is linked:
//!
//! * **Every kernel task has a stack.** `IDLE` and `Tmr Svc` get bodies
//!   (see [`boot`]). With the radio blocking all of its own tasks, the
//!   scheduler WILL choose idle, and a declined switch would leave `current`
//!   naming a task the CPU is not running.
//! * **The timer daemon runs the radio's software timers**, at the top
//!   priority, so a scan or association timeout fires on time rather than
//!   whenever something happens to call `delay`.
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

#![allow(dead_code, reason = "diagnostics kept for the next board run")]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use esp_hal::time::{Duration, Instant};
use esp_hal::timer::systimer::SystemTimer;
use esp_hal::timer::PeriodicTimer;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Trace};
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_kernel_core::{list_slots_for, lists_for, Kernel};
use rusty_rtos_port_esp_radio::Blocked;
use rusty_rtos_port_xtensa::{clear_switch_request, switch_context, Context, XtensaPort};

/// Priorities: idle at 0, the radio's tasks between, the timer service and
/// `main` at the top.
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
    /// The top but one, level with the radio's own ceiling: the daemon
    /// services the radio's timers, and a timer that fires late is a scan or
    /// an association that times out late. `main` takes the top (see `boot`).
    const TIMER_TASK_PRIORITY: u8 = MAX_PRIORITIES - 2;
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
    // Nothing here reads a task name, so the kernel is told not to build one.
    // Without this the trait default is `true` and every traced event costs a
    // name lookup plus a UTF-8 validation for a sink that drops it: measured
    // at 3.86x on one row (2026-09-21).
    const WANTS_NAMES: bool = false;

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
    { <RadioConfig as ::rusty_rtos_core::config::Config>::TIMER_QUEUE_LENGTH },
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
        rusty_rtos_port_esp_radio::context_of(from),
        rusty_rtos_port_esp_radio::context_of(to),
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

// ------------------------------------------------------------ the host --

/// What this cell hands `rusty_rtos_port-esp-radio`.
///
/// The crate holds the five driver implementations and asks a host for the
/// kernel behind them. Everything below is a forward: the interesting part
/// is that there is nothing interesting, which is the point of the seam.
pub struct Host;

/// The one installed host. `&'static` because the driver reaches the crate
/// from `extern "C"` shims that carry no state of their own.
pub static HOST: Host = Host;

/// The kernel, wearing the seam's trait.
///
/// A newtype rather than `impl KernelOps for K`, because the orphan rule
/// forbids that: `Kernel` belongs to `rusty_rtos_kernel-core` and `KernelOps`
/// to `rusty_rtos_port-esp-radio`, and neither is this cell's. Every consumer
/// meets this, so the crate's own docs show the wrapper.
///
/// It borrows rather than owns, so it costs nothing: `Ops(k)` is built inside
/// `with_kernel` around the borrow that already exists.
pub struct Ops<'a>(pub &'a mut K);

impl rusty_rtos_port_esp_radio::KernelOps for Ops<'_> {
    fn current(&mut self) -> TaskHandle {
        self.0.current()
    }

    fn create_task(&mut self, name: &str, priority: u8) -> Option<TaskHandle> {
        self.0.create_task(name, priority).ok()
    }

    fn task_delete(&mut self, task: Option<TaskHandle>) -> Option<()> {
        self.0.task_delete(task).ok()
    }

    fn task_priority_get(&mut self, task: Option<TaskHandle>) -> Option<u8> {
        self.0.task_priority_get(task).ok()
    }

    fn set_priority(&mut self, task: Option<TaskHandle>, priority: u8) -> Option<()> {
        self.0.set_priority(task, priority).ok()
    }

    fn delay(&mut self, ticks: u64) -> Option<()> {
        self.0.delay(ticks).ok()
    }

    fn semaphore_create_counting(&mut self, max: usize, initial: usize) -> Option<QueueHandle> {
        self.0.semaphore_create_counting(max, initial).ok()
    }

    fn semaphore_take(&mut self, semaphore: QueueHandle, ticks: u64) -> Option<Blocked> {
        self.0.semaphore_take(semaphore, ticks).ok().map(wait)
    }

    fn semaphore_give(&mut self, semaphore: QueueHandle) -> Option<Blocked> {
        self.0.semaphore_give(semaphore).ok().map(wait)
    }

    fn semaphore_give_from_isr(&mut self, semaphore: QueueHandle) -> Option<bool> {
        self.0
            .semaphore_give_from_isr(semaphore)
            .ok()
            .map(|w| w == Woken::YES)
    }

    fn semaphore_count(&mut self, semaphore: QueueHandle) -> Option<usize> {
        self.0.semaphore_count(semaphore).ok()
    }

    fn mutex_create(&mut self) -> Option<QueueHandle> {
        self.0.mutex_create().ok()
    }

    fn mutex_create_recursive(&mut self) -> Option<QueueHandle> {
        self.0.mutex_create_recursive().ok()
    }

    fn queue_delete(&mut self, queue: QueueHandle) -> Option<()> {
        self.0.queue_delete(queue).ok()
    }
}

/// The kernel's `Wait<()>` as the seam's [`Blocked`].
///
/// Two states each, and they correspond exactly — there is no timeout arm on
/// either side, because a kernel that parks you answers `Blocked` and expects
/// the same call again.
fn wait(w: Wait<()>) -> Blocked {
    match w {
        Wait::Ready(()) => Blocked::Completed,
        Wait::Blocked => Blocked::Blocked,
    }
}

impl rusty_rtos_port_esp_radio::RadioHost for Host {
    fn max_tasks(&self) -> usize {
        MAX_TASKS
    }

    fn max_priorities(&self) -> u8 {
        MAX_PRIORITIES
    }

    fn tick_hz(&self) -> u32 {
        <RadioConfig as Config>::TICK_RATE_HZ
    }

    fn scheduler_started(&self) -> bool {
        started()
    }

    fn enter_critical(&self) -> u32 {
        mask()
    }

    fn exit_critical(&self, token: u32) {
        unmask(token);
    }

    fn now_us(&self) -> u64 {
        now_us()
    }

    fn yield_and_switch(&self) {
        yield_and_switch();
    }

    fn with_kernel(&self, f: &mut dyn FnMut(&mut dyn rusty_rtos_port_esp_radio::KernelOps)) {
        // The borrow is held for the WHOLE closure, which is the seam's
        // stated contract and the reason it was not flattened to one method
        // per kernel call: several callers do three or four operations here
        // and rely on no other task running between them.
        with_kernel(&mut |k: &mut K| {
            f(&mut Ops(k));
            Some(())
        });
    }
}

// -------------------------------------------------------- radio timers --

// The radio's software timer table USED to be here: 16 slots, a due-time
// comparison, and a `service_timers` the cell's own task called. It is now
// `rusty_rtos_port_esp_radio::service_timers`, because it never touched
// kernel state — only a clock and a mask — and keeping it beside the kernel
// is what made it LOOK like kernel state and kept the glue unconsumable.

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
    let n = ISR_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if ISR_BEAT.load(Ordering::Relaxed) && (n % 1000 == 0 || n == 100) {
        // The PC this tick interrupted: a level-1 interrupt saves it in EPC1.
        // When `main` is stuck spinning, this names the loop.
        #[cfg(target_arch = "xtensa")]
        let epc: u32 = {
            let v: u32;
            // SAFETY: reads a special register.
            unsafe { core::arch::asm!("rsr.epc1 {0}", out(reg) v, options(nostack)) };
            v
        };
        #[cfg(not(target_arch = "xtensa"))]
        let epc: u32 = 0;
        let (e, sw, same, noctx, lf, lt) = switch_counts();
        // SAFETY: as above -- this handler owns the kernel for its duration.
        let cur = unsafe { (*KERNEL.0.get()).as_ref().map(|k| k.current().index()) };
        esp_println::println!(
            "ISRBEAT n={n} epc={epc:#010x} cur={cur:?} sw={sw}/{e} same={same} noctx={noctx} last={lf}->{lt} timers={}",
            TIMERS_FIRED.load(Ordering::Relaxed)
        );
    }
    if want {
        rusty_rtos_port_xtensa::yield_now();
    }
}

/// Ticks the interrupt has taken, and whether it prints a heartbeat.
pub static ISR_TICKS: AtomicU32 = AtomicU32::new(0);
pub static ISR_BEAT: AtomicBool = AtomicBool::new(false);

pub fn boot() -> bool {
    let kernel = match K::new(XtensaPort::new(), NoTrace) {
        Ok(k) => k,
        Err(_) => return false,
    };
    install(kernel);
    // Hand the driver crate this cell's kernel, before anything reaches an
    // adapter type (`xiao-s3-radio` records what LTO does without it).
    rusty_rtos_port_esp_radio::install(&HOST);

    // `main` runs ABOVE the daemon. The kernel's rule (`Kernel::start_scheduler`)
    // is that the task owning the CPU must not sit below `TIMER_TASK_PRIORITY`
    // -- and LEVEL is not enough: the first board run put both at 7, the
    // daemon (created second) became `current` while the CPU was on `main`,
    // and the first switch saved `main`'s registers as the daemon's. Strictly
    // above is the only placement where `current` cannot move at start.
    let main_priority = <RadioConfig as Config>::TIMER_TASK_PRIORITY + 1;
    let Some(main_task) = with_kernel(&mut |k: &mut K| k.create_task("main", main_priority).ok())
    else {
        return false;
    };
    let Some(started) = with_kernel(&mut |k: &mut K| k.start_scheduler().ok()) else {
        return false;
    };
    rusty_rtos_port_esp_radio::register_main(main_task);
    rusty_rtos_port_esp_radio::register_kernel_task(started.idle, idle_body, 2048);
    rusty_rtos_port_esp_radio::register_kernel_task(started.timer, timer_body, 6144);
    let current = with_kernel(&mut |k: &mut K| Some(k.current())).unwrap_or_default();
    esp_println::println!(
        "WIFI boot main={} idle={} tmr={} current={}",
        main_task.index(),
        started.idle.index(),
        started.timer.index(),
        current.index()
    );

    rusty_rtos_port_xtensa::enable_switching();
    mark_started();
    true
}

/// Block the calling task for `ticks` and let the scheduler run something
/// else. The kernel parks the task; the yield is what takes it off the CPU.
pub fn sleep_ticks(ticks: u64) {
    let _ = with_kernel(&mut |k: &mut K| k.delay(ticks).ok());
    yield_and_switch();
}

/// How many radio timers the daemon has fired.
pub static TIMERS_FIRED: AtomicU32 = AtomicU32::new(0);

/// `Tmr Svc`: service the radio's timer table once a tick.
///
/// The callbacks run here, in a task, because a radio timer callback may take
/// a semaphore and an interrupt cannot block.
extern "C" fn timer_body(_: *mut core::ffi::c_void) {
    let mut beat: u32 = 0;
    loop {
        let fired = rusty_rtos_port_esp_radio::service_timers();
        if fired != 0 {
            TIMERS_FIRED.fetch_add(fired, Ordering::Relaxed);
        }
        beat = beat.wrapping_add(1);
        if HEARTBEAT.load(Ordering::Relaxed) && beat % 1000 == 0 {
            let (e, sw, same, noctx, lf, lt) = switch_counts();
            esp_println::println!(
                "BEAT t={}ms sw={sw}/{e} same={same} noctx={noctx} last={lf}->{lt} timers={} {}",
                beat,
                TIMERS_FIRED.load(Ordering::Relaxed),
                ready_report()
            );
        }
        sleep_ticks(1);
    }
}

/// Print a heartbeat from the daemon once a second.
pub static HEARTBEAT: AtomicBool = AtomicBool::new(false);

/// `IDLE`: halt until the next interrupt. Never blocks.
///
/// `waiti 0` drops PS.INTLEVEL to zero, which is where a task already runs;
/// it would destroy a critical section, and there is none here.
extern "C" fn idle_body(_: *mut core::ffi::c_void) {
    loop {
        #[cfg(target_arch = "xtensa")]
        // SAFETY: waits for an interrupt; touches no memory.
        unsafe {
            core::arch::asm!("waiti 0", options(nostack));
        }
        #[cfg(not(target_arch = "xtensa"))]
        core::hint::spin_loop();
    }
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
    // `listen`, or the alarm counts and never interrupts. Without it this
    // cell had NO tick: `xiao-s3-radio` passed anyway because its workers
    // never needed time to pass, and the Wi-Fi cell is where it showed --
    // a 50-tick sleep that never returned, ISR_TICKS reading 0 after 20 ms
    // of spinning with PS.INTLEVEL at 0.
    timer.listen();
    if timer.start(Duration::from_millis(1)).is_err() {
        return false;
    }
    // SAFETY: `main` calls this once, before any task runs.
    unsafe {
        TICKER = Some(timer);
    }
    true
}
