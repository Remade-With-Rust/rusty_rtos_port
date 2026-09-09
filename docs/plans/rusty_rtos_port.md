# rusty_rtos_port — package plan

**One sentence:** The FreeRTOS ports remade in Rust: the deterministic sim port (the oracle harness), the Posix host port, and the Cortex-M, RISC-V and Xtensa ports — the only crates in the family with a fenced unsafe block, at the context switch and the vector table.

Family plan: Kairos `docs/plans/rtos-mission.md` (umbrella repo) — its §2.1
names what this package remakes, wraps and never touches; its §6 carries the
phase this package's kill test belongs to. This file obeys that one.

Written 2026-09-09. Status: **K1 — the sim port is complete and proven.**
It carries the whole of sim contract v1 and no `unsafe` at all; the silicon
ports, which do have `unsafe`, are K3's.

---

## 1. What it is, what it is not

**Is:** `portmacro.h` and `port.c`, split in two. The arch-free half —
critical sections, the yield, the tick source, the nesting bookkeeping — is
`rusty_rtos_port-core` and is `forbid(unsafe)`. The arch half — the context
switch and the vector table — will live in `rusty_rtos_port-cortex-m`,
`-riscv` and `-xtensa`, and is the only place in the family where `unsafe`
is allowed, fenced per block and inventoried in `UNSAFE.md`.

**Is not:** a HAL. A port touches the CPU's own state; anything with a
peripheral in it belongs above.

## 2. The laws this package encodes

1. **`unsafe` is a location, not a habit.** None in `-core`, none in the
   sim port. Where it arrives it is one block, with a `SAFETY:` comment and
   a line in `UNSAFE.md`, counted per release.
2. **The sim port is a contract, not a convenience.** Sim contract v1
   (`ORACLES.md`) is implemented on both sides — here, and as six
   exact-anchor edits to the C Posix port — and the two are compared by
   `kairos conform`. If they disagree the traces diverge, which is the
   point.
3. **The port owns the nesting count, and the kernel owns what it means.**
   The port counts; `Port::take_nesting`, `set_nesting` and `swallow_exits`
   let a stackless kernel model what a real port gets from having stacks.

## 3. The surface as built (2026-09-09)

`rusty_rtos_port-core::sim::SimPort` implements `rusty_rtos_core::port::Port`
whole:

| C | here | note |
|---|---|---|
| `vPortEnterCritical` / `vPortExitCritical` | `enter_critical` / `exit_critical` | the exit is where sim time passes: every 16th outermost exit raises a tick |
| `vPortYield` | `yield_now`, `count_yield` | counted (`ulKairosYields`), never a tick source |
| `vPortSystemTickHandler`'s delivery | `take_pending_tick` | the port *raises* a tick; the kernel takes it where the C signal handler would have run |
| `vPortKairosTick` (the harness's idle-hook tick) | `raise_tick` | rule 2 of the contract |
| `prvWaitForStart`'s `uxCriticalNesting = 0` | `take_nesting` + `swallow_exits` | a first switch-in abandons the outgoing frame's exits |
| `prvSwitchThread`'s `uxSavedCriticalNesting` | `set_nesting` | a resumed task's nesting comes back |
| `prvIsFreeRTOSThread` | `scheduler_started` | exits are counted only once tasks are running |

`EXITS_PER_TICK` is 16 here and in the C patch; changing it is a
sim-contract version bump and re-captures every stored trace.

## 4. Roadmap

| Milestone | Adds | Driven by | Kill test |
|---|---|---|---|
| **K1** (done 2026-09-09) | the deterministic sim port | K1 | `dynamic` trace-identical for 100,000 ticks through this port |
| K3a | `-cortex-m`: PendSV context switch, SysTick, `BASEPRI` critical sections, the vector table | K3 | the corpus on QEMU `lm3s6965evb` |
| K3b | `-riscv`: `mret` switch, `mtime`/`mtimecmp`, the C6 through esp-hal | K3 | the corpus on QEMU `virt` and on a C6 |
| K3c | `-posix`: a host port with real threads, so a scenario can run at speed | K3 | the corpus, and a first timing arm |
| K5 | `-xtensa` for the ESP32-S3, and the `esp-radio-rtos-driver` joint with Janus | K5 | the Janus S1 on a Kairos kernel |

## 5. Deliberately absent

- **The 60-plus legacy ports.** Mission plan §2.1: never.
- **AArch64 and the A-profile.** Later, if at all.
- **A tickless-idle implementation** beyond the seam's default: it needs a
  chip with a low-power timer to be worth anything, so it lands with K3.

## 6. Risks

| Risk | Mitigation |
|---|---|
| The sim port's nesting model is a simulator artefact that no silicon port needs, and the seam grows methods for nobody | every one of them has a named line in the C Posix port; a silicon port takes the defaults and pays nothing |
| The first real context switch needs `unsafe` in a shape the family has not reviewed | it lands as one crate, one fenced block per switch, with `UNSAFE.md` and a K3 kill test on QEMU before any board |

## 7. Decision log

| Date | Decision |
|---|---|
| 2026-09-09 | Stamped from the Kairos template; obeys the family plan. |
| 2026-09-09 | The sim port raises a tick and the kernel takes it, rather than the port calling back into the kernel: a callback would mean the port re-entering the thing that owns it. |
| 2026-09-09 | The nesting-across-a-switch methods (`take_nesting`, `set_nesting`, `swallow_exits`) live on the `Port` seam in `rusty_rtos_core`, not in a second trait, so the kernel is generic over one bound and a silicon port inherits working defaults. |
