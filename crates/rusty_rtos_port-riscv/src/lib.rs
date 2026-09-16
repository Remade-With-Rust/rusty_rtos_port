#![no_std]
//! `rusty_rtos_port-riscv` — the Kairos RISC-V port.
//!
//! RV32, for QEMU `virt` and for the ESP32-C6/C61/H2/P4 family.
//!
//! # Simpler than Xtensa, and not for the reason you would guess
//!
//! Xtensa's difficulty is the register windows: a task several calls deep
//! has live frames in a 64-entry file that must reach its own stack before
//! anything else runs. RISC-V has no windows, so there is nothing to spill.
//!
//! But it is NOT simply "Cortex-M with different names", and the difference
//! is worth stating because it decides the design:
//!
//! | | who saves the callee-saved registers |
//! |---|---|
//! | Cortex-M | the hardware stacks `r0-r3, r12, lr, pc, xPSR`; the port adds `r4-r11` |
//! | Xtensa | the window spill, driven by `xtensa-lx-rt`'s exception entry |
//! | **RISC-V** | **nobody — the port must do all of it** |
//!
//! A RISC-V trap saves *nothing* to the stack. `riscv-rt`'s entry saves the
//! caller-saved registers into a `TrapFrame` so a normal Rust handler can
//! run, and the callee-saved ones survive only because that handler is an
//! ordinary function that preserves them. For a context switch they belong
//! to the outgoing task and have to be written down, so this port keeps its
//! own frame: `ra`, `sp` and `s0`–`s11`.
//!
//! # The switch still happens in the trap
//!
//! The lesson from the Xtensa port holds: the decision and the register
//! swap are one step, taken where the context is already captured. A yield
//! raises the machine software interrupt through the CLINT; the handler
//! swaps and returns. [`Port::COMMITS_SWITCH`] is `true` for exactly that
//! reason — the kernel must not move `current` at the point of the yield,
//! because the registers do not move until the trap is taken.
//!
//! # Unsafe
//!
//! Every use is fenced with `#[expect(unsafe_code)]` and a justification.
//! See `UNSAFE.md`.

use core::sync::atomic::{AtomicU32, Ordering};

use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;

/// Crate version, for manifests and logs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The CLINT's `msip` register for hart 0 on QEMU `virt`.
///
/// Writing 1 raises the machine software interrupt; writing 0 clears it.
/// The ESP32-C6 family reaches the same interrupt through its own
/// peripheral, which is why this is a constant a board can override rather
/// than something baked into the switch.
pub const CLINT_MSIP: usize = 0x0200_0000;

/// How many registers the port itself saves: `ra`, `sp`, `s0`–`s11`.
pub const SAVED_REGISTERS: usize = 14;

/// The saved machine state of one task.
///
/// Only the registers a trap does NOT already preserve. `riscv-rt` captures
/// the caller-saved set into its own `TrapFrame` on the way in and restores
/// it on the way out, so duplicating it here would be two places that must
/// agree about the same bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Context {
    /// Return address — where the task resumes.
    pub ra: usize,
    /// Stack pointer.
    pub sp: usize,
    /// Callee-saved `s0`–`s11`.
    pub s: [usize; 12],
    /// `mepc` — where the task resumes when the trap exits.
    ///
    /// Only the PREEMPTIVE path uses this. A cooperative switch happens at a
    /// call site and resumes through `ra`; a preemptive one resumes through
    /// `mret`, and `mret` jumps to `mepc`.
    ///
    /// Saving it is **not optional, and that is measured rather than
    /// asserted**: the `mepc` in the CSR at the moment of a switch belongs to
    /// the task being switched OUT, so a port that leaves it alone resumes
    /// each task at the other one's address. Deleting the restore hangs
    /// `riscv32-qemu-preempt` on the spot.
    pub mepc: usize,
    /// `mstatus` — in particular `MPIE`, which `mret` copies into `MIE`.
    ///
    /// Saved and restored because a task's interrupt state is its own: a task
    /// preempted inside a critical section must come back with that section
    /// still in force.
    ///
    /// **It is NOT what makes preemption work, and an earlier version of this
    /// comment said it was.** Removing the restore and re-running
    /// `riscv32-qemu-preempt` still passes, because the trap entry has already
    /// copied the live `MIE` into `MPIE`, so `mret` re-enables interrupts
    /// without our help. The two lines that ARE load bearing are [`mepc`]
    /// below and the `mret` in `kairos_riscv_trampoline_trap`; removing
    /// either hangs the cell. Poison-proved, all three.
    ///
    /// [`mepc`]: Context::mepc
    pub mstatus: usize,
}

