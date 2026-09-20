# `mps2-an385-qemu-tickless`

**Tickless idle on an ARMv7-M: 401 wakeups become 0, and the schedule does
not move.**

This is the first cell in the Kairos tree where a timer is really
reprogrammed. `kairos power idle` sized the prize over stored oracle traces,
`kairos power diff` demonstrated the projection over the same traces, and the
kernel's own tickless tests proved the mechanism against a port that *returns
a number*. None of that touched hardware. This does: SysTick is stopped,
reloaded for one long interval, and waited on with `wfi`.

## Running it

```
cargo run --release                        # the plain control arm
cargo run --release --features tickless    # the suppressing arm
```

Each arm gates itself and exits non-zero on failure, so either command alone
is a kill test. The feature is the **only** difference between the two
builds; both run the same tasks, the same delays and the same idle loop, and
the kernel's `idle_suppress_ticks` simply returns when the const is false.

## What it measured

| | control | tickless |
|---|---:|---:|
| logical ticks | 401 | 400 |
| **SysTick wakeups** | **401** | **0** |
| projected events | 193 | 193 |
| context switches | 62 | 62 |
| **schedule digest** | **`94f229d7a5bd8f77`** | **`94f229d7a5bd8f77`** |

Twenty laps of a worker that delays 20 ticks, so the part is idle for
essentially the whole run. Every tick in the control arm cost an interrupt;
in the tickless arm none did.

**Wakeups are a proxy and are named as one.** QEMU has no power model, so an
energy number is not available here and is not claimed. Wakeups are exactly
countable, deterministic, and are what idle energy is proportional to on a
part that spends its idle time in `wfi`. The energy claim belongs to a board
and a current shunt.

## The gate

Three checks, and the second one exists because the first cannot see
everything:

1. **Order digest equals a pinned constant.** FNV-1a over the
   `rusty_rtos_core::trace::Scheduling` projection — every event's kind and,
   where it names one, which task. One constant serves both arms, so the
   cross-arm claim is carried by a number rather than by a promise to run a
   diff. A lost wake or a switch to the wrong task moves it.
2. **The logical tick count lands in a band the scenario's arithmetic
   fixes.** The order digest cannot see a wake that arrives in the right
   *sequence* but N ticks *late*, which is exactly what an oversleeping port
   produces.
3. **The wakeup count collapses** (tickless arm) or **matches the tick count
   exactly** (control arm). The second half is what gives the first something
   to be smaller than.

### Poison-proved

Deleting the one line in `Kernel::step_tick` that leaves the last tick
*pended* rather than stepped — the detail the whole invariance rests on —
produces this:

```
logical ticks         421          <- 20 laps, each one tick late
SCHEDULE DIGEST       94f229d7a5bd8f77    ok    (unchanged!)
      FAIL  the clock landed in the band the scenario's arithmetic fixes
```

The order really did hold, event for event. Check 2 is the one that bit, and
this run is why it is in the file.

## The finding: the simulator's gate does not transfer to silicon

`kairos power diff` compares whole projected **lines**, tick stamp included,
and reports 18/18 invariant. That is sound for the oracle traces, where time
is critical-section exits and a tick stamp is a count of work done. **It is
not sound on a Cortex-M3**, where a tick stamp also records how long the work
took — and the two arms do not take the same time, because one of them spends
the idle windows in `wfi` instead of servicing four hundred interrupts.

Editing this cell twice while building it moved the logical tick count each
time, in *both* arms, and moved a tick-stamped digest with it: the control
read 402, 401 and 400 across those runs and the tickless arm read 400, 400
and 401, with 0 or 1 wakeups. The order digest never moved.

> **A quantity the instrument's own cost can move is not a schedule.**

So a hardware tickless gate is *order* plus a *timing band*, not a line diff.
`kairos power diff` keeps its own guarantee over stored traces; this is a
second gate for a second kind of evidence, and the standing rule that a raw
trace byte-diff may never be quoted as proving the schedule held is unchanged
— it is now merely the weaker of two reasons.

## What the Xtensa sibling proved about this cell's design

