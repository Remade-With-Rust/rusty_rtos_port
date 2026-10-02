#![no_std]
#![no_main]
#![cfg_attr(target_arch = "xtensa", feature(asm_experimental_arch))]
//! K8 SMP, slice S3: ONE Kairos kernel scheduling BOTH cores of an ESP32-S3.
//!
//! `rusty_rtos_kernel`'s SMP slice S1 (`docs/plans/smp.md` in that repo) is
//! the scheduler: per-core `current`, `prvSelectHighestPriorityTask`,
//! `prvYieldForTask`. This cell is the port half that a chip needs and a
//! host test cannot have:
//!
//! * **one kernel, one lock.** Every entry -- a task's call, the tick, the
//!   switch -- masks interrupts on its own core AND takes a cross-core
//!   spinlock, so exactly one core is inside the kernel at a time
//!   (`portGET_TASK_LOCK` + `portGET_ISR_LOCK`, collapsed into one).
//! * **a switch per core.** `Software0` is a CPU-internal interrupt, so each
//!   core has its own; the handler asks the kernel to switch THIS core
//!   (`core_id` is `PRID`). The context copy happens INSIDE the lock: a task
//!   another core may select next must not be loaded before its registers
//!   are saved.
//! * **a yield across cores.** The kernel answers `take_core_yields()`; the
//!   lock's exit raises `FROM_CPU_INTR<n>` for each core named, whose handler
//!   on core `n` raises that core's own `Software0` (`portYIELD_CORE`).
//!
//! # What it checks
//!
//! 1. **Parallelism.** Two spinners at the same priority do a fixed amount of
//!    work. One core would take twice as long as one spinner alone; two must
//!    take about the same as one, and each spinner must report a different
//!    core.
//! 2. **Cross-core hand-off.** Two tasks ping-pong a semaphore pair. While
//!    one runs the other's core is idle, so every give readies a task on the
//!    OTHER core and needs an inter-processor yield. The laps must complete,
//!    the two tasks must have run on different cores, and the inter-processor
//!    interrupts taken are counted.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use esp_backtrace as _;
use esp_hal::interrupt::software::SoftwareInterrupt;
use esp_hal::system::{Cpu, CpuControl, Stack as CoreStack};
use esp_hal::time::{Duration, Instant};
use esp_hal::timer::systimer::{Alarm, SystemTimer};
use esp_hal::timer::Timer;
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

// ------------------------------------------------------------------ config --

#[derive(Debug, Clone, Copy, Default)]
pub struct SmpConfig;

impl Config for SmpConfig {
    type Tick = Bits32;
    const TICK_RATE_HZ: u32 = 1000;
    const MAX_PRIORITIES: u8 = 6;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 8;
    const TIMER_TASK_PRIORITY: u8 = 5;
    const TIMER_TASK_STACK_DEPTH: usize = 128;
    const TIMER_QUEUE_LENGTH: usize = 2;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 1;
    const NUMBER_OF_CORES: u8 = 2;
}

const TASKS: usize = 8;
const QUEUES: usize = 8;
const SLOTS: usize = 16;
const TIMERS: usize = 1;
const GROUPS: usize = 1;
/// Task contexts, plus the two boot contexts each core starts on.
const CONTEXTS: usize = TASKS + 2;
const BOOT: [usize; 2] = [TASKS, TASKS + 1];

/// Iterations of the spin workload. Sized for tens of milliseconds at
/// 240 MHz, long enough that the switch and wake overheads are noise.
const SPIN: u32 = 4_000_000;
/// Ping-pong laps.
const LAPS: u32 = 2_000;

