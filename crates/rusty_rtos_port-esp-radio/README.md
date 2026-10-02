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

**Status: complete, consumed, and re-run on silicon.** All five
implementations are wired; `xiao-s3-radio` and `xiao-s3-wifi` depend on this
crate, and on 2026-10-01 the real `esp-radio` Wi-Fi stack scanned 15 access
points through it on a XIAO ESP32-S3. Connecting to a network (stage 2) has
not been run yet — see *What works today*.

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
| the **seam** — `RadioHost`, `KernelOps`, `Blocked`, `install`, `host` | ✅ landed |
| architecture selection — `Context`, `new_task_context`, `Trampoline`, `raise_switch` | ✅ feature-gated `xtensa` / `riscv` |
| the **adapter body** — `Scheduler`, `Semaphore`, `Queue`, `Timer`, `WaitQueue` | ✅ wired, clippy-clean on both arms |
| the **timer table** — `service_timers` | ✅ moved into the crate; the consumer must call it |
| **re-run on silicon** | ✅ 2026-10-01: `xiao-s3-radio` 50/50 hand-offs through the driver's semaphores; `xiao-s3-wifi` stage 1, a real `esp-radio` scan, 15 APs, with the radio's interrupt side exercised (115 queue sends from interrupt, 0 full) |
| **Wi-Fi association (stage 2) and MQTT over it (stage 3)** | ⬜ not run: both need a network's credentials at build time |
| `unsafe` | every site fenced on its owning item and written up in the port's `UNSAFE.md`; the crate inherits the workspace lints |

Built and clippy-clean for `xtensa-esp32s3-none-elf` and
`riscv32imac-unknown-none-elf`, and for a default-feature build with no
architecture at all. The `xiao-s3-radio` cell links against it.

**What the extraction cost, measured.** Same cell, before and after, same
toolchain: `.text` +3,188 bytes for the `dyn` indirection through the seam,
`.rodata` +596 for the `host()` panic strings, and `.bss` +68 — which is an
exact identity rather than a number to shrug at: 16 extra slot pointers
(`SLOT_CAPACITY` is 32, the cell has 16 tasks) at 4 bytes, plus the 4-byte
host pointer.

**What it has NOT been shown to do.** Association, DHCP and traffic over a
real network; and anything on the C6 (`riscv` arm), which is built and
clippy-clean but has not met a board.

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
rusty_rtos_port-esp-radio = { version = "0.3", features = ["riscv", "esp32c6", "ipc-implementations"] }
```

Exactly one of `xtensa` or `riscv` — a build has one stack layout. With
neither, the adapter and the architecture surface are simply absent and the
rest still compiles, so `cargo check --workspace` works on a crate nobody has
configured yet. The cost is named where it is taken: a default-feature check
does **not** compile the adapter, so both arms are built explicitly.

Then implement the host. The kernel has to be wrapped, because the orphan rule
forbids `impl KernelOps for Kernel<..>` — neither type is yours:

```rust,ignore
struct Ops<'a>(&'a mut MyKernel);

impl rusty_rtos_port_esp_radio::KernelOps for Ops<'_> {
    fn current(&mut self) -> TaskHandle { self.0.current() }
    // ... thirteen more forwards, each `Result` flattened with `.ok()`
}

struct Host;
static HOST: Host = Host;

impl rusty_rtos_port_esp_radio::RadioHost for Host {
    fn max_tasks(&self) -> usize { MAX_TASKS }
    fn max_priorities(&self) -> u8 { MAX_PRIORITIES }
    fn tick_hz(&self) -> u32 { 1_000 }
    fn scheduler_started(&self) -> bool { started() }
    fn enter_critical(&self) -> u32 { mask() }
    fn exit_critical(&self, t: u32) { unmask(t) }
    fn now_us(&self) -> u64 { /* a real microsecond clock */ }
    fn yield_and_switch(&self) { /* raise the switching interrupt */ }

    fn with_kernel(&self, f: &mut dyn FnMut(&mut dyn KernelOps)) {
        my_with_kernel(&mut |k| { f(&mut Ops(k)); Some(()) });
    }
}
```

Then **call `install(&HOST)` before anything touches an adapter type**, and
call `service_timers()` from a task of your own — nothing here has a thread.

### ★ Two ways to wire this that build clean and do nothing

Both are silent, and neither is a type error:

- **Forgetting `install`.** With LTO on, an uncalled `install` makes the host
  pointer provably null, `host()` folds to an unconditional panic, and the
  linker drops every adapter function behind it. Measured on the cell:
  `.text` fell 50,449 → 31,265 bytes and all thirteen kernel queue and
  semaphore symbols vanished, with a clean build and a clean clippy. On the
  board it is a panic on the first driver call.
- **Forgetting `service_timers`.** The table never fires, so the radio's
  retransmits and scan timeouts simply never happen — which looks like a dead
  radio, not a missing call.

If the adapter seems inert, check those two before reading any of it.

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
