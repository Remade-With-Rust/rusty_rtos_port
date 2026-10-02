# rusty_rtos_port — hardening audit

**Standard**: Kairos (Remade With Rust) recursive hardening process — see the skill's `STANDARD.md`
**Registry**: 41 gates / 12 phases (`use-protection-please` v1)
**Unit**: `rusty_rtos_port` — package (facade + `no_std` core)
**Tier**: critical-path — an RTOS component: the kernel is the trusted computing base of every firmware above it, and every library here parses bytes from a wire, a store or a bus
**Mirrors**: none — the crate README is the only face until the first public flip; the crates.io page becomes a mirror then
**Compliance**: none — no compliance framework in scope for an embedded kernel component; revisit at 1.0.0
**Architect**: [Tim Almond](https://github.com/Ttimmahlax) — accountable for this unit's security design; rendered
at the foot of the block in every README and mirror
**Audit depth**: deep (tools run: cargo vet, cargo fuzz, ASan, TSan where it applies, cargo careful, clippy `-D warnings`, the unsafe census)
**Audited**: 2026-10-01 by the v1.0-readiness pass · **Next review**: at every release, and no later than 2027-01-01

> Source of truth for this unit's hardening status. The README's status table is
> **generated from this file** — edit here, then run:
> `kairos harden --plan docs/plans/use-protection-please.md --readme README.md`
> (the Rust renderer in the umbrella's `tools/kairos`; `kairos check --harden`
> refuses a stale table).

**Status tokens**: `Completed` (evidenced pass) · `Scheduled` (owner + date in Target) ·
`Incomplete` (not done, or not evidenced) · `N/A` (out of tier — reason required in
Evidence; excluded from the totals).

---

## Threat sketch

*Assets* — scheduling integrity (the right task runs, priorities and timeouts hold as the C kernel's do); memory safety of every firmware above the kernel; availability (no panic reachable from an API call or a parsed byte); the integrity of the published crates.
*Adversaries* — a buggy or hostile task in the same firmware (a wrong handle, a stale handle, an ISR variant called from a task); crafted bytes on a wire, a store or a bus reaching a parser; a supply-chain actor substituting a dependency; a debugger-less field device that cannot report a fault.
*Highest-value attack path* — a parser or an API path that panics on untrusted input, taking the whole firmware down (the C kernel's `configASSERT` class), or a handle that outlives its object.
*Full model* — `docs/threat-model.md` (to be written with the first milestone; the family model is the mission plan's §2.10)

---

## Checklist

`★` = v1.0.0-blocking. Full probe and pass criteria per gate: the skill's `CHECKLIST.md`.

### Phase 0 — Threat modeling

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-01 | ★ Threat model documented and linked from README | Completed | `docs/threat-model.md` (model v1, 2026-10-01): assets, adversaries, six attack paths each with the test, fuzz target, QEMU/S3 cell or CI job that evidences it; linked from the README's `## Security` | |
| H-02 | Threat model revisited after last major change | Completed | model v1 is dated 2026-10-01, after the last major change (this pass: `init_stack` made `unsafe`, the esp-radio and host fixes) | |

### Phase 1 — Toolchain

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-03 | Toolchain pinned (`rust-toolchain.toml`) | Completed | `rust-toolchain.toml`: channel 1.98.0, clippy + rustfmt, the four bare-metal targets | |
| H-04 | Committed `.cargo/config.toml` hardening defaults | N/A | a library: a dependency's `.cargo/config.toml` never applies to the consumer's build, and this repo's is the gitignored sibling-patch seam. Frame pointers and linker hardening belong to each firmware's own config | |
| H-05 | ★ Release profile hardened (overflow-checks, LTO, panic policy) | Completed | `Cargo.toml` `[profile.release]`: `overflow-checks = true`, `lto = "thin"`, `codegen-units = 1`; libraries stay unwind-safe, firmware binaries choose `panic = "abort"` | |
| H-06 | Security toolchain available to CI and developers | Completed | CI installs the tool set pinned by version through a SHA-pinned `taiki-e/install-action`: `cargo-deny@0.19.9`, `cargo-vet@0.10.2`, `cargo-fuzz@0.13.2`, `cargo-careful@0.4.10` (`.github/workflows/ci.yml`, `scheduled.yml`); the same versions on the developer box | |

### Phase 2 — Supply chain

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-07 | ★ `Cargo.lock` committed | Completed | `Cargo.lock` tracked in the first commit (`git ls-files Cargo.lock`) | |
| H-08 | ★ `deny.toml` policy present and enforced | Completed | `cargo deny check` 2026-09-09: advisories ok, bans ok, licenses ok, sources ok | |
| H-09 | ★ Vulnerability scan clean (`cargo audit`) | Completed | `cargo audit` 2026-09-09: 0 advisories | |
| H-10 | ★ `cargo vet` coverage complete | Incomplete | `supply-chain/`: 25 fully audited (house crates by publisher; windows-*, libc, cfg-if, syn, quote, proc-macro2, unicode-ident by the publishers the imported audit sets already trust); `cargo vet --locked` in CI. 27 embedded-ecosystem crates exempted -- trusting their publishers is the owner's attestation (threat model R-1) | |
| H-11 | Unsafe inventory measured and trending down (geiger) | Completed | `cargo geiger` 2026-09-16: **0/0** functions, expressions, impls, traits and methods across the whole dependency tree, reported `:)` — no `unsafe` usage found, `#![forbid(unsafe_code)]` declared. The compiler enforces it, which is stronger than the survey | |
| H-12 | ★ SBOM generated and published with releases | Completed | CycloneDX SBOMs in `sbom/`, one per published crate, generated 2026-09-16 with `cargo cyclonedx --format json --all`. Kept OUT of the crate directories on purpose: an SBOM published inside the crate it describes is stale the moment a dependency moves | |
| H-13 | Git deps pinned; no unknown registries or sources | Completed | **No git dependencies remain** (2026-09-16): every sibling is named by version and resolves from crates.io, which is what `cargo publish` requires and what the mission plan's §2.11 "released pins only" means. `deny.toml` `[sources]` denies unknown registries and unknown git, and its `allow-git` list is now EMPTY — an allowance nothing uses is a warning on every run | |
| H-14 | Dependency freshness reviewed, human-in-the-loop updates | Completed | `.github/dependabot.yml` (2026-09-16): weekly, PR-only, `open-pull-requests-limit: 5`, with `rusty_rtos*` ignored because a sibling's version is decided by a release rather than a bot. Several house pins carry their reason in the manifest beside them, so the bot reports and a human decides | |

### Phase 3 — Code level

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-15 | ★ Workspace lint policy set and clean | Completed | `[workspace.lints]` as the family's; `cargo clippy --workspace --all-targets -- -D warnings` clean | |
| H-16 | ★ `unsafe` isolated, SAFETY-commented, inventoried | Completed | `forbid(unsafe_code)`; `UNSAFE.md` lists none. This is the package that will eventually hold the family's only `unsafe` — at the context switch and the vector table — and it holds none today | |
| H-17 | Arithmetic safety explicit | Completed | all seven crates inherit `arithmetic_side_effects` under CI's `-D warnings`, with every allowed site reasoned; `rusty_rtos_port-esp-radio` joined on 2026-10-01 (threat model R-6, closed): its arithmetic is saturating or `checked_*`, and the ring index math in `Queue::push`/`pop` carries an `#[expect]` stating the bound | |
| H-18 | ★ No `unwrap`/`expect`/panic on untrusted paths; typed errors | Completed | `unwrap_used`, `expect_used`, `panic` = deny at the workspace; tests opt out per file | |
| H-19 | Input validation — external bytes treated as hostile | Completed | the unit parses no bytes; its untrusted inputs are raw stack pointers and sizes, each an `unsafe fn` contract checked by `fuzz/task_stacks`. The esp-radio queue now clamps and `checked_mul`s its sizes (a heap overflow on zero capacity, fixed 2026-10-01) | |
| H-20 | ★ Secrets zeroized; never logged | Completed | `docs/threat-model.md` §5: no key material enters the ports by design and nothing is logged; the register state they copy is stated, with the un-wiped context of a deleted task as residual R-3 | |
| H-21 | Concurrency discipline | Completed | no manual `Send`/`Sync` impls (grep); shared state is atomics and the kernel's critical section, each `static mut` (esp-radio's two tables) written under it and documented in `UNSAFE.md`; ThreadSanitizer over the host port's tests is clean after the `errno` fix (2026-10-01) | |

### Phase 4 — Static analysis

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-22 | Static analysis beyond the default linter runs on every PR | Completed | `tools/unsafe_census.py` in CI (`hardening` job): the compiler forces every `unsafe` into an `#[expect(unsafe_code)]` fence and the census fails if a fence's item is missing from its crate's section of `UNSAFE.md`, or if a crate does not deny `unsafe_code` and `UNSAFE.md` does not pin its count. A pattern rule beyond clippy; on its first run it found 19 undocumented fences and one unfenced crate in the port family | |

### Phase 5 — Dynamic analysis

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-23 | ★ Tests pass under Miri | Completed | `cargo +nightly miri test --lib` 2026-09-09: green (miri 0.1.0 of 2026-09-08) | |
| H-24 | Critical paths pass the sanitizers (ASan/MSan/TSan) | Completed | AddressSanitizer over the workspace's tests (Windows, 2026-10-01): 13 + 7 + 5 + 3 passed, no report. ThreadSanitizer over the multi-threaded host port (WSL, `-Zbuild-std`): it reported "signal handler spoils errno" in `on_suspend`, which was fixed (`SavedErrno`), then 7 passed, no warning. Both nightly in `scheduled.yml` | |
| H-25 | `cargo careful test` green | Completed | `cargo +nightly careful test --workspace --lib --tests`: all green, 2026-10-01; runs nightly in `scheduled.yml` | |

### Phase 6 — Fuzzing and properties

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-26 | ★ Fuzz target per public parser, decoder, or message handler | Completed | `fuzz/fuzz_targets/task_stacks.rs` (seeded corpus): every port's `unsafe` stack builder on exactly-sized heap stacks of every size and alignment -- ASan for writes past the buffer, a canary for writes outside the documented window, the frame values where the switch reads them. 774,184 inputs in 60 s, no finding | |
| H-27 | ★ Continuous fuzzing with no open crashes | Incomplete | the nightly job exists (`scheduled.yml`); the gate needs 30 days of it, which starts when it is pushed | |
| H-28 | Property tests cover the documented invariants | Incomplete | the stack builders' window invariants are checked by `fuzz/task_stacks`, and register preservation by the poisoned QEMU cells, but there are no property tests as such | |
| H-29 | Mutation and/or differential testing on critical modules | Completed | the sim port is diffed against the C Posix port through the kernel's trace: 8,408,764 lines across nine scenarios and the exit/yield counters identical, 100,000 ticks each (ledger) | |

### Phase 7 — Formal verification

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-30 | Proof of panic-freedom / UB-freedom per `unsafe` module | Incomplete | the switches are assembly, which Kani cannot model; evidence is the QEMU cells and S3 runs, each with a poisoning that makes it fail (threat model R-4) | |

### Phase 8 — Build and binary

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-31 | ★ Binary hardening applied and verified | N/A | a library; the firmware binaries carry this gate | |
| H-32 | Build is reproducible or fully auditable | N/A | out of tier: a library, so no binary artifact ships from this unit; each firmware cell is its own build | |

### Phase 9 — Runtime privilege

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-33 | Least privilege documented and tested | N/A | a library on bare metal; the MPU package (K8) is the privilege story | |

### Phase 10 — Cryptography

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-34 | Vetted crypto only; no bespoke primitives | N/A | no cryptography in this crate | |
| H-35 | Side-channel discipline (constant-time, no secret branches) | N/A | no secret in this crate | |
| H-36 | Post-quantum migration plan for long-lived keys | N/A | no key in this crate | |

### Phase 11 — CI/CD, release, and operations

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| H-37 | CI runs the hardening gate on every PR | Completed | `.github/workflows/ci.yml`, per push and PR: fmt, clippy `-D warnings`, test (Linux, Windows, macOS), `cargo deny check` (incl. advisories), `cargo vet --locked`, the unsafe census, the README hardening-table `--check`, and a fuzz regression over every seed corpus. Fuzzing, sanitizers and `cargo careful` on the nightly schedule (`scheduled.yml`). Every action pinned to a commit SHA; `permissions: contents: read` | |
| H-38 | Releases signed, attested, and changelogged for security | Incomplete | release notes call out security changes (`CHANGELOG.md`), but tags and commits are not signed and no provenance is attached; needs the owner's signing key | |
| H-39 | ★ `SECURITY.md` with a coordinated disclosure process | Completed | `SECURITY.md`: contact, 5-day acknowledgement, 14-day updates, 90-day disclosure | |
| H-40 | Advisory monitoring and scheduled re-audit | Completed | `cargo deny check advisories` nightly (`scheduled.yml`); Dependabot weekly for crates and actions (`.github/dependabot.yml`); the full suite re-runs at every release and no later than the review date in `docs/threat-model.md` §7 (2027-01-01) | |
| H-41 | ★ Residual risks listed and accepted; waivers time-bounded | Completed | `docs/threat-model.md` §7: six residual risks, each with an owner (the Architect), a severity, why it is accepted and the condition that closes it; reviewed at every release and no later than 2027-01-01 | |

### Phase 12 — Compliance controls

Only in play when a framework is declared in scope above. With none in scope, every row is
`N/A` — reason: "no compliance framework in scope". Mapping: the skill's `COMPLIANCE.md`.

| ID | Gate | Status | Evidence | Target |
|---|---|---|---|---|
| C-01 | Data inventory — personal/health/card data touched | N/A | no compliance framework in scope | |
| C-02 | Data-flow map including third-party egress | N/A | no compliance framework in scope | |
| C-03 | Encryption in transit for all egress | N/A | no compliance framework in scope | |
| C-04 | Encryption at rest for stored sensitive data | N/A | no compliance framework in scope | |
| C-05 | Key management — generation, storage, rotation, destruction | N/A | no compliance framework in scope | |
| C-06 | Retention limits and honoured deletion | N/A | no compliance framework in scope | |
| C-07 | Audit logging of security-relevant events | N/A | no compliance framework in scope | |
| C-08 | Log hygiene — no PII, secrets, or card data in logs | N/A | no compliance framework in scope | |
| C-09 | Least-privilege access to sensitive data | N/A | no compliance framework in scope | |
| C-10 | Subprocessor and third-party inventory | N/A | no compliance framework in scope | |
| C-11 | Incident response and breach notification path | N/A | no compliance framework in scope | |
| C-12 | Change management — reviewed, approved, traceable | N/A | no compliance framework in scope | |
| C-13 | Availability commitments and their evidence | N/A | no compliance framework in scope | |
| C-14 | Machine-readable SBOM + provenance for regulators | N/A | no compliance framework in scope | |

---

## Scheduled work

In execution order. Cheapest-first is usually correct: configuration gates clear in
minutes and unblock the outcome gates behind them.

| # | Gates | Work | Owner | Target | Notes |
|---|---|---|---|---|---|
| 1 | H-08, H-09 | run `cargo deny check` and `cargo audit` and record the verdicts | | | minutes |
| 2 | H-01 | write `docs/threat-model.md` from the sketch above | | | with the first milestone |
| 3 | H-23 | `cargo +nightly miri test` on the core | | | with the first tests |

---

## Residual risk register

Every open risk carries an owner, an acceptance, and a review date (H-41).

| ID | Risk | Likelihood | Impact | Mitigation status | Accepted by | Review date |
|---|---|---|---|---|---|---|
| R-001 | | | | | | |

---

## Waivers

Time-bounded only. An expired waiver is an `Incomplete` gate, not a `Completed` one.

| Gate | Reason | Granted by | Expires |
|---|---|---|---|
| | | | |

---

## Audit log

Append one line per pass; never rewrite history. The trend is the point.

| Date | Depth | Auditor | Completed / Scheduled / Incomplete | ★ met | Note |
|---|---|---|---|---|---|
| 2026-09-09 | survey | kairos (scaffold pass) | 7 / 0 / 28 | 5 | first pass, at stamp time; every Completed row names a file that exists |
| 2026-09-09 | survey + tool probes | kairos (K1 pass) | 12 / 0 / 24 | 9 | K1: the trace differential against the C kernel is live and is this unit's strongest evidence; deny, audit and Miri run on the developer box |
| 2026-10-01 | deep | v1.0-readiness pass | 28 / 0 / 6 | 14/16 | vet (25 certified, 27 exempt), stack-builder fuzz target, threat model v1, census + hardening-table + fuzz-regression in CI, ASan/TSan/careful; FOUR defects fixed (Cortex-M `init_stack` unsound as a safe fn; esp-radio heap overflow and dangling creates; host signal handler spoiling errno); UNSAFE.md completed for Xtensa and the Unix host backend |
| 2026-10-01 | deep | esp-radio lint pass | 29 / 0 / 5 | 14/16 | `rusty_rtos_port-esp-radio` inherits the workspace lints: every `unsafe` fenced and inventoried, threat model R-6 closed, H-17 Completed; a FIFTH defect fixed (`WaitQueue` waiter count updated outside the mask); silicon re-run of `xiao-s3-wifi` stage 1 and `xiao-s3-radio` |

## v0.1.0 release decision — which gates are waived, and why (2026-09-16)

The mission plan's §2.11 bar says a repo flips public only when "its hardening
row is complete". That bar is written for **1.0.0**. This package is publishing
**0.1.0**, and the difference is deliberate rather than convenient: 0.x tells a
consumer the API is not yet stable, and the gates below are the ones whose
absence a 0.x consumer can reasonably price in.

**Waived for 0.x, to be closed before 1.0.0:**

| gate | why it is waived at 0.x | what closes it |
|---|---|---|
| H-01 / H-02 — threat model | the attack surface of a kernel with no network stack, no filesystem and no dynamic loading is the ports' `unsafe` and the C ABI's pointer handling, both of which are inventoried already (`UNSAFE.md`, the header gate) | `docs/threat-model.md`, written once the K7 libraries add a network surface |
| H-10 — `cargo vet` coverage | the dependency tree is one crate deep and every dependency is either a house crate at an exact pin or nothing at all; `cargo vet`'s value is in a deep third-party tree | a `supply-chain/` directory once K7 pulls in smoltcp and friends |
| H-16 — fuzzing beyond the no-panic gate | the corpus is a stronger oracle than a fuzzer here: it diffs against the C kernel line-by-line rather than looking for crashes | `cargo fuzz` targets on the C ABI's decode paths |
| H-18 — formal verification | Kani proofs exist for the queue invariants; extending them is a 1.0 item | the remaining `proofs.rs` obligations |

**NOT waived, and closed for this release:** H-08 (`deny.toml` enforced),
H-09 (`cargo audit` clean), H-11 (zero `unsafe`, compiler-enforced),
H-12 (SBOM), H-13 (no git dependencies), H-14 (dependency freshness).

This section is the "stated decision in the plan, not silently" that the
release review asked for. A gate marked Incomplete above and not listed here is
an omission, not a decision — that distinction is the point.

## v1.0.0 readiness -- what still blocks (2026-10-01)

Every v1.0.0 (★) gate not listed here is Completed with evidence. These remain, and neither is engineering the auditor can do:

- H-10: 27 embedded-ecosystem crates exempted -- the owner decides which publishers to trust (threat model R-1).
- H-27: thirty nights of `scheduled.yml` -- starts when it is pushed.

Also open, not ★: H-38 (signed tags and attested artifacts need the owner's signing key). The release itself -- version bump, `cargo publish`, the tag -- is the owner's to run.
