# mps2-an385-qemu-switch — the Cortex-M context switch

Two tasks with **real stacks of their own**, preempted by SysTick and
switched by PendSV, each checking that its own stack survived.

```sh
cargo run --release        # qemu-system-arm, no hardware
```

```text
switches observed   200
task A resumptions  100
task B resumptions  100
ticks               200
      ok    every word of both stacks survived every switch
      ok    the switcher ran to completion
      ok    task A ran
      ok    task B ran
      ok    neither task starved (round robin, within 3x)

RESULT: PASS -- the PendSV switch preserves two real task stacks
QEMU exit code: 0
```

## Why the witness array is the check

"Both counters went up" would pass on a switcher that swapped stack
pointers and lost every callee-saved register. So each task fills a
256-word array **on its own stack** with a pattern derived from its id and
re-checks every word after each switch it sees, plus a value the compiler
is forced to hold in a callee-saved register across the loop.

`r4-r11` are exactly the registers the hardware does *not* stack on
exception entry — the ones the port's assembly has to save, and the ones a
broken switch loses first.

## Proof the check can fail

Deleting one instruction from the handler, which is precisely "stop saving
the callee-saved registers":

```diff
-    "    stmdb r0!, {{r4-r11}}",
+    "    sub   r0, #32",
```

```text
      FAIL  every word of both stacks survived every switch
      corrupted witness word index 0
RESULT: FAIL -- 4 check(s) failed        QEMU exit code: 1
```

## The bug it cost to get here, worth writing down

The first version started nothing at all: `main` printed "starting the
first task..." and hung. The stacked PC in an exception frame has **bit 0
clear**, which is what the architecture wants — but `start_first_task`
enters the task with `bx`, and `bx` reads bit 0 as "switch to ARM state".
An M-profile core has no ARM state, so it took a UsageFault, which
presents as a task that simply never runs.

`orr r6, r6, #1` before the `bx`. The frame stays architecturally correct
for the hardware's own exception return; only the hand-rolled entry needs
the bit.

## What this is not

It is **not the Kairos scheduler**. A port's job is to switch when asked
and to preserve what it switched away from; *choosing* the next task is
the kernel's. This cell installs a round robin so the switch itself can be
judged alone. Wiring `Kernel::switch_context` into `set_scheduler` is the
next increment.

It claims **no timing**. The sibling cells measured six ways that QEMU
supplies no cycle or work counter; the tick number here is a count of
SysTick interrupts, not a measurement of anything.
