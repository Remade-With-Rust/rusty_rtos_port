# Threat model — `rusty_rtos_port`

**Unit tier:** critical-path. **Model version:** 1, 2026-10-01.
**Scope:** the ports — `rusty_rtos_port-core` (the sim port), `-cortex-m`,
`-riscv`, `-xtensa`, `-host`, `-esp-radio`, and the facade. This is where the
family's `unsafe` lives: the context switch, stack initialisation, interrupt
masking, and on the host the freezing of OS threads. The kernel and core are
separate units with their own models.

Satisfies `use-protection-please` **H-01**. The secrets position is §5
(**H-20**); the residual-risk register is §7 (**H-41**).

---

## 1. What this unit is, in one paragraph

A port is the only place the kernel touches the machine. It saves one task's
registers and restores another's, builds the first frame of a task that has
never run, and masks interrupts for the kernel's critical sections. Get any of
that wrong and the failure is not an error return: it is one task running
with another's registers, a write through a stack pointer into somebody
else's memory, or a critical section that an interrupt walks straight into.
The kernel above is `forbid(unsafe)` precisely because this unit carries the
`unsafe` for it, in fences that are inventoried and checked.

---

## 2. Assets

| asset | why it matters | what failure looks like |
|---|---|---|
| **Register integrity across a switch** | every task must resume exactly as it stopped | a callee-saved register clobbered; a task that computes wrong answers only sometimes |
| **Stack memory safety** | the stack builders write through a pointer the firmware supplies | an initial frame written outside the task's stack, over another task's |
| **Critical-section integrity** | the kernel's data structures are consistent only inside one | an interrupt observing a half-updated list |
| **Host-port thread control** | the host port freezes OS threads to preempt them | two tasks on the CPU at once; a thread frozen holding a lock the scheduler needs |
| **Integrity of the published crates** | downstream builds resolve them by name | a substituted dependency in the code that runs in every interrupt |

---

## 3. Adversaries and what they can do

1. **A firmware author misusing an `unsafe` entry point**: a wrong stack top
   to a stack builder, `switch_context` with a context the port did not
   build, or a scheduler hook that is not what it claims to be.
2. **A buggy or hostile task**, which cannot call the ports' internals but
   can run at any point an interrupt can preempt it, and holds whatever
   registers it likes when it is switched out.
3. **Interrupts themselves**, arriving between any two instructions of a
   task, including inside the kernel when a critical section is wrong.
4. **A supply-chain actor**, through the embedded crates the ports stand on:
   `xtensa-lx-rt` writes the trap frame this unit switches; `critical-section`
   and the `riscv` crates sit on the interrupt path.

**Out of scope:** physical attacks, glitching, side channels, a compromised
toolchain, and the radio blob's own code behind `esp-radio`.

---

## 4. The attack paths, and the evidence against each

### 4.1 A register clobbered across a switch

*Mitigation:* each port saves exactly the set the hardware does not
(Cortex-M `r4-r11`; RISC-V `ra`, `sp`, `s0-s11`, plus `mepc` and `mstatus`
when preemptive; Xtensa the whole trap frame `xtensa-lx-rt` fills, windows
already spilled), with `#[repr(C)]` making the offsets a contract.

