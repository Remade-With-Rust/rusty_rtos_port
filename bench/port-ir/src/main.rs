//! Instruction counts for the port seam, which nothing measured until now.
//!
//! The SIM port, because it is the one that runs on the host. The silicon
//! ports are measured by their QEMU cells and, for Xtensa, by a board; what
//! this counts is the seam every kernel call crosses several times.
//!
//! **The critical section is the thing.** `enter_critical` and `exit_critical`
//! are the most-called pair in the whole system -- a queue send takes one, the
//! tick takes one, every wake takes one -- and on this port the OUTERMOST exit
//! is also the simulator's clock, which is what the conformance differential
//! compares. So the workload nests them to four deep and back, because a
//! nested pair costs a counter and an outermost pair costs the clock, and a
//! measurement that only ever took them singly would price the wrong one.
//!
//! The from-ISR half is separate on purpose: it saves and restores a mask
//! rather than counting nesting, and a port that confused the two would still
//! pass a test that only called one of them.
//!
//! A deterministic counter, not a clock. The verdict counts are the work
//! parity anchors: a change that moves any of them changed behaviour, and a
//! compiler that removed the work moves the checksum.

use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_port_core::sim::SimPort;

/// Enough repetitions that process startup is noise in the total.
const REPS: u32 = 1_000_000;

/// How deep the nesting goes. FreeRTOS's own nesting counter is what this
/// exercises: only the outermost exit may restore interrupts.
const DEPTH: u32 = 4;

fn main() {
    let port = SimPort::default();
    // WITHOUT THIS THE INSTRUMENT MEASURES THE WRONG PATH. `exit_critical`
    // only counts an outermost exit once the port is COUNTING, and that count
    // is the simulator's clock -- the quantity the conformance differential
    // compares on both sides. A first cut omitted it and reported `exits 0`
    // over four thousand rounds, which is the tell.
    port.scheduler_started();

    let mut sections = 0u64;
    let mut yields = 0u64;
    let mut ticks = 0u64;
    let mut isr_masks = 0u64;

    for round in 0..REPS {
        // ---- nested critical sections -------------------------------------
        //
        // In, to `DEPTH`, and out again. Only the last exit is an outermost
        // one, so this round advances the sim clock exactly once per pass.
        for _ in 0..DEPTH {
            port.enter_critical();
            sections = sections.wrapping_add(1);
        }
        for _ in 0..DEPTH {
            port.exit_critical();
        }

        // ---- single sections, which are the common shape -------------------
        for _ in 0..4u32 {
            port.enter_critical();
            sections = sections.wrapping_add(1);
            port.exit_critical();
        }

        // ---- the from-ISR half, which counts nothing and saves a mask ------
        for _ in 0..2u32 {
            let mask = port.set_interrupt_mask_from_isr();
            isr_masks = isr_masks.wrapping_add(1);
            port.clear_interrupt_mask_from_isr(mask);
        }

        // ---- yields, both the task half and the from-ISR half --------------
        port.yield_now();
        // The kernel counts the yield separately from requesting it, so an
        // instrument that only requested would leave `yields()` at zero.
        port.count_yield();
        yields = yields.wrapping_add(1);
        // `Woken::NO` must NOT pend a switch; taking only the YES arm would
        // measure half the branch and prove nothing about the other.
        port.yield_from_isr(Woken::YES);
        port.yield_from_isr(Woken::NO);
        yields = yields.wrapping_add(1);
        if port.take_yield() {
            yields = yields.wrapping_add(1);
        }

        // ---- the raised tick, which is how a sim port has a tick at all ----
        port.raise_tick();
        if port.take_pending_tick() {
            port.count_tick();
            ticks = ticks.wrapping_add(1);
        }
        // Asking again with nothing raised: the other arm of the same branch.
        if port.take_pending_tick() {
            ticks = ticks.wrapping_add(1);
        }

        // ---- the unwind pair, which `xTaskResumeAll` drives ----------------
        if round % 8 == 0 {
            port.begin_unwind();
            let _ = port.end_unwind();
        }

        let _ = port.in_isr();
    }

    // The port's own counters are the checksum: they are what the conformance
    // differential compares on the C side, so a change that moved them moved
    // the thing the gate reads.
    let checksum = port
        .exits()
        .wrapping_mul(3)
        .wrapping_add(port.yields().wrapping_mul(5))
        .wrapping_add(port.ticks());

    println!("checksum {checksum}");
    println!(
        "reps {REPS} sections {} yields {} ticks {} isr_masks {}   exits {} port_yields {} port_ticks {}",
        sections / u64::from(REPS),
        yields / u64::from(REPS),
        ticks / u64::from(REPS),
        isr_masks / u64::from(REPS),
        port.exits(),
        port.yields(),
        port.ticks()
    );
}
