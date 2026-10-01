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
| `write_reg` / `read_reg` | `write_volatile` / `read_volatile` on a core peripheral | Every caller passes one of the module's own `const` addresses — `ICSR`, `SYST_*`, `SHPR3` — which are fixed by ARMv7-M and always mapped. Not reachable with a caller-supplied address; both are private. |
| `CortexMPort::idle` | `wfi` | A hint instruction; cannot fault. |
| `suppress_ticks_and_sleep` | `dsb; wfi; isb` around the tickless sleep | Two barriers and a hint: none can fault, none touches memory, and none changes a register the compiler tracks (`nomem, nostack, preserves_flags`). The SysTick reprogramming around it goes through `write_reg` / `read_reg` above. |
| `pend_switch` | `dsb; isb` after writing `PENDSVSET` | The barriers the ARM ARM requires after a write that changes exception state. |

### 2. The context switch

This is the reason the crate exists and the reason it is allowed unsafe.

| item | what it does | why it is sound |
|---|---|---|
| `init_stack` | Writes an initial exception frame | A documented `unsafe fn` (it was a safe `fn` through 0.2.1, which was unsound: safe code could hand it any address). The caller guarantees `top & !7` is one past at least 16 writable words used by nothing else. Every write is inside `aligned_top[-16 .. aligned_top]`, formed only by offsetting within that range, and the frame's layout is the architecture's: `xPSR, PC, LR, R12, R3, R2, R1, R0` high to low, then `R11..R4` below it. `fuzz/task_stacks` checks the window on every size and alignment. |
| the `PendSV` symbol (`global_asm!` in `mod pendsv`) | Saves `r4-r11`, stores the SP, calls the scheduler, restores | `r4-r11` are exactly the registers the hardware does **not** stack on exception entry, so they are the ones that must be saved by hand. The handler refuses to touch anything when `CURRENT_SP_SLOT` or the slot it names is `0`, which is the "nothing is running yet" case. |
| `kairos_pick_next` | `transmute` a `usize` to the scheduler fn pointer | The static only ever holds a value written by `set_scheduler`, whose parameter is that exact fn type; `0` is checked first as "none installed". |
| `start_first_task` | Sets PSP, switches `CONTROL.SPSEL`, enters the task | Documented `unsafe fn`: the caller must have built the stack with `init_stack` and installed a scheduler. |

## `rusty_rtos_port-riscv`

Three kinds. The first two mirror the Cortex-M crate; the third is the one
that is genuinely different, because a RISC-V trap saves nothing.

### 1. Machine register and CLINT access

| item | what it does | why it is sound |
|---|---|---|
| `mask_interrupts` / `unmask_interrupts` (behind `enter_critical` / `exit_critical`) | `csrrci` / `csrsi` on `mstatus.MIE` | A CSR write defined by the ISA. Cannot fault, touches no memory, and the nesting count is `saturating_sub` so a stray exit cannot wrap the depth and mask interrupts for ever. |
| `idle_wait` | `wfi` | A hint instruction; cannot fault, touches no memory. |
| `raise_switch` / `clear_switch_request` | `write_volatile` to `CLINT_MSIP` | A single fixed address, a module `const`, not reachable with a caller-supplied one. |
| `mcycle` / `minstret` | `csrr` | Architecturally-defined counters. No memory, no fault. |

### 2. The COOPERATIVE context switch

| item | what it does | why it is sound |
|---|---|---|
| `new_task_context` | Fills a `Context` and points `ra` at the trampoline | Writes nothing through the stack pointer: it only rounds `stack_top` down to the ABI's 16-byte alignment and records it. The arguments travel in `s1`/`s2` because a switch restores only the callee-saved set. |
| `kairos_riscv_switch` (`global_asm!` in `mod asm`) | Fourteen stores, fourteen loads, `ret` | `ra`, `sp` and `s0`–`s11` are exactly the registers a trap does not preserve. `#[repr(C)]` on `Context` is what makes the offsets a contract rather than a coincidence, and a null outgoing pointer is tested first. |
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
### The Windows backend

| item | what it does | why it is sound |
|---|---|---|
| `backend::thread_handle_of` | `JoinHandle::as_raw_handle` | Borrowed, not owned. This replaced a `DuplicateHandle` of `GetCurrentThread` called from inside the thread, which had a window: a thread that names ITSELF exists, may hold the run permit, and has no handle yet, so a tick landing in that window cannot freeze it. The `JoinHandle` is kept in `threads()` for the life of the process precisely so the handle stays valid; dropping it would close the handle and every later freeze would silently fail. |
| `backend::freeze` | `SuspendThread` | The handle came from one of the two above and names a thread of this process. The SAFETY argument that matters is not about the call but about WHEN it is made: `Ticker::interrupt` takes the critical-section lock first, and every kernel call and every heap call is inside that lock, so a thread that is not holding it is not holding a lock of ours. What that does not cover is a lock we do not own — the C runtime's allocator, or `stdout` — and the cells that use this port touch neither from a task. |
| `backend::thaw` | `ResumeThread` | As `freeze`. |
| `backend::release` | `CloseHandle` | The handle is taken out of its slot with a `swap` first, so it cannot be used again. |

