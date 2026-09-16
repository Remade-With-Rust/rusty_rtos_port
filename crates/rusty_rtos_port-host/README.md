# rusty_rtos_port-host

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![crates.io](https://img.shields.io/crates/v/rusty_rtos_port-host.svg)](https://crates.io/crates/rusty_rtos_port-host)
[![docs.rs](https://docs.rs/rusty_rtos_port-host/badge.svg)](https://docs.rs/rusty_rtos_port-host)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

The **host OS threads** backend of
[`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port), the architecture
seam of [Kairos](https://github.com/Remade-With-Rust/kairos) — FreeRTOS remade
in Rust.

One OS thread per task, a single run permit, and a tick thread that freezes
whoever holds it — so an unmodified C task, which blocks and therefore needs a
real stack, has one.

- **This is one of the crates allowed `unsafe`.** A context switch is a stack
  swap, and the family fences that into the port backends and nowhere else.
  Every block carries a `SAFETY` note naming the invariant it relies on.
- **Target**: `x86-64 Windows and Linux`.

**Known gaps.** No SMP, no MPU.

## Kill test

Its kill test is that a task which **never yields** is still taken off the
CPU: 40 of 40 expected laps, against **1 of 120** with
`KAIROS_HOST_NO_PREEMPT=1`, which is the poison test.

Windows uses `SuspendThread`. Unix has no call that stops another thread from
outside, so the target freezes *itself* in a `SIGUSR1` handler and parks in
`sigsuspend`; `freeze` does not answer until it observes the target parked,
because `pthread_kill` returns when a signal is queued rather than handled, and
answering there would put two tasks on the CPU at once.

## Using it

Hand the kernel a port; everything architecture-shaped is behind the `Port`
trait from [`rusty_rtos_core`](https://crates.io/crates/rusty_rtos_core).

```toml
[dependencies]
rusty_rtos_port-host = "0.1"
```

## Performance

No cycle rows: this port exists to run C demo files and the corpus on a
developer's machine, not to be fast.

```sh
bench/switch-cost/run.sh     # from the Kairos umbrella
```

## Portability

`no_std`, `x86-64 Windows and Linux`.

## Part of Remade With Rust

This crate is part of **[Kairos](https://github.com/Remade-With-Rust/kairos)** —
FreeRTOS remade in memory-safe Rust, as independent packages that expose the API
a FreeRTOS developer already knows and prove every scheduling decision against
the C kernel's own trace. The family:
[`rusty_rtos_core`](https://crates.io/crates/rusty_rtos_core),
[`rusty_rtos_kernel`](https://crates.io/crates/rusty_rtos_kernel),
[`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port),
[`rusty_rtos_heap`](https://crates.io/crates/rusty_rtos_heap). Also check out the
rest of **[github.com/remade-with-rust](https://github.com/remade-with-rust)**.

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.

<!-- /ORG BOILERPLATE -->

## License

MIT OR Apache-2.0, at your option. FreeRTOS is MIT-licensed by Amazon.com, Inc.
or its affiliates; this crate remakes its API and behaviour from the published
sources and links no FreeRTOS code.
