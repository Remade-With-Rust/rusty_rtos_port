# UNSAFE.md — every `unsafe` in `rusty_rtos_port`

`rusty_rtos_port-core` is `forbid(unsafe_code)` and has none. The workspace
denies `unsafe_code`; the per-chip port crates lift it, per-item, with
`#[expect(unsafe_code, reason = "...")]` — which means an unused fence is
itself a warning, so this list cannot rot silently.

The mission plan's founding decisions allow unsafe in exactly two places:
`rusty_rtos_port-*` and the C ABI crate. This is the first of them.

## `rusty_rtos_port-cortex-m`

Two kinds, and nothing else.

### 1. Core peripheral and core register access

| item | what it does | why it is sound |
|---|---|---|
| `disable_interrupts` / `enable_interrupts` | `cpsid i` / `cpsie i` | Sets or clears PRIMASK. Cannot fault, touches no memory (`nomem, nostack`), and the `compiler_fence` on the inside keeps the section's accesses within it. |
| `primask` / `ipsr` | `mrs` from a core register | A read of an architecturally-defined register. No memory, no fault. |
| `write_reg` | `write_volatile` to a core peripheral | Every caller passes one of the module's own `const` addresses — `ICSR`, `SYST_*`, `SHPR3` — which are fixed by ARMv7-M and always mapped. Not reachable with a caller-supplied address. |
| `CortexMPort::idle` | `wfi` | A hint instruction; cannot fault. |
| `pend_switch` | `dsb; isb` after writing `PENDSVSET` | The barriers the ARM ARM requires after a write that changes exception state. |

### 2. The context switch

This is the reason the crate exists and the reason it is allowed unsafe.

| item | what it does | why it is sound |
|---|---|---|
| `init_stack` | Writes an initial exception frame | The caller passes `top`, one past a stack of at least 16 words. Every write is inside `top[-16 .. top]`, formed only by offsetting within that range, and the frame's layout is the architecture's: `xPSR, PC, LR, R12, R3, R2, R1, R0` high to low, then `R11..R4` below it. |
| the `PendSV` symbol (`global_asm!`) | Saves `r4-r11`, stores the SP, calls the scheduler, restores | `r4-r11` are exactly the registers the hardware does **not** stack on exception entry, so they are the ones that must be saved by hand. The handler refuses to touch anything when `CURRENT_SP_SLOT` or the slot it names is `0`, which is the "nothing is running yet" case. |
| `kairos_pick_next` | `transmute` a `usize` to the scheduler fn pointer | The static only ever holds a value written by `set_scheduler`, whose parameter is that exact fn type; `0` is checked first as "none installed". |
| `start_first_task` | Sets PSP, switches `CONTROL.SPSEL`, enters the task | Documented `unsafe fn`: the caller must have built the stack with `init_stack` and installed a scheduler. |

## `rusty_rtos_port-riscv`

Three kinds. The first two mirror the Cortex-M crate; the third is the one
that is genuinely different, because a RISC-V trap saves nothing.

### 1. Machine register and CLINT access

| item | what it does | why it is sound |
|---|---|---|
| `enter_critical` / `exit_critical` | clears / sets `mstatus.MIE` | A CSR write defined by the ISA. Cannot fault, touches no memory, and the nesting count is `saturating_sub` so a stray exit cannot wrap the depth and mask interrupts for ever. |
| `raise_switch` / `clear_switch_request` | `write_volatile` to `CLINT_MSIP` | A single fixed address, a module `const`, not reachable with a caller-supplied one. |
| `mcycle` / `minstret` | `csrr` | Architecturally-defined counters. No memory, no fault. |

### 2. The COOPERATIVE context switch

| item | what it does | why it is sound |
|---|---|---|
| `new_task_context` | Fills a `Context` and points `ra` at the trampoline | Writes nothing through the stack pointer: it only rounds `stack_top` down to the ABI's 16-byte alignment and records it. The arguments travel in `s1`/`s2` because a switch restores only the callee-saved set. |
| `kairos_riscv_switch` (`global_asm!`) | Fourteen stores, fourteen loads, `ret` | `ra`, `sp` and `s0`–`s11` are exactly the registers a trap does not preserve. `#[repr(C)]` on `Context` is what makes the offsets a contract rather than a coincidence, and a null outgoing pointer is tested first. |
| `switch_context` | `call` into the above | Documented `unsafe fn`: both pointers must be valid and `next` must be a context this port built. |

### 3. The PREEMPTIVE context switch

Kept separate from the cooperative one on purpose, so a yield pays only for
what a yield needs.

