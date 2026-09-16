# rusty_rtos_port — the ledger

Every number this package claims, with the run that produced it. A row
without a method is not a number. Counters before clocks; an external oracle
before a self-metric; the method line names the machine, the pinning, the arm
order and the null-arm floor for anything timed.

## The sim port against the C Posix port (2026-09-09, K1)

| gate | result | method |
|---|---|---|
| sim contract v1 implemented identically on both sides | yes | `kairos conform --all --ticks 100000` from the umbrella: 8,408,764 trace lines identical across nine scenarios, and the counters with them. A port that delivered a tick one critical-section exit early or late would move every line after it, so the trace is the proof |
| the tail of a switched-out call is tallied, not counted | yes | `Port::begin_unwind` / `end_unwind`: a thread stops at the switch, a stackless call does not, so the abandoned frame's exits are charged to the task when it next runs. Counting them rather than discarding them is what `blocktim` needed — a tail can open a section of its own, and `xQueueReceive`'s timeout path does |
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

## The host port: a task gets a real stack from the OS (2026-09-11, K6)

`rusty_rtos_port-host` runs one OS thread per task with a single run
permit, the shape FreeRTOS's own Posix and MSVC-MinGW ports use. It exists
because the Kairos kernel is stackless — a blocking call answers
`Wait::Blocked`, meaning "call again when this task next runs" — and a task
written in **C** cannot work that way: `vTaskDelay` must return *later*,
with its locals intact, and locals in C live on a stack.

Until this crate there was nowhere on a host to put one. That is why K6's
kill test ran on QEMU Cortex-M3 before it ran on a laptop, in the opposite
order to the one the plan gives.

### The claim, and the measurement that carries it

`firmware/host-kernel` is the M3 scheduling cell's experiment, moved off the
chip: same kernel, same fixed-priority preemptive scheduler, same
assertions. Its low-priority task **never blocks and never yields** — the
shape of `integer.c` and `flop.c`, which with `configUSE_PREEMPTION` set
rely on being taken off the CPU — so the high task's lap count can only come
out if the port preempts.

| | switches | high-task laps | verdict |
|---|---:|---:|---|
| preemption on | 841 | 40 of an expected 40 | PASS, 8 checks |
| `KAIROS_HOST_NO_PREEMPT=1` | — | **1** of an expected 120 | FAIL, 3 checks |

Method: `cargo run --release`, 400 ticks at 1 kHz, `DELAY_TICKS = 10`. The
poison row is the same binary with preemption declined at runtime, which is
also what the platforms without a backend look like.

### What this cell asserts that the M3 cell could not

**Exactly one task is on the CPU at a time.** On a chip that is free; here
every task is a real OS thread the operating system would happily run in
parallel, so the single run permit is doing real work and a bug in it is a
data race rather than a wrong number. Each task stamps its index on entry
and checks it, and the run reports zero overlaps.

### Why freezing a thread is safe here, and where the argument stops

Suspending an arbitrary thread is a famous way to deadlock. It is safe in
[`Ticker::interrupt`] because of the ORDER: the tick thread takes the
critical-section lock FIRST, and every kernel call and every heap call is
inside that lock, so a thread that is not holding it is not holding a lock
of ours.

What that does not cover is a lock we do not own — the C runtime's
allocator, or `stdout`. The demo files touch neither; a cell that prints
from a task must do it inside `enter_critical`, and this one does.

### Three defects, all of them about the difference from a chip

1. **A voluntary handover could interleave with the tick thread's.** A task
   asked the scheduler, the tick thread froze it mid-answer and handed the
   CPU elsewhere, and the thawed task later completed a handover whose
   answer was stale — granting the permit to a task that should not have
   it. The decision and the handover now happen with the tick locked out.
2. **`pend_switch` took a switch where `PendSV` only PENDS one.** On a chip
   `portYIELD()` writes PENDSVSET and returns; the switch happens when
   every kernel call has finished. Taking it immediately re-entered the
   kernel while an outer call still held a `&mut` to it.
3. **The tick thread asked the scheduler when it could not act on the
   answer.** `switch_context` MOVES the kernel's current task; asking and
   then declining to switch left the kernel and the port disagreeing about
   who was running, so every API meaning "the calling task" named the wrong
   one.

### The Unix backend: a thread that freezes ITSELF (2026-09-11)

Preemption needs a way to stop a thread where it stands. Windows hands one
over — `SuspendThread` stops another thread from outside — and **Unix has
no such call at all**. What it has is a signal, and a signal handler is the
only code that runs in another thread's context without that thread's
cooperation. So on Unix the tick thread sends `SIGUSR1` and the target
parks *itself* in `sigsuspend` until `SIGUSR2` arrives. It is the shape
FreeRTOS's own `portable/ThirdParty/GCC/Posix` port uses.

