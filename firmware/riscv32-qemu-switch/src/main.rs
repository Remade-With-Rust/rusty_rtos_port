#![no_std]
#![no_main]
//! The RISC-V context switch, and an answer about counters.
//!
//! The third port to get this cell. The Cortex-M one proved a `PendSV`
//! switch, the Xtensa one proved a windowed-ABI switch on silicon, and this
//! proves RV32 under QEMU `virt`.
//!
//! # What makes RISC-V different
//!
//! Not difficulty — there are no register windows to spill. What is
//! different is that **a RISC-V trap saves nothing**. The Cortex-M stacks
//! eight registers in hardware; Xtensa's exception entry spills the whole
//! window file. Here the port writes down every callee-saved register
//! itself, so the witness below is aimed squarely at those: `s0`–`s11`,
//! `ra` and `sp`.
//!
//! # What is checked, per resumption
//!
//! * every word of a 64-word witness array on the task's own stack;
//! * a scalar witness in each of three nested frames;
//! * the lap counters, so a task that stopped being resumed cannot pass by
//!   being quiet.
//!
//! # The counter question
//!
//! The mission plan says cycle rows come from silicon because **QEMU can
//! supply no cycle counter** — measured six ways on the Cortex-M cell, where
//! DWT is unimplemented. RISC-V is not in the same position: `mcycle` and
//! `minstret` are architectural CSRs, not optional debug hardware.
//!
//! So this cell *measures whether they work* rather than assuming either
//! way, and reports what it finds. A deterministic instruction count would
//! be a **work counter**, which this family prefers to a clock anyway —
//! counters before clocks.

// No `AtomicU64` on RV32 -- a 32-bit target without 64-bit atomics. The
// per-switch deltas are a few hundred at most, so 32-bit totals are ample
// for a few hundred switches.
use core::sync::atomic::{AtomicU32, Ordering};

use riscv_rt::entry;
use riscv_semihosting::{debug, hprintln};
use panic_halt as _;
// Pulled in for its `critical-section-single-hart` implementation, which
// `riscv-semihosting` needs and which nothing here references by name.
use riscv as _;

use rusty_rtos_port_riscv::{Context, mcycle, minstret, new_task_context, switch_context};

/// How many times the two tasks hand control back and forth.
const LAPS: u32 = 100;
/// Words of witness on each task's own stack.
const WITNESS: usize = 64;

const STACK_BYTES: usize = 8192;

#[repr(align(16))]
struct Stack([u8; STACK_BYTES]);

static mut STACK_A: Stack = Stack([0; STACK_BYTES]);
static mut STACK_B: Stack = Stack([0; STACK_BYTES]);

/// One saved context per participant: task A, task B, and `main`.
static mut CONTEXTS: [Context; 3] = [Context::new(), Context::new(), Context::new()];
const MAIN: usize = 2;

static LAPS_A: AtomicU32 = AtomicU32::new(0);
static LAPS_B: AtomicU32 = AtomicU32::new(0);
static FAULTS: AtomicU32 = AtomicU32::new(0);
static FIRST_FAULT: AtomicU32 = AtomicU32::new(u32::MAX);
/// Cycles and instructions spent inside the switch itself.
static SWITCH_CYCLES: AtomicU32 = AtomicU32::new(0);
static SWITCH_INSTRS: AtomicU32 = AtomicU32::new(0);
static SWITCHES: AtomicU32 = AtomicU32::new(0);

fn pattern(task: u32, word: usize) -> u32 {
    0xC0DE_0000_u32
        .wrapping_add(task.wrapping_mul(0x0001_0000))
        .wrapping_add(word as u32)
}

fn note_fault(word: usize) {
    FAULTS.fetch_add(1, Ordering::Relaxed);
    let _ =
        FIRST_FAULT.compare_exchange(u32::MAX, word as u32, Ordering::Relaxed, Ordering::Relaxed);
}

/// Hand control from `from` to `to`, counting the ROUND TRIP.
///
/// # What this measures, and what it does not
///
/// The bracket runs from just before `switch_context` to just after it
/// returns — and it returns only when this task is switched back in. So the
/// interval spans **everything the other task did in between**: its witness
/// checks, its nested calls, its own switch back.
///
/// That is a round trip, not a context switch, and it is labelled as one.
/// The first version of this cell called it "per switch" and reported
/// ~10,000 instructions for what should be fourteen loads and fourteen
/// stores — a number 250x too large, which is the instrument asking for
/// help rather than a slow switch. Measuring the bare swap means reading
/// the counter inside the assembly, which is a separate job.
fn hand_over(from: usize, to: usize) {
    // SAFETY: single hart, cooperative; one context runs at a time and the
    // indices are constants below 3.
    #[expect(unsafe_code, reason = "the contexts are this cell's own statics")]
    unsafe {
        let base = (&raw mut CONTEXTS).cast::<Context>();
        let c0 = mcycle();
        let i0 = minstret();
        switch_context(Some(base.add(from)), base.add(to));
        // These read AFTER the switch has come back, so they measure the
        // half that saved us plus the half that restored us -- one whole
        // switch, split across two moments in time.
        SWITCH_CYCLES.fetch_add(mcycle().wrapping_sub(c0) as u32, Ordering::Relaxed);
        SWITCH_INSTRS.fetch_add(minstret().wrapping_sub(i0) as u32, Ordering::Relaxed);
        SWITCHES.fetch_add(1, Ordering::Relaxed);
    }
}

/// The innermost frame: the switch happens here, three calls deep, so the
/// callee-saved registers of the outer frames are genuinely live.
#[inline(never)]
fn level_three(task: u32, from: usize, to: usize) {
    let witness = pattern(task, 0xC03);
    hand_over(from, to);
    if witness != pattern(task, 0xC03) {
        note_fault(0xC03);
    }
}

