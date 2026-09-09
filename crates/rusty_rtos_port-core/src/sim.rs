//! The deterministic simulator port — sim contract v1.
//!
//! The contract is written down once, in the umbrella's `ORACLES.md`, and
//! both this port and the patched C Posix port obey exactly it, which is
//! what makes their traces comparable:
//!
//! 1. There is no timer thread and no signal. Time advances only through
//!    a tick delivered at one of the two points below.
//! 2. **A tick is delivered from the idle hook**: each time the idle task
//!    runs its hook, one tick is delivered before it yields.
//! 3. **A tick is delivered on every 16th outermost critical-section exit**
//!    on a task after the scheduler started. That is where a hardware tick
//!    that arrived during the section would fire, so a task that polls
//!    without blocking still sees time move.
//!
//! The C side spells rule 3 as an edit to `vPortExitCritical` (six
//! exact-anchor edits applied by `kairos oracle patch`); this side spells it
//! as [`SimPort::exit_critical`]. Neither delivers the tick itself: both
//! *raise* it, and the kernel takes it at the same point the C kernel's
//! `vPortSystemTickHandler` runs. That is why the raise and the take are
//! separate halves of the [`Port`] seam rather than a callback — a callback
//! would mean the port re-entering the kernel that owns it.
//!
//! Nothing here is `unsafe`, and nothing here is a clock: the sim's time is
//! a count of kernel events, so a trace is a pure function of the program.

use core::cell::Cell;

use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;

/// How many outermost critical-section exits deliver one tick (rule 3).
///
/// The C oracle spells the same number in its `port.c` patch. Changing it is
/// a sim-contract version bump: every stored trace is re-captured.
pub const EXITS_PER_TICK: u64 = 16;

/// The deterministic simulator port.
///
/// Every field is a [`Cell`] because [`Port`] takes `&self`: the port is
/// shared, single-threaded state, exactly as the C port's file-scope
/// statics are. On the sim there is one context by construction, so no
/// atomics and no locks.
#[derive(Debug, Default)]
pub struct SimPort {
    /// `uxCriticalNesting`.
    nesting: Cell<u32>,
    /// Outermost critical-section exits since the scheduler started, the
    /// C patch's `ulKairosExits`.
    exits: Cell<u64>,
    /// `ulKairosYields`: `portYIELD()` calls, reported, never a tick source.
    yields: Cell<u64>,
    /// `ulKairosTicks`: ticks delivered.
    ticks: Cell<u64>,
    /// A tick raised by rule 2 or 3, waiting for the kernel to take it.
    pending_tick: Cell<bool>,
    /// `xYieldPendings[0]` as the port sees it.
    yield_pending: Cell<bool>,
    /// The C patch counts exits only on a FreeRTOS thread — that is, only
    /// once tasks are running. Before that, task creation runs on the main
    /// thread and its critical sections are not counted.
    counting: Cell<bool>,
    /// Whether the kernel is inside its tick entry (the C handler runs with
    /// signals blocked and does not recurse into the exit counter).
    in_isr: Cell<bool>,
    /// Exits belonging to a call the scheduler switched away from.
    ///
    /// The sim has no stacks, so the outgoing task's remaining
    /// `exit_critical` calls really do run — they are the tail of the
    /// function the kernel is still inside. On a real port they sit on a
    /// frozen stack and run when the task does, so they are ignored here
    /// and replayed by the kernel when the task is switched back in.
    swallow: Cell<u32>,
}

impl SimPort {
    /// A port with the scheduler not yet running: no exit is counted until
    /// [`SimPort::scheduler_started`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            nesting: Cell::new(0),
            exits: Cell::new(0),
            yields: Cell::new(0),
            ticks: Cell::new(0),
            pending_tick: Cell::new(false),
            yield_pending: Cell::new(false),
            counting: Cell::new(false),
            in_isr: Cell::new(false),
            swallow: Cell::new(0),
        }
    }

    /// Raise a tick unconditionally — the idle hook's tick, rule 2.
    ///
    /// The C side is `vPortKairosTick()`, which the harness's
    /// `vApplicationIdleHook` calls.
    pub fn raise_tick(&self) {
        self.pending_tick.set(true);
    }

    /// Take a raised tick. The kernel calls this where the C kernel's
    /// signal handler would have run.
    #[must_use]
    pub fn take_tick(&self) -> bool {
        self.pending_tick.replace(false)
    }

    /// Take the pending yield, if any.
    #[must_use]
    pub fn take_yield(&self) -> bool {
        self.yield_pending.replace(false)
    }

    /// Whether a yield is pending, without taking it.
    #[must_use]
    pub fn yield_is_pending(&self) -> bool {
        self.yield_pending.get()
    }

    /// `ulKairosExits`.
    #[must_use]
    pub fn exits(&self) -> u64 {
        self.exits.get()
    }

    /// `ulKairosYields`.
    #[must_use]
    pub fn yields(&self) -> u64 {
        self.yields.get()
    }

    /// `ulKairosTicks`.
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.ticks.get()
    }

    /// `uxCriticalNesting`.
    #[must_use]
    pub fn nesting(&self) -> u32 {
        self.nesting.get()
    }

    /// Exits still to be discarded from an abandoned call.
    #[must_use]
    pub fn swallowing(&self) -> u32 {
        self.swallow.get()
    }
}

