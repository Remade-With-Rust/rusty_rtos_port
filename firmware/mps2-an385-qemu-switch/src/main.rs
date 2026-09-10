//! The PendSV context switch, proven on a Cortex-M3.
//!
//! Two tasks with real stacks of their own, preempted by SysTick, each
//! checking that **its own stack survived every switch**. That last part is
//! what makes this more than "both counters went up": a switcher that
//! swapped stack pointers but lost callee-saved registers, or one that
//! restored the wrong task's frame, would still increment two counters.
//!
//! # How a task proves its stack is intact
//!
//! Each task fills a large local array with a pattern derived from its own
//! id, and re-checks every word of it after each switch it observes. The
//! array is on the task's stack, so it can only still be there if the
//! switch preserved that stack and put the task back on it. A task that
//! finds a single wrong word stops and reports the index.
//!
//! Callee-saved registers get the same treatment: each task keeps a known
//! value in a local the compiler is forced to hold across the loop, and
//! checks it. `r4-r11` are precisely the registers the hardware does *not*
//! stack on exception entry, so they are the ones the port's asm has to
//! save — and the ones a broken switch loses first.
//!
//! # What this is not
//!
//! It is not the Kairos scheduler. The port's job is to switch when asked
//! and to preserve what it switched away from; *choosing* the next task is
//! the kernel's, and wiring `Kernel::switch_context` into
//! [`rusty_rtos_port_cortex_m::set_scheduler`] is the next increment. This
//! cell installs a round robin so the switch itself can be judged alone.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use cortex_m_rt::{entry, exception};
use cortex_m_semihosting::{debug, hprintln};
use panic_semihosting as _;

use rusty_rtos_port_cortex_m::{
    CURRENT_SP_SLOT, CortexMPort, init_stack, pend_switch, set_exception_priorities,
    set_scheduler, start_first_task, start_tick,
};

/// Words per task stack. Each task puts a 256-word witness array on it, so
/// this has to comfortably exceed that plus a frame.
const STACK_WORDS: usize = 1024;
/// Words of the witness pattern each task keeps on its own stack.
const WITNESS: usize = 256;
/// How many switches to run before declaring the port sound.
const SWITCHES_WANTED: u32 = 200;

static mut STACK_A: [usize; STACK_WORDS] = [0; STACK_WORDS];
static mut STACK_B: [usize; STACK_WORDS] = [0; STACK_WORDS];

/// Each task's saved stack pointer. `CURRENT_SP_SLOT` holds the *address*
/// of one of these, which is the shape FreeRTOS gets from putting the SP
/// first in its TCB.
static SLOT_A: AtomicUsize = AtomicUsize::new(0);
static SLOT_B: AtomicUsize = AtomicUsize::new(0);

static ON_A: AtomicBool = AtomicBool::new(true);
static SWITCHES: AtomicU32 = AtomicU32::new(0);
static LAPS_A: AtomicU32 = AtomicU32::new(0);
static LAPS_B: AtomicU32 = AtomicU32::new(0);
/// Set by a task that finds its own stack damaged; the word index, +1.
static CORRUPT: AtomicU32 = AtomicU32::new(0);

static PORT: CortexMPort = CortexMPort::new();

/// The round robin `PendSV` calls. Runs in exception context.
extern "C" fn pick_next() {
    let a_next = !ON_A.load(Ordering::Relaxed);
    ON_A.store(a_next, Ordering::Relaxed);
    let slot = if a_next {
        SLOT_A.as_ptr()
    } else {
        SLOT_B.as_ptr()
    };
    CURRENT_SP_SLOT.store(slot as usize, Ordering::Relaxed);
    SWITCHES.fetch_add(1, Ordering::Relaxed);
}

/// The body both tasks run, with `id` deciding the pattern it writes.
///
/// `#[inline(never)]` so the witness array really is a stack local of this
/// frame and not something the optimiser hoisted into a register or a
/// static.
#[inline(never)]
extern "C" fn task(id: usize) -> ! {
    let mut witness = [0usize; WITNESS];
    for (i, w) in witness.iter_mut().enumerate() {
        *w = i.wrapping_mul(0x9E37_79B9).wrapping_add(id);
    }
    // A value that must live in a callee-saved register across the loop.
    let mut sentinel: usize = 0xC0DE_0000 | id;
    let mut seen = 0u32;

    loop {
        let now = SWITCHES.load(Ordering::Relaxed);
        if now != seen {
            seen = now;
            // Every word of our own stack must still be what we wrote.
            for (i, w) in witness.iter().enumerate() {
                if *w != i.wrapping_mul(0x9E37_79B9).wrapping_add(id) {
                    CORRUPT.store(i as u32 + 1, Ordering::SeqCst);
                    finish();
                }
            }
            if sentinel != (0xC0DE_0000 | id) {
                CORRUPT.store(u32::MAX, Ordering::SeqCst);
                finish();
            }
            if id == 0 {
                LAPS_A.fetch_add(1, Ordering::Relaxed);
            } else {
                LAPS_B.fetch_add(1, Ordering::Relaxed);
            }
            if now >= SWITCHES_WANTED {
                finish();
            }
        }
        // Keep the sentinel live and the loop from being optimised away.
        sentinel = core::hint::black_box(sentinel);
        core::hint::spin_loop();
    }
}

