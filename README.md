# rusty_rtos_port

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![crates.io](https://img.shields.io/crates/v/rusty_rtos_port.svg)](https://crates.io/crates/rusty_rtos_port)
[![docs.rs](https://docs.rs/rusty_rtos_port/badge.svg)](https://docs.rs/rusty_rtos_port)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

The **architecture seam** for Kairos: critical sections, the yield, the tick,
stack initialisation and the context switch. This is where the family keeps the
`unsafe` it cannot avoid — a context switch is a stack swap — and it is fenced
into the smallest surface that can do the job. MIT OR Apache-2.0.

- **The deterministic sim port** carries the whole of sim contract v1 and is
  proven against the C Posix port by a trace diff at 100,000 ticks a scenario,
  with `ulKairosExits` and `ulKairosYields` equal every time. **Zero `unsafe`
  blocks.** A port that delivered a tick one critical-section exit early would
  move every line after it, so the trace is the proof.
- **The silicon and host ports** — Cortex-M, RISC-V, Xtensa, and a host port on
  OS threads — each with a QEMU or on-part kill test, and each poison-proven:
  the test is re-run with the mechanism disabled and must FAIL.

**Known gaps.** No SMP and no MPU. The Xtensa port is the newest and its
context switch is proven from three frames deep rather than exhaustively.

- This package's plan: [docs/plans/rusty_rtos_port.md](https://github.com/Remade-With-Rust/rusty_rtos_port/blob/main/docs/plans/rusty_rtos_port.md)
- Every number: [docs/LEDGER.md](https://github.com/Remade-With-Rust/rusty_rtos_port/blob/main/docs/LEDGER.md)
- The family plan: Kairos [`docs/plans/rtos-mission.md`](https://github.com/Remade-With-Rust/kairos/blob/main/docs/plans/rtos-mission.md)

**Claims discipline:** this README makes no performance or capability claim that
is not backed by a test, a benchmark ledger entry, or a kill test recorded in
the plan. "Scaffold" means scaffold. "Sim only" means the sim port; "builds, not
flashed" means no chip has run it.

## Kill tests

A port is the one place where "it compiles" means least, so every backend
carries a test that fails when the mechanism is removed.

| port | kill test | poison test |
|---|---|---|
| sim | 8,408,764 trace lines identical to the C Posix port across nine scenarios | a tick delivered one exit early moves every later line |
| Cortex-M | `PendSV` switch on QEMU `mps2-an385`; preemption cell | — |
| RISC-V | 201 switches between two tasks that never yield, zero faults | — |
| Xtensa | 100/99 resumptions from three frames deep on a XIAO S3, zero faults | ✅ |
| host | a task that NEVER yields is still taken off the CPU: 40 of 40 expected laps | ✅ **1 of 120** with `KAIROS_HOST_NO_PREEMPT=1` |

**A defect this found, and it is the kind that only a kill test finds.** The
RISC-V `switch_context` resumed with `ret`, so the first preemptive switch was
also the last. The port gained `switch_context_trap` and
`new_task_context_preemptive`, and the cell now runs 201 switches with zero
faults.

**Another, on Cortex-M:** `exit_critical` *enabled* interrupts instead of
restoring them, so a task became schedulable before it had a stack and faulted
to `pc = 0` three hundred and sixty ticks later, in a different task. Three
probes came back clean first — because the cell had no `HardFault` handler, so
a faulting task presented as the whole system stopping.

**Still open:** the Unix backend of the host port freezes a thread by signalling
it to park itself, because Unix has no call that stops another thread from
outside; the Windows backend uses `SuspendThread` directly.

## Using it

Pick a backend crate and hand the kernel a port. The trait is small on purpose
— everything architecture-shaped is one of these methods.

```rust
use rusty_rtos_core::port::Port;

/// A port the kernel can drive. `COMMITS_SWITCH` is the one that bites:
/// it tells the kernel whether the port takes the switch itself.
#[derive(Debug, Default)]
struct MyPort;

impl Port for MyPort {
    /// `true` for every port that owns stacks. Left `false`, the kernel
    /// treats itself as STACKLESS and moves `current` inside `port_yield`
    /// — and a port that then switches again makes two selections per
    /// yield, which on a ready list of two is the same task for ever.
    const COMMITS_SWITCH: bool = true;

    fn yield_now(&self) { /* pend the switching exception */ }
    fn enter_critical(&self) { /* mask */ }
    fn exit_critical(&self) { /* RESTORE, never unconditionally enable */ }
    fn set_interrupt_mask_from_isr(&self) -> u32 { 0 }
    fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}
    fn in_isr(&self) -> bool { false }
    fn set_in_tick_entry(&self, _yes: bool) {}
}
```

Both comments above are defects this family actually shipped and fixed; they
are in the trait's own docs for the same reason.

## Performance

| Cortex-M3, both arms the `PendSV` handler | instructions |
|---|---:|
| FreeRTOS `xPortPendSVHandler` | 19 |
| Kairos `PendSV` | **19** |

| RV32, preemptive switch | cycles | vs C |
|---|---:|---:|
| FreeRTOS | 83 | 1.00× |
| Kairos | **74** | **1.12× cheaper** |

Method: both arms counted in their own sections so the boundary is exact, not
sampled. The RISC-V cooperative row reads 2.77× cheaper and should **not** be
quoted as a win — see the kernel crate's README for why the ARM control is what
makes it readable.

```sh
bench/switch-cost/run.sh     # from the Kairos umbrella
```

## Backends

| crate | target | status |
|---|---|---|
| `rusty_rtos_port-core` | any | the trait and the sim port, zero `unsafe` |
| `rusty_rtos_port-cortex-m` | `thumbv7m-none-eabi`+ | QEMU-proven, corpus 18/18 |
| `rusty_rtos_port-riscv` | `riscv32imac-unknown-none-elf` | QEMU-proven, preemptive path fixed and measured |
| `rusty_rtos_port-xtensa` | `xtensa-esp32s3-none-elf` | **on silicon**, XIAO ESP32-S3 |
| `rusty_rtos_port-host` | x86-64 Windows + Linux | OS threads, one run permit; `SuspendThread` / `SIGUSR1` |

## Layout

```text
crates/rusty_rtos_port          facade: re-exports + prelude; the crate you depend on
crates/rusty_rtos_port-core     no_std (+ alloc); forbid(unsafe); types, traits, algorithms
firmware/                per-chip example projects, excluded from the workspace
docs/plans/              this package's plan and its hardening audit
docs/LEDGER.md           every number, with its method line
```

## Build

```sh
cargo test --workspace                                   # host: the tests
cargo check -p rusty_rtos_port-core --no-default-features \
  --target thumbv7em-none-eabihf                         # Cortex-M4F class, no alloc
cargo check -p rusty_rtos_port-core --no-default-features --features alloc \
  --target riscv32imac-unknown-none-elf                  # ESP32-C6 class, with alloc
```

CI holds the core to `thumbv7em-none-eabihf`, `thumbv8m.main-none-eabihf`,
`riscv32imac-unknown-none-elf` and `riscv32imafc-unknown-none-elf`, with and
without `alloc`, plus `cargo deny check`. Firmware examples (Xtensa needs the
esp toolchain; Cortex-M and RISC-V work on stable) are built from their own
directories under `firmware/`.

## Part of Remade With Rust

This crate is part of **[Kairos](https://github.com/Remade-With-Rust/kairos)** —
FreeRTOS remade in memory-safe Rust, as independent packages that expose the API
a FreeRTOS developer already knows and prove every scheduling decision against
the C kernel's own trace. `rusty_rtos_port` is the seam between it and the metal.

The family:
[`rusty_rtos_core`](https://crates.io/crates/rusty_rtos_core) (the shared
vocabulary),
[`rusty_rtos_kernel`](https://crates.io/crates/rusty_rtos_kernel) (the
scheduler),
[`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port) (the architecture
seam),
[`rusty_rtos_heap`](https://crates.io/crates/rusty_rtos_heap) (the allocators),
`rusty_rtos-capi` (the C ABI, not yet published) and
`rusty_rtos_demo` (the conformance corpus, not yet published). Also check out the rest of
**[github.com/remade-with-rust](https://github.com/remade-with-rust)**.

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.

<!-- /ORG BOILERPLATE -->

## License

MIT OR Apache-2.0, at your option. FreeRTOS is MIT-licensed by Amazon.com,
Inc. or its affiliates; this crate remakes its API and behaviour from the
published sources and links no FreeRTOS code.

---

<!-- HARDENING-TABLE:BEGIN generated by use-protection-please — edit docs/plans/use-protection-please.md, not this block -->
## Hardening status

**Tier** critical-path · **Audited** 2026-09-16 (v0.1.0 release pass) · **v1.0.0 gates** 10/17 · [Full checklist](https://github.com/Remade-With-Rust/rusty_rtos_port/blob/main/docs/plans/use-protection-please.md)

`████████░░░░░░░░░░░░` **42%** &nbsp;·&nbsp; 15 Completed · 0 Scheduled · 21 Incomplete · 19 N/A

| Phase | ✅ Completed | 🗓 Scheduled | ⬜ Incomplete | · N/A |
|---|--:|--:|--:|--:|
| 0 — Threat modeling | 0 | 0 | 2 | 0 |
| 1 — Toolchain | 2 | 0 | 2 | 0 |
| 2 — Supply chain | 7 | 0 | 1 | 0 |
| 3 — Code level | 3 | 0 | 4 | 0 |
| 4 — Static analysis | 0 | 0 | 1 | 0 |
| 5 — Dynamic analysis | 1 | 0 | 2 | 0 |
| 6 — Fuzzing and properties | 1 | 0 | 3 | 0 |
| 7 — Formal verification | 0 | 0 | 1 | 0 |
| 8 — Build and binary | 0 | 0 | 1 | 1 |
| 9 — Runtime privilege | 0 | 0 | 0 | 1 |
| 10 — Cryptography | 0 | 0 | 0 | 3 |
| 11 — CI/CD, release, and operations | 1 | 0 | 4 | 0 |
| 12 — Compliance controls | 0 | 0 | 0 | 14 |
| **Total** | **15** | **0** | **21** | **19** |

Gates waived for 0.x are listed with their reasons in the plan's "v0.1.0 release decision" section — an Incomplete gate not listed there is an omission, not a decision.

**Architect** — [Tim Almond](https://github.com/Ttimmahlax) — accountable for this unit's security design; rendered
<!-- HARDENING-TABLE:END -->