### The Unix backend

The same job with signals, since a POSIX thread cannot be suspended from
outside: `SIGUSR1` asks a thread to park itself, `SIGUSR2` releases it.

| item | what it does | why it is sound |
|---|---|---|
| `backend::install_handlers` | `sigaction` for `SIGUSR1` and `SIGUSR2`, once per process | Both `sigaction` structs are zeroed, then given a real handler with C ABI and the right signature and a real mask, before `sigaction` reads them. The `SIGUSR1` handler's mask blocks `SIGUSR2`, the guard against a lost wake-up; `SA_RESTART` keeps a parked thread's interrupted syscall from seeing an `EINTR` it never asked for. The handlers touch only atomics and `sigsuspend`, which are async-signal-safe. |
| `backend::freeze` | `pthread_kill(tid, SIGUSR1)`, then waits for the thread to report `PARKED` | `tid` was registered by `thread_handle_of` from a live `JoinHandle` the port still owns, and is cleared by `release` before its slot is reused. The `RUNNING -> ASKED` compare-exchange happens BEFORE the signal, so a thread is never asked twice; a thread that never parks has its state put back, and the handler's own `ASKED -> PARKED` swap then refuses. |
| `backend::on_suspend` | The `SIGUSR1` handler: `ASKED -> PARKED`, then `sigsuspend` until the state leaves `PARKED` | Runs in the target thread, which is the point: it parks itself. It calls only `pthread_self`, `sigfillset`, `sigdelset` and `sigsuspend`, all async-signal-safe, and otherwise touches atomics. The mask it suspends with lets only `SIGUSR2` through, and the loop re-checks the state after every wake, so a stray signal cannot release it early. A thread the port did not register, or one not `ASKED`, returns at once. |
| `SavedErrno::take` / `SavedErrno::drop` / `errno_location` | Read `errno` at the top of `on_suspend` and write it back when the handler returns | Through the C runtime's own per-thread accessor (`__errno_location`, `__error`, `__errno`), which takes no argument, returns the calling thread's slot and is async-signal-safe. Added after ThreadSanitizer reported "signal handler spoils errno" on 2026-10-01: `sigsuspend` always sets `errno`, so without this a task frozen between a failing syscall and its `errno` read would see `EINTR` instead. On a Unix without a known accessor it does nothing, the old behaviour. |
| `backend::thaw` | state back to `RUNNING`, then `pthread_kill(tid, SIGUSR2)` | As `freeze` for `tid`. The state goes back FIRST, so the woken handler re-reads `RUNNING` and returns; the other order is a thread that wakes, re-reads `PARKED`, and suspends itself for ever. |

### Both

| item | what it does | why it is sound |
|---|---|---|
| `ask_scheduler` | `transmute` of a `usize` to a `Scheduler` fn pointer | The value is only ever written by `set_scheduler`, from a value of that exact type, and nothing else writes that static. Zero means "no scheduler installed" and is checked before the transmute. |

On a platform that is neither Windows nor Unix the backend has no `unsafe`
at all: it cannot freeze a thread, and says so through `PREEMPTIVE` rather
than pretending. (Through 0.2.1 this note said the same of every
non-Windows platform, which stopped being true when the Unix backend landed;
`tools/unsafe_census.py` is what found it.)

## `rusty_rtos_port-esp-radio`

**The one crate in the family whose `unsafe` is NOT fenced item by item.** It
does not inherit the workspace lints, so `unsafe_code` is not denied in it,
and nothing forced its `unsafe` into `#[expect]` fences. Found by the census
on 2026-10-01; recorded here rather than hidden, with the count pinned so a
new site fails CI until it is written up.

**Unfenced: 100 `unsafe` sites.**

It has not been published to crates.io (the 0.2.1 release left it out), and
it is the glue between this kernel and Espressif's `esp-radio-rtos-driver`
0.4.2, whose traits a closed C radio driver calls through. By file:

