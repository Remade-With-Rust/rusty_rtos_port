# mps2-an385-qemu-kernel — the Kairos kernel scheduling real tasks

The sibling `mps2-an385-qemu-switch` cell proved the port's half: PendSV
saves and restores a task's context. It chose the next task with a round
robin, because a port's job is to *switch*, not to choose.

This cell hands the choosing to `Kernel::switch_context`. What runs here is
the same fixed-priority preemptive scheduler the conformance corpus proves
against C FreeRTOS, driving an ARMv7-M.

```sh
cargo run --release        # qemu-system-arm, no hardware
```

```text
ticks                 402
switches              81
high-priority laps    40   (expected about 40)
low-priority laps     38273
woke early            0
      ok    the run ended on the high task's own schedule, not on the deadline
      ok    the kernel chose a task at least once
      ok    the high-priority task ran
      ok    the low-priority task ran, so it was not starved out
      ok    the high-priority task never resumed before its delay expired
      ok    its lap count is ticks/delay, so `delay` blocked it for exactly as long as it asked

RESULT: PASS -- the Kairos scheduler drove real tasks on a Cortex-M3
QEMU exit code: 0
```

## The three joints

| joint | here |
|---|---|
| who runs next | `PendSV` → `pick_next` → `Kernel::switch_context` → `CURRENT_SP_SLOT` |
| when to switch | `SysTick` → `Kernel::increment_tick` → pend a PendSV if it says so |
| how a task blocks | `Kernel::delay` then a yield, and it resumes on the line after |

## Why the lap count is the check

Both tasks running proves nothing — a round robin does that. **40 laps
against 402 ticks and a 10-tick delay** is what only a priority scheduler
with a working `delay` produces:

- too *many* laps would mean `delay` never blocked the task;
- too *few* would mean it blocked and was never woken;
- `woke early == 0` is the task reading the tick on both sides of its own
  delay, so it cannot have come back before its time.

Meanwhile the low-priority task got 38,273 laps — every cycle the high one
was not using, and none that it was.

## Proof the check can fail

Deleting the one line that asks the kernel anything:

```diff
-        k.switch_context();
+        // POISONED
```

```text
high-priority laps    1   (expected about 120)
      FAIL  the run ended on the high task's own schedule, not on the deadline
      FAIL  its lap count is ticks/delay, ...
RESULT: FAIL -- 2 check(s) failed        QEMU exit code: 1
```

One lap: the task blocked once and was never resumed, because nothing ever
chose it.

## Two things it cost, both worth keeping

**Every task the kernel creates needs a stack — including the two it
creates for itself.** `start_scheduler` makes `IDLE` and `Tmr Svc`
unconditionally, and `Tmr Svc` sits at `TIMER_TASK_PRIORITY`, *above* both
application tasks. The first run locked up on `can't escalate 3 to
HardFault` with `PC=0`: the scheduler quite correctly chose the
highest-priority ready task, and that task had no stack. A missing stack is
not a soft failure.

**A cell that hangs is worse than one that fails.** The high-priority task
ends the run — but if the scheduling joint is broken it never runs again,
and nothing would ever end anything. `kairos check --qemu` would wait
forever. So the task that always runs owns a deadline, and an overrun is
reported as a failure. The poison above hung before that was added.

## What this is not

The timer daemon sleeps through the run rather than servicing a queue:
software timers are not what this cell claims, and a highest-priority task
that spun would starve everything below it.

No timing. QEMU supplies no cycle or work counter — measured six ways in
`rusty_rtos_core/firmware/mps2-an385-qemu-region`. The tick figure here is
a count of SysTick interrupts.