#[derive(Debug, Default)]
pub struct NoTrace;
impl Trace for NoTrace {
    const WANTS_NAMES: bool = false;
    fn event(&mut self, _tick: u64, _event: Event<'_>) {}
}

// -------------------------------------------------------------------- port --

/// Which core is executing: `PRID` through esp-hal.
#[inline(always)]
fn core_index() -> usize {
    Cpu::current() as usize
}

/// The port the SMP kernel runs on.
///
/// Its critical sections are EMPTY on purpose: the kernel is only ever
/// entered through [`with_kernel`], which already holds this core's mask and
/// the cross-core lock, so a nested mask would protect nothing more. The
/// counters come from the shared [`XtensaPort`].
struct SmpPort {
    inner: XtensaPort,
}

impl Port for SmpPort {
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
    fn core_id(&self) -> u8 {
        core_index() as u8
    }
    fn count_tick(&self) {
        self.inner.count_tick();
    }
    fn count_yield(&self) {
        self.inner.count_yield();
    }
}

type K = Kernel<
    SmpConfig,
    SmpPort,
    NoTrace,
    NoTickHook,
    TASKS,
    {
        list_slots_for(
            TASKS,
            TIMERS,
            lists_for(SmpConfig::MAX_PRIORITIES, QUEUES, GROUPS),
        )
    },
    { lists_for(SmpConfig::MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    1,
    8,
    TIMERS,
    GROUPS,
    { <SmpConfig as Config>::TIMER_QUEUE_LENGTH },
>;

struct KernelCell(UnsafeCell<Option<K>>);
// SAFETY: every access holds `LOCK` with this core's interrupts masked.
unsafe impl Sync for KernelCell {}
static KERNEL: KernelCell = KernelCell(UnsafeCell::new(None));

/// The one kernel lock, shared by both cores.
static LOCK: AtomicBool = AtomicBool::new(false);
/// How often a core found the lock taken and had to spin -- contention, as
/// a number rather than a guess.
static CONTENDED: AtomicU32 = AtomicU32::new(0);

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
    // SAFETY: restores a state this core was already in.
    unsafe { core::arch::asm!("wsr.ps {0}", "rsync", in(reg) ps, options(nostack)) };
}

#[cfg(not(target_arch = "xtensa"))]
fn mask() -> u32 {
    0
}
#[cfg(not(target_arch = "xtensa"))]
fn unmask(_: u32) {}

/// Run `f` on the kernel with this core masked and the other core locked
/// out, then raise every cross-core yield the kernel asked for.
///
/// The IPIs go out AFTER the lock is released, so the interrupted core can
/// take the kernel the moment its handler runs.
fn with_kernel<R>(f: impl FnOnce(&mut K) -> R) -> Option<R> {
    let ps = mask();
    lock();
    // SAFETY: masked here and locked against the other core.
    let k = unsafe { &mut *KERNEL.0.get() };
    let out = k.as_mut().map(f);
    let yields = k.as_mut().map(Kernel::take_core_yields).unwrap_or(0);
    LOCK.store(false, Ordering::Release);
    unmask(ps);
    raise_core_yields(yields);
    out
}

fn lock() {
    if LOCK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return;
    }
    CONTENDED.fetch_add(1, Ordering::Relaxed);
    while LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

// --------------------------------------------------- cross-core interrupts --

struct Ipi<const N: u8>(UnsafeCell<Option<SoftwareInterrupt<'static, N>>>);
// SAFETY: written once by the core that binds it, before any other core can
// need it; read-only afterwards (`raise` takes `&self`).
unsafe impl<const N: u8> Sync for Ipi<N> {}
static IPI0: Ipi<0> = Ipi(UnsafeCell::new(None));
static IPI1: Ipi<1> = Ipi(UnsafeCell::new(None));
/// Inter-processor yields TAKEN, per core.
static IPIS_TAKEN: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

fn raise_core_yields(mask: u8) {
    if mask & 0b01 != 0 {
        // SAFETY: see `Ipi`.
        if let Some(sw) = unsafe { &*IPI0.0.get() } {
            sw.raise();
        }
    }
    if mask & 0b10 != 0 {
        // SAFETY: see `Ipi`.
        if let Some(sw) = unsafe { &*IPI1.0.get() } {
            sw.raise();
        }
    }
}

/// `FROM_CPU_INTR0`, bound on core 0: another core asked core 0 to yield.
#[esp_hal::handler]
fn ipi_core0() {
    // SAFETY: see `Ipi`.
    if let Some(sw) = unsafe { &*IPI0.0.get() } {
        sw.reset();
    }
    IPIS_TAKEN[0].fetch_add(1, Ordering::Relaxed);
    yield_now();
}

/// `FROM_CPU_INTR1`, bound on core 1.
#[esp_hal::handler]
fn ipi_core1() {
    // SAFETY: see `Ipi`.
    if let Some(sw) = unsafe { &*IPI1.0.get() } {
        sw.reset();
    }
    IPIS_TAKEN[1].fetch_add(1, Ordering::Relaxed);
    yield_now();
}

// ------------------------------------------------------------- the switch --

/// A task stack: 16-byte aligned, which the Xtensa ABI requires of `SP`.
#[repr(align(16))]
struct Stack(
    #[allow(dead_code, reason = "addressed as storage, never read as a field")] [u8; 6144],
);
const STACK_INIT: Stack = Stack([0; 6144]);
static mut STACKS: [Stack; TASKS] = [STACK_INIT; TASKS];

static mut CONTEXTS_STORE: [Context; CONTEXTS] =
    [const { unsafe { core::mem::zeroed() } }; CONTEXTS];
/// Which context each core is running. Written only by that core's switch
/// handler, under the lock.
static CURRENT: [AtomicU32; 2] = [
    AtomicU32::new(BOOT[0] as u32),
    AtomicU32::new(BOOT[1] as u32),
];
static SWITCHES: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// `Software0`: switch THIS core.
#[esp_hal::ram]
#[unsafe(export_name = "Software0")]
fn switching_interrupt(trap_frame: &mut Context) {
    clear_switch_request();
    let core = core_index();
    let ps = mask();
    lock();
    // SAFETY: masked and locked, as `with_kernel`.
    let k = unsafe { &mut *KERNEL.0.get() };
    let mut yields = 0;
    if let Some(k) = k.as_mut() {
        k.switch_context();
        let to = k.current_on(core).index() as usize;
        let from = CURRENT[core].load(Ordering::Relaxed) as usize;
        if to < TASKS && from != to && from < CONTEXTS {
            CURRENT[core].store(to as u32, Ordering::Relaxed);
            SWITCHES[core].fetch_add(1, Ordering::Relaxed);
            // Inside the lock: the moment it is released, the other core may
            // select `from` and load its context, so it must be saved first.
            //
            // SAFETY: both indices are in range, and `from`/`to` are each
            // held by exactly one core, so no other core touches these slots.
            unsafe {
                let base = (&raw mut CONTEXTS_STORE).cast::<Context>();
                switch_context(Some(base.add(from)), base.add(to), trap_frame);
            }
        }
        yields = k.take_core_yields();
    }
    LOCK.store(false, Ordering::Release);
    unmask(ps);
    raise_core_yields(yields);
}

// ------------------------------------------------------------------ the tick --

struct AlarmCell(UnsafeCell<Option<Alarm<'static>>>);
// SAFETY: core 0 only -- the tick is bound there and nothing else touches it.
unsafe impl Sync for AlarmCell {}
static ALARM: AlarmCell = AlarmCell(UnsafeCell::new(None));
static TICKS: AtomicU32 = AtomicU32::new(0);

/// SYSTIMER alarm 0, on core 0: `xTaskIncrementTick` and, per the SMP tick,
/// a yield here and/or an IPI to the other core.
#[esp_hal::handler]
fn tick_interrupt() {
    // SAFETY: core 0 only.
    if let Some(alarm) = unsafe { (*ALARM.0.get()).as_mut() } {
        alarm.clear_interrupt();
    }
    TICKS.fetch_add(1, Ordering::Relaxed);
    if with_kernel(Kernel::increment_tick).unwrap_or(false) {
        yield_now();
    }
}

// ------------------------------------------------------------- the tasks --

extern "C" fn task_entry(task_fn: usize, param: usize) {
    // SAFETY: `task_fn` is always one of the bodies below.
    let entry: extern "C" fn(usize) -> ! = unsafe { core::mem::transmute(task_fn) };
    entry(param);
}

/// Block on `sem` until it is given.
fn take(sem: QueueHandle) {
    loop {
        match with_kernel(|k| k.semaphore_take(sem, u64::MAX)) {
            Some(Ok(Wait::Ready(()))) => return,
            Some(Ok(Wait::Blocked)) => yield_now(),
            _ => return,
        }
    }
}

fn give(sem: QueueHandle) {
    loop {
        match with_kernel(|k| k.semaphore_give(sem)) {
            Some(Ok(Wait::Blocked)) => yield_now(),
            _ => return,
        }
    }
}

fn sleep(ticks: u64) {
    let _ = with_kernel(|k| k.delay(ticks));
    yield_now();
}

#[derive(Clone, Copy)]
struct Sems {
    go: QueueHandle,
    done: QueueHandle,
    ping: QueueHandle,
    pong: QueueHandle,
    pp_go: QueueHandle,
}
static mut SEMS: Option<Sems> = None;
fn sems() -> Sems {
    // SAFETY: written in `main` before any task runs, read-only after.
    unsafe { (*(&raw const SEMS)).unwrap_or_else(|| loop {}) }
}

fn spin(n: u32) -> u32 {
    let mut x = 0u32;
    for i in 0..n {
        x = core::hint::black_box(x.wrapping_mul(1_664_525).wrapping_add(i));
    }
    x
}

/// Which core each spinner ran on: bit 0 = core 0, bit 1 = core 1, sampled
/// throughout its run.
static SPINNER_CORES: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

extern "C" fn task_spinner(which: usize) -> ! {
    let s = sems();
    take(s.go);
    for chunk in 0..16 {
        let _ = chunk;
        SPINNER_CORES[which].fetch_or(1 << core_index(), Ordering::Relaxed);
        core::hint::black_box(spin(SPIN / 16));
    }
    give(s.done);
    loop {
        sleep(100_000);
    }
}

static PING_LAPS: AtomicU32 = AtomicU32::new(0);
/// Laps where the pinger and the ponger reported different cores.
static PING_CORES: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
static PONG_CORES: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

extern "C" fn task_ping(_: usize) -> ! {
    let s = sems();
    take(s.pp_go);
    for _ in 0..LAPS {
        PING_CORES[core_index()].fetch_add(1, Ordering::Relaxed);
        give(s.ping);
        take(s.pong);
        PING_LAPS.fetch_add(1, Ordering::Relaxed);
    }
    give(s.done);
    loop {
        sleep(100_000);
    }
}

extern "C" fn task_pong(_: usize) -> ! {
    let s = sems();
    loop {
        take(s.ping);
        PONG_CORES[core_index()].fetch_add(1, Ordering::Relaxed);
        give(s.pong);
    }
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

/// The controller: measures one spin alone, then two in parallel, then the
/// ping-pong, and reports.
extern "C" fn task_ctl(_: usize) -> ! {
    let s = sems();
    let t = Instant::now();
    core::hint::black_box(spin(SPIN));
    let solo_us = t.elapsed().as_micros();

    let t = Instant::now();
    give(s.go);
    give(s.go);
    take(s.done);
    take(s.done);
    let pair_us = t.elapsed().as_micros();

    let ipis_before = IPIS_TAKEN[0].load(Ordering::Relaxed) + IPIS_TAKEN[1].load(Ordering::Relaxed);
    let t = Instant::now();
    give(s.pp_go);
    take(s.done);
    let pp_us = t.elapsed().as_micros();
    let ipis =
        IPIS_TAKEN[0].load(Ordering::Relaxed) + IPIS_TAKEN[1].load(Ordering::Relaxed) - ipis_before;

    report(solo_us, pair_us, pp_us, ipis);
    loop {
        sleep(100_000);
    }
}

fn report(solo_us: u64, pair_us: u64, pp_us: u64, ipis: u32) {
    let sc = [
        SPINNER_CORES[0].load(Ordering::Relaxed),
        SPINNER_CORES[1].load(Ordering::Relaxed),
    ];
    let laps = PING_LAPS.load(Ordering::Relaxed);
    let ping = [
        PING_CORES[0].load(Ordering::Relaxed),
        PING_CORES[1].load(Ordering::Relaxed),
    ];
    let pong = [
        PONG_CORES[0].load(Ordering::Relaxed),
        PONG_CORES[1].load(Ordering::Relaxed),
    ];
    println!();
    println!("SMP parallel  one spin alone {solo_us} us, two spins on two cores {pair_us} us");
    println!(
        "SMP parallel  spinner cores (bit0=core0 bit1=core1): s0={:#04b} s1={:#04b}",
        sc[0], sc[1]
    );
    println!(
        "SMP pingpong  {laps}/{LAPS} laps in {pp_us} us = {} ns per hand-off",
        if laps > 0 {
            pp_us * 1000 / (2 * u64::from(laps))
        } else {
            0
        }
    );
    println!(
        "SMP pingpong  ping on core0/core1 = {}/{}, pong on core0/core1 = {}/{}",
        ping[0], ping[1], pong[0], pong[1]
    );
    println!(
        "SMP ipis taken {ipis} (core0 {} core1 {}), switches core0 {} core1 {}, lock contended {} times, ticks {}",
        IPIS_TAKEN[0].load(Ordering::Relaxed),
        IPIS_TAKEN[1].load(Ordering::Relaxed),
        SWITCHES[0].load(Ordering::Relaxed),
        SWITCHES[1].load(Ordering::Relaxed),
        CONTENDED.load(Ordering::Relaxed),
        TICKS.load(Ordering::Relaxed)
    );
    let parallel = pair_us * 10 < solo_us * 13
        && sc[0] != sc[1]
        && sc[0].count_ones() == 1
        && sc[1].count_ones() == 1;
    let crossed = laps == LAPS && ((ping[0] > 0 && pong[1] > 0) || (ping[1] > 0 && pong[0] > 0));
    println!();
    if parallel && crossed {
        println!("RESULT: PASS -- two cores, one Kairos kernel: parallel spins and {laps} cross-core hand-offs");
    } else {
        println!("RESULT: FAIL -- parallel={parallel} crossed={crossed}");
    }
}

fn arm(handle: TaskHandle, body: extern "C" fn(usize) -> !, param: usize) -> bool {
    let i = handle.index() as usize;
    if i >= TASKS {
        return false;
    }
    // SAFETY: called before any task runs; each slot and stack is used by
    // exactly one task.
    unsafe {
        let stack = (&raw mut STACKS).cast::<Stack>().add(i);
        let top = stack.cast::<u8>().add(core::mem::size_of::<Stack>());
        (&raw mut CONTEXTS_STORE)
            .cast::<Context>()
            .add(i)
            .write(new_task_context(
                task_entry,
                body as *const () as usize,
                param,
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

static mut APP_CORE_STACK: CoreStack<8192> = CoreStack::new();
static CORE1_UP: AtomicBool = AtomicBool::new(false);

#[esp_hal::main]
fn main() -> ! {
    let p =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()));

    println!();
    println!("=== K8 SMP S3: one Kairos kernel on BOTH cores of a XIAO ESP32-S3 ===");

    let mut k = match K::new(
        SmpPort {
            inner: XtensaPort::new(),
        },
        NoTrace,
    ) {
        Ok(k) => k,
        Err(_) => halt("geometry"),
    };
    let mk = |k: &mut K, name: &str, prio: u8| {
        k.create_task(name, prio)
            .unwrap_or_else(|_| halt("create_task"))
    };
    let ctl = mk(&mut k, "ctl", 4);
    let s0 = mk(&mut k, "s0", 2);
    let s1 = mk(&mut k, "s1", 2);
    let ping = mk(&mut k, "ping", 3);
    let pong = mk(&mut k, "pong", 3);
    let sem = |k: &mut K| {
        k.semaphore_create_counting(4, 0)
            .unwrap_or_else(|_| halt("semaphore"))
    };
    let sems_made = Sems {
        go: sem(&mut k),
        done: sem(&mut k),
        ping: sem(&mut k),
        pong: sem(&mut k),
        pp_go: sem(&mut k),
    };
    // SAFETY: no task runs yet.
    unsafe {
        *(&raw mut SEMS) = Some(sems_made);
    }
    let started = k
        .start_scheduler()
        .unwrap_or_else(|_| halt("start_scheduler"));
    let armed = arm(ctl, task_ctl, 0)
        && arm(s0, task_spinner, 0)
        && arm(s1, task_spinner, 1)
        && arm(ping, task_ping, 0)
        && arm(pong, task_pong, 0)
        && arm(started.idle, task_idle, 0)
        && arm(started.passive_idle, task_idle, 0)
        && arm(started.timer, task_timer, 0);
    if !armed {
        halt("a task index fell outside the context table");
    }
    // SAFETY: no task and no other core runs yet.
    unsafe {
        *KERNEL.0.get() = Some(k);
    }

    // Core 0's cross-core interrupt, bound here (on core 0).
    let mut ipi0 = SoftwareInterrupt::new(p.FROM_CPU_INTR0);
    ipi0.set_interrupt_handler(ipi_core0);
    // SAFETY: before core 1 exists.
    unsafe {
        *IPI0.0.get() = Some(ipi0);
    }

    // Core 1: bind ITS cross-core interrupt from core 1, enable its switch,
    // and switch away from its boot context.
    let ipi1_peripheral = p.FROM_CPU_INTR1;
    let mut cpu = CpuControl::new(p.CPU_CTRL);
    // SAFETY: the stack is used by core 1 only.
    let guard = cpu.start_app_core(unsafe { &mut *(&raw mut APP_CORE_STACK) }, move || {
        let mut ipi1 = SoftwareInterrupt::new(ipi1_peripheral);
        ipi1.set_interrupt_handler(ipi_core1);
        // SAFETY: core 1 is the only writer, before any raise targets it.
        unsafe {
            *IPI1.0.get() = Some(ipi1);
        }
        enable_switching();
        CORE1_UP.store(true, Ordering::Release);
        yield_now();
        halt("core 1's first switch never left its boot context");
    });
    match guard {
        Ok(g) => core::mem::forget(g),
        Err(_) => halt("core 1 would not start"),
    }
    while !CORE1_UP.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    println!("SMP core 1 up; tick on core 0; switching");

    let alarm = SystemTimer::new(p.SYSTIMER).alarm0;
    alarm.set_interrupt_handler(tick_interrupt);
    alarm.enable_auto_reload(true);
    if alarm.load_value(Duration::from_millis(1)).is_err() {
        halt("tick");
    }
    alarm.enable_interrupt(true);
    alarm.start();
    // SAFETY: core 0 only, before the tick can fire into it meaningfully.
    unsafe {
        *ALARM.0.get() = Some(alarm);
    }

    enable_switching();
    yield_now();
    halt("core 0's first switch never left main");
}
