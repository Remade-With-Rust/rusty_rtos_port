#![no_std]
#![no_main]
// Xtensa inline asm is nightly-only; this cell is always built with the esp
// fork, which is nightly.
#![cfg_attr(target_arch = "xtensa", feature(asm_experimental_arch))]
//! K5b: the Kairos kernel behind `esp-radio-rtos-driver`, exercised on
//! silicon **through the driver's own interface**.
//!
//! `esp-radio` itself cannot be linked yet — see the README: the published
//! radio pins `esp-hal ~1.1.0` against this family's `=1.2.1`, and
//! `xtensa-lx-rt` is a `links` crate, so cargo refuses the pair outright.
//!
//! That blocks the *radio*. It does not block testing the *seam*, and this
//! cell does not wait for it. Everything below goes through
//! `SchedulerImplementation` and `SemaphoreImplementation` exactly as the
//! blob would call them:
//!
//! * `task_create` — with a C function pointer and a stack size, which is
//!   the method a stackless kernel cannot serve and the reason
//!   `rusty_rtos_port-xtensa` had to exist;
//! * `Semaphore::create` / `take` / `give` — the blocking the blob performs
//!   from inside its own call frames;
//! * `yield_task` — and therefore `Kernel::switch_context`, the same
//!   fixed-priority scheduler the corpus proves against C FreeRTOS on three
//!   other architectures.
//!
//! # What it checks
//!
//! Two worker tasks, each created through `task_create`, ping-pong a fixed
//! number of times on two adapter semaphores and then signal a third. Every
//! hand-off is a real block and a real context switch. The cell asserts:
//!
//! * both workers ran their FULL count — a worker that was never scheduled
//!   cannot pass by being quiet;
//! * each worker's own stack witness survived every switch, because a switch
//!   that corrupted a heap-allocated stack is the failure this port exists
//!   to prevent.
//!
//! # What it does NOT claim
//!
//! Nothing about Wi-Fi, BLE, or the blob's own demands. It says the
//! scheduler half of the seam works on hardware, and exercises the
//! data-structure half only as far as two semaphores go.

extern crate alloc;

mod adapter;
mod kernel;

// The registration macros expand to `extern "C"` shims that name `c_void`,
// so it has to be in scope where they are written.
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use esp_backtrace as _;
use esp_println::println;

use esp_radio_rtos_driver::semaphore::{SemaphoreImplementation, SemaphoreKind, SemaphorePtr};
use esp_radio_rtos_driver::SchedulerImplementation;

use rusty_rtos_alloc::small_metal::{good_region_size, Region};

esp_bootloader_esp_idf::esp_app_desc!();

/// The heap the adapter allocates from: task stacks, queue storage, and one
/// box per radio object.
const BUDGET: usize = 96 * 1024;
type Heap = Region<{ good_region_size(BUDGET) }>;
static HEAP: Heap = Heap::new();

#[global_allocator]
static ALLOC: rusty_rtos_alloc::Alloc = rusty_rtos_alloc::Alloc;

// The five implementations, handed to the driver. Each emits the
// `#[no_mangle] esp_rtos_*` symbols the driver declares `extern`, which is
// why exactly one scheduler may be linked — and why `esp-rtos` must not be.
esp_radio_rtos_driver::register_scheduler_implementation!(
    static SCHEDULER: adapter::Scheduler = adapter::Scheduler
);
esp_radio_rtos_driver::register_semaphore_implementation!(adapter::Semaphore);
esp_radio_rtos_driver::register_queue_implementation!(adapter::Queue);
esp_radio_rtos_driver::register_timer_implementation!(adapter::Timer);
esp_radio_rtos_driver::register_wait_queue_implementation!(adapter::WaitQueue);

/// Hand-offs each worker performs.
const ROUNDS: u32 = 50;
/// Words of witness on each worker's own (heap-allocated) stack.
const WITNESS: usize = 32;

static LAPS_A: AtomicU32 = AtomicU32::new(0);
static LAPS_B: AtomicU32 = AtomicU32::new(0);
static FAULTS: AtomicU32 = AtomicU32::new(0);
/// Did a worker's first instruction ever execute? A counter rather than a
/// print: formatting on a fresh 4 KiB stack is its own risk, and this has to
/// answer a question about whether the stack works at all.
static ENTERED: AtomicU32 = AtomicU32::new(0);

/// The two ping-pong semaphores and the one the workers finish on.
static mut SEM: [Option<SemaphorePtr>; 3] = [None; 3];

fn sem(which: usize) -> SemaphorePtr {
    // SAFETY: written once by `main` before either worker exists, and read
    // only afterwards.
    unsafe { *(&raw const SEM).cast::<Option<SemaphorePtr>>().add(which) }
        .unwrap_or(core::ptr::NonNull::dangling())
}

fn pattern(task: u32, word: usize) -> u32 {
    0x5A5A_0000_u32
        .wrapping_add(task.wrapping_mul(0x0001_0000))
        .wrapping_add(word as u32)
}