Measured on Linux (WSL2, Ubuntu, glibc), `firmware/host-kernel`, the same
cell and the same assertions as the Windows row above:

| | switches | "could not freeze" | high-task laps | verdict |
|---|---:|---:|---:|---|
| preemption on | 400 of 400 wanted | **0** | 40 of an expected 40 | **PASS, 11 checks** |
| `KAIROS_HOST_NO_PREEMPT=1` | — | — | **1** | **FAIL, 5 checks** |

The poison row is the point. A preemption test that only ever passes is not
evidence, because it might be passing for some other reason; the same
binary with the mechanism declined at runtime has to fail, and it fails on
exactly the checks that depend on it — including "the SECOND task at the
low task's priority ran at all", which goes from 4,734,144 laps to **zero**.

#### Two races, and both are in the handoff rather than the signal

**`pthread_kill` returns when the signal is QUEUED, not when it is
handled.** If `freeze` answered there, the tick thread would grant the CPU
to a new task while the old one was still running on it — two tasks live at
once, which is the one thing a single run permit exists to prevent. So
`freeze` waits until it can observe the target parked and answers `false`
if it never does, in which case the caller declines the switch instead of
taking it unsafely. That is why the table's "could not freeze" column
exists and why its value being 0 is worth printing.

**The lost wakeup.** The handler marks itself parked and then calls
`sigsuspend`; a thaw landing between those two would be delivered to a
thread that is not yet waiting, and the thread would sleep for ever on a
wake-up that already happened. `SIGUSR2` is therefore in the suspend
handler's `sa_mask`, so it is blocked for the whole handler: a thaw in that
gap stays *pending*, and `sigsuspend` atomically unblocks it, which
delivers it immediately and returns. The handler still re-checks the state
in a loop rather than trusting the wake-up.

Nothing inside the handler allocates, locks, or touches a thread-local — a
Rust thread-local can allocate on first touch, which is not safe in a
signal handler. It reads `pthread_self`, scans a fixed static table of
atomics, and calls `sigsuspend`. All of those are async-signal-safe.

`PREEMPTIVE` is now `true` on Windows and on Unix, and `false` on a
platform that is neither — reported rather than assumed, so a cell can
still refuse to call a starved demo a pass.

### A port must say that it commits the switch

`Port::COMMITS_SWITCH` tells the kernel whether the port takes the context
switch itself. Every bare-metal port in this package declares it `true`.
`HostPort` did not, and the default is `false` — so the kernel treated
itself as STACKLESS, moved `current` inside `port_yield` by calling
`switch_context` directly, and the port then switched AGAIN on the way out.

**Two selections per yield.** The round robin advanced twice, and on a
ready list of two that is the same task every time, for ever.

The cell that found it is this package's own:

| `firmware/host-kernel`, two never-yielding tasks at ONE priority | laps |
|---|---|
| before | 13,489,456 against **0** |
| after | 6,077,079 against 6,065,221 |

It matters well beyond this package. In `rusty_rtos-capi`'s host cell it was
what stopped `EventGroups` and `dynamic` dead under sustained checking,
because their tasks sit at the same priority as a busier demo's — per-task
turns showed `QProdB2` (priority 0) on 107,299 against `SetB` (priority 0)
on **124**. With the constant declared, that cell went from 19 of 21 to
**21 of 21**.

The trait's doc also names the other half of the cost, and it is the same
sentence that describes a bug already worked around elsewhere: "Between the
two, code runs as a task the kernel no longer thinks is current — and a
blocking call made in that gap parks the wrong task." That was the identity
divergence `GenQTest` found, patched at the time with a `reconcile()`
function in the C ABI host cell. With the port declaring itself properly
there is nothing to reconcile, and `reconcile()` was deleted rather than
left standing beside its own fix.

**Three refutations came first**, and they are worth as much as the fix:
arena exhaustion (heap free constant at 7,584 all run), a global stall
(62k/64k/63k switches per period — the system was never slow), and
`integer.c` hogging the CPU (removing it changed nothing). The list
primitive and the kernel were both exonerated by tests that are now in the
tree: 4 in `rusty_rtos_core` including rotation across other lists being
emptied and refilled, 2 in `rusty_rtos_kernel-core`.

`grants_to`, `tick_stats`, `thread_of`, `orphaned_threads`,
`switch_pending`, `critical_depth` and `my_index` were added for this hunt
and stayed. A port that cannot say who it gave the CPU to cannot be
debugged from outside.

| gate | result |
|---|---|
| `cargo test -p rusty_rtos_port-host` | 6 tests pass |
| `cargo run --release` in `firmware/host-kernel` | passes, 8 checks including equal-priority fairness |
| `unsafe` blocks | 4, all `SuspendThread` / `ResumeThread` / `DuplicateHandle` / `CloseHandle`, each fenced with `#[expect(unsafe_code)]` |