Two of this cell's decisions looked like fussiness until `xiao-s3-tickless`
was written without them, and each cost a real defect there:

* **Sleeping to a tick BOUNDARY rather than for N whole periods.** The section
  below says, in as many words, that the alternative "would shift every later
  tick by a fraction of one, for ever". It was not carried across. The Xtensa
  cell restarted its period at the wake and ran **0.99 % slow — 38.7 seconds in
  an hour** — with a flawless logical tick count and a matching digest. It now
  drives its tick from an absolute grid, which is the same idea by other means.
* **Clearing the pending tick rather than letting the handler run.** On Xtensa
  the equivalent is impossible — `waiti 0` unmasks — so the suppression has to
  move into the handler, and getting that wrong stopped the scheduler dead.

> **Knowing a hazard clearly, in prose, in your own repository, is no defence
> against walking into it in the second implementation.** Only a gate is.

And the gate lesson that followed, which applies to this cell too: every check
here counts **logical** ticks and compares the two arms to each other. Both
arms produce 400 ticks however badly the timer is driven, and a digest that
matches says nothing about the clock. The Xtensa cell now bounds
wall-time-per-logical-tick against a free-running counter — the only check in
it that compares the kernel to something outside itself.

> **A gate that only compares the system to itself cannot catch the system's
> shared reference drifting.**

This cell has no equivalent check because QEMU's wall clock is not physical, so
there is nothing outside it to compare against. That is a limit of the
emulator, and it is stated here rather than left to be assumed.

## What this cell does NOT claim

* **Not an energy number.** See above. And note the control arm **busy-spins**
  — `idle_suppress_ticks` returns before ever reaching `Port::idle`, so this
  arm never executes a `wfi`. That is fine for the quantity being measured,
  because a `wfi`-ing idle task would be woken by every one of those 400 ticks
  just the same. It would be badly wrong for a *current* comparison: against a
  spinning baseline tickless flatters itself. The XIAO sibling
  (`xiao-s3-tickless`) halts its control arm for exactly that reason, and that
  is the cell to copy if you ever put a meter on this.
* **Not a policy.** The sleep is `expected_idle_time` less nothing at all —
  the fixed policy FreeRTOS ships. Choosing a margin, or fitting one, is
  later work and no differential covers it.
* **Not the ESP32-S3.** That is `xiao-s3-tickless`, which now exists and has
  run on a real XIAO: 400 alarm wakeups to 0 with the same digest in both
  arms. Its sleep is a different mechanism for a reason — `waiti 0` unmasks
  where `wfi` does not, so the suppression lives in the handler there and in
  `PENDSTCLR` here — and it is worth reading for the defect it found that
  this cell could not.
* **Not `configPRE_SUPPRESS_TICKS_AND_SLEEP_PROCESSING`.** The kernel has no
  `Hooks` seam, so the application veto is unwired; a port declines by
  returning zero.

## Re-pinning the digest

A kernel or scheduler change that legitimately moves this scenario's schedule
moves `PINNED_ORDER`, and both arms move together. Run both, check they agree
**with each other**, and write the new value into `src/main.rs`. If they do
not agree with each other there is nothing to pin: the change broke tickless,
which is what this cell is for.

## Where the mechanism lives

`rusty_rtos_port_cortex_m::suppress_ticks_and_sleep` — beside the rest of the
SysTick register map, because that crate is the one with the fenced-`unsafe`
licence. Two details there are load-bearing:

* **It sleeps to a tick BOUNDARY**, not for a whole number of ticks: the
  window is what is left of the current tick plus `want - 1` whole ones. That
  is what lets the counter restart at full reload with the tick phase
  unchanged; sleeping `want` whole ticks from here would shift every later
  tick by a fraction of one, for ever.
* **It clears the pending SysTick before returning** (`ICSR.PENDSTCLR`). The
  caller holds PRIMASK so the exception was never taken — but it is pending,
  and the moment the mask drops it would be delivered and the kernel would
  count a tick it has just been told about. That bit is the difference
  between suppressing ticks and deferring them.

It reads the tick geometry back from `start_tick` rather than being told it a
second time, so there is one place to get the cycles-per-tick wrong instead
of two.
