# `mps2-an385-qemu-preempt` — the window between deciding a switch and enacting it

A witness for one contract question: **when the kernel commits a switch at
the point of a call, what happens to the code still running as the old
task?**

## The window

`Kernel::port_yield` calls `switch_context`, which moves `self.current`. On a
**stackless** kernel that is the whole switch — changing which task the runner
steps next is all a switch means when no task owns a stack. On a **stacked**
port the CPU has not moved yet, so until it does, code runs as a task the
kernel no longer thinks is current.

The sibling `mps2-an385-qemu-kernel` cell never opens that window: it pends a
switch by hand immediately after every `delay`, so nothing happens in the gap.
This cell deliberately does not.

## What it took to actually reach the condition

Three versions of this cell passed without testing anything, and each failure
is worth keeping:

1. **Producer high, consumer low.** A `give` that wakes a *lower*-priority
   task causes no preemption, so `current` never moved. 198/200 hand-offs and
   a clean bill of health, proving nothing. The window only opens when the
   woken task **outranks** the caller.
2. **Counting attempts, not successes.** `with_kernel(...)` answers `Some`
   whenever the kernel is reachable, regardless of the inner `Result`, so
   `is_some()` counted attempts. That read 200 producer hand-offs against 100
   consumer ones and looked exactly like a scheduling defect. It was a
   measurement defect.
3. **A pre-loaded token.** With `free` starting at 1 the producer's second
   call always succeeded immediately, so it never *blocked* in the window —
   and blocking is the only case that can park the wrong task. Both
   semaphores now start empty.

So the cell counts the condition itself, and **checks it**:

```
window opened         199   of which blocked inside: 1
      ok    the cell REACHED its condition: a blocking call made while the
            kernel had already moved `current`
```

A pass with `window opened = 0` would prove nothing, which is why that is a
check and not a print.

## The result, 2026-09-11 — before any kernel change

```
ticks                 1004
switches              601
producer hand-offs    200   (want 200)
consumer hand-offs    199   (want 200)
scheduler stalls      0   first: None
window opened         199   of which blocked inside: 1
RESULT: PASS
```

**ARMv7-M is exposed but not failing.** The window opens on essentially every
round; the harmful sub-case — actually blocking inside it — happened once in
200 and the system recovered through the call's own timeout.

That is not the same as "ARM is fine", and it is not the Xtensa result, where
the same contract produced `laps 0/0` with every ready list empty. Read it as:
the exposure is real and shared, the consequence is rare here and immediate
there.

## Why the numbers differ from the sibling cell

This one ends on the producer's count rather than a tick deadline, and its
tasks yield explicitly rather than sleeping, so `ticks` and `switches` are not
comparable with `mps2-an385-qemu-kernel`. They test different things: that one
asserts fixed-priority scheduling with a blocking `delay`; this one asserts
what happens in the gap between a decision and its enactment.

## A packaging note

This cell depends on `rusty_rtos_kernel-core` **by path**, against
`firmware/README.md`'s "siblings by git URL as usual". A git dependency
resolves to the *published* kernel, so a cell meant to gate kernel changes
would build green against code that is not under test — which is what the
sibling cell does today. The cost is that this cell cannot be built from a
standalone clone of `rusty_rtos_port`. That trade is the owner's to revisit.

## Running it

```sh
cargo run --release        # needs qemu-system-arm on PATH
```
