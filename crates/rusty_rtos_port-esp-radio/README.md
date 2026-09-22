# rusty_rtos_port-esp-radio

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![crates.io](https://img.shields.io/crates/v/rusty_rtos_port-esp-radio.svg)](https://crates.io/crates/rusty_rtos_port-esp-radio)
[![docs.rs](https://docs.rs/rusty_rtos_port-esp-radio/badge.svg)](https://docs.rs/rusty_rtos_port-esp-radio)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

`esp-radio` on a [Kairos](https://github.com/Remade-With-Rust/kairos) kernel:
the five [`esp-radio-rtos-driver`](https://crates.io/crates/esp-radio-rtos-driver)
implementations — scheduler, semaphores, queues, timers, wait queues — as a
crate rather than glue inside one firmware.

**Status: the seam is landed; the adapter body is not yet wired.** See
*What works today* below, which is the honest line and not the hopeful one.

## Why it exists

The adapter passed on silicon from 2026-09-11 inside
`rusty_rtos_port/firmware/xiao-s3-radio`. A firmware directory is not
consumable, and Janus said so:

> Janus cannot consume ~1,450 lines of `adapter.rs` + `kernel.rs` from inside
> a firmware directory. If that glue became a crate, the Janus mesh node
> could take it the day S1's baseline exists, instead of Janus duplicating it
> and the two copies drifting.
>
> — `docs/plans/janus-rtos.md` §5b

Two copies of a driver adapter that drift is the failure this avoids.

## What works today

| | |
|---|---|
| the **seam** — `RadioHost`, `KernelOps`, `Blocked`, `install`, `host` | ✅ landed, compiles for `riscv32imac` and `riscv32imafc` |
| architecture selection — `Context`, `new_task_context`, `Trampoline` | ✅ feature-gated `xtensa` / `riscv` |
| the **adapter body** (the five `impl`s) | ⏳ present as `src/adapter.rs`, **not compiled yet** |

A consumer can write their `RadioHost` against the seam now; they cannot run
`esp-radio` on it until the adapter is wired.

**What is left, exactly** — 16 call sites in 895 lines: 34 `K` →
`dyn KernelOps`, 21 `with_kernel`, 9 `crate::kernel::`, 5 `Wait` →
`Blocked`, 4 `yield_and_switch`, 3 `now_us`, 3 `Context`. Eight of the 16 are
multi-line closures that returned a value, and the seam cannot return one
generically through a `dyn`, so each is a semantic edit rather than a
substitution — in glue that currently passes on silicon. It is being done
deliberately for that reason.

## The design, and the one decision worth arguing about

The obvious extraction gives the host one method per kernel call —
`host().semaphore_give(s)`. **It is wrong, and quietly so.** Several places
do three or four kernel calls inside one closure: creating a queue takes two
semaphores and a mutex; deleting one releases four handles. The original held
a single borrow across each group. One method per call turns one acquisition
into four and opens a window where another task can run — a change no type
would catch, showing up as a rare failure on a board.

So the seam is shaped like the thing it replaces: `with_kernel` hands out a
`&mut dyn KernelOps` for one closure, and atomicity stays the host's.

The cost is that a `dyn` trait cannot have a generic method, so the closure
returns nothing and callers capture into a local. That is the price of a
single installed host, and it is paid explicitly.

## Using it

```toml
[dependencies]
rusty_rtos_port-esp-radio = { version = "0.2", features = ["riscv", "esp32c6", "ipc-implementations"] }
```

Exactly one of `xtensa` or `riscv` — a build has one stack layout. With
neither, the architecture surface is simply absent and the seam still
compiles, so `cargo check --workspace` works on a crate nobody has configured
yet.

## Portability

`no_std`. Needs a global allocator: the driver hands out raw pointers and
takes runtime sizes.

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

MIT OR Apache-2.0, at your option. FreeRTOS is MIT-licensed by Amazon.com,
Inc. or its affiliates; this crate remakes its API and behaviour from the
published sources and links no FreeRTOS code.
