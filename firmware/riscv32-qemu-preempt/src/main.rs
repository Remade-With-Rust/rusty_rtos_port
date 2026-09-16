#![no_std]
#![no_main]
//! The RISC-V switch under PREEMPTION, and what it actually costs.
//!
//! # Why this cell exists
//!
//! `bench/switch-cost` counts a Kairos cooperative RV32 switch at 30 retired
//! instructions against FreeRTOS's 83, and then says the honest thing: under
//! preemption the arms converge, because a preempted task's caller-saved
//! registers are live and somebody has to write them down. It puts the
//! Kairos side at **67 as a LOWER bound** — `riscv-rt`'s 37-instruction trap
//! entry plus our 30 — and flags that `_start_trap_rust` spills registers of
//! its own that the bound does not count.
//!
//! A bound is not a measurement. This cell makes the preemptive path real so
//! the number can be counted instead of estimated.
//!
//! # What preemption means here, and how it is PROVEN
//!
//! The two tasks never yield. They sit in a loop incrementing their own
//! counter and re-checking a witness. Nothing in either task's code hands
//! control to the other.
//!
//! Control moves anyway, because the CLINT's machine timer fires, and its
//! handler raises the machine SOFTWARE interrupt — which is the port's own
//! design ([`raise_switch`], and `Port::COMMITS_SWITCH` being `true` for
//! exactly this reason: the kernel must not move `current` at the point of a
//! yield, because the registers do not move until the trap is taken). The
//! software-interrupt handler is where the swap happens.
//!
//! So the verdict rests on a thing that cannot happen cooperatively: **both
//! counters advance**. A cell where only one advanced would be a cell where
//! preemption never happened, and it would otherwise pass quietly — that is
//! the failure the ARM preempt cell shipped three times before it was
//! caught, and it is why `both_ran` is a check and not a print.
//!
//! # What is NOT claimed
//!
//! No kernel. `Kernel::switch_context` is not in this loop; the handler
//! picks the other task directly. This is the PORT's preemptive path, which
//! is what the instruction count is about.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use riscv_rt::entry;
use riscv_semihosting::{debug, hprintln};
use panic_halt as _;
use riscv as _;

use rusty_rtos_port_riscv::{
    clear_switch_request, new_task_context_preemptive, raise_switch, switch_context_trap, Context,
};

/// QEMU `virt`'s CLINT. `MSIP` lives in the port (it is a port fact); these
/// two are this machine's timer and belong to the firmware.
const CLINT_MTIMECMP: usize = 0x0200_4000;
const CLINT_MTIME: usize = 0x0200_BFF8;

/// How often the timer preempts, in `mtime` ticks. QEMU `virt` runs `mtime`
/// at 10 MHz, so this is about 200 µs — short enough that the run finishes
/// promptly, long enough that each task makes visible progress between
/// switches (which is what makes "both counters advanced" mean something).
const PREEMPT_PERIOD: u64 = 2_000;

/// How many preemptive switches to take before reporting.
const SWITCHES: u32 = 200;

/// How many laps one task may run before the cell concludes that the timer
/// has stopped arriving. Far more than the handful a task gets between two
/// 200 µs preemptions, so reaching it means something is wrong.
const DEADLINE_LAPS: u32 = 2_000_000;

const STACK_BYTES: usize = 8192;

#[repr(align(16))]
struct Stack([u8; STACK_BYTES]);

static mut STACK_A: Stack = Stack([0; STACK_BYTES]);
static mut STACK_B: Stack = Stack([0; STACK_BYTES]);

/// Task A, task B, and `main`.
static mut CONTEXTS: [Context; 3] = [Context::new(), Context::new(), Context::new()];
const MAIN: usize = 2;

/// Which context is running. The handler reads it to know who to save.
static CURRENT: AtomicUsize = AtomicUsize::new(MAIN);

static WORK_A: AtomicU32 = AtomicU32::new(0);
static WORK_B: AtomicU32 = AtomicU32::new(0);
static SWITCHED: AtomicU32 = AtomicU32::new(0);
static FAULTS: AtomicU32 = AtomicU32::new(0);
/// Set when the run is over, so the tasks stop and `main` is resumed.
static DONE: AtomicU32 = AtomicU32::new(0);

