#![no_std]
#![no_main]
#![cfg_attr(target_arch = "xtensa", feature(asm_experimental_arch))]
//! Nested interrupts on the Kairos kernel, on a XIAO ESP32-S3.
//!
//! FreeRTOS's `IntQueue` demo exists to prove that queues stay consistent
//! when interrupts of DIFFERENT priorities use them, including one
//! interrupting another. The Kairos corpus runs `IntQueue` on the sim, and
//! the sim cannot nest: its one-core state machine takes one interrupt at a
//! time, which is why that scenario matches the C only up to 2,000 ticks and
//! why the mission plan lists "the nesting the demo was written for" as not
//! exercised. Silicon can nest, so this cell does.
//!
//! # The rule that makes nesting safe
//!
//! EVERY kernel entry -- task, tick, switch, and both timer interrupts --
//! masks to level 3 and RESTORES the saved `PS` on the way out:
//! `portSET_INTERRUPT_MASK_FROM_ISR`, with `configMAX_SYSCALL_INTERRUPT_PRIORITY`
//! at level 3. A handler that assumes it "already has exclusivity" because
//! it is an interrupt is only right while nothing above its level calls the
//! kernel. Here something does.
//!
//! # The workload
//!
//! * `low`: TIMG0, **level 1**, every 97 µs. It sends `(1 << 32) | seq` to
//!   `from_isr`, then BUSY-WAITS 30 µs with interrupts open (outside any
//!   kernel section) to hold a wide window open for nesting, then receives
//!   from `to_isr`.
//! * `high`: TIMG1, **level 3**, every 41 µs. If `low` is mid-flight, that is
//!   a NESTED interrupt and it is counted. It sends `(2 << 32) | seq` to
//!   `from_isr`.
//! * `consumer` (task): receives from `from_isr` and checks each source's
//!   sequence: strictly increasing, with any gap fully explained by that
//!   source's own `queue full` refusals.
//! * `producer` (task): sends an increasing sequence to `to_isr`, which `low`
//!   checks.
//!
//! **PASS** requires nesting to have happened (counted) and, across every
//! value sent:
//! * zero order violations;
//! * every value either received or refused as `full`, never lost.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use esp_backtrace as _;
use esp_hal::interrupt::{InterruptHandler, Priority};
use esp_hal::time::{Duration, Instant};
use esp_hal::timer::systimer::{Alarm, SystemTimer};
use esp_hal::timer::timg::TimerGroup;
use esp_hal::timer::{PeriodicTimer, Timer};
use esp_println::println;

use rusty_rtos_core::config::Config;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_core::hooks::NoTickHook;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::tick::Bits32;
use rusty_rtos_core::trace::{Event, Trace};
use rusty_rtos_kernel_core::queue::Wait;
use rusty_rtos_kernel_core::{list_slots_for, lists_for, Kernel};
use rusty_rtos_port_xtensa::{
    clear_switch_request, enable_switching, new_task_context, switch_context, yield_now, Context,
    XtensaPort,
};

esp_bootloader_esp_idf::esp_app_desc!();

#[derive(Debug, Clone, Copy, Default)]
pub struct NestConfig;

impl Config for NestConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = 5;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 4;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 2;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
}

const TASKS: usize = 6;
const QUEUES: usize = 4;
const SLOTS: usize = 64;
const TIMERS: usize = 1;
const GROUPS: usize = 1;
const CONTEXTS: usize = TASKS + 1;
const BOOT: usize = TASKS;
const QUEUE_LEN: usize = 16;
/// How long the run lasts.
const RUN_MS: u64 = 5_000;

#[derive(Debug, Default)]
pub struct NoTrace;
impl Trace for NoTrace {
    const WANTS_NAMES: bool = false;
    fn event(&mut self, _tick: u64, _event: Event<'_>) {}
}

// -------------------------------------------------------------------- port --

/// The kernel's port. Its critical sections are empty because every kernel
/// entry is already inside [`with_kernel`]'s mask -- see the module docs.
struct NestPort {
    inner: XtensaPort,
}

impl Port for NestPort {
    const COMMITS_SWITCH: bool = true;
    fn yield_now(&self) {
        yield_now();
    }
    fn yield_from_isr(&self, woken: Woken) {
        if woken == Woken::YES {
            yield_now();
        }
    }
    fn enter_critical(&self) {}
    fn exit_critical(&self) {}
    fn set_interrupt_mask_from_isr(&self) -> u32 {
        0
    }
    fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}
    fn in_isr(&self) -> bool {
        self.inner.in_isr()
    }
    fn count_tick(&self) {
        self.inner.count_tick();
    }
    fn count_yield(&self) {
        self.inner.count_yield();
    }
}