impl Context {
    /// An empty context, for a slot the first switch will fill in.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ra: 0,
            sp: 0,
            s: [0; 12],
            mepc: 0,
            mstatus: 0,
        }
    }
}

/// Build the saved context of a task that has never run.
///
/// `stack_top` is one past the highest usable byte; it is rounded down to
/// the 16-byte alignment the ABI requires of `sp`. When the task is first
/// switched to, `wrapper` runs with `task_fn` and `param` as its arguments.
///
/// The wrapper exists because a task body must not simply `return`: there is
/// nothing beneath it to return into.
///
/// # Safety
///
/// `stack_top` must be one past a writable stack that outlives the task.
#[expect(unsafe_code, reason = "writing the initial frame")]
#[must_use]
pub unsafe fn new_task_context(
    wrapper: extern "C" fn(task_fn: usize, param: usize) -> !,
    task_fn: usize,
    param: usize,
    stack_top: *mut u8,
) -> Context {
    // Pointer-width arithmetic, narrowed nowhere: the Xtensa port shipped a
    // version that did this in `u32` and wrote through a truncated address
    // the first time a host test called it.
    let top = (stack_top as usize) & !0xf;
    let mut context = Context::new();
    // A fresh task resumes through a TRAMPOLINE, not straight into the
    // wrapper, and the reason is the calling convention.
    //
    // The switch restores only the callee-saved set — `ra`, `sp`, `s0`-`s11`
    // — because those are the registers a trap does not preserve. A
    // `extern "C"` function reads its arguments from `a0`/`a1`, which are
    // caller-saved and therefore NOT in that set. Putting the arguments in
    // `s0`/`s1` and jumping straight to the wrapper hands it whatever
    // `a0`/`a1` happened to hold: the first version of this port did that
    // and hung on the first switch.
    //
    // So the entry point and its two arguments travel in `s0`-`s2`, and
    // three `mv` instructions put them where the ABI expects.
    context.ra = (kairos_riscv_trampoline as *const ()) as usize;
    context.sp = top;
    context.s[0] = wrapper as *const () as usize;
    context.s[1] = task_fn;
    context.s[2] = param;
    context
}

#[cfg(target_arch = "riscv32")]
#[expect(unsafe_code, reason = "the trampoline is defined in this crate's asm")]
unsafe extern "C" {
    /// Defined in this crate's assembly; see [`new_task_context`].
    fn kairos_riscv_trampoline();
    /// Defined in this crate's assembly; see [`new_task_context_preemptive`].
    fn kairos_riscv_trampoline_trap();
}

/// Off RISC-V the trampoline is a plain address for the frame arithmetic.
#[cfg(not(target_arch = "riscv32"))]
extern "C" fn kairos_riscv_trampoline() {}

/// Off RISC-V, as above.
#[cfg(not(target_arch = "riscv32"))]
extern "C" fn kairos_riscv_trampoline_trap() {}

/// Swap the running task.
///
/// Called from inside the switching trap. `current` is where the outgoing
/// task's registers are written — `None` when it is being discarded — and
/// `next` is the task to resume.
///
/// # Safety
///
/// Both pointers must be valid, and `next` must hold a context built by
/// [`new_task_context`] or written by a previous call to this.
#[cfg(target_arch = "riscv32")]
#[expect(unsafe_code, reason = "the context switch itself")]
#[inline(always)]
pub unsafe fn switch_context(current: Option<*mut Context>, next: *const Context) {
    // SAFETY: the contract above. `kairos_riscv_switch` writes the outgoing
    // registers through `a0` and loads the incoming ones from `a1`; it never
    // touches memory outside those two structs.
    unsafe {
        core::arch::asm!(
            "call kairos_riscv_switch",
            in("a0") current.unwrap_or(core::ptr::null_mut()),
            in("a1") next,
            clobber_abi("C"),
        );
    }
}

/// Off RISC-V this does nothing, so callers keep type-checking on the host.
///
/// # Safety
/// As the RISC-V version; this one is a no-op.
#[cfg(not(target_arch = "riscv32"))]
#[expect(unsafe_code, reason = "signature parity with the RISC-V version")]
#[inline(always)]
pub unsafe fn switch_context(_current: Option<*mut Context>, _next: *const Context) {}

