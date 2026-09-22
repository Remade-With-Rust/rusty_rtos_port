# rusty_rtos_port-cortex-m

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![crates.io](https://img.shields.io/crates/v/rusty_rtos_port-cortex-m.svg)](https://crates.io/crates/rusty_rtos_port-cortex-m)
[![docs.rs](https://docs.rs/rusty_rtos_port-cortex-m/badge.svg)](https://docs.rs/rusty_rtos_port-cortex-m)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

The **Cortex-M** backend of
[`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port), the architecture
seam of [Kairos](https://github.com/Remade-With-Rust/kairos) — FreeRTOS remade
in Rust.

Critical sections that **restore** rather than enable, `PendSV` for the
switch, SysTick for the tick, and `pxPortInitialiseStack`'s frame.

- **This is one of the crates allowed `unsafe`.** A context switch is a stack
  swap, and the family fences that into the port backends and nowhere else.
  Every block carries a `SAFETY` note naming the invariant it relies on.
- **Target**: `thumbv7m-none-eabi`.

**Known gaps.** No SMP, no MPU.

## Kill test

Proven on QEMU `mps2-an385`: the conformance corpus 18/18, a preemption cell,
and 25 unmodified C demo files through the C ABI.

**A defect this port shipped and fixed, because it is the instructive kind.**
`exit_critical` *enabled* interrupts instead of restoring them, so a task became
schedulable before it had a stack and faulted to `pc = 0` three hundred and
sixty ticks later — in a different task. Three probes came back clean first,
because the cell had no `HardFault` handler, so a faulting task presented as the
whole system stopping.

## Using it

Hand the kernel a port; everything architecture-shaped is behind the `Port`
trait from [`rusty_rtos_core`](https://crates.io/crates/rusty_rtos_core).

```toml
[dependencies]
rusty_rtos_port-cortex-m = "0.2"
```

## Performance

| Cortex-M3 `PendSV` | instructions |
|---|---:|
| FreeRTOS `xPortPendSVHandler` | 19 |
| Kairos `PendSV` | **19** |

Exact parity: both kernels reach the switch the same way, so the shapes match
and so do the counts.

```sh
bench/switch-cost/run.sh     # from the Kairos umbrella
```

## Portability

`no_std`, `thumbv7m-none-eabi`.

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
