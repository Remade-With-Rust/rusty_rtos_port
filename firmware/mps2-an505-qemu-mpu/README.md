# `mps2-an505-qemu-mpu`: an MPU refusal on Cortex-M33

**PASS, 2026-10-01, QEMU `mps2-an505`.** The K8 kill test, "the
privileged / unprivileged demo on M33 refuses a bad access": two Kairos
tasks run **unprivileged**, each confined by the ARMv8-M MPU to its own
stack and data. One of them tries four accesses it has no right to, and all
four are refused.

```
MPU regions implemented: 8
switches              173   into an unprivileged task: 102
kernel calls by svc   102
good                  laps=50 (want 50) done=1
rogue                 laps=25 (want 25) done=1
canary                0xc0ffee11 (want 0xc0ffee11)
MPU enabled at end    true
refusals              4
  0 MemManage at 0x38001de0  by rogue     <- good's lap counter
  1 MemManage at 0x38000000  by rogue     <- privileged data (the canary)
  2 MemManage at 0x38000008  by rogue     <- the kernel
  3 BusFault  at 0xe000ed94  by rogue     <- MPU_CTRL = 0
RESULT: PASS -- four bad accesses from an unprivileged task, four refusals;
        the kernel, the canary and the other task untouched.
```

## The privilege boundary

| | privileged | unprivileged |
|---|---|---|
| who | the kernel, `PendSV`, `SysTick`, the fault handlers, `IDLE`, `Tmr Svc`, the judge | `good` and `rogue` |
| memory | everything (`MPU_CTRL.PRIVDEFENA`) | code read-only (region 0); its OWN stack and data (region 1) |
| the kernel | called directly, interrupts masked | `svc #0` only: two calls, `delay` and `tick_count` |

The scheduler hook that `PendSV` calls ([`pick_next`](src/main.rs)) sets
region 1 and `CONTROL.nPRIV` for whichever task the kernel chose, before the
exception returns into it. This is the part of FreeRTOS's
`portRESTORE_CONTEXT` for its MPU ports that this cell claims. The kernel,
the port and `PendSV` are unchanged: the MPU is layered above them.

A refusal is logged and the faulting instruction is stepped over, so one
task can show four refusals and the system can be seen to keep scheduling.
Only a fault raised by an **unprivileged** thread is stepped over; a fault
in privileged code stops the cell as a defect.

## Two poisons, both FAIL

| run | what changes | result |
|---|---|---|
| `cargo run --release` | -- | **PASS**, 9/9 checks |
| `--features poison` | the MPU is left OFF | **FAIL**, 4 checks: `good` reads 57005 (`0xDEAD`), canary `0x0bad`, 1 refusal of 4, MPU off |
| `--features poison-region` | the MPU is on, but each task's region spans all of RAM | **FAIL**, 3 checks: the same two corruptions, 1 refusal of 4 |

The second poison is the one that matters. It shows the PASS comes from the
**per-task region** being sized correctly, not merely from the MPU being
enabled.

**One refusal survives both poisons, and that is correct.** The write to
`MPU_CTRL` is refused by the System Control Space's own privilege check (a
BusFault), which holds with or without an MPU. That is why the cell counts
it separately rather than letting it carry the verdict.

## NOT claimed

- **Silicon.** QEMU models PMSAv8 (8 regions, Secure instance), but this is
  an emulator. No M33 board has run it.
- **The `rusty_rtos_mpu` package.** Two kernel calls cross the boundary, not
  FreeRTOS's ~80 MPU wrappers. There are no access-control lists, no
  `PSPLIM` stack guard, and no TrustZone split: everything runs Secure.
- **Kernel-object protection.** An unprivileged task reaches the kernel only
  through `svc`, but the shim does not check *which* objects it may name.

## Run

```sh
cargo run --release                          # PASS, exit 0
cargo run --release --features poison        # FAIL, exit 1
cargo run --release --features poison-region # FAIL, exit 1
```

It needs `qemu-system-arm` with the `mps2-an505` machine and the
`thumbv8m.main-none-eabi` target (soft-float, so no FP context is in play).
Like the other QEMU cells, it builds the kernel and core by **path**, so it
runs from the umbrella checkout rather than a standalone clone.
