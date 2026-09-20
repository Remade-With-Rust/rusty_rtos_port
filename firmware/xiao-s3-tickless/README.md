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

## ⚠ This cell has never executed

It compiles and links for `xtensa-esp32s3-none-elf`, both arms. **It has not
been flashed.** The machine it was written on has no XIAO attached and no
Espressif QEMU — stock `qemu-system-xtensa` offers `kc705`, `lx60`, `lx200`,
`ml605`, `sim` and `virt`, and no `esp32s3`.

So every number this README describes is a number *the board must produce*.
None is quoted from a run. Its ARM sibling is the arm that has actually run,
and its measured findings are what this is modelled on.

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

Each arm gates itself on laps, stalls, the tick band and its own wakeup
claim. The cross-arm claim is that the two `SCHEDULE DIGEST` lines are equal.
Unlike the ARM cell the digest is **not pinned**, because nobody has run this
to learn what it is; pin it once the board has said.

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
