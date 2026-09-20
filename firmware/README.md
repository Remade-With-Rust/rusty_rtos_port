# firmware/

Per-chip example projects for `rusty_rtos_port`. Each directory here is a **separate
cargo project**, excluded from the workspace, because every chip needs its own
target triple, linker script and (for Xtensa parts) its own toolchain. n0's
iroh-on-ESP32 work and the Janus family both reached the same conclusion: keep
the firmware projects out of the library workspace so architecture-specific
patches never leak into it.

Naming: `<board>-<demo>/`, for example `lm3s6965-qemu-flash/` or
`esp32c6-devkitc-blink/`.

One cell here has no board: `host-kernel/` runs on the machine you are reading
this on, through `rusty_rtos_port-host`. It is kept beside the others because
it is the same kind of thing — one project, one port, one claim — and because
it is the M3 scheduling cell's experiment moved off the chip, so the two are
meant to be read together.

| Chip class | Runtime | Target |
|---|---|---|
| Cortex-M3 (QEMU `lm3s6965evb`) | `cortex-m-rt` + `rusty_rtos_port-cortex-m` | `thumbv7m-none-eabi` |
| Cortex-M4F / M7 | same | `thumbv7em-none-eabihf` |
| Cortex-M33 | same | `thumbv8m.main-none-eabihf` |
| RISC-V RV32 (QEMU `virt`) | `riscv-rt` + `rusty_rtos_port-riscv` | `riscv32imac-unknown-none-elf` |
| ESP32-C6 / P4 | `esp-hal` + `rusty_rtos_port-riscv` | `riscv32imac-unknown-none-elf` / `riscv32imafc-unknown-none-elf` |
| ESP32 / ESP32-S3 | `esp-hal` (esp toolchain) + `rusty_rtos_port-xtensa` | `xtensa-esp32-none-elf` / `xtensa-esp32s3-none-elf` |

## The cells, and the one claim each makes

A cell proves exactly one thing and says what it does **not** claim. Each
gates itself and exits non-zero on failure, so `cargo run --release` is the
whole kill test.

| cell | claim |
|---|---|
| `mps2-an385-qemu-switch` | `PendSV` saves and restores a context correctly |
| `mps2-an385-qemu-kernel` | the Kernel chooses, driven by a tick |
| `mps2-an385-qemu-preempt` | a blocking call made in the window a `give` opens parks the right task — 200/200, window closed |
| `mps2-an385-qemu-tickless` | **401 SysTick interrupts -> 0, schedule unmoved** |
| `riscv32-qemu-switch` · `riscv32-qemu-preempt` | the same two on RV32; 201 switches, zero faults |
| `xiao-s3-switch` | an Xtensa LX7 switch **on silicon**, 64 witness words a task |
| `xiao-s3-radio` | the joint `esp-radio-rtos-driver` blocks on |
| `xiao-s3-tickless` | **the Kernel on a tick on Xtensa**, and **400 alarm interrupts -> 0** on a real XIAO |
| `host-kernel` | the scheduling cell moved off the chip, on OS threads |

The two tickless cells are worth reading together: they reach the same result
by **opposite mechanisms**, because `wfi` leaves the interrupt mask alone and
`waiti 0` does not. Each README says why its own shape is forced.

Rules:

- Depend on this repo's crates by **path** (`../../crates/rusty_rtos_port`) inside a
  firmware example; depend on siblings by git URL as usual.
- Release profile for a chip: `opt-level = "s"` (or `"z"`), `lto = true`,
  `codegen-units = 1`, `panic = "abort"`, `overflow-checks = true`.
- A firmware example is not a test. The library's tests run on the host and
  on the sim port.
