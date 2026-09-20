# `xiao-s3-tickless`

**The Kairos kernel driven by a tick interrupt on a XIAO ESP32-S3 — and the
same tickless idle the ARM cell proves.**

Two claims in one cell, because the second cannot be made without the first
and the first did not exist anywhere in this tree:

1. **The Kernel runs from a tick on an Xtensa part.** Every other
   kernel-driving cell is Cortex-M under QEMU. The two existing XIAO cells
   (`xiao-s3-switch`, `xiao-s3-radio`) prove the *port's* switch and nothing
   above it. This wires `SYSTIMER` alarm 0 to `Kernel::increment_tick` and
   `Software0` to `Kernel::switch_context` — the joint the
   `mps2-an385-qemu-*` cells prove on ARM.
2. **Tickless idle suppresses those interrupts without moving the schedule**,
   as `mps2-an385-qemu-tickless` has measured on ARM.

## Measured on the board

Flashed to a XIAO ESP32-S3 (rev v0.2, 8 MB flash, 40 MHz crystal, MAC
68:ee:8f:51:74:64) over `espflash` on COM4.

**Claim 1 — the Kernel runs from a tick on Xtensa — PASSES on silicon.**

```
arm                   plain
laps                  20   (want 20)
logical ticks         400
alarm wakeups         400
scheduler stalls      0   first: None
projected events      195   switches 63
SCHEDULE DIGEST       ebb908b74bccb99e
RESULT: PASS
```

400 ticks for 400 laps' worth of delay, every one paid for with an
interrupt, no stalls. That is the K3 prerequisite met: `SYSTIMER` alarm 0 ->
`Kernel::increment_tick` -> `Software0` -> `Kernel::switch_context`, on a
real Xtensa part.

**Claim 2 — tickless suppresses them without moving the schedule — PASSES
too.**

| | control | tickless |
|---|---:|---:|
| logical ticks | 400 | 400 |
| **alarm wakeups** | **400** | **0** |
| projected events | 195 | 195 |
| context switches | 63 | 63 |
| **schedule digest** | `ebb908b74bccb99e` | `ebb908b74bccb99e` |

Four hundred wakeups to none, same schedule, digest byte-identical and
pinned. The sleep diagnostics read 20 sleeps, 400 ticks asked and 400 slept,
the last window measuring 20,014 us against a 20,000 us request — the
elapsed time is genuinely read off the counter, not assumed.

## ★★ M3c: with a FAIR baseline, tickless LOSES on this part

The table above counts wakeups. Wakeups are not energy, and the honest
measurement needed two things the first version did not have: a control arm
that **halts the core** the way FreeRTOS's idle task does, and a measure of
how long the core was actually running.

Both arms now halt in `waiti` and accumulate time-halted off the free-running
SYSTIMER. The result inverts the headline:

| | control (`waiti` per tick) | tickless |
|---|---:|---:|
| wall | 399,595 us | 399,611 us |
| **core active** | **2,506 us** | **3,722 us** |
| **duty cycle** | **0.6 %** | **0.9 %** |
| alarm wakeups | 400 | 0 |
| clock drift | 0.1 % | 0.0 % |

**Tickless spent ~48 % MORE time with the core running.** Four hundred wakeups
became zero and the part worked *harder*.

### Why, and it is arithmetic rather than a defect

A `waiti` control is **already 99.4 % halted.** That number is the ceiling for
any idle optimisation whatsoever on this workload: even a tickless
implementation that cost literally nothing could remove at most 2,656 us of a
399,666 us run. There is no prize here to win.

And this one is not free. Each sleep replaces ~20 tick interrupts at ~6.6 us
apiece (~133 us) with one suspend / reprogram / sleep / measure / restore /
resume cycle costing ~182 us — a net **+49 us per sleep**, twenty times over.

The deeper reason is that **`waiti` is a shallow halt**: it stops the core
clock and leaves everything else powered, so re-entering it costs one
interrupt entry — and on Xtensa that is a register-window spill, not a cheap
one, but still only microseconds. Tickless pays for itself when a wakeup is
*expensive*. It is not, here.

### The verdict, which prunes a milestone

> **Tickless idle is a lever on sleep DEPTH, not on sleep COUNT. Removing
> wakeups is worth nothing until a wakeup is worth something.**

So the next brick is `Rtc::sleep_light` — which gates clocks and drops power
domains, making a wake cost hundreds of microseconds and real charge, at
which point 400 to 0 becomes the whole game. The mechanism here is built to
survive that swap: elapsed time is read off the counter rather than trusted,
which is exactly what a sleep that reports nothing requires.

**A fitted sleep-length policy is pruned.** Tuning *when* to sleep cannot
help when the sleep is the wrong *depth*, and no policy beats a 0.6 % ceiling.

### Predicted in advance, and wrong