/// Swap the running task from inside a trap, so the incoming task resumes
/// through `mret`.
///
/// # Why this is a second function and not a flag on the first
///
/// [`switch_context`] resumes a task by RETURNING to its `ra`. That is
/// exactly right at a call site, and it is what makes a cooperative yield 30
/// instructions against FreeRTOS's 83.
///
/// It is wrong inside a trap. A trap is left with `mret`, which restores
/// `MIE` from `MPIE` and jumps to `mepc`; a task entered with `ret` instead
/// keeps running in trap context with interrupts masked, and the hart takes
/// no further interrupts — the first preemptive switch becomes the last.
/// That was a real defect, witnessed by
/// `rusty_rtos_port/firmware/riscv32-qemu-preempt`.
///
/// So this one also swaps `mepc` and `mstatus`, and a fresh task leaves its
/// first trap through `mret` rather than `ret`. Keeping the two switches
/// apart means a yield still pays only for what a yield needs.
///
/// Of those three changes, poison-proving says **`mepc` and the `mret` are
/// what make it work** — remove either and the cell hangs — while `mstatus`
/// is correctness for the general case rather than the thing that was
/// broken. See [`Context::mstatus`].
///
/// # Safety
///
/// Must be called from inside a trap handler. Both pointers must be valid,
/// and `next` must hold a context built by [`new_task_context_preemptive`]
/// or written by a previous call to this.
#[cfg(target_arch = "riscv32")]
#[expect(unsafe_code, reason = "the context switch itself")]
#[inline(always)]
pub unsafe fn switch_context_trap(current: *mut Context, next: *const Context) {
    // SAFETY: the contract above.
    unsafe {
        core::arch::asm!(
            "call kairos_riscv_switch_trap",
            in("a0") current,
            in("a1") next,
            clobber_abi("C"),
        );
    }
}

/// Off RISC-V this does nothing, so callers keep type-checking on the host.
///
/// # Safety
/// As the RISC-V version; this one is a no-op.
#[cfg(not(target_arch = "riscv32"))]
#[expect(unsafe_code, reason = "signature parity with the RISC-V version")]
#[inline(always)]
pub unsafe fn switch_context_trap(_current: *mut Context, _next: *const Context) {}

/// `mstatus` for a task that has never run: `MPIE` set so `mret` enables
/// interrupts, and `MPP` = M-mode so it returns to the privilege it left.
///
/// FreeRTOS computes the same value (`0x188 << 4`) in
/// `pxPortInitialiseStack`, ORing it into the live `mstatus` to preserve the
/// FPU and VPU dirty bits. This port has no FPU save, so there is nothing to
/// preserve and the constant stands alone.
pub const FRESH_MSTATUS: usize = 0x1880;

/// Build the saved context of a task that has never run, for the PREEMPTIVE
/// path.
///
/// The difference from [`new_task_context`] is where the task resumes from.
/// There, the first switch `ret`s into a trampoline and falls straight into
/// the wrapper. Here the first switch happens inside a trap, so the task must
/// leave through `mret` like any preempted task would — otherwise it runs
/// with interrupts masked for ever.
///
/// So `mepc` is the wrapper and `mstatus` carries `MPIE`, and the trampoline
/// this context returns to does nothing but move the two arguments into
/// place and `mret`.
///
/// # Safety
///
/// `stack_top` must be one past a writable stack that outlives the task.
#[expect(unsafe_code, reason = "writing the initial frame")]
#[must_use]
pub unsafe fn new_task_context_preemptive(
    wrapper: extern "C" fn(task_fn: usize, param: usize) -> !,
    task_fn: usize,
    param: usize,
    stack_top: *mut u8,
) -> Context {
    let mut context = Context::new();
    context.ra = (kairos_riscv_trampoline_trap as *const ()) as usize;
    context.sp = (stack_top as usize) & !0xf;
    // The arguments travel in `s1`/`s2` for the same reason as the
    // cooperative path: a switch restores only the callee-saved set, and a
    // `extern "C"` function reads its arguments from the caller-saved one.
    context.s[1] = task_fn;
    context.s[2] = param;
    context.mepc = (wrapper as *const ()) as usize;
    context.mstatus = FRESH_MSTATUS;
    context
}

