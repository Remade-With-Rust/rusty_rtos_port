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

/// The scheduler is running, so critical-section exits are counted.
///
/// The C patch counts exits only on a FreeRTOS thread — that is, only once
/// tasks are running. Before that, task creation runs on the main thread and
/// its critical sections are not counted.
const COUNTING: u8 = 1 << 0;
/// The kernel is inside its tick entry.
///
/// The C handler runs with signals blocked and does not recurse into the
/// exit counter.
const IN_ISR: u8 = 1 << 1;
/// A switched-out frame's tail is running.
///
/// The sim has no stacks, so the outgoing task's call really does run to its
/// end — the sections it had open and any section it opens after the switch.
/// On a real port all of that sits on a frozen stack and happens when the
/// task runs again, so while this is set the exits are tallied into
/// [`SimPort::unwound`] instead of counted, and the kernel replays the tally
/// when the task is switched back in.
const UNWINDING: u8 = 1 << 2;

/// Counting, on a task, with no abandoned tail: the exit is sim time.
const COUNTS: u8 = COUNTING;
/// The same, inside an abandoned tail: the exit is tallied, not counted.
const TALLIES: u8 = COUNTING | UNWINDING;

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
    /// [`COUNTING`], [`IN_ISR`] and [`UNWINDING`], in one word.
    ///
    /// They were three `Cell<bool>`s, and every outermost critical-section
    /// exit read all three to decide a single three-way question. Together
    /// they answer it in one load: see [`COUNTS`] and [`TALLIES`].
    flags: Cell<u8>,
    /// Outermost exits the running tail has made so far.
    unwound: Cell<u32>,
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
            flags: Cell::new(0),
            unwound: Cell::new(0),
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

    /// Whether a switched-out frame's tail is running.
    #[must_use]
    pub fn is_unwinding(&self) -> bool {
        self.flags.get() & UNWINDING != 0
    }

    /// Raise or drop one of [`COUNTING`], [`IN_ISR`], [`UNWINDING`].
    fn set_flag(&self, bit: u8, yes: bool) {
        let flags = self.flags.get();
        self.flags.set(if yes { flags | bit } else { flags & !bit });
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
        // `wrapping_add`, not `saturating_add`: this is the single
        // most-executed write in the port, and saturating cost four
        // instructions where one will do.
        //
        // The guard it replaces was never real. Saturating at `u32::MAX`
        // would mean four billion critical sections entered and none left,
        // and from there `nesting` could never reach zero again, so the sim
        // would stop producing ticks for ever. The C's `uxCriticalNesting++`
        // does not guard this either. The `saturating_sub` on the way out
        // stays, because THAT one is load-bearing: `end_unwind` zeroes the
        // nesting under an abandoned frame, and a stray exit after it has to
        // stay at zero rather than wrap to the top.
        self.nesting.set(self.nesting.get().wrapping_add(1));
    }

    fn exit_critical(&self) {
        let nesting = self.nesting.get().saturating_sub(1);
        self.nesting.set(nesting);
        if nesting != 0 {
            return;
        }
        // Rule 3, and the exact shape of the C patch: the count happens
        // after the nesting reaches zero and before interrupts are
        // re-enabled, on a task only, and the kernel's own tick entry does
        // not recurse into it. One load answers all of that.
        match self.flags.get() {
            COUNTS => {
                let exits = self.exits.get().wrapping_add(1);
                self.exits.set(exits);
                if exits.checked_rem(EXITS_PER_TICK) == Some(0) {
                    self.pending_tick.set(true);
                }
            }
            // The tail of a call the scheduler abandoned: the C port's
            // thread has not run this yet, so it is not sim time yet.
            TALLIES => self.unwound.set(self.unwound.get().saturating_add(1)),
            _ => {}
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
        self.flags.get() & IN_ISR != 0
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
        self.set_flag(IN_ISR, yes);
    }

    fn scheduler_started(&self) {
        self.set_flag(COUNTING, true);
    }

    fn begin_unwind(&self) {
        self.set_flag(UNWINDING, true);
        self.unwound.set(0);
    }

    fn end_unwind(&self) -> u32 {
        self.set_flag(UNWINDING, false);
        // The frame is gone; whatever it still had open went with it.
        self.nesting.set(0);
        self.unwound.replace(0)
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
    fn a_tail_is_tallied_rather_than_counted() {
        let p = SimPort::new();
        Port::scheduler_started(&p);
        // Two sections open when the scheduler switched away, and one more
        // that the abandoned frame opens and closes afterwards: three
        // exits, one of them not outermost, so two the task owes.
        p.enter_critical();
        p.enter_critical();
        Port::begin_unwind(&p);
        p.exit_critical();
        p.exit_critical();
        p.enter_critical();
        p.exit_critical();
        assert_eq!(p.exits(), 0, "none of that is sim time yet");
        assert_eq!(Port::end_unwind(&p), 2);
        assert_eq!(p.nesting(), 0, "the frame went with the switch");
        assert!(!p.is_unwinding());
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

    /// `set_flag` RAISES a bit; it does not toggle it.
    ///
    /// `cargo mutants` replaced its `|` with `^` and nothing noticed,
    /// because no test ever set a flag that was already set. The two differ
    /// on exactly that input, and the scheduler-started flag is the one
    /// where it would hurt: a second `scheduler_started()` under `^` would
    /// turn exit COUNTING BACK OFF, and sim time would silently stop.
    #[test]
    fn setting_a_flag_that_is_already_set_leaves_it_set() {
        let p = SimPort::default();
        p.scheduler_started();
        p.scheduler_started();

        p.enter_critical();
        p.exit_critical();
        assert_eq!(
            p.exits(),
            1,
            "COUNTING survived being set twice -- with a toggle it would be \
             off again and every exit after it would go uncounted"
        );

        // The same for the unwinding flag, which `is_unwinding` reports.
        p.begin_unwind();
        assert!(p.is_unwinding());
        p.begin_unwind();
        assert!(p.is_unwinding(), "still unwinding after a second begin");
    }

    /// The counters answer what they have counted, and are not constants.
    ///
    /// These are the three columns every conformance trace ends with, and
    /// the whole sim-time contract rests on them -- `exits` IS the clock.
    /// All three were replaceable by a literal with nothing objecting.
    #[test]
    fn the_trace_counters_report_what_was_actually_counted() {
        let p = SimPort::default();
        assert_eq!(p.ticks(), 0, "nothing counted yet");
        assert_eq!(p.yields(), 0);
        assert_eq!(p.exits(), 0);

        p.count_tick();
        p.count_tick();
        p.count_tick();
        assert_eq!(p.ticks(), 3, "three, not zero and not one");

        p.count_yield();
        p.count_yield();
        assert_eq!(p.yields(), 2, "two, not one");

        p.scheduler_started();
        for _ in 0..4_u32 {
            p.enter_critical();
            p.exit_critical();
        }
        assert_eq!(p.exits(), 4);
    }

    /// `yield_is_pending` OBSERVES and `take_yield` CONSUMES. A reader that
    /// took the flag would make the kernel's own check destructive.
    #[test]
    fn a_pending_yield_can_be_observed_without_being_taken() {
        let p = SimPort::default();
        assert!(!p.yield_is_pending(), "nothing pending on a fresh port");

        p.yield_now();
        assert!(p.yield_is_pending(), "pending");
        assert!(p.yield_is_pending(), "and asking twice did not consume it");

        assert!(p.take_yield(), "the take reports it");
        assert!(!p.yield_is_pending(), "and IS what consumed it");
        assert!(!p.take_yield(), "so a second take has nothing");
    }

    /// `yield_from_isr` yields only when the woken flag says a higher
    /// priority task is ready. Replacing its whole body with `()` -- never
    /// yielding at all -- survived, because nothing called it.
    #[test]
    fn yield_from_isr_yields_only_when_something_was_woken() {
        let p = SimPort::default();
        p.yield_from_isr(Woken::NO);
        assert!(
            !p.yield_is_pending(),
            "nothing was woken, so nothing is pending"
        );

        p.yield_from_isr(Woken::YES);
        assert!(p.yield_is_pending(), "and something was, so it is");
    }

    /// `is_unwinding` reports the flag rather than a constant, and
    /// `end_unwind` clears it.
    #[test]
    fn is_unwinding_follows_begin_and_end() {
        let p = SimPort::default();
        assert!(!p.is_unwinding(), "not unwinding to start with");
        p.begin_unwind();
        assert!(p.is_unwinding());
        let _ = p.end_unwind();
        assert!(!p.is_unwinding(), "and not, once the frame is done");
    }

    /// `in_isr` follows the tick-entry flag both ways, which is what keeps
    /// the kernel's own tick from counting its exits as a task's.
    #[test]
    fn in_isr_follows_the_tick_entry_flag_both_ways() {
        let p = SimPort::default();
        assert!(!p.in_isr());
        p.set_in_tick_entry(true);
        assert!(p.in_isr());
        p.set_in_tick_entry(false);
        assert!(!p.in_isr(), "and back off again, not latched");
    }
}