impl Port for SimPort {
    fn yield_now(&self) {
        self.yield_pending.set(true);
    }

    fn yield_from_isr(&self, woken: Woken) {
        if woken.needed() {
            self.yield_pending.set(true);
        }
    }

    fn enter_critical(&self) {
        self.nesting.set(self.nesting.get().saturating_add(1));
    }

    fn exit_critical(&self) {
        // The tail of a call the scheduler abandoned: the C port's thread
        // never runs these, so neither does the count.
        let swallow = self.swallow.get();
        if swallow > 0 {
            self.swallow.set(swallow.saturating_sub(1));
            return;
        }
        let nesting = self.nesting.get().saturating_sub(1);
        self.nesting.set(nesting);
        // Rule 3, and the exact shape of the C patch: the count happens
        // after the nesting reaches zero and before interrupts are
        // re-enabled, on a task only, and the kernel's own tick entry does
        // not recurse into it.
        if nesting == 0 && self.counting.get() && !self.in_isr.get() {
            let exits = self.exits.get().wrapping_add(1);
            self.exits.set(exits);
            if exits.checked_rem(EXITS_PER_TICK) == Some(0) {
                self.pending_tick.set(true);
            }
        }
    }

    fn set_interrupt_mask_from_isr(&self) -> u32 {
        self.enter_critical();
        0
    }

    fn clear_interrupt_mask_from_isr(&self, _saved: u32) {
        self.exit_critical();
    }

    fn in_isr(&self) -> bool {
        self.in_isr.get()
    }

    fn idle(&self) {
        // The sim's idle is the hook's tick, which the kernel delivers at
        // the point the C idle task calls `vApplicationIdleHook`; there is
        // nothing for the port to do here.
    }

    // ----------------------------------------------------- the tick source --

    fn take_pending_tick(&self) -> bool {
        self.take_tick()
    }

    fn count_tick(&self) {
        self.ticks.set(self.ticks.get().wrapping_add(1));
    }

    fn count_yield(&self) {
        self.yields.set(self.yields.get().wrapping_add(1));
    }

    fn exits(&self) -> u64 {
        self.exits.get()
    }

    fn set_in_tick_entry(&self, yes: bool) {
        self.in_isr.set(yes);
    }

    fn scheduler_started(&self) {
        self.counting.set(true);
    }

    fn take_nesting(&self) -> u32 {
        self.nesting.replace(0)
    }

    fn set_nesting(&self, nesting: u32) {
        self.nesting.set(nesting);
    }

    fn swallow_exits(&self, n: u32) {
        self.swallow.set(self.swallow.get().saturating_add(n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exits_are_not_counted_before_the_scheduler_runs() {
        let p = SimPort::new();
        for _ in 0..64 {
            p.enter_critical();
            p.exit_critical();
        }
        assert_eq!(p.exits(), 0, "task creation runs on the main thread");
        assert!(!p.take_tick());
    }

    #[test]
    fn every_sixteenth_outermost_exit_raises_a_tick() {
        let p = SimPort::new();
        Port::scheduler_started(&p);
        let mut raised = 0;
        for _ in 0..64 {
            p.enter_critical();
            p.exit_critical();
            if p.take_tick() {
                raised += 1;
            }
        }
        assert_eq!(p.exits(), 64);
        assert_eq!(raised, 4, "64 exits / 16 per tick");
    }

    #[test]
    fn nested_sections_count_once() {
        let p = SimPort::new();
        Port::scheduler_started(&p);
        for _ in 0..16 {
            p.enter_critical();
            p.enter_critical();
            p.exit_critical();
            assert_eq!(p.nesting(), 1);
            p.exit_critical();
        }
        assert_eq!(p.exits(), 16, "the inner exit is not outermost");
        assert!(p.take_tick());
    }

    #[test]
    fn the_kernels_own_tick_entry_does_not_count_its_exits() {
        let p = SimPort::new();
        Port::scheduler_started(&p);
        Port::set_in_tick_entry(&p, true);
        for _ in 0..32 {
            p.enter_critical();
            p.exit_critical();
        }
        Port::set_in_tick_entry(&p, false);
        assert_eq!(p.exits(), 0);
    }

    #[test]
    fn a_raised_tick_is_taken_once() {
        let p = SimPort::new();
        p.raise_tick();
        assert!(p.take_tick());
        assert!(!p.take_tick());
    }

    #[test]
    fn yields_are_counted_and_taken_once() {
        let p = SimPort::new();
        p.yield_now();
        Port::count_yield(&p);
        assert!(p.yield_is_pending());
        assert!(p.take_yield());
        assert!(!p.take_yield());
        assert_eq!(p.yields(), 1);
    }
}
