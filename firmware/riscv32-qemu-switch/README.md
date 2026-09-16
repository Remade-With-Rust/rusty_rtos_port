# `riscv32-qemu-switch` — the RISC-V context switch, and an answer about counters

```
SWITCH laps_a=100 laps_b=99 want=100
SWITCH faults=0 first_bad_word=none
COUNTERS switches=200 mcycle_total=199902 minstret_total=199102
COUNTERS per_round_trip cycles=999 instructions=995  (NOT per switch)
RESULT: PASS -- 100 and 99 resumptions on RV32, every callee-saved
        register intact across each one.
```

The third port to get this cell, after Cortex-M (`PendSV`) and Xtensa
(windowed ABI, on silicon).

## What makes RISC-V different — and it is not difficulty

There are no register windows to spill, so the thing that made Xtensa the
"long pole" simply is not here. What *is* different is that **a RISC-V trap
saves nothing**:

| | who saves the callee-saved registers |
|---|---|
| Cortex-M | hardware stacks `r0-r3, r12, lr, pc, xPSR`; the port adds `r4-r11` |
| Xtensa | `xtensa-lx-rt`'s exception entry spills the whole window file |
| **RISC-V** | **nobody — the port writes down all fourteen itself** |

So the witness here is aimed squarely at `ra`, `sp` and `s0`–`s11`, and the
switch happens three call frames deep so those registers are genuinely live.

### The calling-convention trap

A fresh task resumes through a **trampoline**, not straight into its entry
point. The switch restores only the callee-saved set; a `extern "C"` function
reads its arguments from `a0`/`a1`, which are caller-saved and therefore not
in that set. The first version put the arguments in `s0`/`s1` and jumped
straight to the wrapper — it hung on the very first switch. Three `mv`
instructions fix it.

## The counter question, answered

The mission plan says cycle rows come from silicon because **QEMU can supply
no cycle counter** — measured six ways on the Cortex-M cell, where DWT is
unimplemented. **That reasoning does not carry to RV32.** `mcycle` and
`minstret` are architectural CSRs, not optional debug hardware, and QEMU
implements them.

But implementing them is not the same as making them useful:

| | three consecutive runs of this cell |
|---|---|
| plain | `1,932,185` / `2,107,644` / `2,320,843` — a **20% spread** |
| **`-icount shift=0`** | `199,902` / `199,902` / `199,902` — **identical to the instruction** |

Without `-icount`, QEMU is tracking host time and the counter is a number
rather than a measurement. With it, the count is **exactly reproducible**, so
the runner sets it and this cell is deterministic by construction.

**What that buys, stated narrowly.** An emulator's cycle count is still not a
chip's, and no absolute timing claim should come from here. A deterministic
retired-instruction count is a **work counter**: it compares two builds of
the same workload, which is what "did this change cost anything?" actually
asks — and this family prefers counters to clocks anyway.

### The number is a round trip, not a switch

`per_round_trip instructions=995`. The bracket runs from before
`switch_context` to after it returns, and it returns only when this task is
switched back in — so it spans everything the *other* task did in between.

The first version called it "per switch" and reported ~10,000 instructions
for what is fourteen loads and fourteen stores. A number 250× too large is
the instrument asking for help, not a slow switch. Measuring the bare swap
means reading the counter inside the assembly, which is a separate job.

## Poison-proving

Deleting the twelve `s0`–`s11` stores — the analogue of removing
`stmdb r0!, {r4-r11}` from the ARM port — makes the cell **hang**, never
reaching a verdict. Corrupting the callee-saved set destroys control flow, so
no code survives to report a fault. A hang is a failure, and the gate treats
it as one; it is simply not a diagnosis.

## What this does NOT claim

Cooperative switching between two tasks. No preemption, no tick, and
`Kernel::switch_context` is not in this loop — that is the next cell, as it
was for the other two ports. This answers "can we leave a task and come back
to it intact on RV32", and it answers the counter question.

It also says nothing about the ESP32-C6. The port is written against QEMU
`virt`; the C6 reaches the same software interrupt through its own
peripheral, and `CLINT_MSIP` is a constant for exactly that reason.

## Running it

```sh
cargo run --release        # needs qemu-system-riscv32 on PATH
```