| file | sites | what the `unsafe` is | why it is sound, in summary |
|---|---|---|---|
| `adapter.rs` | 51 blocks, 32 `unsafe fn`, 1 `extern` | The five `esp-radio-rtos-driver` trait impls. Most of the `unsafe fn`s are REQUIRED by the trait's signatures (`SemaphoreImplementation::take`, `QueueImplementation::send_to_back`, ...). The blocks: task slots kept as `*mut TaskSlot` in a `static mut` table indexed by kernel task index, and read by the switching interrupt; heap-allocated task stacks and queue payload buffers (`alloc`/`dealloc` with the stored `Layout`); `copy_nonoverlapping` of `item_size` bytes into and out of those buffers. | Verified in the 2026-10-01 pass: every slot pointer comes from a `Box::into_raw` this module made; the table is WRITTEN inside `with_kernel`, with interrupts masked, and read elsewhere only as single aligned pointer loads on one core; a queue's buffer holds `capacity * item_size` bytes and every copy is `item_size` bytes at an index below `capacity`. **Two defects found and fixed in that pass:** a zero-capacity queue got a one-byte buffer while believing it held one `item_size` item (a heap overflow on the first push; the capacity is now clamped before the buffer is sized, and the product is `checked_mul`); and every create that failed returned `NonNull::dangling()` to a driver with no failure path, which then used it (now `exhausted()`: a halt naming what ran out). |
| `timers.rs` | 7 blocks, 1 `unsafe fn`, 2 `extern` | A fixed `static mut` table of radio timers; calling the radio's C callbacks with their data pointer. | Every table index is checked against `MAX_RADIO_TIMERS` before use; a callback is called exactly as the driver registered it, with the pointer it registered. Who may touch the table concurrently is argued at its definition, not re-audited in this pass. |
| `port.rs` | 3 blocks, 2 `unsafe fn` | Forwarding to the per-architecture `new_task_context`; on Xtensa, a `transmute` of the wrapper from `extern "C" fn(usize, usize) -> !` to `extern "C" fn(usize, usize)`. | The forwards carry the callee's own contract (documented `unsafe fn`). The transmute changes only the return type to one the callee will never see used: the wrapper never returns, and the two types share an ABI. |
| `lib.rs` | 1 block | Reading the installed `&'static dyn RadioHost` back out of its slot. | The slot is written once, from a value of that exact type, before the radio starts. |

**Residual (port threat model R-6):** fence each site with an
`#[expect(unsafe_code, reason)]` and a row here, by inheriting the workspace
lints (`[lints] workspace = true`). Not done in the 2026-10-01 pass, because
the crate is built only inside the Janus firmware, and changing its lint set
would have to be verified there.

## `rusty_rtos_port-xtensa`

The ESP32 / ESP32-S3 port. Two kinds, as on Cortex-M: register access, and
the context switch. The switch differs from the other two ports because it
does not save registers itself: `xtensa-lx-rt`'s interrupt entry has already
spilled the register windows and filled a trap frame (`Context`), and the
exit restores whatever that frame holds. Switching is replacing it.

### 1. Core register access

| item | what it does | why it is sound |
|---|---|---|
| `mask_interrupts` | `rsil` to `CRITICAL_INTLEVEL`, answering the old `PS` | One instruction reads `PS` and raises `INTLEVEL`; it touches no memory and cannot fault. Level 3, not 15: levels 4+ are the non-maskable and debug levels a critical section has no business holding off, and 3 is what esp-hal's own critical section uses. |
| `restore_interrupts` | `wsr.ps`, then `rsync` | The value came from `mask_interrupts` on this core, so it is a state this core was already in. `rsync` is the barrier the ISA requires before the new `PS` is guaranteed to govern the following instructions. |
| `mask_intlevel` | `rsr.ps` | A register read; no memory, no fault. |
| `idle_wait` | `waiti 0` | A hint: sleeps until an interrupt at any level. Cannot fault. |
| `enable_switching` | sets the switching software interrupt's bit in `INTENABLE` | One CPU-internal interrupt this port owns; its handler is the firmware's, and it does nothing until `yield_now` raises it. |
| `yield_now` / `clear_switch_request` | sets / clears that interrupt through `INTSET` / `INTCLEAR` | The same one interrupt, and nothing else. |

### 2. The context switch

| item | what it does | why it is sound |
|---|---|---|
| `new_task_context` | Writes four words below `stack_top & !15` and fills a `Context` | Documented `unsafe fn`: the caller guarantees `[top - 16, top)` is writable. Those four words are the ABI's base-save area, with the frame's own stack pointer at `top - 12`, which is what a window underflow follows when the task's first `entry` unwinds. Address arithmetic is done at pointer width and narrowed only into the register fields; doing it in `u32` wrote through a truncated address on the 64-bit host build. `fuzz/task_stacks` checks the window on every size and alignment. |
| `switch_context` | Copies the trap frame out to `current` and `next` into it | Documented `unsafe fn`: called inside the switching interrupt with the frame its handler was given; `current` and `next` are valid `Context`s. Both are `Copy` structs of `u32`s and the exit restores whatever the frame holds. The copy is `copy_nonoverlapping`, i.e. the S3's mask-ROM `memcpy`, which measured faster than every hand-written alternative (see the comment at the call). |