/// The switch itself.
#[cfg(target_arch = "riscv32")]
#[expect(unsafe_code, reason = "the context switch itself")]
mod asm {
    core::arch::global_asm!(
        ".section .text.kairos_riscv_switch,\"ax\"",
        ".align 2",
        ".global kairos_riscv_switch",
        ".type kairos_riscv_switch, @function",
        "kairos_riscv_switch:",
        // a0 = *mut Context for the outgoing task, or null.
        // a1 = *const Context for the incoming task.
        //
        // Fourteen words, in the order `Context` declares them: ra, sp,
        // then s0-s11. `#[repr(C)]` is what makes that order a contract
        // rather than a coincidence.
        "  beqz  a0, 2f",
        "  sw    ra, 0(a0)",
        "  sw    sp, 4(a0)",
        "  sw    s0, 8(a0)",
        "  sw    s1, 12(a0)",
        "  sw    s2, 16(a0)",
        "  sw    s3, 20(a0)",
        "  sw    s4, 24(a0)",
        "  sw    s5, 28(a0)",
        "  sw    s6, 32(a0)",
        "  sw    s7, 36(a0)",
        "  sw    s8, 40(a0)",
        "  sw    s9, 44(a0)",
        "  sw    s10, 48(a0)",
        "  sw    s11, 52(a0)",
        "2:",
        "  lw    ra, 0(a1)",
        "  lw    sp, 4(a1)",
        "  lw    s0, 8(a1)",
        "  lw    s1, 12(a1)",
        "  lw    s2, 16(a1)",
        "  lw    s3, 20(a1)",
        "  lw    s4, 24(a1)",
        "  lw    s5, 28(a1)",
        "  lw    s6, 32(a1)",
        "  lw    s7, 36(a1)",
        "  lw    s8, 40(a1)",
        "  lw    s9, 44(a1)",
        "  lw    s10, 48(a1)",
        "  lw    s11, 52(a1)",
        "  ret",
        ".size kairos_riscv_switch, . - kairos_riscv_switch",
        // A fresh task lands here rather than in its entry point, because
        // the switch restores callee-saved registers and the C ABI passes
        // arguments in caller-saved ones. See `new_task_context`.
        ".section .text.kairos_riscv_trampoline,\"ax\"",
        ".align 2",
        ".global kairos_riscv_trampoline",
        ".type kairos_riscv_trampoline, @function",
        "kairos_riscv_trampoline:",
        "  mv    a0, s1", // task_fn
        "  mv    a1, s2", // param
        "  jr    s0",     // the wrapper, which never returns
        ".size kairos_riscv_trampoline, . - kairos_riscv_trampoline",
        // ------------------------------------------------------------------
        // The PREEMPTIVE switch. Same fourteen registers, plus the two CSRs
        // that decide how the trap is left: `mepc` (where) and `mstatus`
        // (with what interrupt state). Without those two a task entered from
        // a trap runs with interrupts masked for ever -- which is exactly
        // the defect `riscv32-qemu-preempt` witnessed.
        //
        // `t0` is scratch. It is caller-saved, so `riscv-rt`'s trap epilogue
        // reloads it from the INCOMING task's frame on the way out; clobbering
        // it after `sp` has moved is therefore safe.
        ".section .text.kairos_riscv_switch_trap,\"ax\"",
        ".align 2",
        ".global kairos_riscv_switch_trap",
        ".type kairos_riscv_switch_trap, @function",
        "kairos_riscv_switch_trap:",
        "  sw    ra, 0(a0)",
        "  sw    sp, 4(a0)",
        "  sw    s0, 8(a0)",
        "  sw    s1, 12(a0)",
        "  sw    s2, 16(a0)",
        "  sw    s3, 20(a0)",
        "  sw    s4, 24(a0)",
        "  sw    s5, 28(a0)",
        "  sw    s6, 32(a0)",
        "  sw    s7, 36(a0)",
        "  sw    s8, 40(a0)",
        "  sw    s9, 44(a0)",
        "  sw    s10, 48(a0)",
        "  sw    s11, 52(a0)",
        "  csrr  t0, mepc",
        "  sw    t0, 56(a0)",
        "  csrr  t0, mstatus",
        "  sw    t0, 60(a0)",
        "  lw    ra, 0(a1)",
        "  lw    sp, 4(a1)",
        "  lw    s0, 8(a1)",
        "  lw    s1, 12(a1)",
        "  lw    s2, 16(a1)",
        "  lw    s3, 20(a1)",
        "  lw    s4, 24(a1)",
        "  lw    s5, 28(a1)",
        "  lw    s6, 32(a1)",
        "  lw    s7, 36(a1)",
        "  lw    s8, 40(a1)",
        "  lw    s9, 44(a1)",
        "  lw    s10, 48(a1)",
        "  lw    s11, 52(a1)",
        "  lw    t0, 56(a1)",
        "  csrw  mepc, t0",
        "  lw    t0, 60(a1)",
        "  csrw  mstatus, t0",
        "  ret",
        ".size kairos_riscv_switch_trap, . - kairos_riscv_switch_trap",
        // A task that has never run cannot `ret` into `_start_trap_rust`:
        // there is no frame of that function on its stack to return into.
        // So it leaves the trap here instead, which is also what lets it
        // pick up the interrupt state `mstatus` carries.
        ".section .text.kairos_riscv_trampoline_trap,\"ax\"",
        ".align 2",
        ".global kairos_riscv_trampoline_trap",
        ".type kairos_riscv_trampoline_trap, @function",
        "kairos_riscv_trampoline_trap:",
        "  mv    a0, s1", // task_fn
        "  mv    a1, s2", // param
        "  mret",         // -> mepc, which is the wrapper
        ".size kairos_riscv_trampoline_trap, . - kairos_riscv_trampoline_trap",
    );
}