/// Report and exit. Called from whichever task gets there first.
fn finish() -> ! {
    let switches = SWITCHES.load(Ordering::SeqCst);
    let a = LAPS_A.load(Ordering::SeqCst);
    let b = LAPS_B.load(Ordering::SeqCst);
    let corrupt = CORRUPT.load(Ordering::SeqCst);

    hprintln!();
    hprintln!("switches observed   {}", switches);
    hprintln!("task A resumptions  {}", a);
    hprintln!("task B resumptions  {}", b);
    hprintln!("ticks               {}", PORT.tick_count());

    let mut failed = 0u32;
    let mut check = |ok: bool, what: &str| {
        if ok {
            hprintln!("      ok    {}", what);
        } else {
            failed += 1;
            hprintln!("      FAIL  {}", what);
        }
    };
    check(corrupt == 0, "every word of both stacks survived every switch");
    check(switches >= SWITCHES_WANTED, "the switcher ran to completion");
    check(a > 0, "task A ran");
    check(b > 0, "task B ran");
    // Round robin, so neither may starve: each should have had roughly half.
    let lo = a.min(b);
    let hi = a.max(b);
    check(
        lo > 0 && hi <= lo.saturating_mul(3),
        "neither task starved (round robin, within 3x)",
    );

    if corrupt != 0 && corrupt != u32::MAX {
        hprintln!("      corrupted witness word index {}", corrupt - 1);
    } else if corrupt == u32::MAX {
        hprintln!("      a callee-saved register was lost across a switch");
    }

    hprintln!();
    if failed == 0 {
        hprintln!("RESULT: PASS -- the PendSV switch preserves two real task stacks");
        debug::exit(debug::EXIT_SUCCESS);
    } else {
        hprintln!("RESULT: FAIL -- {} check(s) failed", failed);
        debug::exit(debug::EXIT_FAILURE);
    }
    loop {
        core::hint::spin_loop();
    }
}

extern "C" fn task_a(_: usize) -> ! {
    task(0)
}
extern "C" fn task_b(_: usize) -> ! {
    task(1)
}

#[exception]
fn SysTick() {
    // `xPortSysTickHandler`: count the tick, and ask for a switch. A real
    // kernel would ask its scheduler whether one is required; a round robin
    // always wants one.
    rusty_rtos_port_cortex_m::tick(&PORT, true);
}

#[entry]
fn main() -> ! {
    hprintln!();
    hprintln!("=== the Kairos Cortex-M context switch (mps2-an385, QEMU) ===");
    hprintln!("two tasks, {} words of stack each, {} words of witness on it",
        STACK_WORDS, WITNESS);
    hprintln!("SysTick preempts; PendSV switches; each task re-checks its own");
    hprintln!("stack and a callee-saved register after every switch it sees.");

    set_exception_priorities();

    // Build both stacks. `&raw mut` so no reference to the static is ever
    // created, which is what makes taking these addresses sound.
    let top_a = core::ptr::addr_of_mut!(STACK_A).cast::<usize>().wrapping_add(STACK_WORDS);
    let top_b = core::ptr::addr_of_mut!(STACK_B).cast::<usize>().wrapping_add(STACK_WORDS);
    SLOT_A.store(init_stack(top_a, task_a, 0), Ordering::SeqCst);
    SLOT_B.store(init_stack(top_b, task_b, 1), Ordering::SeqCst);

    set_scheduler(pick_next);
    // Start on A.
    ON_A.store(true, Ordering::SeqCst);
    CURRENT_SP_SLOT.store(SLOT_A.as_ptr() as usize, Ordering::SeqCst);

    // A short tick so the run finishes quickly under an emulator. The
    // number is not a measurement of anything; see the region cell for why
    // this machine cannot supply one.
    start_tick(20_000);
    let _ = pend_switch;

    hprintln!("starting the first task...");
    // SAFETY: both stacks are built, a scheduler is installed, and
    // CURRENT_SP_SLOT names task A's slot.
    unsafe { start_first_task() }
}