type K = Kernel<
    NestConfig,
    NestPort,
    NoTrace,
    NoTickHook,
    TASKS,
    {
        list_slots_for(
            TASKS,
            TIMERS,
            lists_for(NestConfig::MAX_PRIORITIES, QUEUES, GROUPS),
        )
    },
    { lists_for(NestConfig::MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    1,
    8,
    TIMERS,
    GROUPS,
    { <NestConfig as Config>::TIMER_QUEUE_LENGTH },
>;

struct KernelCell(UnsafeCell<Option<K>>);
// SAFETY: every access is inside `with_kernel`'s mask, on one core.
unsafe impl Sync for KernelCell {}
static KERNEL: KernelCell = KernelCell(UnsafeCell::new(None));

#[cfg(target_arch = "xtensa")]
#[inline(always)]
fn mask() -> u32 {
    let ps: u32;
    // SAFETY: raises INTLEVEL; touches no memory.
    unsafe { core::arch::asm!("rsil {0}, 3", out(reg) ps, options(nostack)) };
    ps
}
#[cfg(target_arch = "xtensa")]
#[inline(always)]
fn unmask(ps: u32) {
    // SAFETY: restores a state this core was already in -- which, inside a
    // level-1 handler, is level 1 and NOT zero. That restore is the whole
    // difference between nesting safely and unmasking a handler mid-flight.
    unsafe { core::arch::asm!("wsr.ps {0}", "rsync", in(reg) ps, options(nostack)) };
}
#[cfg(not(target_arch = "xtensa"))]
fn mask() -> u32 {
    0
}
#[cfg(not(target_arch = "xtensa"))]
fn unmask(_: u32) {}

/// The one way into the kernel, from any level at or below 3.
fn with_kernel<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    let ps = mask();
    // SAFETY: masked to level 3 on the only core; nothing that touches the
    // kernel can run until `unmask`.
    let out = unsafe { (*KERNEL.0.get()).as_mut() }.map(f);
    unmask(ps);
    out
}

// ------------------------------------------------------------- the switch --

#[repr(align(16))]
struct Stack(
    #[allow(dead_code, reason = "addressed as storage, never read as a field")] [u8; 6144],
);
const STACK_INIT: Stack = Stack([0; 6144]);
static mut STACKS: [Stack; TASKS] = [STACK_INIT; TASKS];
static mut CONTEXTS_STORE: [Context; CONTEXTS] =
    [const { unsafe { core::mem::zeroed() } }; CONTEXTS];
static CURRENT: AtomicU32 = AtomicU32::new(BOOT as u32);

#[esp_hal::ram]
#[unsafe(export_name = "Software0")]
fn switching_interrupt(trap_frame: &mut Context) {
    clear_switch_request();
    let ps = mask();
    // SAFETY: masked; see `with_kernel`.
    if let Some(k) = unsafe { (*KERNEL.0.get()).as_mut() } {
        k.switch_context();
        let to = k.current().index() as usize;
        let from = CURRENT.load(Ordering::Relaxed) as usize;
        if to < TASKS && from != to && from < CONTEXTS {
            CURRENT.store(to as u32, Ordering::Relaxed);
            // SAFETY: indices in range; masked, one core.
            unsafe {
                let base = (&raw mut CONTEXTS_STORE).cast::<Context>();
                switch_context(Some(base.add(from)), base.add(to), trap_frame);
            }
        }
    }
    unmask(ps);
}

// ------------------------------------------------------------- interrupts --

struct Cell<T>(UnsafeCell<Option<T>>);
// SAFETY: each is touched only by its own handler and by `main` before the
// handler is enabled.
unsafe impl<T> Sync for Cell<T> {}

static ALARM: Cell<Alarm<'static>> = Cell(UnsafeCell::new(None));
static LOW_TIMER: Cell<PeriodicTimer<'static, esp_hal::Blocking>> = Cell(UnsafeCell::new(None));
static HIGH_TIMER: Cell<PeriodicTimer<'static, esp_hal::Blocking>> = Cell(UnsafeCell::new(None));

static QUEUES_MADE: Cell<(QueueHandle, QueueHandle)> = Cell(UnsafeCell::new(None));
fn queues() -> (QueueHandle, QueueHandle) {
    // SAFETY: written before any handler or task runs, read-only after.
    unsafe { (*QUEUES_MADE.0.get()).unwrap_or((QueueHandle::NULL, QueueHandle::NULL)) }
}

static RUNNING: AtomicBool = AtomicBool::new(true);
/// `low` is between its entry and its exit.
static IN_LOW: AtomicBool = AtomicBool::new(false);
static NESTED: AtomicU32 = AtomicU32::new(0);
/// Deepest observed: a `high` arriving while `low` was INSIDE a kernel
/// section would be a defect, so it is counted separately and must be zero.
static NESTED_IN_KERNEL: AtomicU32 = AtomicU32::new(0);
static LOW_IN_KERNEL: AtomicBool = AtomicBool::new(false);

/// Per source (index 0 = `low`, 1 = `high`): values sent, refused as full.
static SENT: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static REFUSED: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
/// What the ISR side received from `producer`, and order violations seen.
static ISR_RECEIVED: AtomicU32 = AtomicU32::new(0);
static ISR_ORDER_BAD: AtomicU32 = AtomicU32::new(0);
/// 32 bits: the part has no 64-bit atomics, and the sequence stays far below.
static ISR_LAST: AtomicU32 = AtomicU32::new(0);
static TICKS: AtomicU32 = AtomicU32::new(0);

/// SYSTIMER alarm 0, level 1: the tick.
extern "C" fn tick_isr() {
    // SAFETY: this handler is the alarm's only user once enabled.
    if let Some(a) = unsafe { (*ALARM.0.get()).as_mut() } {
        a.clear_interrupt();
    }
    TICKS.fetch_add(1, Ordering::Relaxed);
    if with_kernel(Kernel::increment_tick).unwrap_or(false) {
        yield_now();
    }
}

/// TIMG0, level 1.
extern "C" fn low_isr() {
    // SAFETY: its own timer.
    if let Some(t) = unsafe { (*LOW_TIMER.0.get()).as_mut() } {
        t.clear_interrupt();
    }
    if !RUNNING.load(Ordering::Relaxed) {
        return;
    }
    IN_LOW.store(true, Ordering::SeqCst);
    let (from_isr, to_isr) = queues();
    let seq = u64::from(SENT[0].fetch_add(1, Ordering::Relaxed));
    let send = |k: &mut K| {
        LOW_IN_KERNEL.store(true, Ordering::SeqCst);
        let r = k.queue_send_from_isr(from_isr, (1 << 32) | seq);
        LOW_IN_KERNEL.store(false, Ordering::SeqCst);
        r
    };
    // `--features poison`: enter the kernel WITHOUT the mask, the way a
    // handler that believes "an interrupt already has exclusivity" would.
    // The instrument must catch it -- a check that cannot fail is not one.
    #[cfg(feature = "poison")]
    // SAFETY: deliberately unsound; the poison arm of the experiment.
    let woken = unsafe { (*KERNEL.0.get()).as_mut() }.map(send);
    #[cfg(not(feature = "poison"))]
    let woken = with_kernel(send);
    let mut wake = false;
    match woken {
        Some(Ok(w)) => wake |= w == Woken::YES,
        _ => {
            REFUSED[0].fetch_add(1, Ordering::Relaxed);
        }
    }
    // The nesting window: interrupts are OPEN here (outside the mask), so a
    // level-3 interrupt can and should land in it.
    let t = Instant::now();
    while t.elapsed() < Duration::from_micros(30) {}
    if let Some(Ok((value, w))) = with_kernel(|k| k.queue_receive_from_isr(to_isr)) {
        wake |= w == Woken::YES;
        let value = value as u32;
        let last = ISR_LAST.swap(value, Ordering::Relaxed);
        if value <= last && last != 0 {
            ISR_ORDER_BAD.fetch_add(1, Ordering::Relaxed);
        }
        ISR_RECEIVED.fetch_add(1, Ordering::Relaxed);
    }
    IN_LOW.store(false, Ordering::SeqCst);
    if wake {
        yield_now();
    }
}

/// TIMG1, level 3: preempts `low`.
extern "C" fn high_isr() {
    // SAFETY: its own timer.
    if let Some(t) = unsafe { (*HIGH_TIMER.0.get()).as_mut() } {
        t.clear_interrupt();
    }
    if !RUNNING.load(Ordering::Relaxed) {
        return;
    }
    if IN_LOW.load(Ordering::SeqCst) {
        NESTED.fetch_add(1, Ordering::Relaxed);
    }
    if LOW_IN_KERNEL.load(Ordering::SeqCst) {
        NESTED_IN_KERNEL.fetch_add(1, Ordering::Relaxed);
    }
    let (from_isr, _) = queues();
    let seq = u64::from(SENT[1].fetch_add(1, Ordering::Relaxed));
    match with_kernel(|k| k.queue_send_from_isr(from_isr, (2 << 32) | seq)) {
        Some(Ok(w)) => {
            if w == Woken::YES {
                // A level-1 software interrupt, pended from level 3: it is
                // taken only when every handler above it has returned --
                // `portEND_SWITCHING_ISR` deferred to the outermost exit.
                yield_now();
            }
        }
        _ => {
            REFUSED[1].fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ------------------------------------------------------------- the tasks --

extern "C" fn task_entry(task_fn: usize, param: usize) {
    // SAFETY: `task_fn` is always one of the bodies below.
    let entry: extern "C" fn(usize) -> ! = unsafe { core::mem::transmute(task_fn) };
    entry(param);
}

/// What the consumer received per source, and order violations.
static RECEIVED: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static ORDER_BAD: AtomicU32 = AtomicU32::new(0);
static MALFORMED: AtomicU32 = AtomicU32::new(0);

extern "C" fn task_consumer(_: usize) -> ! {
    let (from_isr, _) = queues();
    let mut next = [0u64; 2];
    loop {
        let value = loop {
            match with_kernel(|k| k.queue_receive(from_isr, 50)) {
                Some(Ok(Wait::Ready(v))) => break Some(v),
                Some(Ok(Wait::Blocked)) => yield_now(),
                _ => break None,
            }
        };
        let Some(v) = value else { continue };
        let source = (v >> 32) as usize;
        let seq = v & 0xffff_ffff;
        if !(1..=2).contains(&source) {
            MALFORMED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let i = source - 1;
        // Strictly increasing per source. A gap is allowed -- the ISR may
        // have found the queue full -- and is reconciled at the end; going
        // BACKWARDS or repeating is not.
        if seq < next[i] {
            ORDER_BAD.fetch_add(1, Ordering::Relaxed);
        }
        next[i] = seq + 1;
        RECEIVED[i].fetch_add(1, Ordering::Relaxed);
    }
}

static PRODUCED: AtomicU32 = AtomicU32::new(0);

extern "C" fn task_producer(_: usize) -> ! {
    let (_, to_isr) = queues();
    let mut seq = 1u64;
    loop {
        match with_kernel(|k| k.queue_send(to_isr, seq, 5)) {
            Some(Ok(Wait::Ready(()))) => {
                PRODUCED.fetch_add(1, Ordering::Relaxed);
                seq += 1;
            }
            Some(Ok(Wait::Blocked)) => yield_now(),
            _ => {}
        }
        if !RUNNING.load(Ordering::Relaxed) {
            loop {
                sleep(1000);
            }
        }
    }
}

fn sleep(ticks: u64) {
    let _ = with_kernel(|k| k.delay(ticks));
    yield_now();
}

extern "C" fn task_idle(_: usize) -> ! {
    loop {
        #[cfg(target_arch = "xtensa")]
        // SAFETY: waits for an interrupt; touches no memory.
        unsafe {
            core::arch::asm!("waiti 0", options(nostack));
        }
    }
}

extern "C" fn task_timer(_: usize) -> ! {
    loop {
        sleep(100_000);
    }
}

extern "C" fn task_ctl(_: usize) -> ! {
    sleep(RUN_MS);
    RUNNING.store(false, Ordering::SeqCst);
    // Let the consumer drain what is still queued.
    sleep(100);
    let sent = [
        SENT[0].load(Ordering::Relaxed),
        SENT[1].load(Ordering::Relaxed),
    ];
    let refused = [
        REFUSED[0].load(Ordering::Relaxed),
        REFUSED[1].load(Ordering::Relaxed),
    ];
    let received = [
        RECEIVED[0].load(Ordering::Relaxed),
        RECEIVED[1].load(Ordering::Relaxed),
    ];
    let nested = NESTED.load(Ordering::Relaxed);
    let nested_in_kernel = NESTED_IN_KERNEL.load(Ordering::Relaxed);
    let order_bad = ORDER_BAD.load(Ordering::Relaxed);
    let isr_bad = ISR_ORDER_BAD.load(Ordering::Relaxed);
    let malformed = MALFORMED.load(Ordering::Relaxed);
    println!();
    println!(
        "NEST run {RUN_MS} ms, ticks {}",
        TICKS.load(Ordering::Relaxed)
    );
    println!(
        "NEST low  (level 1) sent {} refused {} received {}",
        sent[0], refused[0], received[0]
    );
    println!(
        "NEST high (level 3) sent {} refused {} received {}",
        sent[1], refused[1], received[1]
    );
    println!(
        "NEST nested {nested} (a level-3 interrupt inside a level-1 one); inside a kernel section {nested_in_kernel}"
    );
    println!(
        "NEST to_isr: produced {} received by low {} order_bad {isr_bad}",
        PRODUCED.load(Ordering::Relaxed),
        ISR_RECEIVED.load(Ordering::Relaxed)
    );
    println!("NEST consumer order_bad {order_bad} malformed {malformed}");
    let accounted = (0..2).all(|i| received[i] + refused[i] == sent[i]);
    let pass = nested > 0
        && nested_in_kernel == 0
        && order_bad == 0
        && isr_bad == 0
        && malformed == 0
        && accounted
        && received[0] > 0
        && received[1] > 0;
    println!();
    if pass {
        println!(
            "RESULT: PASS -- {nested} nested interrupts, every value received in order or refused as full"
        );
    } else {
        println!("RESULT: FAIL -- accounted={accounted} (sent = received + refused, per source)");
    }
    loop {
        sleep(100_000);
    }
}

fn arm(handle: TaskHandle, body: extern "C" fn(usize) -> !) -> bool {
    let i = handle.index() as usize;
    if i >= TASKS {
        return false;
    }
    // SAFETY: before any task runs; one stack and slot per task.
    unsafe {
        let stack = (&raw mut STACKS).cast::<Stack>().add(i);
        let top = stack.cast::<u8>().add(core::mem::size_of::<Stack>());
        (&raw mut CONTEXTS_STORE)
            .cast::<Context>()
            .add(i)
            .write(new_task_context(
                task_entry,
                body as *const () as usize,
                0,
                top,
            ));
    }
    true
}

fn halt(why: &str) -> ! {
    println!("RESULT: FAIL -- {why}");
    loop {
        core::hint::spin_loop();
    }
}

#[esp_hal::main]
fn main() -> ! {
    let p =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()));
    println!();
    println!("=== nested interrupts on the Kairos kernel, XIAO ESP32-S3 ===");
    println!("low: TIMG0 level 1 every 97 us (30 us busy window); high: TIMG1 level 3 every 41 us");

    let mut k = K::new(
        NestPort {
            inner: XtensaPort::new(),
        },
        NoTrace,
    )
    .unwrap_or_else(|_| halt("geometry"));
    let mk =
        |k: &mut K, n: &str, pr: u8| k.create_task(n, pr).unwrap_or_else(|_| halt("create_task"));
    let ctl = mk(&mut k, "ctl", 3);
    let consumer = mk(&mut k, "consumer", 2);
    let producer = mk(&mut k, "producer", 1);
    let from_isr = k.queue_create(QUEUE_LEN).unwrap_or_else(|_| halt("queue"));
    let to_isr = k.queue_create(QUEUE_LEN).unwrap_or_else(|_| halt("queue"));
    // SAFETY: nothing runs yet.
    unsafe {
        *QUEUES_MADE.0.get() = Some((from_isr, to_isr));
    }
    let started = k.start_scheduler().unwrap_or_else(|_| halt("start"));
    if !(arm(ctl, task_ctl)
        && arm(consumer, task_consumer)
        && arm(producer, task_producer)
        && arm(started.idle, task_idle)
        && arm(started.timer, task_timer))
    {
        halt("a task index fell outside the context table");
    }
    // SAFETY: nothing runs yet.
    unsafe {
        *KERNEL.0.get() = Some(k);
    }

    let alarm = SystemTimer::new(p.SYSTIMER).alarm0;
    alarm.set_interrupt_handler(InterruptHandler::new(tick_isr, Priority::Priority1));
    alarm.enable_auto_reload(true);
    if alarm.load_value(Duration::from_millis(1)).is_err() {
        halt("tick");
    }
    alarm.enable_interrupt(true);
    alarm.start();
    // SAFETY: before the handler can run meaningfully; its only user after.
    unsafe {
        *ALARM.0.get() = Some(alarm);
    }

    let mut low = PeriodicTimer::new(TimerGroup::new(p.TIMG0).timer0);
    low.set_interrupt_handler(InterruptHandler::new(low_isr, Priority::Priority1));
    low.listen();
    let mut high = PeriodicTimer::new(TimerGroup::new(p.TIMG1).timer0);
    high.set_interrupt_handler(InterruptHandler::new(high_isr, Priority::Priority3));
    high.listen();
    if low.start(Duration::from_micros(97)).is_err()
        || high.start(Duration::from_micros(41)).is_err()
    {
        halt("timers");
    }
    // SAFETY: each handler's only user after this.
    unsafe {
        *LOW_TIMER.0.get() = Some(low);
        *HIGH_TIMER.0.get() = Some(high);
    }

    enable_switching();
    yield_now();
    halt("the first switch never left main");
}