// -------------------------------------------------------------- counters --

/// Read `mcycle`, the architectural cycle counter.
///
/// This is the register the Cortex-M does not have under QEMU — DWT is
/// unimplemented there, which is why the mission plan says cycle rows come
/// from silicon. RISC-V defines `mcycle` in the ISA, so whether it means
/// anything under an emulator is a question to MEASURE rather than assume,
/// and the switch cell measures it.
#[cfg(target_arch = "riscv32")]
#[must_use]
pub fn mcycle() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: reads two CSRs; touches no memory and cannot fault at
    // machine level.
    #[expect(unsafe_code, reason = "reading a counter CSR")]
    unsafe {
        core::arch::asm!("csrr {0}, mcycle", "csrr {1}, mcycleh", out(reg) lo, out(reg) hi, options(nomem, nostack));
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// `minstret`: instructions retired. A *work* count rather than a clock,
/// which is the instrument this family prefers.
#[cfg(target_arch = "riscv32")]
#[must_use]
pub fn minstret() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: as `mcycle`.
    #[expect(unsafe_code, reason = "reading a counter CSR")]
    unsafe {
        core::arch::asm!("csrr {0}, minstret", "csrr {1}, minstreth", out(reg) lo, out(reg) hi, options(nomem, nostack));
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// Off RISC-V there is no `mcycle`; the signature is kept so callers
/// type-check on the host.
#[cfg(not(target_arch = "riscv32"))]
#[must_use]
pub fn mcycle() -> u64 {
    0
}

/// As [`mcycle`].
#[cfg(not(target_arch = "riscv32"))]
#[must_use]
pub fn minstret() -> u64 {
    0
}

// ------------------------------------------------------------- the port --

/// The Kairos RISC-V port.
#[derive(Debug, Default)]
pub struct RiscvPort {
    nesting: AtomicU32,
    /// Whether `mstatus.MIE` was set when the outermost section began.
    was_enabled: AtomicU32,
    yields: AtomicU32,
    ticks: AtomicU32,
    exits: AtomicU32,
}

impl RiscvPort {
    /// A port with nothing counted yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            nesting: AtomicU32::new(0),
            was_enabled: AtomicU32::new(0),
            yields: AtomicU32::new(0),
            ticks: AtomicU32::new(0),
            exits: AtomicU32::new(0),
        }
    }

    /// Ticks delivered so far.
    #[must_use]
    pub fn tick_count(&self) -> u64 {
        u64::from(self.ticks.load(Ordering::Relaxed))
    }

    /// Count one tick. A firmware calls this from its timer trap.
    pub fn note_tick(&self) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
    }

    /// `portYIELD()` calls counted.
    #[must_use]
    pub fn yield_count(&self) -> u64 {
        u64::from(self.yields.load(Ordering::Relaxed))
    }
}

/// Clear `mstatus.MIE`, answering whether it had been set.
#[cfg(target_arch = "riscv32")]
#[inline(always)]
fn mask_interrupts() -> bool {
    let old: usize;
    // SAFETY: clears the global interrupt-enable bit and reports its old
    // value in one instruction; touches no memory.
    #[expect(unsafe_code, reason = "the critical section is an mstatus write")]
    unsafe {
        core::arch::asm!("csrrci {0}, mstatus, 8", out(reg) old, options(nomem, nostack));
    }
    old & 8 != 0
}

