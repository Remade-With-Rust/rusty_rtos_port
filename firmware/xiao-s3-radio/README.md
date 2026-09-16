# `xiao-s3-radio` — the Kairos kernel behind `esp-radio-rtos-driver`

**Status: it RUNS on silicon and PASSES.** Tasks created through the
driver's own `task_create` — C function pointer, heap stack — block on
adapter semaphores and are scheduled by the Kairos kernel:

```
RADIO laps_a=50 laps_b=50 want=50 faults=0 workers_entered=2
RADIO switch_entries=200 swaps=102 declined_same=98 declined_no_ctx=0
RESULT: PASS -- 50 and 50 hand-offs through the driver's own traits,
        every stack witness intact.
```

It first reported `laps 0/0` with every ready list empty, which turned out to
be a kernel contract defect rather than a bug here:
`Kernel::port_yield` committed the switch before a stacked port could enact
it. `Port::COMMITS_SWITCH` fixes that — see `docs/LEDGER.md`, "The commit
point". `esp-radio` itself still cannot be linked; that reason is a
companion-set conflict and is below.

## What exists

All five `esp-radio-rtos-driver` 0.4.1 implementation traits, against the
Kairos kernel and the Xtensa port:

| trait | how |
|---|---|
| `SchedulerImplementation` | `Kernel` calls throughout; `task_create` allocates a stack and builds a `Context`; the switch is `rusty_rtos_port-xtensa`'s |
| `SemaphoreImplementation` | the kernel's counting semaphores and mutexes, one per radio semaphore |
| `QueueImplementation` | a heap ring buffer guarded by two counting semaphores — the payload cannot go through the kernel's queues, see below |
| `WaitQueueImplementation` | a counting semaphore plus a waiter count, so `notify` broadcasts |
| `TimerImplementation` | the cell's own timer table, serviced on `delay` and a microsecond clock |

`cargo +esp build --release` succeeds for `xtensa-esp32s3-none-elf`.

## What that does and does not prove

**Does:** the five implementations satisfy the trait signatures, and the
registration macros expand. That was the open question — whether a kernel of
this shape can satisfy the interface at all.

**Does not:** that the seam links against the radio. `nm` on the built ELF
finds **zero** `esp_rtos_*` symbols out of 2,295: LTO drops the `#[no_mangle]`
shims because nothing references them, and nothing does because `esp-radio` is
not linked. A compile is a type-check here, not a link proof, and the ELF is
what says so. The cell calls the implementations directly instead, which is
why it can run at all.

## Why `esp-radio` is not linked — a companion-set conflict

Not a choice. The only published radio is `esp-radio 1.0.0-beta.0`, and it
pins:

| | esp-hal | esp-radio-rtos-driver |
|---|---|---|
| `esp-radio 1.0.0-beta.0` | `~1.1.0` | **0.3.0** |
| every Kairos S3 cell | `=1.2.1` | — |
| this adapter, and the plan | — | **0.4.1** |

`xtensa-lx-rt` is a `links` crate — `esp-radio` wants `^0.22`, `esp-hal 1.2.1`
wants `^0.23` — so the two cannot coexist in one binary. Cargo refuses, which
is the right answer.

This also corrects a claim made earlier in `docs/plans/rtos-mission.md`: the
0.4.1-versus-0.3.0 question was "settled" by observing that 0.4.1 is current
on crates.io. That is true of the **driver** and misleading about the
**stack** — the published radio consumes 0.3.0, so the 0.3.0 on this box was
not stale, it was the matching one.

Three ways out, none of them this cell's to choose:

1. **Drop to the radio's set** — `esp-hal 1.1.x` + driver 0.3.0. Forks the
   esp-hal pin away from every other Kairos S3 cell, which is exactly the
   drift §2.7 says one seam crate must own.
2. **Wait** for an `esp-radio` on driver 0.4.x / esp-hal 1.2.
3. **Keep both** — this cell on 0.4.1 for the seam, a second on 0.3.0 for the
   radio. Two adapters to maintain.

## What it actually exercises on the board

`esp-radio` cannot be linked, but that blocks the *radio*, not the *seam*.
The cell drives the seam directly, calling exactly what the blob would:

* `SchedulerImplementation::task_create` — C function pointer, stack size;
* `Semaphore::create` / `take` / `give` — real blocking from inside a task's
  own call frames;
* `yield_task`, and therefore `Kernel::switch_context`.

Two workers ping-pong on adapter semaphores, each re-checking a witness array
on its own heap-allocated stack. Measured: the heap gives 65,536 bytes, both
tasks are created, **one worker's first instruction executes** and two real
context switches occur — then the hand-offs stop, for the kernel reason in
the header.

## The heap, and why

The adapter allocates — task stacks, queue storage, a box per radio object.
That is forced by the interface: `create(capacity, item_size)` returns a
pointer and takes runtime sizes, where the kernel's queues are `[u64; SLOTS]`
behind arena handles fixed at compile time. Fixed pools cannot serve a
consumer that sizes its own objects.

The allocator is the family's `rusty_rtos_alloc`, already proven on this part
by `rusty_rtos_core/firmware/esp32s3-devkit-region`.

**This is the one Kairos cell that allocates on purpose.** The "allocates
nothing (0 allocator symbols)" claim the gate prints covers the kernel and
the corpus; it does not cover this adapter, and it should not be read as
though it did.

## The 1 kHz rounding

`RadioConfig::TICK_RATE_HZ` is 1000, so one tick is 1,000 µs. The radio asks
for sub-millisecond sleeps. The adapter rounds **up** — a wait of 1,500 µs
becomes two ticks — because waking early is a wrong answer and waking late is
a slow one. Whether the radio tolerates that is a hardware question and is
open.

## Running it

```sh
cargo +esp run --release
```

It reports `RESULT: FAIL` today, with the counters that say why:
`workers_entered`, `switch_entries`, `swaps`, and the ready-list census. A
Wi-Fi scan lands when the kernel's commit point moves and the pin conflict is
resolved.
