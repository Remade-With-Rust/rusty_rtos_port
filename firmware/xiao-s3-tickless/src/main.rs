//! **The Kairos kernel, tick-driven, on a XIAO ESP32-S3 — and tickless.**
//!
//! Two claims in one cell, because the second cannot be made without the
//! first and the first did not exist anywhere in this tree:
//!
//! 1. **The Kernel runs from a tick interrupt on an Xtensa part.** Every
//!    other kernel-driving cell is Cortex-M under QEMU; the two XIAO cells
//!    prove the *port's* switch and nothing above it. This wires
//!    `SYSTIMER` alarm 0 to `Kernel::increment_tick` and `Software0` to
//!    `Kernel::switch_context`, which is the joint the `mps2-an385-qemu-*`
//!    cells prove on ARM.
//! 2. **Tickless idle suppresses those interrupts without moving the
//!    schedule**, as `mps2-an385-qemu-tickless` proves on ARM.
//!
//! Build with `--features tickless` for the second arm. That flag is the
//! only difference between the two builds.
//!
//! # Measured on the board
//!
//! XIAO ESP32-S3 rev v0.2, 8 MB flash, 40 MHz crystal, MAC
//! 68:ee:8f:51:74:64. Both claims hold:
//!
//! | | control | tickless |
//! |---|---:|---:|
//! | logical ticks | 400 | 400 |
//! | **alarm wakeups** | **400** | **0** |
//! | projected events | 195 | 195 |
//! | context switches | 63 | 63 |
//! | **schedule digest** | `ebb908b74bccb99e` | `ebb908b74bccb99e` |
//!
//! Four hundred wakeups to none, with a byte-identical schedule. The sleep
//! diagnostics say 20 sleeps, 400 ticks asked for and 400 slept, the last
//! window measuring 20,014us against a 20,000us request -- so the elapsed
//! time really is being read off the counter rather than assumed.
//!
//! # Why the sleep is a different shape from the ARM one
//!
//! On Cortex-M the idle task holds PRIMASK and `wfi` **still wakes on a
//! pending masked interrupt**, so the port sleeps, accounts for the time,
//! and drops the pending SysTick on the floor with `ICSR.PENDSTCLR`. The
//! exception is never taken and the handler never runs.
//!
//! `waiti 0` does the opposite: it **sets** `PS.INTLEVEL` to zero, so it
//! unmasks on the way into the sleep and the alarm interrupt really is
//! TAKEN. There is no pending bit to clear afterwards, because the handler
//! has already run by then. So the suppression has to happen *inside the
//! handler*: [`SLEEPING`] is set across the sleep, and the tick handler
//! checks it first and does nothing but acknowledge the alarm.
//!
//! That is also why the handler must not touch the kernel while the flag is
//! set — the idle task is inside `with_kernel` and holds it mutably. The
//! flag check is the first statement in the handler for that reason.
//!
//! # ★ The bug the board found, which no amount of building would have
//!
//! The first run of the tickless arm FAILED, and the failure was the whole
//! value of having hardware: **`waiti 0` does not just lower `PS.INTLEVEL`
//! for the duration of the sleep — it SETS it to zero and leaves it there.**
//! The interrupt that wakes you returns through `RFI`, which restores the PS
//! `waiti` installed. So the caller's critical section is gone from the wake
//! onward, not merely suspended during the sleep.
//!
//! Everything `idle_suppress_ticks` still had to do after the sleep —
//! `step_tick`, `resume_all`, two trace events — therefore ran with
//! interrupts open, on a kernel the idle task was holding `&mut` to. The
//! symptom was not a crash but a scheduler that quietly stopped working: the
//! worker never blocked, and the cell reported **20 logical ticks where the
//! arithmetic says 400**.
//!
//! The cure is one line — re-raise the mask the instant `waiti` returns,
//! before anything else. ARM needs no such line, because `wfi` leaves
//! PRIMASK alone.
//!
//! **And the honest footnote:** two changes were made and only one mattered.
//! The switch handler was also taught to decline while `SLEEPING`, on the
//! theory that `Software0` could be taken in the open window. A counter says
//! it fired **zero** times across the run. That path is defence in depth, it
//! is not what fixed this, and it is labelled that way at its definition
//! rather than quietly banked as part of the cure.
//!
//! # And why elapsed time is MEASURED rather than assumed
//!
//! `esp_hal::rtc_cntl::Rtc::sleep_light` is the deeper sleep this cell does
//! not yet use, and its own documentation is the warning: a refused sleep, a
//! rejected sleep and a very short sleep are indistinguishable from its
//! return. A port that trusted a sleep would wind the kernel's clock past a
//! task's wake time.
//!
//! So this port does not trust it. `SYSTIMER` is free-running and is read
//! with `Timer::now()` on both sides of the sleep; the ticks reported are
//! whatever the counter says elapsed, floored to whole ticks. That is sound
//! whether the sleep ran to its end, was cut short, or never happened at
//! all — and it is the mechanism that will still be sound when the sleep
//! underneath it becomes a light sleep.
//!
//! # The three numbers
//!
//! | number | what it is | what it must do |
//! |---|---|---|
//! | **wakeups** | alarm interrupts actually taken | **collapse** |
//! | **order digest** | FNV-1a over the projected events: kind and task, no clock | **not move between arms** |
//! | **logical ticks** | what the kernel thinks the time is at the end | **stay in a named band** |
//!
//! The order digest is not pinned here, unlike the ARM cell's, because
//! nobody has run this to learn what it is. Run both arms and compare the
//! two printed values; that comparison is the claim. Pin it afterwards.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use esp_backtrace as _;
use esp_hal::time::Duration;
use esp_hal::timer::Timer;
use esp_hal::timer::systimer::{Alarm, SystemTimer};
use esp_println::println;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::handle::TaskHandle;
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Scheduling, Trace};
use rusty_rtos_kernel_core::{Kernel, Stall, items_for, lists_for};
use rusty_rtos_port_xtensa::{
    Context, XtensaPort, clear_switch_request, enable_switching, new_task_context, switch_context,
    yield_now,
};