/// The verdict, callable from `main` OR from a task that got stuck -- which
/// is the whole point, because once a task is running `main` cannot be
/// reached except through the handler this cell is testing.
fn report(a: u32, b: u32, switched: u32, faults: u32) -> ! {
    hprintln!();
    hprintln!("PREEMPT switches={} want={}", switched, SWITCHES);
    hprintln!("PREEMPT work_a={} work_b={}", a, b);
    hprintln!("PREEMPT faults={}", faults);
    hprintln!();

    let mut failed = 0u32;
    let mut check = |ok: bool, what: &str| {
        if ok {
            hprintln!("      ok    {}", what);
        } else {
            failed += 1;
            hprintln!("      FAIL  {}", what);
        }
    };
    check(
        a > 0 && b > 0,
        "BOTH tasks ran -- neither yields, so a switch DID happen",
    );
    check(faults == 0, "every witness survived every preemptive switch");
    check(
        switched >= SWITCHES,
        "preemption CONTINUED -- the timer kept arriving after the first switch",
    );

    if failed == 0 {
        hprintln!();
        hprintln!("RESULT: PASS -- {} preemptive switches between two tasks that", switched);
        hprintln!("        never yield, {} and {} laps of work, zero faults.", a, b);
        debug::exit(debug::EXIT_SUCCESS);
    } else {
        hprintln!();
        hprintln!("RESULT: FAIL -- the port cannot yet preempt, and here is exactly why.");
        hprintln!();
        hprintln!("  The FIRST switch works: task A ran, the timer fired, the");
        hprintln!("  software interrupt took the swap, task B started. Then the");
        hprintln!("  timer never arrives again.");
        hprintln!();
        hprintln!("  `switch_context` restores `ra` and RETURNS (`ret`), and a");
        hprintln!("  fresh task's `ra` is the trampoline (`new_task_context`).");
        hprintln!("  That is correct for a COOPERATIVE switch at a call site. Taken");
        hprintln!("  inside a trap handler it means the task starts running in TRAP");
        hprintln!("  CONTEXT: `mstatus.MIE` is still clear from trap entry and");
        hprintln!("  `mepc` is never consumed, so the hart takes no further");
        hprintln!("  interrupts. The first preemptive switch is also the last.");
        hprintln!();
        hprintln!("  FreeRTOS does not have this problem because its");
        hprintln!("  `portcontextRESTORE_CONTEXT` ENDS IN `mret` -- its switch owns");
        hprintln!("  the trap exit. That is the same fact as its 83 instructions");
        hprintln!("  against our 30 in `bench/switch-cost`: we are cheaper because");
        hprintln!("  we do less, and this is the part we do not do.");
        hprintln!();
        hprintln!("  The fix is a port change, not a firmware one: a fresh task's");
        hprintln!("  initial context has to be resumable through `mret`.");
        debug::exit(debug::EXIT_FAILURE);
    }
    loop {
        core::hint::spin_loop();
    }
}

fn pattern(task: u32, word: u32) -> u32 {
    0xC0DE_0000_u32
        .wrapping_add(task.wrapping_mul(0x0001_0000))
        .wrapping_add(word)
}

fn rearm() {
    // SAFETY: the CLINT is memory-mapped at a fixed address on QEMU `virt`.
    #[expect(unsafe_code, reason = "the machine's own timer registers")]
    unsafe {
        let now = (CLINT_MTIME as *const u64).read_volatile();
        (CLINT_MTIMECMP as *mut u64).write_volatile(now + PREEMPT_PERIOD);
    }
}

/// The machine timer: it does not switch, it ASKS for a switch.
///
/// This is the port's contract. A yield raises the software interrupt and
/// the swap happens there, so a timer that swapped directly would be a
/// second, different switching path.
#[unsafe(export_name = "MachineTimer")]
extern "C" fn machine_timer() {
    rearm();
    raise_switch();
}

/// The switching interrupt: the whole point of the cell.
#[unsafe(export_name = "MachineSoft")]
extern "C" fn machine_soft() {
    clear_switch_request();

    let from = CURRENT.load(Ordering::Relaxed);
    // Round-robin between the two tasks; `main` is only ever switched OUT
    // of, at the start, and back INTO at the end.
    let to = if DONE.load(Ordering::Relaxed) != 0 {
        MAIN
    } else {
        match from {
            0 => 1,
            1 => 0,
            _ => 0,
        }
    };
    if to == from {
        return;
    }
    CURRENT.store(to, Ordering::Relaxed);
    SWITCHED.fetch_add(1, Ordering::Relaxed);

    // SAFETY: single hart; exactly one context runs at a time and the
    // indices are below 3. This runs inside the trap, which is the only
    // place the outgoing registers are the outgoing task's.
    #[expect(unsafe_code, reason = "the context switch itself")]
    unsafe {
        let base = (&raw mut CONTEXTS).cast::<Context>();
        switch_context_trap(base.add(from), base.add(to));
    }
}