*Evidence that can fail, and has been made to:* the QEMU cells re-check every
word of each task's stack and a value pinned in a callee-saved register after
every switch. Deleting one instruction from the Cortex-M handler ("stop
saving `r4-r11`") fails the cell; so do deleting the RISC-V `mepc` restore and
the trampoline's `mret`. `UNSAFE.md` records each poisoning. On the S3, the
`xiao-s3-realtime` firmware runs twelve tasks through tens of thousands of
in-trap and `Software0` switches per run with every check passing.

### 4.2 A stack builder writing outside its stack

*Mitigation:* each builder documents the window it writes (Xtensa: four words
below `top & !15`; Cortex-M: sixteen words below `top & !7`; RISC-V: none) and
is an `unsafe fn` whose `# Safety` section makes that window the caller's
obligation. Cortex-M's `init_stack` was a SAFE `fn` through 0.2.1, which was
unsound; the hardening audit of 2026-10-01 made it `unsafe` (a breaking
change, released as such).

*Evidence:* `fuzz/task_stacks` drives all three on exactly-sized heap stacks
of every size and alignment. AddressSanitizer reports any write outside the
buffer; a canary fill catches any write inside it but outside the documented
window; and the frame values are checked where the switch reads them.

### 4.3 A broken critical section

*Mitigation:* masking is one instruction per port (`cpsid i`, `csrrci`,
`rsil`); the nesting depth is `saturating_sub`, so a stray exit cannot wrap
it and leave interrupts masked for ever; the Xtensa level is 3, which leaves
the non-maskable and debug levels alone and matches esp-hal's own critical
section.

*Evidence:* the kernel's conformance corpus runs on the sim port, whose clock
IS critical-section exits, so a missing or extra exit changes the trace; the
QEMU and S3 cells exercise the real ones.

### 4.4 The host port's thread freezing

*Mitigation:* a thread is frozen only by the tick thread holding the
critical-section lock that every kernel and heap call also takes, so a frozen
thread never holds a lock of ours. On Unix the target parks itself in a
`SIGUSR1` handler that touches only atomics and async-signal-safe calls, with
`SIGUSR2` blocked until it is inside `sigsuspend`, which closes the lost
wake-up.

*Evidence:* `firmware/host-kernel`'s check that exactly one task is on the CPU
at a time, though each is a real OS thread; the C demo corpus on the host.

### 4.5 `unsafe` that nobody wrote up

*Mitigation:* the workspace denies `unsafe_code`, so every `unsafe` compiles
only inside an `#[expect(unsafe_code, reason)]` fence, and
`tools/unsafe_census.py` (in CI) fails if any fence's item is missing from its
crate's section of `UNSAFE.md`. The first run of the census found 19 fences
undocumented, including all of `rusty_rtos_port-xtensa`, and a false claim
that the non-Windows host backend had no `unsafe`. Fixed in the same pass:
47 fences, every one inventoried.

### 4.6 A substituted dependency

*Mitigation:* committed `Cargo.lock`; `cargo deny` (no `*-sys`, `ring`,
`aws-lc-sys`); `cargo vet` with 25 dependencies covered by publisher trust or
imported audits, in CI with `--locked`.

*Residual:* 27 embedded-ecosystem crates are exempted — §7, R-1.

---

## 5. Secrets — H-20

**No key material enters this unit, by design.** The ports hold no keys, no
credentials and no entropy source, and log nothing.

What a port DOES handle is every task's register file, which may at any
moment contain whatever an application was computing, secrets included. The
port copies it into the task's context slot or stack and back, never logs it,
and never moves it anywhere else. It does not zeroize a context when a task
is deleted (R-3): a firmware handling key material must zeroize its own
buffers, as the family's PKCS#11 and identity crates do, and must not rely on
a deleted task's stack being wiped.

---

## 6. Assumptions this model depends on

1. **`xtensa-lx-rt` spills every register window and fills the trap frame
   it documents**, and restores from it on exit. The Xtensa switch is
   correct only if that holds; it is exempted rather than audited in
   `cargo vet` (R-1) and evidenced by the S3 firmware runs.
2. **esp-hal's dispatcher calls a peripheral handler with the frame it will
   restore**, at the pinned `=1.2.1`. Only the in-trap switch in
   `xiao-s3-realtime` relies on it, and it says so at the call.
3. **The firmware upholds each `unsafe fn`'s contract.** The ports cannot
   check a raw stack pointer; they document what they need and the fuzz
   target checks they need no more.

---

## 7. Residual risks — H-41

Every row has an owner, the reason it is accepted, and the condition that
closes it. **Owner:** the Architect named in the README's hardening block.
**Review:** at every release, and no later than 2027-01-01.

| # | residual risk | severity | why accepted for now | closes when |
|---|---|---|---|---|
| R-1 | **27 embedded-ecosystem dependencies are exempted in `cargo vet`** (H-10): the esp-rs crates (incl. `xtensa-lx-rt`), `critical-section`, `embedded-hal`, `heapless`, `embassy-sync`, `riscv`, `futures-*`, `portable-atomic`. No imported audit set covers them. | medium | trusting their publishers is a security attestation for the owner to make, and auditing ~200k lines is not a pass of this audit; `cargo deny` still checks them for advisories and licences | the owner runs `cargo vet trust` for the publishers they accept, or audits are certified |
| R-2 | **Continuous fuzzing has not yet run 30 days** (H-27). | medium | the gate is calendar time | 30 nights of `scheduled.yml` with no open crash |
| R-3 | **A deleted task's context and stack are not zeroized.** | low | the ports never log or move register state, and zeroizing is the owner of the secret's job | a `delete` path that wipes the slot, if a firmware needs it |
| R-4 | **No Kani proof per `unsafe` module** (H-30). The switches are assembly, which Kani cannot model. | medium | the QEMU cells and the S3 runs are the evidence, each with a poisoning that makes it fail | a model of the frame layout proved against the asm, or a verified switch |
| R-5 | **Releases are not signed** (H-38). | medium | needs the owner's signing key | tags are signed and the release workflow attests artifacts |
| R-6 | **`rusty_rtos_port-esp-radio`'s 100 `unsafe` sites are not fenced item by item.** The crate does not inherit the workspace lints, so nothing forced its `unsafe` into `#[expect]` fences; `UNSAFE.md` documents it per file and pins the count, which the census enforces. Two defects found there in the audit were fixed (a heap overflow on a zero-capacity queue; dangling pointers handed to the radio driver on any failed create). | medium | the crate is built only inside the Janus firmware, and changing its lint set must be verified there; it is not yet published | it inherits `[lints] workspace = true`, every site is fenced and has a row, and the Janus firmware is re-verified |

---

## 8. How to attack this document

Ask of every mitigation: **which command proves it, and has that command been
made to fail?** A fence around nothing is a fence; the rows in `UNSAFE.md`
that carry a poisoning are the ones to trust most, and the ones without are
where to start.