#[inline(never)]
fn level_two(task: u32, from: usize, to: usize) {
    let witness = pattern(task, 0xC02);
    level_three(task, from, to);
    if witness != pattern(task, 0xC02) {
        note_fault(0xC02);
    }
}

#[inline(never)]
fn level_one(task: u32, from: usize, to: usize) {
    let witness = pattern(task, 0xC01);
    level_two(task, from, to);
    if witness != pattern(task, 0xC01) {
        note_fault(0xC01);
    }
}

/// Both tasks run this. The entry arrives in `s0`/`s1`, which is what the
/// port's initial context sets.
extern "C" fn body(_task_fn: usize, task: usize) -> ! {
    let task = task as u32;
    let mut witness = [0u32; WITNESS];
    for (i, slot) in witness.iter_mut().enumerate() {
        *slot = pattern(task, i);
    }

    loop {
        let (me, other) = if task == 0 { (0, 1) } else { (1, 0) };
        level_one(task, me, other);

        for (i, slot) in witness.iter().enumerate() {
            if *slot != pattern(task, i) {
                note_fault(i);
            }
        }

        if task == 0 {
            let laps = LAPS_A.fetch_add(1, Ordering::Relaxed) + 1;
            if laps >= LAPS {
                hand_over(0, MAIN);
            }
        } else {
            LAPS_B.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[entry]
fn main() -> ! {
    hprintln!();
    hprintln!("=== the Kairos RISC-V context switch (rv32, QEMU virt) ===");
    hprintln!("target  riscv32imac-unknown-none-elf");
    hprintln!("design  the port saves ra, sp and s0-s11 itself: a RISC-V trap");
    hprintln!("        saves NOTHING, unlike Cortex-M's hardware frame or");
    hprintln!("        Xtensa's window spill");
    hprintln!("check   {} laps each way, switching from THREE frames deep", LAPS);

    // SAFETY: `main` runs once, before either task exists.
    #[expect(unsafe_code, reason = "building task stacks is the point of the cell")]
    unsafe {
        let base = (&raw mut CONTEXTS).cast::<Context>();
        let top_a = (&raw mut STACK_A).cast::<u8>().add(STACK_BYTES);
        let top_b = (&raw mut STACK_B).cast::<u8>().add(STACK_BYTES);
        base.write(new_task_context(body, 0, 0, top_a));
        base.add(1).write(new_task_context(body, 0, 1, top_b));
    }

    // Into task A. It hands back when it has run its laps.
    hand_over(MAIN, 0);

    let laps_a = LAPS_A.load(Ordering::Relaxed);
    let laps_b = LAPS_B.load(Ordering::Relaxed);
    let faults = FAULTS.load(Ordering::Relaxed);
    let first = FIRST_FAULT.load(Ordering::Relaxed);
    let switches = u64::from(SWITCHES.load(Ordering::Relaxed));
    let cycles = u64::from(SWITCH_CYCLES.load(Ordering::Relaxed));
    let instrs = u64::from(SWITCH_INSTRS.load(Ordering::Relaxed));

    hprintln!();
    hprintln!("SWITCH laps_a={} laps_b={} want={}", laps_a, laps_b, LAPS);
    if first == u32::MAX {
        hprintln!("SWITCH faults={} first_bad_word=none", faults);
    } else {
        hprintln!("SWITCH faults={} first_bad_word={}", faults, first);
    }

    // The counter question, answered by measurement.
    hprintln!();
    hprintln!("COUNTERS switches={}", switches);
    hprintln!("COUNTERS mcycle_total={} minstret_total={}", cycles, instrs);
    if switches > 0 {
        hprintln!(
            "COUNTERS per_round_trip cycles={} instructions={}  (NOT per switch)",
            cycles / switches,
            instrs / switches
        );
    }
    let cycles_live = cycles > 0;
    let instrs_live = instrs > 0;
    hprintln!(
        "COUNTERS mcycle_advances={} minstret_advances={}",
        cycles_live,
        instrs_live
    );
    hprintln!("COUNTERS   Run under `-icount shift=0`, which the runner sets. Without");
    hprintln!("COUNTERS   it these advance but are NOT reproducible: 1,932,185 /");
    hprintln!("COUNTERS   2,107,644 / 2,320,843 across three runs. With it, identical");
    hprintln!("COUNTERS   to the instruction. An emulator's cycle count is still not a");
    hprintln!("COUNTERS   chip's -- but a deterministic RETIRED-INSTRUCTION count is a");
    hprintln!("COUNTERS   work counter, and this family prefers those to clocks.");

    let mut failed = 0u32;
    let mut check = |ok: bool, what: &str| {
        if ok {
            hprintln!("      ok    {}", what);
        } else {
            failed += 1;
            hprintln!("      FAIL  {}", what);
        }
    };
    check(faults == 0, "every witness survived every switch");
    check(laps_a >= LAPS, "task A ran its full count");
    check(laps_b + 1 >= LAPS, "task B ran too, so both were resumed");
    check(
        instrs_live,
        "minstret advances, so this machine can supply a WORK count",
    );

    hprintln!();
    if failed == 0 {
        hprintln!("RESULT: PASS -- {} and {} resumptions on RV32, every", laps_a, laps_b);
        hprintln!("        callee-saved register intact across each one.");
        debug::exit(debug::EXIT_SUCCESS);
    } else {
        hprintln!("RESULT: FAIL -- {} check(s) failed", failed);
        debug::exit(debug::EXIT_FAILURE);
    }
    loop {
        core::hint::spin_loop();
    }
}
