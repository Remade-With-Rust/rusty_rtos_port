# rusty_rtos_port-xtensa

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![crates.io](https://img.shields.io/crates/v/rusty_rtos_port-xtensa.svg)](https://crates.io/crates/rusty_rtos_port-xtensa)
[![docs.rs](https://docs.rs/rusty_rtos_port-xtensa/badge.svg)](https://docs.rs/rusty_rtos_port-xtensa)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

The **Xtensa LX7** backend of
[`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port), the architecture
seam of [Kairos](https://github.com/Remade-With-Rust/kairos) — FreeRTOS remade
in Rust.

Windowed-register-aware context save, for the ESP32-S3.

- **This is one of the crates allowed `unsafe`.** A context switch is a stack
  swap, and the family fences that into the port backends and nowhere else.
  Every block carries a `SAFETY` note naming the invariant it relies on.
- **Target**: `xtensa-esp32s3-none-elf`.

**Known gaps.** No SMP, no MPU.

## Kill test

Proven on **silicon**, a XIAO ESP32-S3: 100 of 99 expected resumptions from
three frames deep, zero faults, poison-proven.

Worth knowing: the conformance corpus runs on this part **without** a context
switch at all — a scenario is a state machine and a task owns no stack, so the
switch is what you need to host tasks *with* stacks, not what you need to prove
conformance.

## Using it

Hand the kernel a port; everything architecture-shaped is behind the `Port`
trait from [`rusty_rtos_core`](https://crates.io/crates/rusty_rtos_core).

```toml
[dependencies]
rusty_rtos_port-xtensa = "0.1"
```

## Performance

No switch-cost row yet. The kernel's own scheduling round was measured on
this part: **8,313 ns / 1,995 cycles** for a queue send, a queue receive and two
context switches — 88 ppm of the P-256 signature it was measured beside.

```sh
bench/switch-cost/run.sh     # from the Kairos umbrella
```

## Portability

`no_std`, `xtensa-esp32s3-none-elf`.

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