Before the run the prediction on record was that the difference would be
"small". It was neither small nor in the predicted direction, which is the
more useful outcome: a confirmed guess teaches nothing, and this one produced
the ceiling argument that closed out the mission.

The instrument is deterministic: repeat runs reproduce to within a couple of
microseconds, so the difference is orders of magnitude larger than the spread.

(An earlier version of this table read 2,656 and 3,638 us with the tickless
arm's wall at 403,960 us. That extra 4,294 us was a **clock-drift defect**,
not a cost — see the next section. The conclusion is unchanged and slightly
stronger without it.)

## ★★★ The second bug, found by a question rather than a test

The table above originally showed the tickless arm taking **4,294 us longer
in wall time** for the same 400 logical ticks. That was reported as incidental.
A reader asked the obvious question — *if it took longer, isn't that a bad
thing?* — and it was:

| | us per logical tick | drift |
|---|---:|---:|
| control | 999.2 | -0.08 % |
| **tickless, before** | **1009.9** | **+0.99 %** |

**The tickless kernel was losing real time.** An hour of it would run **38.7
seconds slow**. Delays, timeouts and software timers all stretch by 1 %.

### The cause

The tick was a free-running 1 ms period, and the sleep **restarted that period
at the instant it woke**. So every sleep discarded however much of a period had
already elapsed while the worker ran — ~180 us a lap — and the loss compounded.
91 % of the 4,294 us gap is exactly the measured active time. A standalone
simulation of the old logic predicts +0.965 % against +0.99 % measured.

### The fix: an absolute grid, not a period

`NEXT_TICK_US` holds the absolute microsecond instant of the next tick. Every
tick is a **one-shot armed at `grid - now`**, so latency in arming shortens
that one interval instead of moving the grid, and a sleep targets a **grid
point** rather than a duration. Errors cannot accumulate because every arming
is computed from the grid rather than from the last one.

Measured after: wall 399,611 us against the control's 399,595 us — a
**16 us** gap where there had been 4,294 — and 0.0 % drift.

### Why no test caught it, which is the part worth keeping

Every check in this cell counted **logical** ticks, and both arms produce
exactly 400 of those *however badly the timer is driven*. The schedule digest
matched too, because the order of events was never wrong. The defect lived in
the mapping from logical ticks to real time, and nothing measured that mapping.

> **A gate that only compares the system to itself cannot catch the system's
> shared reference drifting.** Both arms agreed with each other and both were
> wrong about the wall clock. The cure is an *external* reference — here the
> free-running counter — and a check that the two agree.

There is now one: wall-time-per-logical-tick must be within 0.3 % of nominal.
It is the only check in this cell that compares the kernel to something
outside itself.

### And it was a known hazard, written down on the other port

The ARM cell's README says, in as many words, that sleeping for N whole
periods from here "would shift every later tick by a fraction of one, for
ever". Boundary-sleeping was implemented there deliberately — and then not
carried across to Xtensa. Knowing a hazard clearly is no defence against
walking into it in the second implementation.

## ★ The bug the board found, which building never would have

The first run of the tickless arm **failed**, and that failure is the whole
argument for owning hardware.

`waiti 0` does not merely lower `PS.INTLEVEL` for the duration of the sleep.
It **sets it to zero and leaves it there** — the interrupt that wakes you
returns through `RFI`, which restores the PS that `waiti` installed. So the
caller's critical section is gone from the wake onward, not just suspended
during the sleep.

Everything `idle_suppress_ticks` still had to do after the sleep —
`step_tick`, `resume_all`, two trace events — ran with interrupts open, on a
kernel the idle task held `&mut` to. It did not crash. The scheduler just
quietly stopped working: the worker never blocked, and the cell reported
**20 logical ticks where the arithmetic says 400**.

The cure is one line, re-raising the mask the instant `waiti` returns. ARM
needs no equivalent, because `wfi` leaves PRIMASK alone. This is the kind of
defect that reads as a scheduler logic bug for a day before anyone suspects
the instruction — and no amount of compiling, linking, section-sizing or
disassembly would have found it.

### The honest footnote

Two changes were made and **only one mattered.** The switch handler was also
taught to decline while `SLEEPING`, on the theory that `Software0` could be
taken in the open window. The counter for it reads **zero** across the whole
run. That path is defence in depth; it is not what fixed this, and it is
labelled so rather than banked as part of the cure.

```
cargo run --release                        # the plain control arm
cargo run --release --features tickless    # the suppressing arm
```

`cargo run` flashes and monitors via `espflash`.

## What to look for

```
arm                   plain | TICKLESS
laps                  20   (want 20)
logical ticks         ~400
alarm wakeups         ~400  |  near zero
projected events      ...   switches ...
SCHEDULE DIGEST       xxxxxxxxxxxxxxxx   <- must MATCH across the two arms
```

