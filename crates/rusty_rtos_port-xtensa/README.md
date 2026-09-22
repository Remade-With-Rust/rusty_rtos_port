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
rusty_rtos_port-xtensa = "0.2"
```

## Performance

No switch-cost row yet. What this part does carry is the family's only
**cycle** rows, because Xtensa `ccount` is a real cycle counter at one cycle
of resolution where QEMU has none worth the name:

| XIAO ESP32-S3, 240 MHz | cycles | |
|---|---:|---:|
| tick | **54** | 225 ns |
| context switch | **166** | 691 ns |
| ISR-API wake, to the task holding the value | **430** | 1,792 ns |

A full scheduling round — a queue send, a queue receive and two context
switches — costs **3,724 ns / 893 cycles**, which is **39 ppm** of the P-256
signature it was measured beside.

> Re-measured 2026-09-21. These read 131 / 623 / 949 and 1,995 until the
> measurement cells were found to be hand-rolling a `NoTrace` that shadowed
> the one `rusty_rtos_core` ships, inheriting `WANTS_NAMES = true` so every
> traced event built a task name for a sink that drops it. No port or kernel
> code changed.

```sh
bench/switch-cost/run.sh     # from the Kairos umbrella
```

## Portability

`no_std`, `xtensa-esp32s3-none-elf`.

## Part of Remade With Rust

This crate is part of **[Kairos](https://github.com/Remade-With-Rust/kairos)** — FreeRTOS remade in memory-safe
Rust, as independent packages that expose the API a FreeRTOS developer already
knows and prove every scheduling decision against the C kernel's own trace.

**Where this sits for Mata.** Kairos is the real-time layer on the device
itself, and [`rusty_rtos_mqtt`](https://crates.io/crates/rusty_rtos_mqtt) is the way out of it.
Paired with the **MATA distributed cloud**, robotics and sensor data has two
routes — read it on the machine, or reach it through the cloud — with the same
memory-safe crates at both ends.

The family:
[`rusty_rtos_core`](https://crates.io/crates/rusty_rtos_core) (the shared vocabulary),
[`rusty_rtos_kernel`](https://crates.io/crates/rusty_rtos_kernel) (the scheduler),
[`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port) (the architecture seam),
[`rusty_rtos_heap`](https://crates.io/crates/rusty_rtos_heap) (the allocators),
[`rusty_rtos_json`](https://crates.io/crates/rusty_rtos_json) (coreJSON),
[`rusty_rtos_sntp`](https://crates.io/crates/rusty_rtos_sntp) (coreSNTP),
[`rusty_rtos_mqtt`](https://crates.io/crates/rusty_rtos_mqtt) (coreMQTT),
[`rusty_rtos_backoff`](https://crates.io/crates/rusty_rtos_backoff) (backoffAlgorithm),
[`rusty_rtos-capi`](https://crates.io/crates/rusty_rtos-capi) (the C ABI) and
[`rusty_rtos_demo`](https://crates.io/crates/rusty_rtos_demo) (the conformance corpus).
All ten are on crates.io. Also check out
the rest of **[github.com/remade-with-rust](https://github.com/remade-with-rust)**.

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

**[Mata Network](https://www.mata.network/)** builds sovereign, self-hostable
privacy infrastructure — *"stop sacrificing your privacy for convenience"*:
wallet & identity, a password manager, a contact manager, and a browser
extension that stops your information leaking as you browse.

**Remade With Rust** is our open-source home for the permissively-licensed
building blocks that work depends on — including
[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs) (the
FFmpeg alternative) and [FFAI](https://github.com/Remade-With-Rust/FFAI) (the
AI media toolkit).

→ **[www.mata.network](https://www.mata.network/)**

<!-- /ORG BOILERPLATE -->

## License

MIT OR Apache-2.0, at your option. FreeRTOS is MIT-licensed by Amazon.com, Inc.
or its affiliates; this crate remakes its API and behaviour from the published
sources and links no FreeRTOS code.
