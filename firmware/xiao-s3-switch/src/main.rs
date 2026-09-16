#![no_std]
#![no_main]
//! The Xtensa context switch, on silicon.
//!
//! The Cortex-M twin of this cell (`mps2-an385-qemu-switch`) proved an ARM
//! switch under QEMU. This proves an **Xtensa LX7** switch on a real
//! ESP32-S3, and it is a harder claim for one reason: the windowed ABI.
//!
//! # What makes this test different from the ARM one
//!
//! An ARM switch saves eight callee-saved registers. Xtensa keeps a 64-entry
//! physical register file behind a rotating 16-register window, and a task
//! several calls deep has several live windows in that file. If those are
//! not spilled, the hardware later writes them onto whichever stack is
//! current — the *incoming* task's — and the corruption surfaces far from
//! its cause.
//!
//! So this test yields from **three calls deep**, not from the task body.
//! `level_one` calls `level_two` calls `level_three`, each holding a witness
//! it re-checks after the yield returns. A switch that mishandles the
//! register file corrupts the outer frames specifically, and a test that
//! only ever yielded from the top frame would never see it. That is not
//! hypothetical: an earlier design of this port switched from task context
//! with a hand-written window spill, passed a shallow probe, and hung as
//! soon as a task nested deeply enough to need the file back.
//!
//! # What is checked, per resumption
//!
//! * every word of a 64-word witness array on the task's own stack;
//! * a scalar witness in each of the three nested frames;
//! * the lap counters, so a task that stopped being resumed cannot pass by
//!   being quiet.
//!
//! # What this does NOT claim
//!
//! Round-robin between two tasks driven by an explicit yield. There is no
//! tick preemption here and no priority scheduler — `Kernel::switch_context`
//! is not in this loop. That is deliberate: this cell answers "can we leave
//! a task and come back to it intact on this ISA", which is what the
//! `esp-radio-rtos-driver` joint blocks on, and nothing more.

use core::sync::atomic::{AtomicU32, Ordering};

use esp_backtrace as _;
use esp_println::println;

use rusty_rtos_port_xtensa::{
    Context, clear_switch_request, enable_switching, new_task_context, switch_context, yield_now,
};

esp_bootloader_esp_idf::esp_app_desc!();

/// How many times each task is resumed.
const LAPS: u32 = 100;
/// Words of witness on each task's own stack.
const WITNESS: usize = 64;
/// Two tasks plus the context `main` is parked in while they run.
const CONTEXTS: usize = 3;
/// The index of `main`'s own context.
const MAIN: usize = 2;

/// A task stack. 16-byte aligned, which the Xtensa ABI requires of `SP`.
#[repr(align(16))]
struct Stack([u8; 8192]);

static mut STACK_A: Stack = Stack([0; 8192]);
static mut STACK_B: Stack = Stack([0; 8192]);

/// One saved machine state per context.
static mut CONTEXTS_STORE: [Context; CONTEXTS] =
    [const { unsafe { core::mem::zeroed() } }; CONTEXTS];
/// Who is running, and who should run next. The handler reads both.
static CURRENT: AtomicU32 = AtomicU32::new(MAIN as u32);
static NEXT: AtomicU32 = AtomicU32::new(MAIN as u32);

static LAPS_A: AtomicU32 = AtomicU32::new(0);
static LAPS_B: AtomicU32 = AtomicU32::new(0);
static FAULTS: AtomicU32 = AtomicU32::new(0);
/// The first corrupted witness, so a failure names a word rather than a mood.
static FIRST_FAULT: AtomicU32 = AtomicU32::new(u32::MAX);

/// Which task a witness belongs to, so two patterns cannot be confused for
/// one another if a switch lands on the wrong stack.
const fn pattern(task: u32, word: usize) -> u32 {
    0xA5A5_0000_u32
        .wrapping_add(task.wrapping_mul(0x0001_0000))
        .wrapping_add(word as u32)
}

fn note_fault(word: usize) {
    FAULTS.fetch_add(1, Ordering::Relaxed);
    let _ =
        FIRST_FAULT.compare_exchange(u32::MAX, word as u32, Ordering::Relaxed, Ordering::Relaxed);
}

/// Hand control to `to` and come back when someone hands it back.
fn switch_to(to: usize) {
    NEXT.store(to as u32, Ordering::Release);
    yield_now();
}

// ------------------------------------------------------- the switch itself --

/// `Software0`: the interrupt the port raises to switch.
///
/// This is the ONLY place a context is saved or restored. By the time it
/// runs, `xtensa-lx-rt`'s interrupt entry has spilled every register window
/// and written the whole machine into `trap_frame` — which is exactly why
/// the switch itself can be two struct copies.
#[esp_hal::ram]
#[unsafe(export_name = "Software0")]
fn switching_interrupt(trap_frame: &mut Context) {
    clear_switch_request();

    let from = CURRENT.load(Ordering::Acquire) as usize;
    let to = NEXT.load(Ordering::Acquire) as usize;
    if from == to {
        return;
    }
    CURRENT.store(to as u32, Ordering::Release);

    // SAFETY: single core, and this handler is the only reader or writer of
    // the store while a switch is in progress. Both indices are below
    // `CONTEXTS` by construction — every writer of `NEXT` passes a constant.
    #[expect(unsafe_code, reason = "the context switch")]
    unsafe {
        let base = (&raw mut CONTEXTS_STORE).cast::<Context>();
        switch_context(Some(base.add(from)), base.add(to), trap_frame);
    }
}

