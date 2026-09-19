# rusty_rtos_port-core

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![crates.io](https://img.shields.io/crates/v/rusty_rtos_port-core.svg)](https://crates.io/crates/rusty_rtos_port-core)
[![docs.rs](https://docs.rs/rusty_rtos_port-core/badge.svg)](https://docs.rs/rusty_rtos_port-core)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

The pure `no_std` core of
[`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port): the `Port` trait
every Kairos backend implements, and the deterministic **sim port** that proves
the kernel against the C kernel's trace. **Zero `unsafe` blocks.**

- **The trait**: critical sections, the yield, the tick entry, the ISR mask,
  and `COMMITS_SWITCH` — which tells the kernel whether the port takes the
  switch itself. Left wrong, the kernel makes two selections per yield, and on
  a ready list of two that is the same task for ever.
- **The sim port**: sim contract v1, including `begin_unwind` / `end_unwind`,
  which exist because a stackless kernel's abandoned frame runs to its end and
  that tail must not be counted as sim time.

**Known gaps.** No SMP, no MPU. This crate has no architecture in it — the
silicon backends are separate crates.

## Conformance

The sim port is proven against the C Posix port by a trace diff:
**8,408,764 lines identical across nine scenarios** at 100,000 ticks each, with
`ulKairosExits` and `ulKairosYields` equal on every one. A port that delivered
a tick one critical-section exit early or late would move every line after it,
so the trace is the proof.

## Using it

```rust
use rusty_rtos_core::port::Port;

#[derive(Debug, Default)]
struct MyPort;

impl Port for MyPort {
    /// `true` for every port that owns stacks.
    const COMMITS_SWITCH: bool = true;

    fn yield_now(&self) {}
    fn enter_critical(&self) {}
    /// RESTORE, never unconditionally enable — that was a real defect, and
    /// it faulted 360 ticks later in a different task.
    fn exit_critical(&self) {}
    fn set_interrupt_mask_from_isr(&self) -> u32 { 0 }
    fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}
    fn in_isr(&self) -> bool { false }
    fn set_in_tick_entry(&self, _yes: bool) {}
}
```

## Performance

No rows of its own: the sim port is deterministic and untimed by design.
The measured switch costs belong to the silicon backends — see the
[repository README](https://github.com/Remade-With-Rust/rusty_rtos_port#performance).

## Portability

`no_std` on host, `thumbv7m-none-eabi`, `riscv32imac-unknown-none-elf` and
`xtensa-esp32s3-none-elf`.

## Part of Remade With Rust

This crate is part of **[Kairos](https://github.com/Remade-With-Rust/kairos)** — FreeRTOS remade in memory-safe
Rust, as independent packages that expose the API a FreeRTOS developer already
knows and prove every scheduling decision against the C kernel's own trace.

**Where this sits for Mata.** Kairos is the real-time layer on the device
itself, and [`rusty_rtos_mqtt`](https://github.com/Remade-With-Rust/rusty_rtos_mqtt) is the way out of it.
Paired with the **MATA distributed cloud**, robotics and sensor data has two
routes — read it on the machine, or reach it through the cloud — with the same
memory-safe crates at both ends.

The family:
[`rusty_rtos_core`](https://crates.io/crates/rusty_rtos_core) (the shared vocabulary),
[`rusty_rtos_kernel`](https://crates.io/crates/rusty_rtos_kernel) (the scheduler),
[`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port) (the architecture seam),
[`rusty_rtos_heap`](https://crates.io/crates/rusty_rtos_heap) (the allocators),
[`rusty_rtos_json`](https://github.com/Remade-With-Rust/rusty_rtos_json) (coreJSON),
[`rusty_rtos_sntp`](https://github.com/Remade-With-Rust/rusty_rtos_sntp) (coreSNTP),
[`rusty_rtos_mqtt`](https://github.com/Remade-With-Rust/rusty_rtos_mqtt) (coreMQTT),
[`rusty_rtos_backoff`](https://github.com/Remade-With-Rust/rusty_rtos_backoff) (backoffAlgorithm),
[`rusty_rtos-capi`](https://github.com/Remade-With-Rust/rusty_rtos-capi) (the C ABI) and
[`rusty_rtos_demo`](https://github.com/Remade-With-Rust/rusty_rtos_demo) (the conformance corpus).
The last six are on GitHub and not yet on crates.io. Also check out
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