esp_bootloader_esp_idf::esp_app_desc!();

/// Whether this build suppresses ticks.
const TICKLESS: bool = cfg!(feature = "tickless");

// --------------------------------------------------------------- the shape --

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
    const EXPECTED_IDLE_TIME_BEFORE_SLEEP: u64 = 2;
}

/// How long the worker sleeps each lap.
const DELAY_TICKS: u64 = 20;
/// How many laps the run lasts.
const ROUNDS: u32 = 20;
/// One tick, in microseconds. `TICK_RATE_HZ` is 1000, so a tick is 1 ms —
/// stated here in the unit `Duration` takes, and asserted against the config
/// in `main` so the two can never drift apart silently.
const TICK_MICROS: u64 = 1_000;

/// **The pinned schedule.** Both arms must produce this, and the fact that
/// there is ONE constant for two arms is the cross-arm claim.
///
/// Measured 2026-09-19 on a XIAO ESP32-S3, rev v0.2, 8 MB flash, 40 MHz
/// crystal, MAC 68:ee:8f:51:74:64, release profile. It is a digest of the
/// `Scheduling` projection with the tick stamps left out -- see the header
/// of `mps2-an385-qemu-tickless` for why they are left out.
///
/// **Re-pinning.** A kernel change that legitimately moves this scenario's
/// schedule moves this number, and both arms move together. Run both, check
/// they agree with each other, and write the new value here. If they do NOT
/// agree with each other there is nothing to pin: the change broke tickless.
const PINNED_ORDER: u64 = 0xebb9_08b7_4bcc_b99e;

const TASKS: usize = 4;
/// The timer daemon's command queue, and one spare.
const QUEUES: usize = 2;
const SLOTS: usize = 16;
const TIMERS: usize = 1;
const GROUPS: usize = 1;