Each arm gates itself on laps, stalls, the pinned digest, the tick band and
its own wakeup claim, so either command alone is a kill test. The digest is
pinned at `ebb908b74bccb99e`, and one constant serves both arms — that is the
cross-arm claim, carried by a number rather than by a promise to run a diff.

If the control arm alone passes, claim 1 is proved and the K3 prerequisite is
gone even if tickless needs work.

## Two ways this differs from the ARM cell, both forced

### `waiti 0` unmasks; `wfi` does not

On Cortex-M the idle task holds PRIMASK and `wfi` **still wakes on a pending
masked interrupt**, so the port sleeps, accounts for the time, and drops the
pending SysTick with `ICSR.PENDSTCLR`. The exception is never taken.

`waiti 0` does the opposite — it *sets* `PS.INTLEVEL` to zero, so it unmasks
on the way in and the alarm interrupt really is **taken**. There is no
pending bit left to clear, because the handler has already run.

So the suppression moves into the handler. A `SLEEPING` flag is set across
the sleep, and the tick handler reads it as its very first statement: when it
is set the handler acknowledges the alarm and returns, touching nothing else.
It *must* touch nothing else — the idle task is inside `with_kernel` and
holds the kernel mutably at that moment.

### Elapsed time is measured, not assumed

`esp_hal`'s `Rtc::sleep_light` is the deeper sleep this cell does not yet
use, and its own documentation is the warning: **a refused sleep, a rejected
sleep and a very short sleep are indistinguishable from its return.** A port
that trusted a sleep would wind the kernel's clock past a task's wake time,
and the kernel cannot rescue a report it was told was whole.

So this port does not trust it. `SYSTIMER` is free-running and is read with
`Timer::now()` on both sides of the sleep; the ticks reported are whatever
the counter says elapsed, floored to whole ticks and clamped to what was
asked. That is sound whether the sleep ran to its end, was cut short by
another interrupt, or never happened — and it is the mechanism that stays
sound when the sleep underneath it becomes a light sleep.

## Why the mechanism is here and not in the port crate

On ARM it lives in `rusty_rtos_port-cortex-m`, beside the rest of the SysTick
register map. That is not an inconsistency: **SysTick is a *core* peripheral,
so the ARM port already owns its registers, while `SYSTIMER` is a *chip*
peripheral belonging to `esp-hal`.** `rusty_rtos_port-xtensa` is HAL-free by
design — it depends on `xtensa-lx` and nothing else — so a sleep that needs
`esp-hal` cannot live in it without making every Xtensa user take a HAL.

The cell therefore defines a `TicklessPort` newtype wrapping `XtensaPort`,
delegating the `Port` trait and adding `suppress_ticks_and_sleep`. If a
second ESP part ever wants this, the newtype is what gets promoted into a
`rusty_rtos_port-esp` crate, not the Xtensa port.

## Verified without the board

Both arms build, link, and produce a flashable merged image. Measured on the
linked artifact rather than argued from the source:

| | control | tickless |
|---|---:|---:|
| app image | 97,696 B | 99,344 B |
| share of the 4 MB partition | 2.37% | 2.41% |
| `waiti` instructions | **0** | **1** |
| `SLEEPING` static | absent | present |

`.bss` is 25,792 B — the three 8 KB task stacks, present and correctly sized
— and `.data` + `.bss` + `.stack` is ~334 KB of the S3's 512 KB SRAM, so it
fits with room. The `KERNEL` static is 2,264 B.

The instruction counts are a **reachability** check, not trivia: a cell that
compiles is not a cell whose new code is wired. With `USE_TICKLESS_IDLE`
false LLVM proves the whole sleep path dead and deletes it, `waiti` included;
with it true the sleep is there. The two images differ, so the feature flag
reaches the binary rather than being quietly ignored.

### ⚠ The control arm busy-spins, so it is not a fair CURRENT baseline

That zero is also a caveat. `idle_suppress_ticks` returns immediately when
the const is false, and this cell's idle task does nothing else — so the
control arm never reaches `XtensaPort::idle`'s `waiti 0` and spins the core
flat out instead.

That is the right control for the claim being made here, because **wakeups**
is the quantity, and a `waiti`-ing idle task would be woken by every one of
those ticks just the same. It is the WRONG control for a current measurement:
against a spinning baseline tickless would look far better than it deserves.
M3c must compare against an idle task that calls `Port::idle`, not against
this one.

## What this cell does NOT claim

* **Not an energy number.** It counts wakeups. Energy needs a shunt, and that
  is the point of having the board at all — this cell is what makes such a
  measurement possible, not the measurement.
* **Not light or deep sleep.** The sleep is `waiti 0`, which stops the core
  and leaves the peripherals running. `Rtc::sleep_light` is the next step and
  the measurement above is designed to survive it.
* **Not a policy.** `expected_idle_time` less nothing at all, which is the
  fixed policy the C ships.
* **Not proved.** See the banner.
