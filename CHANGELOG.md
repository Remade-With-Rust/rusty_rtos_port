# Changelog

Security-relevant changes are called out under **Security** (hardening gate
H-38). Versions follow SemVer; in 0.x a minor bump may break the API.

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