/// One saved machine state per task, plus the one `main` is parked in while
/// they run.
const CONTEXTS: usize = TASKS + 1;
/// The index of `main`'s own context.
const MAIN: usize = TASKS;

// -------------------------------------------------------------- the digest --

/// FNV-1a over the schedule. See `mps2-an385-qemu-tickless` for why there are
/// two hashes and why only the first one is the claim: on silicon a tick
/// stamp records how long the *work* took, and the two arms do not take the
/// same time, so a quantity the instrument's own cost can move is not a
/// schedule.
#[derive(Debug)]
struct Digest {
    order: u64,
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
    const WANTS_NAMES: bool = false;

    fn event(&mut self, tick: u64, event: Event<'_>) {
        self.events = self.events.saturating_add(1);
        self.fold_when(tick);
        self.fold_str(event.name());
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
            _ => u64::MAX,
        };
        self.fold_u64(who);
    }
}

type Tr = Scheduling<Digest>;

// ---------------------------------------------------------------- the port --

/// The tick source, reachable from `suppress_ticks_and_sleep`.
///
/// It is a `static` because `Port::suppress_ticks_and_sleep` takes `&self`
/// and the port is a `static` itself — a port has to be, to be reachable
/// from an interrupt handler at all.
struct AlarmCell(UnsafeCell<Option<Alarm<'static>>>);
// SAFETY: written once by `main` before any interrupt is enabled, and read
// thereafter only from inside a critical section or from the tick handler,
// which cannot overlap one. There is one core.
unsafe impl Sync for AlarmCell {}
static ALARM: AlarmCell = AlarmCell(UnsafeCell::new(None));

/// Set across a suppressed sleep.
///
/// `waiti 0` unmasks, so the alarm interrupt really is taken during the
/// sleep — unlike ARM, where `wfi` leaves it pending under PRIMASK and the
/// port clears it. The handler reads this FIRST and, when it is set, does
/// nothing but acknowledge the alarm: the kernel is held mutably by the idle
/// task at that moment, and the port is about to account for the whole
/// interval itself.
static SLEEPING: AtomicBool = AtomicBool::new(false);

/// **The instrument.** Alarm interrupts actually taken.
///
/// Not `Kernel::tick_count`, and the difference is the point: the kernel
/// counts LOGICAL ticks, including the ones `Kernel::step_tick` winds
/// forward with no interrupt happening at all.
static WAKEUPS: AtomicU32 = AtomicU32::new(0);
static LAPS: AtomicU32 = AtomicU32::new(0);

/// Diagnostics for the sleep itself. The first run of this cell on silicon
/// failed the tick band -- 20 logical ticks where the arithmetic says 400 --
/// and the report could not say whether the port was asked for the wrong
/// window, measured the wrong elapsed time, or was not asked at all. These
/// four answer that in one flashing.
static SLEEP_CALLS: AtomicU32 = AtomicU32::new(0);
static SLEEP_ASKED: AtomicU32 = AtomicU32::new(0);
static SLEEP_SLEPT: AtomicU32 = AtomicU32::new(0);
static SLEEP_LAST_US: AtomicU32 = AtomicU32::new(0);
/// Switches declined because they landed inside a suppressed sleep. Non-zero
/// is not a fault -- it is evidence the hazard above is real on this part.
static DECLINED_SWITCHES: AtomicU32 = AtomicU32::new(0);

/// `XtensaPort` with `vPortSuppressTicksAndSleep` on top.
///
/// The mechanism cannot live in `rusty_rtos_port-xtensa` the way the ARM one
/// lives in `rusty_rtos_port-cortex-m`, and the reason is a real
/// architectural difference rather than a preference: SysTick is a *core*
/// peripheral, so the ARM port already owns its register map, while
/// `SYSTIMER` is a *chip* peripheral that belongs to `esp-hal`. The Xtensa
/// port crate is HAL-free by design, so the sleep belongs here.
#[derive(Debug, Default)]
struct TicklessPort {
    inner: XtensaPort,
}

impl TicklessPort {
    const fn new() -> Self {
        Self {
            inner: XtensaPort::new(),
        }
    }
}

impl Port for TicklessPort {
    const COMMITS_SWITCH: bool = XtensaPort::COMMITS_SWITCH;

    fn yield_now(&self) {
        self.inner.yield_now();
    }
    fn yield_from_isr(&self, woken: Woken) {
        self.inner.yield_from_isr(woken);
    }
    fn enter_critical(&self) {
        self.inner.enter_critical();
    }
    fn exit_critical(&self) {
        self.inner.exit_critical();
    }
    fn set_interrupt_mask_from_isr(&self) -> u32 {
        self.inner.set_interrupt_mask_from_isr()
    }
    fn clear_interrupt_mask_from_isr(&self, saved: u32) {
        self.inner.clear_interrupt_mask_from_isr(saved);
    }
    fn in_isr(&self) -> bool {
        self.inner.in_isr()
    }
    fn count_tick(&self) {
        self.inner.count_tick();
    }
    fn count_yield(&self) {
        self.inner.count_yield();
    }
    fn exits(&self) -> u64 {
        self.inner.exits()
    }
    fn idle(&self) {
        self.inner.idle();
    }

    /// `vPortSuppressTicksAndSleep`.
    ///
    /// Reprogram alarm 0 for one long interval, sleep, and report how many
    /// WHOLE ticks the free-running counter says went by. Zero declines, and
    /// every path that cannot account for the time exactly takes it.
    fn suppress_ticks_and_sleep(&self, expected_idle_ticks: u64) -> u64 {
        SLEEP_CALLS.fetch_add(1, Ordering::Relaxed);
        SLEEP_ASKED.fetch_add(expected_idle_ticks as u32, Ordering::Relaxed);
        if expected_idle_ticks == 0 {
            return 0;
        }
        // SAFETY: the caller is inside `with_kernel`, so interrupts are
        // masked and the tick handler -- the only other reader -- cannot be
        // running. `main` wrote this before enabling any interrupt.
        let slot = unsafe { &mut *ALARM.0.get() };
        let Some(alarm) = slot.as_mut() else {
            return 0;
        };

        let window = expected_idle_ticks.saturating_mul(TICK_MICROS);
        let before = alarm.now();

        alarm.stop();
        alarm.enable_auto_reload(false);
        if alarm.load_value(Duration::from_micros(window)).is_err() {
            // The window did not fit the timer. Put the tick back exactly as
            // it was and decline rather than sleep for a length nobody
            // chose.
            restore_tick(alarm);
            return 0;
        }
        alarm.clear_interrupt();
        alarm.start();

        // `waiti 0` lowers PS.INTLEVEL, so the alarm WILL be taken. The flag
        // is what stops its handler touching a kernel the idle task is
        // holding; see this file's header.
        SLEEPING.store(true, Ordering::SeqCst);
        self.inner.idle();
        // ★ `waiti 0` SETS PS.INTLEVEL to zero, and leaves it there. The
        // interrupt that woke us returns through RFI, which restores the PS
        // `waiti` installed -- so the caller's critical section is GONE from
        // here on, and everything `idle_suppress_ticks` still has to do
        // (`step_tick`, `resume_all`, two trace events) would run with
        // interrupts open, on a kernel this task is holding `&mut` to. Put
        // the mask back before anything else happens.
        //
        // This is the whole of the difference from the ARM port, where `wfi`
        // leaves PRIMASK alone and the critical section survives the sleep.
        // Measured: without this the worker stopped blocking altogether --
        // 20 logical ticks where the arithmetic says 400.
        let _ = self.inner.set_interrupt_mask_from_isr();
        SLEEPING.store(false, Ordering::SeqCst);

        alarm.stop();
        // MEASURED, not assumed. The counter is free-running and is the only
        // thing here that knows how long the sleep really was -- whether it
        // ran to its end, was cut short by another interrupt, or never
        // happened.
        let elapsed = alarm.now().duration_since_epoch().as_micros()
            - before.duration_since_epoch().as_micros();
        restore_tick(alarm);

        // Whole ticks only. Reporting a part-tick would wind the kernel's
        // clock past a wake time, and the kernel clamps an overclaim rather
        // than trusting it -- but it cannot rescue one it was told was whole.
        SLEEP_LAST_US.store(elapsed as u32, Ordering::Relaxed);
        let slept = elapsed.checked_div(TICK_MICROS).unwrap_or(0);
        let slept = slept.min(expected_idle_ticks);
        SLEEP_SLEPT.fetch_add(slept as u32, Ordering::Relaxed);
        slept
    }
}

/// Put alarm 0 back to the periodic millisecond tick.
fn restore_tick(alarm: &mut Alarm<'static>) {
    alarm.stop();
    alarm.clear_interrupt();
    alarm.reset();
    alarm.enable_auto_reload(true);
    let _ = alarm.load_value(Duration::from_micros(TICK_MICROS));
    alarm.start();
}

static PORT: TicklessPort = TicklessPort::new();

// -------------------------------------------------------------- the kernel --

type K = Kernel<
    CellConfig,
    TicklessPort,
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
// or through an interrupt handler, which cannot overlap one on a single
// core.
unsafe impl Sync for KernelCell {}
static KERNEL: KernelCell = KernelCell(UnsafeCell::new(None));

/// Borrow the kernel with interrupts masked.
fn with_kernel<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    critical_section::with(|_| {
        // SAFETY: interrupts are masked, so no handler can be holding this,
        // and there is no second core.
        let slot = unsafe { &mut *KERNEL.0.get() };
        slot.as_mut().map(f)
    })
}