/// Set `mstatus.MIE`.
#[cfg(target_arch = "riscv32")]
#[inline(always)]
fn unmask_interrupts() {
    // SAFETY: sets the global interrupt-enable bit; touches no memory.
    #[expect(unsafe_code, reason = "the critical section is an mstatus write")]
    unsafe {
        core::arch::asm!("csrsi mstatus, 8", options(nomem, nostack));
    }
}

#[cfg(not(target_arch = "riscv32"))]
#[inline(always)]
fn mask_interrupts() -> bool {
    false
}

#[cfg(not(target_arch = "riscv32"))]
#[inline(always)]
fn unmask_interrupts() {}

/// Raise the machine software interrupt.
#[cfg(target_arch = "riscv32")]
#[expect(unsafe_code, reason = "raising an interrupt is a device write")]
#[inline]
pub fn raise_switch() {
    // SAFETY: `CLINT_MSIP` is the CLINT's software-interrupt register for
    // hart 0 on this machine; writing 1 raises it and nothing else.
    unsafe {
        (CLINT_MSIP as *mut u32).write_volatile(1);
    }
}

/// Clear the machine software interrupt. The handler must do this first.
#[cfg(target_arch = "riscv32")]
#[expect(unsafe_code, reason = "clearing an interrupt is a device write")]
#[inline]
pub fn clear_switch_request() {
    // SAFETY: as `raise_switch`.
    unsafe {
        (CLINT_MSIP as *mut u32).write_volatile(0);
    }
}

/// Off RISC-V there is no CLINT and nothing to raise.
#[cfg(not(target_arch = "riscv32"))]
#[inline]
pub fn raise_switch() {}

/// As [`raise_switch`].
#[cfg(not(target_arch = "riscv32"))]
#[inline]
pub fn clear_switch_request() {}

impl Port for RiscvPort {
    /// This port SWITCHES STACKS, so the kernel must not commit a switch at
    /// the point of the yield — the registers do not move until the trap is
    /// taken. See the Xtensa port for what happens when it does.
    const COMMITS_SWITCH: bool = true;

    fn yield_now(&self) {
        // No counter here: `Kernel::port_yield` calls `count_yield` just
        // before this, and counting in both places doubles every yield.
        raise_switch();
    }

    fn yield_from_isr(&self, woken: Woken) {
        if woken == Woken::YES {
            raise_switch();
        }
    }

    fn enter_critical(&self) {
        let was = mask_interrupts();
        // Only the OUTERMOST section's state is kept: an inner mask reports
        // interrupts already off, and restoring that on the way out would
        // leave them off for good.
        if self.nesting.fetch_add(1, Ordering::Relaxed) == 0 {
            self.was_enabled.store(u32::from(was), Ordering::Relaxed);
        }
    }

    fn exit_critical(&self) {
        let n = self.nesting.load(Ordering::Relaxed).saturating_sub(1);
        self.nesting.store(n, Ordering::Relaxed);
        if n == 0 {
            self.exits.fetch_add(1, Ordering::Relaxed);
            if self.was_enabled.load(Ordering::Relaxed) != 0 {
                unmask_interrupts();
            }
        }
    }

    fn set_interrupt_mask_from_isr(&self) -> u32 {
        u32::from(mask_interrupts())
    }

    fn clear_interrupt_mask_from_isr(&self, saved: u32) {
        if saved != 0 {
            unmask_interrupts();
        }
    }

    fn in_isr(&self) -> bool {
        // RISC-V has no equivalent of ARM's `IPSR`: a hart in a trap looks
        // like a hart with interrupts masked. This is therefore weaker than
        // the Cortex-M answer -- a task inside a critical section reads
        // `true` -- and it is safe in the direction that matters, because a
        // task taking the from-ISR half of an API is sound where the
        // reverse would not be.
        self.nesting.load(Ordering::Relaxed) != 0
    }

    fn count_tick(&self) {
        self.note_tick();
    }

    fn count_yield(&self) {
        self.yields.fetch_add(1, Ordering::Relaxed);
    }

    fn exits(&self) -> u64 {
        u64::from(self.exits.load(Ordering::Relaxed))
    }

    fn idle(&self) {
        idle_wait();
    }
}

/// `wfi`: sleep until the next interrupt.
#[cfg(target_arch = "riscv32")]
#[expect(unsafe_code, reason = "the idle instruction")]
#[inline]
fn idle_wait() {
    // SAFETY: `wfi` is a hint; it cannot fault.
    unsafe {
        core::arch::asm!("wfi", options(nomem, nostack));
    }
}

#[cfg(not(target_arch = "riscv32"))]
#[inline]
fn idle_wait() {}
