#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
//! `rusty_rtos_port-core` — the arch-agnostic half of the Kairos ports.
//!
//! A FreeRTOS port is `portmacro.h` plus `port.c`: critical sections, the
//! yield, the tick source, the first-task start and the context switch. The
//! *logic* of the first three is arch-free and lives here; the assembly of
//! the last two lives in `rusty_rtos_port-cortex-m`, `-riscv`, `-xtensa`,
//! the only crates in the family with a fenced `unsafe` block.
//!
//! This crate ships one complete port: [`sim`], the deterministic simulator
//! the conformance corpus runs on. It has no assembly at all, because the
//! sim does not switch stacks — tasks are resumable state machines the
//! runner drives (`rusty_rtos_demo`), which is what lets a `forbid(unsafe)`
//! kernel produce a trace comparable, line for line, with the C kernel's.
//!
//! `forbid(unsafe)`. `no_std` (+ `alloc`).

pub mod sim;

pub use sim::SimPort;

/// Crate version, for manifests and logs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The names a firmware or a scenario wants in scope.
pub mod prelude {
    pub use crate::sim::{EXITS_PER_TICK, SimPort};
    pub use rusty_rtos_core::port::Port;
}