/// Both tasks run this, and NEITHER yields. Control leaves only through the
/// timer.
extern "C" fn body(_task_fn: usize, task: usize) -> ! {
    let task = task as u32;
    // Witnesses in callee-saved registers and on this task's own stack. A
    // preemptive switch that loses either shows up here.
    let witness = pattern(task, 0xC01);
    let mut local = [0u32; 16];
    for (i, slot) in local.iter_mut().enumerate() {
        *slot = pattern(task, i as u32);
    }

    loop {
        if task == 0 {
            WORK_A.fetch_add(1, Ordering::Relaxed);
        } else {
            WORK_B.fetch_add(1, Ordering::Relaxed);
        }

        if witness != pattern(task, 0xC01) {
            FAULTS.fetch_add(1, Ordering::Relaxed);
        }
        for (i, slot) in local.iter().enumerate() {
            if *slot != pattern(task, i as u32) {
                FAULTS.fetch_add(1, Ordering::Relaxed);
            }
        }

        if SWITCHED.load(Ordering::Relaxed) >= SWITCHES {
            DONE.store(1, Ordering::Relaxed);
        }
        // `main` is unreachable once a task is running, so the verdict is
        // reported from here. If preemption keeps working, SWITCHED reaches
        // its target, the handler switches back to `main`, and `main`
        // reports. If it stalls, whichever task is stuck reaches this
        // deadline and reports instead -- which is the case this cell was
        // built to catch.
        let mine = if task == 0 {
            WORK_A.load(Ordering::Relaxed)
        } else {
            WORK_B.load(Ordering::Relaxed)
        };
        if mine >= DEADLINE_LAPS {
            report(
                WORK_A.load(Ordering::Relaxed),
                WORK_B.load(Ordering::Relaxed),
                SWITCHED.load(Ordering::Relaxed),
                FAULTS.load(Ordering::Relaxed),
            );
        }
        core::hint::spin_loop();
    }
}

#[entry]
fn main() -> ! {
    hprintln!();
    hprintln!("=== the Kairos RISC-V switch under PREEMPTION (rv32, QEMU virt) ===");
    hprintln!("target  riscv32imac-unknown-none-elf");
    hprintln!("design  the CLINT timer ASKS for a switch (raise_switch); the");
    hprintln!("        machine SOFTWARE interrupt takes it. That is the port's");
    hprintln!("        contract, and COMMITS_SWITCH is true for that reason.");
    hprintln!("check   NEITHER task yields, so both counters advancing is the");
    hprintln!("        only thing that can prove preemption happened");
    hprintln!();

    // SAFETY: `main` runs once, before either task exists.
    #[expect(unsafe_code, reason = "building task stacks is the point of the cell")]
    unsafe {
        let base = (&raw mut CONTEXTS).cast::<Context>();
        let top_a = (&raw mut STACK_A).cast::<u8>().add(STACK_BYTES);
        let top_b = (&raw mut STACK_B).cast::<u8>().add(STACK_BYTES);
        base.write(new_task_context_preemptive(body, 0, 0, top_a));
        base.add(1)
            .write(new_task_context_preemptive(body, 0, 1, top_b));
    }

    rearm();
    // SAFETY: enabling the two interrupts this cell handles, then the global
    // enable. Nothing else is armed.
    #[expect(unsafe_code, reason = "arming the machine's own interrupts")]
    unsafe {
        riscv::register::mie::set_mtimer();
        riscv::register::mie::set_msoft();
        riscv::register::mstatus::set_mie();
    }

    // Ask for the first switch and WAIT for it, rather than switching here.
    //
    // This is not ceremony. A task built for the preemptive path leaves its
    // first switch through `mret`, and `mret` is only meaningful inside a
    // trap. Switching into task A directly from `main` would execute it
    // outside one. So `main` raises the software interrupt like anything
    // else would, and the handler takes the switch — which also means
    // `main`'s own context is saved by the same code path that saves a
    // task's, and can be resumed by it at the end.
    raise_switch();
    while DONE.load(Ordering::Relaxed) == 0 {
        core::hint::spin_loop();
    }

    // Reached again only when the handler switches back to `main`.
    // Reached only if the handler switched back here, which needs
    // preemption to have kept working all the way to SWITCHES.
    report(
        WORK_A.load(Ordering::Relaxed),
        WORK_B.load(Ordering::Relaxed),
        SWITCHED.load(Ordering::Relaxed),
        FAULTS.load(Ordering::Relaxed),
    );
}
