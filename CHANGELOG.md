# Changelog

Security-relevant changes are called out under **Security** (hardening gate
H-38). Versions follow SemVer; in 0.x a minor bump may break the API.

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
