# UNSAFE.md — every `unsafe` in `rusty_rtos_port`

`rusty_rtos_port-core` is `forbid(unsafe_code)` and has none. The workspace
denies `unsafe_code`; `rusty_rtos_port-cortex-m` is the one crate here that
lifts it, per-item, with `#[expect(unsafe_code, reason = "...")]` — which
means an unused fence is itself a warning, so this list cannot rot silently.

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

## What proves it, rather than what argues it

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
