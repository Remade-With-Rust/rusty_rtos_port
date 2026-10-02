//! Property tests (hardening gate H-28): `SimPort` against a model of the sim
//! contract, after every one of many random operations.
//!
//! The contract (`ORACLES.md`, rules 1-3, and this crate's `sim.rs`) is
//! small enough to model exactly:
//!
//! - critical nesting never goes below zero, and `end_unwind` drops it to
//!   zero (the abandoned frame's open sections went with it);
//! - an exit is COUNTED only when it is outermost, on a task (not inside
//!   the tick's entry), after the scheduler started, and not inside an
//!   unwind; every [`EXITS_PER_TICK`]th counted exit raises a tick;
//! - inside an unwind an outermost exit is TALLIED instead, and
//!   `end_unwind` hands the tally back and zeroes it;
//! - a raised tick is taken exactly once.
//!
//! A seeded xorshift, as the kernel's property tests use: no dependency,
//! and a failure names the seed and step.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    reason = "a property test: it asserts by panicking"
)]

use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_port_core::sim::{EXITS_PER_TICK, SimPort};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[derive(Debug, Default)]
struct Model {
    nesting: u32,
    started: bool,
    isr: bool,
    unwinding: bool,
    exits: u64,
    unwound: u32,
    tick: bool,
    yielded: bool,
}

#[test]
fn sim_port_obeys_the_sim_contract_after_every_operation() {
    let mut counted = 0_u64;
    let mut tallied = 0_u64;
    let mut ticks = 0_u64;
    for seed in 1..=64_u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let p = SimPort::new();
        let mut m = Model::default();
        for step in 0..5_000 {
            let at = || format!("seed {seed} step {step}");
            match rng.next() % 16 {
                // Sections dominate, as they do in a kernel.
                0..=4 => {
                    p.enter_critical();
                    m.nesting += 1;
                }
                5..=9 => {
                    p.exit_critical();
                    m.nesting = m.nesting.saturating_sub(1);
                    if m.nesting == 0 && m.started && !m.isr {
                        if m.unwinding {
                            m.unwound += 1;
                            tallied += 1;
                        } else {
                            m.exits += 1;
                            counted += 1;
                            if m.exits % EXITS_PER_TICK == 0 {
                                m.tick = true;
                            }
                        }
                    }
                }
                10 => {
                    p.scheduler_started();
                    m.started = true;
                }
                11 => {
                    let yes = rng.next() % 2 == 0;
                    p.set_in_tick_entry(yes);
                    m.isr = yes;
                }
                12 => {
                    if !m.unwinding {
                        p.begin_unwind();
                        m.unwinding = true;
                        m.unwound = 0;
                    } else {
                        let got = p.end_unwind();
                        assert_eq!(got, m.unwound, "{}: the unwind tally", at());
                        m.unwinding = false;
                        m.unwound = 0;
                        m.nesting = 0;
                    }
                }
                13 => {
                    let got = p.take_tick();
                    assert_eq!(got, m.tick, "{}: a raised tick is taken exactly once", at());
                    if got {
                        ticks += 1;
                    }
                    m.tick = false;
                }
                14 => {
                    if rng.next() % 2 == 0 {
                        p.raise_tick();
                        m.tick = true;
                    } else {
                        p.yield_from_isr(Woken::YES);
                        m.yielded = true;
                    }
                }
                _ => {
                    let got = p.take_yield();
                    assert_eq!(got, m.yielded, "{}: a yield is taken exactly once", at());
                    m.yielded = false;
                }
            }
            assert_eq!(p.nesting(), m.nesting, "{}: nesting", at());
            assert_eq!(p.exits(), m.exits, "{}: counted exits", at());
            assert_eq!(p.is_unwinding(), m.unwinding, "{}: unwinding", at());
            assert_eq!(p.yield_is_pending(), m.yielded, "{}: pending yield", at());
        }
    }
    // The run must reach every arm the contract has.
    assert!(counted > 10_000, "only {counted} counted exits");
    assert!(tallied > 1_000, "only {tallied} tallied exits");
    assert!(ticks > 500, "only {ticks} ticks taken");
}