| item | what it does | why it is sound |
|---|---|---|
| `new_task_context_preemptive` | As above, but `mepc` is the entry point and `mstatus` carries `MPIE` | Same reasoning; it writes nothing through the stack pointer either. `FRESH_MSTATUS` is `0x1880` — `MPIE` set, `MPP` = M-mode — the same value FreeRTOS computes in `pxPortInitialiseStack`. |
| `kairos_riscv_switch_trap` (`global_asm!`) | The fourteen, plus `mepc` and `mstatus` | Must run inside a trap, which is what makes swapping those two CSRs correct: `mepc` at that moment belongs to the task being switched **out**. `t0` is used as scratch after `sp` has moved, which is sound because `t0` is caller-saved and the trap epilogue reloads it from the incoming task's frame. |
| `kairos_riscv_trampoline_trap` (`global_asm!`) | `mv a0, s1; mv a1, s2; mret` | The three instructions a task that has never run needs in order to leave the trap properly. It cannot `ret` into `_start_trap_rust` — there is no frame of that function on a fresh task's stack to return into. |
| `switch_context_trap` | `call` into the above | Documented `unsafe fn`: **must be called from inside a trap handler**, because it ends by handing the task an `mret`. |

### What proves the RISC-V half

`firmware/riscv32-qemu-switch` proves the cooperative switch — 100/99
resumptions from three call frames deep, every word of a 64-word stack
witness re-checked after each one.

`firmware/riscv32-qemu-preempt` proves the preemptive one — **201 switches
between two tasks that never yield**, which is the only thing that can prove
preemption rather than cooperation, and it is a check rather than a print.

And the fences are proven to be around something. Three lines were deleted
one at a time and the preemptive cell re-run:

| removed | result |
|---|---|
| the `mepc` restore | **hangs** |
| `mret` in the trampoline (→ `ret`) | **hangs** |
| the `mstatus` restore | **still passes** |

The third one is why this table says what it says about `mstatus`: it is
saved because a task's interrupt state is its own, **not** because its
absence broke preemption. An earlier note claimed the latter and was wrong;
the poison run is what caught it.

## What proves the Cortex-M half, rather than what argues it

`firmware/mps2-an385-qemu-switch` runs two tasks with real stacks under
QEMU and has each **re-check every word of its own stack** after every
switch it observes, plus a value the compiler is forced to keep in a
callee-saved register. It exits with a code, so it gates.

The claim that this test can fail is not an assertion. Deleting one
instruction from the handler —

```diff
-    "    stmdb r0!, {{r4-r11}}",
+    "    sub   r0, #32",
```

— which is precisely "stop saving the callee-saved registers", gives:

```text
      FAIL  every word of both stacks survived every switch
      corrupted witness word index 0
RESULT: FAIL -- 4 check(s) failed        QEMU exit code: 1
```

and restoring it gives 100/100 resumptions and exit 0. **A fenced `unsafe`
whose test has never been made to fail is a fence around nothing.**

## `rusty_rtos_port-host`

One kind, and nothing else: **stopping and starting an OS thread**. There is
no assembly here and no peripheral — the operating system owns the stacks and
the switch, and this crate owns the policy.

| item | what it does | why it is sound |
|---|---|---|
| `backend::thread_handle_of` | `JoinHandle::as_raw_handle` | Borrowed, not owned. This replaced a `DuplicateHandle` of `GetCurrentThread` called from inside the thread, which had a window: a thread that names ITSELF exists, may hold the run permit, and has no handle yet, so a tick landing in that window cannot freeze it. The `JoinHandle` is kept in `threads()` for the life of the process precisely so the handle stays valid; dropping it would close the handle and every later freeze would silently fail. |
| `backend::freeze` | `SuspendThread` | The handle came from one of the two above and names a thread of this process. The SAFETY argument that matters is not about the call but about WHEN it is made: `Ticker::interrupt` takes the critical-section lock first, and every kernel call and every heap call is inside that lock, so a thread that is not holding it is not holding a lock of ours. What that does not cover is a lock we do not own — the C runtime's allocator, or `stdout` — and the cells that use this port touch neither from a task. |
| `backend::thaw` | `ResumeThread` | As `freeze`. |
| `backend::release` | `CloseHandle` | The handle is taken out of its slot with a `swap` first, so it cannot be used again. |
| `ask_scheduler` | `transmute` of a `usize` to a `Scheduler` fn pointer | The value is only ever written by `set_scheduler`, from a value of that exact type, and nothing else writes that static. Zero means "no scheduler installed" and is checked before the transmute. |

On platforms without a backend these are all absent: the `cfg(not(windows))`
module has no `unsafe` at all, because it cannot freeze a thread and says so
through `PREEMPTIVE` rather than pretending.
