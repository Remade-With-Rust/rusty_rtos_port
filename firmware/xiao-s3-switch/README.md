# `xiao-s3-switch` — the Xtensa context switch, on silicon

```
=== the Kairos Xtensa context switch, on ESP32-S3 SILICON ===
design  the switch runs in a software interrupt, so the register
        windows are spilled by the exception entry, not by hand

SWITCH laps_a=100 laps_b=99 want=100
SWITCH faults=0 first_bad_word=none

RESULT: PASS -- 100 and 99 resumptions, every witness
        intact, yielded from three frames deep every time.
```

The Cortex-M twin (`mps2-an385-qemu-switch`) proved an ARM switch under
QEMU. This proves an **Xtensa LX7** switch on a real ESP32-S3.

## Why it yields from three frames deep

An ARM switch saves eight callee-saved registers. Xtensa keeps a 64-entry
physical register file behind a rotating 16-register window, and a task
several calls deep has several live windows in that file. If they are not
spilled, the hardware later writes them onto whichever stack is current —
the *incoming* task's — and the corruption surfaces far from its cause.

So the yield happens in `level_three`, three calls below the task body, with
each level holding a witness it re-checks on the way back. A test that only
ever yielded from the top frame would never see this class of bug.

That is not hypothetical. **The first design of this port switched from task
context, spilling the windows by hand** with `xtensa-lx-rt`'s
`SPILL_REGISTERS` sequence. It started a task, ran it, printed from it — and
then hung the moment the task nested calls deeply enough to need the register
file back. A shallow probe passed it.

## The design that works

The switch runs **inside a software interrupt**. By the time the handler
runs, `xtensa-lx-rt`'s interrupt entry has already spilled every window and
written the whole machine — `A0`–`A15`, `PC`, `PS`, `SAR`, loop and MAC
registers — into a `Context`. So the switch is two struct copies: save the
trap frame into the outgoing task's slot, copy the incoming task's slot over
the trap frame, return. The exception exit restores it, windows and all.

A yield therefore *raises* an interrupt rather than calling a switcher. This
is what FreeRTOS's Xtensa port and Espressif's own `esp-rtos` do, and this is
why.

## Poison-proving

**Removing the outgoing-context save** — the analogue of deleting
`stmdb r0!, {r4-r11}` from the ARM port — makes the cell **hang without ever
reaching a verdict**. It cannot return to `main`, because `main`'s state was
never written down. The gate fails, which is what a gate must be able to do.

A second poison is recorded because it did **not** fire: clobbering `A12` in
the resumed context changed nothing, and the cell still passed. The witnesses
the compiler generated are stack-resident, so this cell proves **stack and
frame integrity across a switch**, not that every individual register is
preserved. The 100 laps of nested calls are strong evidence the restore is
complete, but that is evidence, not the direct assertion, and the difference
is worth stating.

## What this does NOT claim

Round-robin between two tasks driven by an explicit yield. No tick
preemption, no priority scheduler — `Kernel::switch_context` is not in this
loop. This cell answers "can we leave a task and come back to it intact on
this ISA", which is what the `esp-radio-rtos-driver` joint blocks on.

It is also **not** what the conformance corpus needs: `xiao-s3-corpus` runs
all 18 scenarios on this same chip byte-identically to C FreeRTOS with no
port at all, because a Kairos task owns no stack. The two results are
complementary, and confusing them is the mistake the K5 plan row originally
made.

## Running it

Needs the `esp` toolchain and a board on a serial port, so it is never
started by a gate.

```sh
cargo +esp run --release
```