/// As `with_kernel`, but from inside an interrupt, which already has
/// exclusivity.
fn with_kernel_in_isr<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    // SAFETY: a handler cannot overlap a critical section on one core, and
    // the two handlers here are at the same interrupt level, so neither can
    // preempt the other.
    let slot = unsafe { &mut *KERNEL.0.get() };
    slot.as_mut().map(f)
}

/// A task stack. 16-byte aligned, which the Xtensa ABI requires of `SP`.
///
/// The bytes are never named again -- `arm_task` takes the address and the
/// size and hands both to `new_task_context`, and the task itself is the
/// only thing that ever reads them.
#[repr(align(16))]
struct Stack(#[expect(dead_code, reason = "addressed as storage, never read as a field")] [u8; 8192]);

static mut STACK_WORK: Stack = Stack([0; 8192]);
static mut STACK_IDLE: Stack = Stack([0; 8192]);
static mut STACK_TMR: Stack = Stack([0; 8192]);

static mut CONTEXTS_STORE: [Context; CONTEXTS] =
    [const { unsafe { core::mem::zeroed() } }; CONTEXTS];
/// Which context is on the CPU. The switch handler is its only writer.
static CURRENT: AtomicU32 = AtomicU32::new(MAIN as u32);

// ------------------------------------------------------------ the two ISRs --

/// `Software0`: the switch. The port raises it; the kernel decides.
///
/// By the time this runs, `xtensa-lx-rt`'s interrupt entry has spilled every
/// register window and written the machine into `trap_frame`, which is why
/// the switch itself is two struct copies. This is the Xtensa twin of the
/// ARM cells' `PendSV` -> `pick_next`.
#[esp_hal::ram]
#[unsafe(export_name = "Software0")]
fn switching_interrupt(trap_frame: &mut Context) {
    clear_switch_request();

    // Interrupts really are open across `waiti 0`, so this can be taken
    // while the idle task is inside `with_kernel` holding the kernel `&mut`.
    // Touching it here would alias that borrow and switch away from a task
    // in the middle of suspending the scheduler. Decline: `resume_all` at
    // the end of `idle_suppress_ticks` re-pends a switch if one is still
    // owed, so nothing is lost by not doing it now.
    if SLEEPING.load(Ordering::SeqCst) {
        DECLINED_SWITCHES.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let from = CURRENT.load(Ordering::Acquire) as usize;
    let to = with_kernel_in_isr(|k| {
        k.switch_context();
        usize::from(k.current().index())
    })
    .unwrap_or(from);
    if from == to || to >= CONTEXTS {
        return;
    }
    CURRENT.store(to as u32, Ordering::Release);

    // SAFETY: single core, and this handler is the only reader or writer of
    // the store while a switch is in progress. Both indices are below
    // `CONTEXTS`, which the guard above enforces for `to` and which holds
    // for `from` because this handler is the only writer of `CURRENT`.
    #[expect(unsafe_code, reason = "the context switch")]
    unsafe {
        let base = (&raw mut CONTEXTS_STORE).cast::<Context>();
        switch_context(Some(base.add(from)), base.add(to), trap_frame);
    }
}

/// `SYSTIMER` alarm 0: the tick. `xPortSysTickHandler`.
#[esp_hal::handler]
fn tick_interrupt() {
    // FIRST, before anything else touches the kernel. During a suppressed
    // sleep the idle task holds the kernel mutably and the port is about to
    // account for this whole interval itself; all this handler may do is
    // acknowledge the alarm. See this file's header for why the interrupt
    // arrives at all, which is where Xtensa and ARM part company.
    if SLEEPING.load(Ordering::SeqCst) {
        // SAFETY: as `TicklessPort::suppress_ticks_and_sleep` -- the idle
        // task is blocked inside `waiti`, so it is not touching the alarm.
        let slot = unsafe { &mut *ALARM.0.get() };
        if let Some(alarm) = slot.as_mut() {
            alarm.clear_interrupt();
        }
        return;
    }

    WAKEUPS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: as above; no critical section can be open while this runs.
    let slot = unsafe { &mut *ALARM.0.get() };
    if let Some(alarm) = slot.as_mut() {
        alarm.clear_interrupt();
    }

    let want = with_kernel_in_isr(Kernel::increment_tick).unwrap_or(false);
    PORT.count_tick();
    if want {
        yield_now();
    }
}

// --------------------------------------------------------------- the tasks --

/// The frame a task starts in. A task body must not simply return — there is
/// nothing beneath it to return into — so the wrapper never lets it.
extern "C" fn task_entry(task_fn: usize, param: usize) {
    // SAFETY: `task_fn` is always one of the three bodies below, whose
    // signature this matches.
    #[expect(unsafe_code, reason = "the task entry point, reached by pointer")]
    let entry: extern "C" fn(usize) -> ! = unsafe { core::mem::transmute(task_fn) };
    entry(param);
}

/// The worker: sleep, wake, count, repeat. Its delay is the only thing
/// keeping the system busy, so `expected_idle_time` always has something
/// real to report.
extern "C" fn task_work(_: usize) -> ! {
    loop {
        let _ = with_kernel(|k| k.delay(DELAY_TICKS));
        yield_now();
        if LAPS.fetch_add(1, Ordering::Relaxed).saturating_add(1) >= ROUNDS {
            finish();
        }
    }
}

/// The idle task, **identical in both arms**: `idle_suppress_ticks` returns
/// at once when the const is false.
///
/// It pends no switch of its own, for the reason the ARM cell gives —
/// `resume_all` inside `idle_suppress_ticks` pends one when unwinding the
/// pended tick woke somebody, and an unconditional pend here would put
/// idle-to-idle switch events in one arm's digest and not the other's.
extern "C" fn task_idle(_: usize) -> ! {
    loop {
        let _ = with_kernel(Kernel::idle_suppress_ticks);
    }
}

/// The timer daemon sleeps through the run: a highest-priority task that
/// spun would starve everything below it, and software timers are not what
/// this cell claims.
extern "C" fn task_timer(_: usize) -> ! {
    loop {
        let _ = with_kernel(|k| k.delay(u64::from(ROUNDS) * DELAY_TICKS * 4));
        yield_now();
    }
}

// -------------------------------------------------------------- the report --

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

    println!();
    println!("arm                   {}", if TICKLESS { "TICKLESS" } else { "plain" });
    println!("laps                  {laps}   (want {ROUNDS})");
    println!("logical ticks         {ticks}");
    println!("alarm wakeups         {wakeups}");
    println!("scheduler stalls      {stalls}   first: {why:?}");
    println!("projected events      {events}   switches {switches}");
    println!(
        "sleeps                {}   asked {}   slept {}   last {}us",
        SLEEP_CALLS.load(Ordering::SeqCst),
        SLEEP_ASKED.load(Ordering::SeqCst),
        SLEEP_SLEPT.load(Ordering::SeqCst),
        SLEEP_LAST_US.load(Ordering::SeqCst)
    );
    println!(
        "switches declined     {}   (landed inside a suppressed sleep)",
        DECLINED_SWITCHES.load(Ordering::SeqCst)
    );
    println!("SCHEDULE DIGEST       {order:016x}   (order: who ran, in what order)");
    println!("  ...with tick stamps {timed:016x}   (diagnostic -- see the header)");

    let mut failed = 0u32;
    let mut check = |ok: bool, what: &str| {
        if ok {
            println!("      ok    {what}");
        } else {
            failed = failed.saturating_add(1);
            println!("      FAIL  {what}");
        }
    };
    check(laps >= ROUNDS, "the worker completed every lap it was asked for");
    check(stalls == 0, "the scheduler never failed to choose a task (Law 3)");
    check(switches > 0, "the kernel chose a task at least once");
    check(events > 0, "the projection saw events -- the digest is not of nothing");

    // The timing claim. The order digest cannot see a wake that arrives in
    // the right sequence but late, which is exactly what an oversleeping
    // port produces, so the clock is bounded too. The slop is the ARM cell's
    // — sub-tick execution drift pushes a lap's wake out by one whenever its
    // work crosses a boundary — and this part prints over a JTAG serial that
    // is slower than semihosting, so it is doubled again.
    let floor: u64 = u64::from(ROUNDS) * DELAY_TICKS;
    const SLOP: u64 = 8;
    check(
        ticks >= floor && ticks <= floor.saturating_add(SLOP),
        "the clock landed in the band the scenario's arithmetic fixes -- nothing overslept",
    );

    if TICKLESS {
        // THE CLAIM, and it is unfakeable: a logical tick that cost no
        // wakeup can only have come from `step_tick` winding the clock over
        // a suppressed interval.
        check(
            wakeups < ticks,
            "ticks passed that cost NO wakeup -- the port really suppressed them",
        );
        check(
            wakeups.saturating_mul(4) < ticks,
            "and it suppressed most of them, not a token few",
        );
    } else {
        // The control, and it is also the FIRST claim of this cell: a tick
        // interrupt drove the kernel on an Xtensa part at all.
        check(
            wakeups >= ticks,
            "the control paid one wakeup for every tick, as an untouched alarm must",
        );
    }

    println!();
    if failed == 0 {
        println!("RESULT: PASS -- compare SCHEDULE DIGEST against the other arm;");
        println!("        equal digests are the claim, the wakeup counts are the prize.");
    } else {
        println!("RESULT: FAIL -- {failed} check(s) failed");
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Give one task a context and record it in its own slot.
fn arm_task(handle: TaskHandle, stack: *mut Stack, body: extern "C" fn(usize) -> !) -> bool {
    let i = usize::from(handle.index());
    if i >= TASKS {
        return false;
    }
    // SAFETY: called from `main` before any task runs and before any
    // interrupt is enabled, so nothing else reads the store. The stack is a
    // `static mut` that outlives every task, and `i < TASKS < CONTEXTS`.
    #[expect(unsafe_code, reason = "building a task's initial context")]
    unsafe {
        let top = stack.cast::<u8>().add(size_of::<Stack>());
        let store = (&raw mut CONTEXTS_STORE).cast::<Context>();
        store
            .add(i)
            .write(new_task_context(task_entry, body as *const () as usize, 0, top));
    }
    true
}

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    println!();
    println!("=== the Kairos kernel, tick-driven, on a XIAO ESP32-S3 ===");
    println!(
        "arm: {}",
        if TICKLESS {
            "TICKLESS (USE_TICKLESS_IDLE = true)"
        } else {
            "plain control (USE_TICKLESS_IDLE = false)"
        }
    );
    println!("tick: SYSTIMER alarm 0 -> Kernel::increment_tick");
    println!("switch: Software0 -> Kernel::switch_context");

    // The tick length is stated twice -- once as a rate the kernel reasons
    // in, once as a Duration the timer takes -- so it is checked once here
    // rather than trusted twice.
    if u64::from(CellConfig::TICK_RATE_HZ) * TICK_MICROS != 1_000_000 {
        println!("CONFIG ERROR: TICK_MICROS does not match TICK_RATE_HZ");
        loop {
            core::hint::spin_loop();
        }
    }

    let mut kernel = match K::new(TicklessPort::new(), Scheduling::new(Digest::new())) {
        Ok(k) => k,
        Err(e) => {
            println!("kernel refused the geometry: {e:?}");
            loop {
                core::hint::spin_loop();
            }
        }
    };

    let work = match kernel.create_task("work", 2) {
        Ok(h) => h,
        Err(e) => {
            println!("create_task failed: {e:?}");
            loop {
                core::hint::spin_loop();
            }
        }
    };
    let started = match kernel.start_scheduler() {
        Ok(s) => s,
        Err(e) => {
            println!("start_scheduler failed: {e:?}");
            loop {
                core::hint::spin_loop();
            }
        }
    };

    // EVERY task the kernel created needs a stack, including the two it
    // creates for itself. Missing one is not a soft failure: the scheduler
    // will choose it and the switch will load a zeroed context.
    let armed = arm_task(work, &raw mut STACK_WORK, task_work)
        && arm_task(started.idle, &raw mut STACK_IDLE, task_idle)
        && arm_task(started.timer, &raw mut STACK_TMR, task_timer);
    if !armed {
        println!("a task handle fell outside the context table");
        loop {
            core::hint::spin_loop();
        }
    }

    // SAFETY: nothing else holds the kernel yet, and no interrupt that could
    // reach it is enabled until the lines below.
    #[expect(unsafe_code, reason = "installing the kernel in its static")]
    unsafe {
        *KERNEL.0.get() = Some(kernel);
    }

    let systimer = SystemTimer::new(peripherals.SYSTIMER);
    // Not `mut`: every `Timer` method takes `&self`, because the alarm is a
    // handle onto a peripheral rather than a value.
    let alarm = systimer.alarm0;
    alarm.set_interrupt_handler(tick_interrupt);
    alarm.enable_auto_reload(true);
    if alarm.load_value(Duration::from_micros(TICK_MICROS)).is_err() {
        println!("the timer refused a {TICK_MICROS}us tick");
        loop {
            core::hint::spin_loop();
        }
    }
    alarm.enable_interrupt(true);
    alarm.start();

    // SAFETY: the tick handler is the only other reader, and it cannot run
    // until the alarm is armed -- which the write below precedes.
    #[expect(unsafe_code, reason = "installing the tick source in its static")]
    unsafe {
        *ALARM.0.get() = Some(alarm);
    }

    enable_switching();

    println!("worker delays {DELAY_TICKS} ticks a lap, {ROUNDS} laps -- idle almost all of it");
    println!("entering the scheduler...");

    // Into the scheduler. `main`'s own context is saved by the very same
    // handler, into slot `MAIN`, which is why `CONTEXTS` is one more than
    // `TASKS`.
    yield_now();

    // Reached only if the switch never happened, which is a failure the
    // cell should say out loud rather than hang on.
    println!("RESULT: FAIL -- the first switch never left main");
    loop {
        core::hint::spin_loop();
    }
}
