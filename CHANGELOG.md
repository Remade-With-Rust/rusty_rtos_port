# Changelog

Security-relevant changes are called out under **Security** (hardening gate
H-38). Versions follow SemVer; in 0.x a minor bump may break the API.

## Unreleased

### Fixed
- `rusty_rtos_port-host`: a reused task slot no longer inherits its last
  occupant's run permit or freeze. The tick can freeze a thread between its
  grant and its taking it; if that task is then deleted, the slot kept both
  flags. A standing grant let the new thread run uninvited while the kernel
  had another task current (Linux: "this thread is task 77, the kernel
  believes 43", `death.c`); a standing freeze made the new occupant's first
  grant a thaw of nothing, so the kernel's current task ran nowhere until the
  next tick (`TimerDemo`'s exact-tick checks). Clearing one alone produced the
  other's failure. The C ABI cell on Linux pthreads now passes 25/25 at
  30,000 ticks with the case met and cleared in every run; it had failed or
  broken identity in every run. `stale_grants()` counts the occurrences.

### Added
- `rusty_rtos_port-host`: `set_core` / `core`, the core the run permit
  stands for, reported as `Port::core_id`. A two-core cell passes the one
  permit between two kernel cores (virtual cores: every two-core kernel path,
  never two threads in the kernel at once).
- `rusty_rtos_port-riscv`: a `small` feature, the flash profile, matching
  `rusty_rtos_kernel-core`'s. Without it `RiscvPort::enter_critical` and
  `exit_critical` are `#[inline]`. As opaque calls they cost more than the
  call: every queue and TCB field read before one was re-read and re-checked
  after it. On the shipped port (`bench/tick-work --features real-port`)
  thirteen rows fall by 284 instructions (`peek_ok` 73 -> 41,
  `queue_roundtrip` 152 -> 120, `block_cycle` 947 -> 855); `bench/kernel-flash`
  +1,934 B on the speed profile and unchanged under `small` (17,666 B).
  Enable `small` with the kernel's to keep the old shape.

## 0.3.1 — 2026-10-02

A patch release of the whole family (`-core`, `-cortex-m`, `-riscv`,
`-xtensa`, `-host`, the facade and `-esp-radio`).

### Fixed
- `rusty_rtos_port-riscv`: `switch_context` carries a reasoned
  `allow(clippy::unwrap_or_default)` -- the lint's suggestion needs
  `Default` for raw pointers, which is Rust 1.88, and the MSRV is 1.85. No
  code change; found by linting the radio crate's RISC-V arm.

### Security
- **Kani proofs of the three stack builders** (`*/src/proofs.rs`, built
  only under `cfg(kani)`): `init_stack` (Cortex-M) and `new_task_context`
  (RISC-V, Xtensa) write only inside their documented window for every
  top that meets their contract (RISC-V: for ANY top) -- 519 checks, 0
  failures, poison-proven. Hardening gate H-30, partly; the switches stay
  residual risk R-4.
- **Property tests** (H-28): the sim port against a model of its contract,
  and the three stack builders over 20,000 seeded inputs each.

## `rusty_rtos_port-esp-radio` 0.3.0 — 2026-10-01 (first publish)

The rest of the family is unchanged at 0.3.0; this is the radio glue's first
release, at the family version.

### Security
- The crate inherits the workspace lints. Every `unsafe` is fenced with an
  `#[expect(unsafe_code, reason)]` on its owning item and written up in
  `UNSAFE.md`; the "Unfenced: 105" declaration is gone (threat model R-6,
  closed).
- `WaitQueue`: the waiter count was a bare read-modify-write a tick could
  preempt, losing a registration and leaving a waiter that `notify` never
  released. It is now updated under the interrupt mask.
- Arithmetic is `checked_*` or saturating (`max_task_priority` no longer
  underflows on a host with fewer than two priorities); the ring index math
  states its bound in an `#[expect]`.

### Changed
- Depends on `rusty_rtos_port-xtensa` / `-riscv` 0.3.0.

## 0.3.0 — 2026-10-01 (breaking)

### Security
- **BREAKING:** `rusty_rtos_port_cortex_m::init_stack` is an `unsafe fn`. It
  was a safe `fn` that writes sixteen words below a raw pointer, which let
  safe code corrupt memory: unsound. Callers wrap it in `unsafe` and uphold
  the documented contract.
- `rusty_rtos_port-esp-radio`: a zero-capacity queue overflowed its heap
  buffer on the first push; fixed. Every create that failed handed the radio
  driver a dangling pointer it then used; it now halts with a message naming
  what ran out.
- `rusty_rtos_port-host` (Unix): the `SIGUSR1` handler did not restore
  `errno`, so a frozen task could read `EINTR` from a syscall that set
  nothing of the kind (found by ThreadSanitizer); fixed.
- `UNSAFE.md` now covers every `unsafe` in the family, including the
  previously undocumented Xtensa port, and `tools/unsafe_census.py` fails CI
  when it does not. `rusty_rtos_port-esp-radio` is declared as not fenced
  item by item, with its count pinned.
- `cargo vet` coverage (`supply-chain/`): 25 certified, 27 embedded-ecosystem
  crates exempted pending the owner's decision.
- `fuzz/task_stacks`: every port's stack builder against its documented
  write window.
- A threat model (`docs/threat-model.md`) with a residual-risk register.
- CI: every action pinned to a commit SHA, `permissions: contents: read`,
  `cargo vet --locked`, the unsafe census, the hardening-table check and a
  fuzz regression per push; fuzzing, AddressSanitizer, ThreadSanitizer (the
  host port) and `cargo careful` nightly.

### Fixed
- CI was red at 0.2.1 (clippy on host-only dead code and a test file,
  `cargo fmt`); green.
