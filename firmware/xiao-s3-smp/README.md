# `xiao-s3-smp`: one Kairos kernel on both cores of an ESP32-S3

**Status: PASSES on a XIAO ESP32-S3 (2026-10-01).** This is K8 SMP slice S3
(`rusty_rtos_kernel/docs/plans/smp.md`). The kernel is slice S1, by path;
it is not in a release yet.

```
SMP parallel  one spin alone 301470 us, two spins on two cores 342056 us
SMP parallel  spinner cores (bit0=core0 bit1=core1): s0=0b10 s1=0b01
SMP pingpong  2000/2000 laps in 64562 us = 16140 ns per hand-off
SMP pingpong  ping on core0/core1 = 1656/344, pong on core0/core1 = 342/1658
SMP ipis taken 4009 (core0 2000 core1 2656), switches core0 4035 core1 4327, lock contended 12109 times, ticks 710
RESULT: PASS
```

- **Parallel:** two spinners at one priority, each on its own core. They
  finish in 1.13x the time of one spinner alone; one core would need 2x.
- **Cross-core:** a semaphore ping-pong whose two tasks run on different
  cores, so every hand-off is an inter-processor yield. All 2,000 laps
  complete and 4,009 IPIs are taken, about two per lap. Each task stays on
  "its" core about 83 % of the time; the rest is legitimate migration with
  no affinity set.

## How it is built

| piece | how |
|---|---|
| one kernel | `Kernel<.., NUMBER_OF_CORES = 2>` in a static. Every entry masks this core (`rsil 3`) AND takes a cross-core spinlock: `portGET_TASK_LOCK` and `portGET_ISR_LOCK` collapsed into one. The port's own critical sections are empty, because the lock is always already held. |
| the switch | `Software0`, a CPU-internal interrupt, so each core has its own. The handler switches THIS core (`core_id` = `PRID`). The context save and restore happen INSIDE the lock: once it is released, the other core may select the outgoing task, and its registers must already be saved. |
| `portYIELD_CORE` | the kernel's `take_core_yields()` is drained as the lock is released. `FROM_CPU_INTR<n>` is raised for each core named, and its handler, bound on core `n`, raises that core's own `Software0`. |
| core 1 | `CpuControl::start_app_core`. Core 1 binds its own `FROM_CPU_INTR1`, enables its switch, and yields out of its boot context. |
| the tick | `SYSTIMER` alarm 0, on core 0 only. Per the SMP tick, it may switch core 0 and IPI core 1. |

## What it does not cover

- Tasks that migrate between cores restore an integer-only context. Nothing
  here uses the FPU, and per-core `CPENABLE` is not handled.
- **16.1 µs per cross-core hand-off is the first number, not a tuned one.**
  The one-core ISR-wake-to-task is 430 cycles. The 12,109 lock contentions
  are the obvious first suspect, and no cycle decomposition has been done.
- SMP slice S1b (delete, suspend and priority-set of a task running on the
  OTHER core) is not in the kernel yet. This workload avoids those calls.