// --------------------------------------------------------------- the tasks --

/// The innermost frame: this is where the yield happens.
#[inline(never)]
fn level_three(task: u32, other: usize) {
    let witness = pattern(task, 0xC03);
    switch_to(other);
    if witness != pattern(task, 0xC03) {
        note_fault(0xC03);
    }
}

#[inline(never)]
fn level_two(task: u32, other: usize) {
    let witness = pattern(task, 0xC02);
    level_three(task, other);
    if witness != pattern(task, 0xC02) {
        note_fault(0xC02);
    }
}

#[inline(never)]
fn level_one(task: u32, other: usize) {
    let witness = pattern(task, 0xC01);
    level_two(task, other);
    if witness != pattern(task, 0xC01) {
        note_fault(0xC01);
    }
}

/// Both tasks run this. `task` is 0 or 1 and decides which witness pattern
/// it uses and which context it hands control to.
extern "C" fn body(_unused: usize, task: usize) -> ! {
    let task = task as u32;
    // The witness array lives on THIS task's stack, which is the memory the
    // other task must not have touched.
    let mut witness = [0u32; WITNESS];
    for (i, slot) in witness.iter_mut().enumerate() {
        *slot = pattern(task, i);
    }

    loop {
        let other = if task == 0 { 1 } else { 0 };
        level_one(task, other);

        // Every word, not a sample: a switch that clobbered one word of a
        // stack is exactly as broken as one that clobbered all of them, and
        // far harder to find later.
        for (i, slot) in witness.iter().enumerate() {
            if *slot != pattern(task, i) {
                note_fault(i);
            }
        }

        if task == 0 {
            let laps = LAPS_A.fetch_add(1, Ordering::Relaxed) + 1;
            if laps >= LAPS {
                // Hand control back to `main`, which reports. `main` never
                // hands it back, so this never returns.
                switch_to(MAIN);
            }
        } else {
            LAPS_B.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The frame a task starts in. A task body must not simply return — there is
/// nothing beneath it to return into — so the wrapper never lets it.
extern "C" fn task_entry(task_fn: usize, param: usize) {
    // SAFETY: `task_fn` is always `body`, whose signature this matches.
    #[expect(unsafe_code, reason = "the task entry point, reached by pointer")]
    let entry: extern "C" fn(usize, usize) -> ! = unsafe { core::mem::transmute(task_fn) };
    entry(0, param);
}

#[esp_hal::main]
fn main() -> ! {
    let _p = esp_hal::init(esp_hal::Config::default());

    println!();
    println!("=== the Kairos Xtensa context switch, on ESP32-S3 SILICON ===");
    println!("target  xtensa-esp32s3-none-elf, windowed ABI");
    println!("design  the switch runs in a software interrupt, so the register");
    println!("        windows are spilled by the exception entry, not by hand");
    println!("check   {LAPS} laps each way, yielding from THREE frames deep,");
    println!("        {WITNESS} witness words per task re-checked every resumption");
    println!();

    // SAFETY: `main` runs once and is the only writer of the store until the
    // first switch; the stacks are `static mut` arrays that outlive it.
    #[expect(unsafe_code, reason = "building task contexts is the point of the cell")]
    unsafe {
        let store = (&raw mut CONTEXTS_STORE).cast::<Context>();
        let top_a = (&raw mut STACK_A).cast::<u8>().add(size_of::<Stack>());
        let top_b = (&raw mut STACK_B).cast::<u8>().add(size_of::<Stack>());
        store
            .add(0)
            .write(new_task_context(task_entry, body as *const () as usize, 0, top_a));
        store
            .add(1)
            .write(new_task_context(task_entry, body as *const () as usize, 1, top_b));
    }

    enable_switching();
    // Into task 0. Control returns here when it has run its laps, because
    // `main`'s own context is saved by the very same handler.
    switch_to(0);

    let laps_a = LAPS_A.load(Ordering::Relaxed);
    let laps_b = LAPS_B.load(Ordering::Relaxed);
    let faults = FAULTS.load(Ordering::Relaxed);
    let first = FIRST_FAULT.load(Ordering::Relaxed);

    println!("SWITCH laps_a={laps_a} laps_b={laps_b} want={LAPS}");
    if first == u32::MAX {
        println!("SWITCH faults={faults} first_bad_word=none");
    } else {
        println!("SWITCH faults={faults} first_bad_word={first}");
    }

    // Both tasks must have run. A switch that never reached task 1 would
    // leave `laps_b` at zero and, without this, still report no faults.
    let ok = faults == 0 && laps_a >= LAPS && laps_b >= LAPS - 1;
    println!();
    if ok {
        println!("RESULT: PASS -- {laps_a} and {laps_b} resumptions, every witness");
        println!("        intact, yielded from three frames deep every time.");
    } else {
        println!("RESULT: FAIL -- faults={faults} laps_a={laps_a} laps_b={laps_b}");
    }

    loop {
        core::hint::spin_loop();
    }
}
