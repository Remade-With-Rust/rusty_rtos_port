# `riscv32-qemu-preempt` — the RISC-V port preempts, and what that costs

```
PREEMPT switches=201 want=200
PREEMPT work_a=224541 work_b=224515
PREEMPT faults=0

      ok    BOTH tasks ran -- neither yields, so a switch DID happen
      ok    every witness survived every preemptive switch
      ok    preemption CONTINUED -- the timer kept arriving after the first switch

RESULT: PASS -- 201 preemptive switches between two tasks that
        never yield, 224541 and 224515 laps of work, zero faults.
```

```sh
cargo run --release        # needs qemu-system-riscv32 on PATH
```

Two tasks that never yield, sharing the hart almost exactly evenly, with every
callee-saved register and every stack witness intact across 201 switches.

## What it found, before it passed

This cell was built to turn `bench/switch-cost`'s preemptive **lower bound**
into a measurement. It could not, because the path did not work: the first
preemptive switch succeeded and was also the last.

`switch_context` resumes a task by restoring `ra` and **returning** (`ret`).
That is exactly right at a call site — it is what makes a cooperative yield 30
instructions, and what `riscv32-qemu-switch` proves with 100/99 resumptions
three frames deep. Inside a trap it is wrong: a trap is left with `mret`, and a
task entered any other way keeps running in trap context.

## The fix

A second switch, kept deliberately separate from the first:

| | resumes by | swaps |
|---|---|---|
| `switch_context` | `ret` | `ra`, `sp`, `s0`–`s11` — **30 instructions** |
| `switch_context_trap` | `ret`, then the task's own `mret` | the same fourteen **plus `mepc` and `mstatus`** — **37** |

and `new_task_context_preemptive`, which builds a fresh task so its first
switch leaves through `mret`: `mepc` is the entry point, `mstatus` carries
`MPIE`, and the trampoline it returns to does nothing but move the two
arguments into place and `mret`.

Keeping the two switches apart means a **yield still pays only for what a
yield needs**. The cooperative row is unchanged and still measured at 30.

`main` does not switch into the first task directly either. It raises the
software interrupt and waits, so the first switch is taken in the trap like
every other — which also means `main`'s own context is saved by the same code
that saves a task's, and can be resumed by it at the end.

## Poison-proving, which refuted the first explanation

Three lines were removed one at a time and the cell re-run:

| removed | result |
|---|---|
| the `mepc` restore | **hangs** |
| `mret` in the trampoline (→ `ret`) | **hangs** |
| the `mstatus` restore | **still passes** |

So `mepc` and the `mret` are what make preemption work, and the first
write-up of this — which said `mstatus` was "the field whose absence made
preemption impossible" — **was wrong**. The trap entry has already copied the
live `MIE` into `MPIE`, so `mret` re-enables interrupts without our help.

`mstatus` is still saved and restored, because a task's interrupt state is its
own and a task preempted inside a critical section must come back with that
section in force. But this cell does not demonstrate that, and the port's
comment now says so rather than claiming a proof it does not have.

## The measurement the cell was built for

```
    Kairos kairos_riscv_switch_trap      37
    + riscv-rt default_start_trap        37
    = a preemptive switch                74   vs FreeRTOS 83   1.12x
```

Straight line, no conditional branches, so the static count is the retired
count — the same discipline `bench/switch-cost` applies to every other row,
and it pins all of these.

**Both rows are true and they say different things.** A *yield* is 2.77× cheaper
on our side, because FreeRTOS routes yields through the interrupt trap
(`portYIELD()` is `ecall`) and we do not. A *preemptive switch* is near parity,
because there both kernels must write down what an interrupt could have
clobbered. Quoting only the first would be quoting the easy half.

## How preemption is proven rather than assumed

Neither task yields. Each sits in a loop incrementing its own counter and
re-checking a witness in a callee-saved register and on its own stack. Nothing
in either task's code can hand control to the other, so **both counters
advancing is the only thing that can prove a switch happened** — and it is a
check, not a print.

That matters because the sibling ARM preempt cell shipped three times without
testing anything: a producer that woke a *lower*-priority task, a counter that
counted attempts rather than successes, and a pre-loaded token that meant the
blocking case never ran.

The verdict can also be reported **from inside a task**, not only from `main`.
Once a task is running, `main` is reachable only through the very handler under
test — so a cell that reported only from `main` would hang instead of
diagnosing, and a hang is not a diagnosis. That is how the original defect was
found: the first run simply hung, and an in-task counter dump turned it into
`switched=1`.

## What this cell does NOT claim

No kernel. `Kernel::switch_context` is not in this loop — the handler picks the
other task directly. This is the **port's** preemptive path, which is what the
instruction count is about.

Nothing about the ESP32-C6, which reaches the same software interrupt through
its own peripheral.
