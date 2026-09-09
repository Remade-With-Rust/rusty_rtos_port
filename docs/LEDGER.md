# rusty_rtos_port — the ledger

Every number this package claims, with the run that produced it. A row
without a method is not a number. Counters before clocks; an external oracle
before a self-metric; the method line names the machine, the pinning, the arm
order and the null-arm floor for anything timed.

## The sim port against the C Posix port (2026-09-09, K1)

| gate | result | method |
|---|---|---|
| sim contract v1 implemented identically on both sides | yes | `kairos conform dynamic --ticks 100000` from the umbrella: 1,219,231 trace lines identical, and the counters with them. A port that delivered a tick one critical-section exit early or late would move every line after it, so the trace is the proof |
| `ulKairosExits` at 100,000 ticks | 1,066,689 on both sides | the harness's `KAIROS_RESULT` line; on the C side from the patched `port.c`, here from `SimPort` |
| `ulKairosYields` at 100,000 ticks | 179,588 on both sides | as above |
| `unsafe` blocks in this package | 0 | `UNSAFE.md`; `forbid(unsafe_code)` in both crates. The silicon ports, which will have some, are K3 |

## The build fact (2026-09-09)

| gate | result | method |
|---|---|---|
| `cargo test --workspace` | 6 tests pass | the sim port's own: exits are not counted before the scheduler runs, every sixteenth outermost exit raises a tick, nested sections count once, the tick entry does not recurse into the counter, a raised tick is taken once, yields are counted and taken once |
| `cargo check -p rusty_rtos_port-core --no-default-features` and `--features alloc` on `thumbv7em-none-eabihf`, `thumbv8m.main-none-eabihf`, `riscv32imac-unknown-none-elf`, `riscv32imafc-unknown-none-elf` | all 8 rungs pass | `kairos check rusty_rtos_port --fmt --clippy --test --deny`, exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean | same run |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok | same run |
| Miri | green | `cargo +nightly miri test --lib`, miri 0.1.0 of 2026-09-08 |

No speed number, no size number, no chip. A port's numbers are cycle counts
at a context switch, and this package has no context switch yet — that is
K3, with the first silicon.