/// A worker, entered exactly as the blob's tasks are: a C function pointer
/// and a `*mut c_void`, on a stack the adapter allocated.
extern "C" fn worker(param: *mut c_void) {
    ENTERED.fetch_add(1, Ordering::Relaxed);
    let me = param as usize as u32;
    // This array lives on the HEAP-allocated stack `task_create` made. If a
    // switch mishandles it, these are the words that move.
    let mut witness = [0u32; WITNESS];
    for (i, slot) in witness.iter_mut().enumerate() {
        *slot = pattern(me, i);
    }

    let (wait_on, signal) = if me == 0 { (0, 1) } else { (1, 0) };

    for _ in 0..ROUNDS {
        // A real block, through the driver's own trait, from inside this
        // task's own call frame.
        // SAFETY: the semaphores were made by `main` and outlive both
        // workers.
        unsafe {
            adapter::Semaphore::take(sem(wait_on), None);
        }

        for (i, slot) in witness.iter().enumerate() {
            if *slot != pattern(me, i) {
                FAULTS.fetch_add(1, Ordering::Relaxed);
            }
        }

        if me == 0 {
            LAPS_A.fetch_add(1, Ordering::Relaxed);
        } else {
            LAPS_B.fetch_add(1, Ordering::Relaxed);
        }

        // SAFETY: as above.
        unsafe {
            adapter::Semaphore::give(sem(signal));
        }
    }

    // SAFETY: as above.
    unsafe {
        adapter::Semaphore::give(sem(2));
    }
}

#[esp_hal::main]
fn main() -> ! {
    let p =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()));

    println!();
    println!("=== K5b: the Kairos kernel behind esp-radio-rtos-driver, SILICON ===");
    println!("scheduler  rusty_rtos_kernel + rusty_rtos_port-xtensa");
    println!("seam       esp-radio-rtos-driver 0.4.1, all five traits registered");
    println!("exercised  task_create (C fn ptr + heap stack), Semaphore take/give,");
    println!("           yield_task -> Kernel::switch_context");
    println!("NOT here   esp-radio itself: it pins esp-hal ~1.1.0 against our");
    println!("           =1.2.1, and xtensa-lx-rt is a `links` crate (README)");
    println!();

    match HEAP.give() {
        Ok(usable) => println!("RADIO heap_usable={usable}"),
        Err(e) => {
            println!("RADIO heap_refused={e:?}");
            println!("RESULT: FAIL -- no heap, so nothing below could run");
            park();
        }
    }

    if !kernel::boot() {
        println!("RESULT: FAIL -- the kernel would not start");
        park();
    }
    if !kernel::start_tick(p.SYSTIMER) {
        println!("RESULT: FAIL -- the tick would not start, so nothing can time out");
        park();
    }
    println!("RADIO kernel_started tick=1kHz");

    // SAFETY: single-threaded here; no task exists yet.
    unsafe {
        let table = (&raw mut SEM).cast::<Option<SemaphorePtr>>();
        *table = Some(adapter::Semaphore::create(SemaphoreKind::Counting {
            max: 1,
            initial: 1,
        }));
        *table.add(1) = Some(adapter::Semaphore::create(SemaphoreKind::Counting {
            max: 1,
            initial: 0,
        }));
        *table.add(2) = Some(adapter::Semaphore::create(SemaphoreKind::Counting {
            max: 2,
            initial: 0,
        }));
    }
    // A `create` that failed answers `NonNull::dangling()`, and every later
    // take on it fails instantly rather than blocking -- which looks exactly
    // like "the workers never ran".
    let dangling = core::ptr::NonNull::<()>::dangling().as_ptr();
    println!(
        "RADIO semaphores_made a_ok={} b_ok={} done_ok={}",
        sem(0).as_ptr() != dangling,
        sem(1).as_ptr() != dangling,
        sem(2).as_ptr() != dangling
    );

    // Through the DRIVER's interface, with a stack size, exactly as the blob
    // would. Priority above this task's, so they run when it blocks.
    let a = SCHEDULER.task_create("w0", worker, core::ptr::null_mut(), 4, None, 4096);
    let b = SCHEDULER.task_create("w1", worker, 1 as *mut c_void, 4, None, 4096);
    println!("RADIO tasks_created a={:p} b={:p}", a.as_ptr(), b.as_ptr());
    println!("RADIO after_create {}", kernel::ready_report());

    // Wait for both to finish. Blocking THIS task is how the workers get the
    // CPU at all.
    for i in 0..2 {
        // SAFETY: the semaphores were made above.
        let got = unsafe { adapter::Semaphore::take(sem(2), Some(10_000_000)) };
        println!("RADIO main_take{i}={got} {}", kernel::ready_report());
    }

    let laps_a = LAPS_A.load(Ordering::Relaxed);
    let laps_b = LAPS_B.load(Ordering::Relaxed);
    let faults = FAULTS.load(Ordering::Relaxed);

    let (entries, swaps, same, no_ctx, lf, lt) = kernel::switch_counts();
    println!(
        "RADIO laps_a={laps_a} laps_b={laps_b} want={ROUNDS} faults={faults} workers_entered={}",
        ENTERED.load(Ordering::Relaxed)
    );
    println!("RADIO switch_entries={entries} swaps={swaps} declined_same={same} declined_no_ctx={no_ctx} last_from={lf} last_to={lt}");

    let ok = faults == 0 && laps_a == ROUNDS && laps_b == ROUNDS;
    println!();
    if ok {
        println!("RESULT: PASS -- {laps_a} and {laps_b} hand-offs through the driver's");
        println!("        own traits, every stack witness intact.");
    } else {
        println!("RESULT: FAIL -- laps_a={laps_a} laps_b={laps_b} faults={faults}");
    }
    park();
}

fn park() -> ! {
    loop {
        core::hint::spin_loop();
    }
}
