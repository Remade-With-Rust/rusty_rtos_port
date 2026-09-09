#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
//! `rusty_rtos_port` — The FreeRTOS ports remade in Rust: the deterministic sim port (the oracle harness), the Posix host port, and the Cortex-M, RISC-V and Xtensa ports — the only crates in the family with a fenced unsafe block, at the context switch and the vector table.
//!
//! This is the facade: it re-exports the `no_std` core. Depend on this crate;
//! reach into the sub-crates only when you are building a port or a backend.
//!
//! Part of Kairos (Remade With Rust). Plan: `docs/plans/rusty_rtos_port.md`.

pub use rusty_rtos_port_core::*;

/// The names a firmware wants in scope.
pub mod prelude {
    pub use rusty_rtos_port_core::prelude::*;
}
