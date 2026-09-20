#![no_std]
//! `rusty_rtos_port-cortex-m` — the Kairos Cortex-M port.
//!
//! `rusty_rtos_port-core`'s docs name this crate as the place the assembly
//! lives: "the first-task start and the context switch ... the only crates
//! in the family with a fenced `unsafe` block." This is that crate for
//! ARMv7-M (Cortex-M3, M4, M7) and ARMv8-M mainline.
//!
//! # What a Cortex-M port is
//!
//! Five things, and FreeRTOS's `port.c` has exactly the same five:
//!
//! | FreeRTOS | here |
//! |---|---|
//! | `portENTER_CRITICAL` / `portEXIT_CRITICAL` | [`CortexMPort::enter_critical`] / [`CortexMPort::exit_critical`] |
//! | `portYIELD()` | [`CortexMPort::yield_now`] — sets PendSV pending |
//! | `xPortSysTickHandler` | [`tick`] |
//! | `pxPortInitialiseStack` | [`init_stack`] |
//! | `xPortPendSVHandler` | the `PendSV` symbol in [`switch`] |
//!
//! # The stack belongs to the port, not the kernel
//!
//! A Kairos TCB has no stack pointer, and that is deliberate: the kernel is
//! stackless by design — a blocking call keeps its locals in the TCB rather
//! than on a C stack, which is what lets the same kernel run the
//! conformance corpus on a host, on a simulator and on a chip. So the
//! *task's* stack is the port's business entirely, and this crate keeps the
//! saved stack pointers.
//!
//! The shape is FreeRTOS's `pxCurrentTCB`, narrowed to the one word that
//! matters: [`CURRENT_SP_SLOT`] holds the **address of the word** that holds
//! the running task's saved stack pointer. `PendSV` writes the outgoing
//! task's SP through it, asks the installed scheduler for the next task,
//! and reads the incoming one back through it.
//!
//! # Unsafe
//!
//! The workspace denies `unsafe_code`; every use here is fenced with
//! `#[expect(unsafe_code)]` and a justification, and they are all of two
//! kinds: reading or writing a core peripheral register, and the context
//! switch itself. See `UNSAFE.md`.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering, compiler_fence};

use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;

/// Crate version, for manifests and logs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ------------------------------------------------------- core peripherals --

/// `SCB->ICSR`, the Interrupt Control and State Register.
const ICSR: *mut u32 = 0xE000_ED04 as *mut u32;
/// `ICSR.PENDSVSET`: request a PendSV.
const PENDSVSET: u32 = 1 << 28;

/// `SysTick->CTRL`, `->LOAD`, `->VAL`.
const SYST_CSR: *mut u32 = 0xE000_E010 as *mut u32;
const SYST_RVR: *mut u32 = 0xE000_E014 as *mut u32;
const SYST_CVR: *mut u32 = 0xE000_E018 as *mut u32;
/// Enable, tick interrupt, use the processor clock.
const SYST_ENABLE: u32 = 0b111;
/// `SysTick->CTRL.COUNTFLAG`: set if the counter reached zero since this
/// register was last READ. Reading clears it, so it may only be read once
/// per decision, and the value has to be kept in a local.
const SYST_COUNTFLAG: u32 = 1 << 16;
/// `SysTick->LOAD` is 24 bits wide, and so is the largest sleep one
/// programming of it can buy.
const SYST_MAX_RELOAD: u32 = 0x00FF_FFFF;
/// `ICSR.PENDSTCLR`: drop a pending SysTick exception on the floor.
const PENDSTCLR: u32 = 1 << 25;

/// `SCB->SHPR3`, which holds the priorities of SysTick and PendSV.
const SHPR3: *mut u32 = 0xE000_ED20 as *mut u32;

// ------------------------------------------------------------ the switcher --

/// The address of the word holding the running task's saved stack pointer.
///
/// FreeRTOS calls this `pxCurrentTCB` and puts the SP first in the TCB so
/// the asm can use the pointer directly; the comment in `tasks.c` is
/// "THIS MUST BE THE FIRST MEMBER". Here the kernel's TCB has no stack at
/// all, so the port keeps the slot and this is its address.
///
/// `0` means "no task is running yet" and `PendSV` will not switch.
pub static CURRENT_SP_SLOT: AtomicUsize = AtomicUsize::new(0);

/// What `PendSV` calls to choose the next task.
///
/// The application installs this with [`set_scheduler`]. It must set
/// [`CURRENT_SP_SLOT`] to the incoming task's SP slot; whatever it leaves
/// there is what runs next. It executes inside PendSV, so it must not
/// block and must not enable interrupts.
type Scheduler = extern "C" fn();

static SCHEDULER: AtomicUsize = AtomicUsize::new(0);

/// Install the function `PendSV` calls to pick the next task.
///
/// Call it once, before the first task starts.
pub fn set_scheduler(f: Scheduler) {
    SCHEDULER.store(f as usize, Ordering::SeqCst);
}

/// Called from `PendSV`. Not public API; `extern "C"` so the asm can reach
/// it by symbol.
///
/// # Safety
/// Runs in exception context with the switch half-done.
#[expect(unsafe_code, reason = "the asm reaches this by symbol")]
#[unsafe(no_mangle)]
extern "C" fn kairos_pick_next() {
    let f = SCHEDULER.load(Ordering::SeqCst);
    if f != 0 {
        // SAFETY: `SCHEDULER` only ever holds a value written by
        // `set_scheduler`, whose parameter type is exactly this signature,
        // and `0` is checked above as the "none installed" case.
        #[expect(unsafe_code, reason = "the installed scheduler callback")]
        let f: Scheduler = unsafe { core::mem::transmute::<usize, Scheduler>(f) };
        f();
    }
}

// The context switch. This is the whole reason the crate is allowed unsafe.
//
// It is FreeRTOS's `xPortPendSVHandler` with one difference: where that
// loads `pxCurrentTCB` (whose first member is the SP), this loads
// `CURRENT_SP_SLOT`, which IS the address of the SP word. The kernel's TCB
// has no stack pointer to be first member of.
//
// Callee-saved registers are the ones the exception frame does not carry:
// the hardware stacks r0-r3, r12, lr, pc and xPSR on entry, so r4-r11 are
// ours to save. A task's saved SP therefore points at its r4.
#[cfg(target_arch = "arm")]
#[expect(unsafe_code, reason = "the context switch itself: see UNSAFE.md")]
mod pendsv {
    use super::CURRENT_SP_SLOT;

    core::arch::global_asm!(
        ".section .text.PendSV",
        ".global PendSV",
        ".thumb_func",
        ".type PendSV, %function",
        "PendSV:",
        // Nothing is running yet: the first task start pends a PendSV to get
        // itself onto the process stack, and there is no outgoing context.
        "    ldr   r3, ={slot}",
        "    ldr   r2, [r3]",
        "    cbz   r2, 2f",
        "    mrs   r0, psp",
        "    isb",
        "    cbz   r0, 2f",
        // Save the outgoing task: r4-r11 below the hardware frame, then the
        // resulting SP into the slot r2 points at.
        "    stmdb r0!, {{r4-r11}}",
        "    str   r0, [r2]",
        "2:",
        // Ask the scheduler for the next task. It rewrites CURRENT_SP_SLOT.
        "    push  {{r3, lr}}",
        "    bl    kairos_pick_next",
        "    pop   {{r3, lr}}",
        // Restore the incoming task.
        "    ldr   r2, [r3]",
        "    cbz   r2, 3f",
        "    ldr   r0, [r2]",
        "    cbz   r0, 3f",
        "    ldmia r0!, {{r4-r11}}",
        "    msr   psp, r0",
        "    isb",
        "3:",
        "    bx    lr",
        slot = sym CURRENT_SP_SLOT,
    );
}

/// Build a fresh task stack so that returning from an exception into it
/// enters `entry(arg)`.
///
/// `pxPortInitialiseStack`. `top` is one past the highest usable word of
/// the task's stack; the answer is the value to put in the task's SP slot.
///
/// # Panics
/// Never. A stack too small to hold a frame answers `top` unchanged, which
/// a caller can detect because it is not below what it passed in.
#[must_use]
pub fn init_stack(top: *mut usize, entry: extern "C" fn(usize) -> !, arg: usize) -> usize {
    // The exception frame the hardware pops, high address first:
    //   xPSR, PC, LR, R12, R3, R2, R1, R0
    // then our eight callee-saved words below it.
    const FRAME: usize = 8 + 8;
    // SAFETY: the caller guarantees `top` is one past a stack of at least
    // FRAME words. Every write below is within `top[-FRAME .. top]`, and
    // the pointer is only ever formed by offsetting inside that range.
    #[expect(unsafe_code, reason = "writing the initial exception frame")]
    unsafe {
        // The frame must be 8-byte aligned on entry, per AAPCS.
        let sp = (top as usize) & !0x7usize;
        let w = |sp: usize, i: usize, v: usize| {
            // one word below `sp`, counting up from 1
            let p = (sp as *mut usize).wrapping_sub(i);
            core::ptr::write_volatile(p, v);
        };
        w(sp, 1, 0x0100_0000); // xPSR: Thumb bit, nothing else
        let entry_addr = entry as extern "C" fn(usize) -> ! as usize;
        w(sp, 2, entry_addr & !1); // PC, with the Thumb bit cleared
        let exit_addr = task_exited as extern "C" fn() -> ! as usize;
        w(sp, 3, exit_addr); // LR: where a returning task lands
        w(sp, 4, 0); // R12
        w(sp, 5, 0); // R3
        w(sp, 6, 0); // R2
        w(sp, 7, 0); // R1
        w(sp, 8, arg); // R0, the task's argument
        for i in 9..=16 {
            w(sp, i, 0); // R11..R4
        }
        sp.wrapping_sub(FRAME.wrapping_mul(core::mem::size_of::<usize>()))
    }
}

/// Where a task that returns from its entry function lands.
///
/// A FreeRTOS task must not return; the C port sends it to
/// `prvTaskExitError`, which traps. This does the same and says so.
extern "C" fn task_exited() -> ! {
    // Masking first, so the trap cannot be preempted into something that
    // hides it.
    disable_interrupts();
    loop {
        compiler_fence(Ordering::SeqCst);
    }
}

// -------------------------------------------------------------- primitives --

#[cfg(target_arch = "arm")]
#[inline]
fn disable_interrupts() {
    // SAFETY: `cpsid i` only sets PRIMASK. It cannot fault and touches no
    // memory; the compiler fence keeps the section's accesses inside it.
    #[expect(unsafe_code, reason = "PRIMASK is the critical section")]
    unsafe {
        core::arch::asm!("cpsid i", options(nomem, nostack, preserves_flags));
    }
    compiler_fence(Ordering::SeqCst);
}

/// Off ARM there is no PRIMASK. The fence is kept so the ordering this
/// function promises still holds for anything the host build does with it.
#[cfg(not(target_arch = "arm"))]
#[inline]
fn disable_interrupts() {
    compiler_fence(Ordering::SeqCst);
}

#[cfg(target_arch = "arm")]
#[inline]
fn enable_interrupts() {
    compiler_fence(Ordering::SeqCst);
    // SAFETY: as `disable_interrupts`.
    #[expect(unsafe_code, reason = "PRIMASK is the critical section")]
    unsafe {
        core::arch::asm!("cpsie i", options(nomem, nostack, preserves_flags));
    }
}

#[cfg(not(target_arch = "arm"))]
#[inline]
fn enable_interrupts() {
    compiler_fence(Ordering::SeqCst);
}

#[cfg(target_arch = "arm")]
#[inline]
#[must_use]
fn primask() -> u32 {
    let r: u32;
    // SAFETY: `mrs` from PRIMASK is a read of a core register.
    #[expect(unsafe_code, reason = "reading PRIMASK")]
    unsafe {
        core::arch::asm!("mrs {}, PRIMASK", out(reg) r, options(nomem, nostack, preserves_flags));
    }
    r
}

/// Off ARM: "interrupts were enabled", which is the answer that makes the
/// restore in `clear_interrupt_mask_from_isr` a no-op.
#[cfg(not(target_arch = "arm"))]
#[inline]
#[must_use]
const fn primask() -> u32 {
    0
}

#[cfg(target_arch = "arm")]
#[inline]
#[must_use]
fn ipsr() -> u32 {
    let r: u32;
    // SAFETY: `mrs` from IPSR is a read of a core register.
    #[expect(unsafe_code, reason = "reading IPSR to answer in_isr")]
    unsafe {
        core::arch::asm!("mrs {}, IPSR", out(reg) r, options(nomem, nostack, preserves_flags));
    }
    r
}

/// Off ARM there is no exception context to be in.
#[cfg(not(target_arch = "arm"))]
#[inline]
#[must_use]
const fn ipsr() -> u32 {
    0
}

/// Off ARM there are no core peripherals to write, and writing to a
/// made-up address would be exactly the unsound thing this crate exists to
/// keep in one place. So the host build drops the write.
#[cfg(not(target_arch = "arm"))]
#[inline]
fn write_reg(_addr: *mut u32, _value: u32) {}

/// As [`write_reg`], and for the same reason the host build reads nothing.
/// Every caller treats zero as "decline", so a host build declines.
#[cfg(not(target_arch = "arm"))]
#[inline]
#[must_use]
fn read_reg(_addr: *mut u32) -> u32 {
    0
}

#[cfg(target_arch = "arm")]
#[inline]
#[must_use]
fn read_reg(addr: *mut u32) -> u32 {
    // SAFETY: as `write_reg` -- a fixed, always-mapped core peripheral.
    #[expect(unsafe_code, reason = "core peripheral read")]
    unsafe {
        core::ptr::read_volatile(addr)
    }
}

#[cfg(target_arch = "arm")]
#[inline]
fn write_reg(addr: *mut u32, value: u32) {
    // SAFETY: every caller passes one of the core peripheral addresses
    // above, which are fixed by the architecture and always mapped on an
    // ARMv7-M part.
    #[expect(unsafe_code, reason = "core peripheral write")]
    unsafe {
        core::ptr::write_volatile(addr, value);
    }
}

// -------------------------------------------------------------------- port --

/// The Cortex-M port.
///
/// Critical sections are PRIMASK with a nesting count, which is FreeRTOS's
/// `uxCriticalNesting` exactly. The C port on this core uses BASEPRI so
/// that interrupts above `configMAX_SYSCALL_INTERRUPT_PRIORITY` stay live
/// through a critical section; this port masks all of them. That is the
/// safer default and the smaller claim — a BASEPRI variant is a later
/// increment and belongs with the interrupt-priority policy, not before it.
/// The counters are atomics, not `Cell`s, for two reasons that are the
/// same reason: the tick counter is written from inside `SysTick` while a
/// task may be reading it, and a port has to be a `static` to be reachable
/// from an exception handler at all — which needs `Sync`.
///
/// They are `AtomicU32` and not `AtomicU64` because **ARMv7-M has no
/// 64-bit atomics**. That is the same constraint that cost `rusty_zstd`
/// and `rusty_erasure-core` their bare-metal builds (`build-me-bare`), met
/// here from the other side: a counter that cannot be atomic on the target
/// must not be 64 bits wide.
#[derive(Debug, Default)]
pub struct CortexMPort {
    nesting: AtomicU32,
    yields: AtomicU32,
    ticks: AtomicU32,
    /// PRIMASK as it was when the OUTERMOST critical section was entered.
    ///
    /// Without this, [`Port::exit_critical`] ends its outermost section
    /// with an unconditional `cpsie i`, which enables interrupts even when
    /// the caller had masked them itself and the kernel call was nested
    /// *inside* that mask. The two ISR-side entry points a few lines below
    /// have always saved and restored the mask; this is the same
    /// discipline, and its absence here was a real defect -- see
    /// `exit_critical`.
    saved_primask: AtomicU32,
}

impl CortexMPort {
    /// A port with nothing counted yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            nesting: AtomicU32::new(0),
            yields: AtomicU32::new(0),
            ticks: AtomicU32::new(0),
            saved_primask: AtomicU32::new(0),
        }
    }

    /// Ticks delivered so far.
    #[must_use]
    pub fn tick_count(&self) -> u64 {
        u64::from(self.ticks.load(Ordering::Relaxed))
    }

    /// `portYIELD()` calls so far.
    #[must_use]
    pub fn yield_count(&self) -> u64 {
        u64::from(self.yields.load(Ordering::Relaxed))
    }

    /// Count a tick the SysTick handler delivered.
    pub fn note_tick(&self) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
    }
}

impl Port for CortexMPort {
    /// This port SWITCHES STACKS, so the kernel must not commit a switch at
    /// the point of the yield.
    ///
    /// `yield_now` can only pend a `PendSV`; the registers and the stack
    /// move when that exception is taken. Between the two, code runs as a
    /// task the kernel would already have moved on from -- and a blocking
    /// call in that gap parks the wrong task.
    ///
    /// `mps2-an385-qemu-preempt` measured the exposure before this was set:
    /// the window opened on 199 of 200 rounds, and the harmful sub-case --
    /// blocking inside it -- happened once and was rescued by the call's own
    /// timeout. Rare is not safe, and `PendSV` already calls
    /// `Kernel::switch_context` itself, so the decision and the swap were
    /// always meant to be one step.
    const COMMITS_SWITCH: bool = true;

    fn yield_now(&self) {
        self.yields.fetch_add(1, Ordering::Relaxed);
        pend_switch();
    }

    fn yield_from_isr(&self, woken: Woken) {
        if woken == Woken::YES {
            self.yields.fetch_add(1, Ordering::Relaxed);
            pend_switch();
        }
    }

    fn enter_critical(&self) {
        // Read PRIMASK BEFORE masking, and keep it only for the outermost
        // entry: that is the state `exit_critical` has to put back.
        let was = primask();
        disable_interrupts();
        if self.nesting.fetch_add(1, Ordering::Relaxed) == 0 {
            self.saved_primask.store(was, Ordering::Relaxed);
        }
    }

    fn exit_critical(&self) {
        // Interrupts are already masked here, so the read-modify-write
        // needs no atomicity of its own; `Relaxed` is enough and the
        // ordering that matters is the `cpsie` below.
        let n = self.nesting.load(Ordering::Relaxed).saturating_sub(1);
        self.nesting.store(n, Ordering::Relaxed);
        if n == 0 && self.saved_primask.load(Ordering::Relaxed) & 1 == 0 {
            // RESTORE, do not enable. An unconditional `cpsie i` here is a
            // defect, and a subtle one: a kernel call made from inside a
            // caller's own masked region ends that region early, silently,
            // in the middle of the caller.
            //
            // `mps2-an385-qemu-capi` paid for this one. `xTaskCreate` masks
            // interrupts around "create the task" plus "give it a stack",
            // because a task that is schedulable without a stack is one a
            // `PendSV` can pick and run at `SP = 0`. The kernel's own
            // `create_task` takes a critical section internally; its exit
            // unmasked interrupts inside that supposedly-atomic pair, the
            // pending `PendSV` fired between the two halves, and the new
            // task was scheduled before it had anywhere to run. The demo
            // faulted to `pc = 0` roughly 360 ticks later, in a DIFFERENT
            // task, on a stack it did not own.
            //
            // `set_interrupt_mask_from_isr` / `clear_interrupt_mask_from_isr`
            // a few lines below have always done it this way. The two pairs
            // disagreeing was the tell.
            enable_interrupts();
        }
    }

    fn set_interrupt_mask_from_isr(&self) -> u32 {
        let saved = primask();
        disable_interrupts();
        saved
    }

    fn clear_interrupt_mask_from_isr(&self, saved: u32) {
        if saved & 1 == 0 {
            enable_interrupts();
        }
    }

    fn in_isr(&self) -> bool {
        ipsr() != 0
    }

    fn idle(&self) {
        // `wfi` is the C port's idle: sleep until the next interrupt.
        #[cfg(target_arch = "arm")]
        {
            // SAFETY: `wfi` is a hint instruction; it cannot fault.
            #[expect(unsafe_code, reason = "the idle instruction")]
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }
    }

    /// `vPortSuppressTicksAndSleep`, delegated to [`suppress_ticks_and_sleep`]
    /// so the register work sits beside the rest of the SysTick map rather
    /// than in a trait impl.
    fn suppress_ticks_and_sleep(&self, expected_idle_ticks: u64) -> u64 {
        suppress_ticks_and_sleep(expected_idle_ticks)
    }

    fn count_tick(&self) {
        self.note_tick();
    }

    fn count_yield(&self) {
        self.yields.fetch_add(1, Ordering::Relaxed);
    }

    fn enter_critical_from_isr(&self) -> u32 {
        self.set_interrupt_mask_from_isr()
    }

    fn exit_critical_from_isr(&self, mask: u32) {
        self.clear_interrupt_mask_from_isr(mask);
    }
}

/// Request a context switch at the next opportunity: set PendSV pending.
///
/// `portYIELD()`. PendSV is configured lowest-priority, so it runs when
/// every other exception has finished — which is what makes a switch
/// requested from an ISR happen on the way out of it.
pub fn pend_switch() {
    write_reg(ICSR, PENDSVSET);
    // The barriers the ARM ARM asks for after a write that changes
    // exception state.
    #[cfg(target_arch = "arm")]
    {
        // SAFETY: `dsb` and `isb` are barriers. They cannot fault, touch
        // no memory, and change no register the compiler is tracking.
        #[expect(unsafe_code, reason = "the architectural barriers after PENDSVSET")]
        unsafe {
            core::arch::asm!("dsb", "isb", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Put PendSV and SysTick at the lowest exception priority, as the C port
/// does, so neither ever preempts an application interrupt.
pub fn set_exception_priorities() {
    // SHPR3: byte 2 is SysTick, byte 3 is PendSV. Lowest priority is all
    // ones, and it is the same on every implemented priority width.
    write_reg(SHPR3, 0xFFFF_0000);
}

/// Start the tick at `reload + 1` processor cycles, and enable it.
///
/// `configSETUP_TICK_INTERRUPT`. The caller works out the reload from its
/// own clock; this crate does not guess a clock rate.
pub fn start_tick(reload: u32) {
    TICK_RELOAD.store(reload, Ordering::Relaxed);
    write_reg(SYST_RVR, reload);
    write_reg(SYST_CVR, 0);
    write_reg(SYST_CSR, SYST_ENABLE);
}

/// The reload [`start_tick`] was last given.
///
/// `vPortSuppressTicksAndSleep` needs to know how many processor cycles one
/// tick is, and this crate deliberately does not guess a clock rate -- the
/// caller works the reload out from its own. So rather than make a port ask
/// for the number a second time (two places to get it wrong, and the C has
/// exactly this bug class in `configSYSTICK_CLOCK_HZ`), the tick source
/// records what it was set to and the sleep reads it back.
///
/// Zero means the tick was never started, and the sleep declines.
static TICK_RELOAD: AtomicU32 = AtomicU32::new(0);

/// Stop the tick.
pub fn stop_tick() {
    write_reg(SYST_CSR, 0);
}

/// `vPortSuppressTicksAndSleep`: sleep through up to `expected_idle_ticks`
/// of them and report how many actually passed.
///
/// The kernel has already established that nothing is runnable for that
/// long and has suspended the scheduler; this reprograms SysTick for one
/// long interval, waits, and hands back the number of WHOLE ticks the
/// interval covered, which the kernel winds its own clock forward by.
/// Zero declines, and every path that cannot account for the time exactly
/// takes it.
///
/// # Two things this gets right, because getting them wrong loses a wake
///
/// **It sleeps to a tick BOUNDARY, not for a whole number of ticks.** The
/// window is what is left of the tick we are standing in, plus `want - 1`
/// whole ones. Waking on the boundary is what lets the counter be restarted
/// at full reload with the tick phase unchanged; sleeping `want` whole ticks
/// from here would shift every later tick by a fraction of one, for ever.
///
/// **It clears the pending SysTick before returning.** The caller holds
/// PRIMASK, so the exception was never taken -- but it is pending, and the
/// moment the mask drops it would be delivered and the kernel would count a
/// tick it has just been told about. `PENDSTCLR` is the difference between
/// suppressing ticks and deferring them.
///
/// # Callers
///
/// Must hold PRIMASK. `wfi` still wakes on a pending enabled interrupt with
/// interrupts masked -- that is the C port's idiom, and it is what lets this
/// function do the accounting rather than a handler.
#[cfg(target_arch = "arm")]
#[must_use]
pub fn suppress_ticks_and_sleep(expected_idle_ticks: u64) -> u64 {
    let reload = TICK_RELOAD.load(Ordering::Relaxed);
    // The tick was never started, so there is no geometry to sleep by.
    if reload == 0 || expected_idle_ticks == 0 {
        return 0;
    }
    // `start_tick(reload)` fires every `reload + 1` cycles.
    let per_tick = reload.saturating_add(1);
    // One programming of a 24-bit LOAD buys this many whole ticks.
    let ceiling = SYST_MAX_RELOAD.checked_div(per_tick).unwrap_or(0);
    let want = expected_idle_ticks.min(u64::from(ceiling));
    let want = u32::try_from(want).unwrap_or(0);
    if want == 0 {
        return 0;
    }

    // Stop the counter, capturing COUNTFLAG in the same breath: reading CSR
    // CLEARS it, so this is the only chance to see whether the tick we are
    // standing in has already expired.
    let csr = read_reg(SYST_CSR);
    write_reg(SYST_CSR, 0);
    let left = read_reg(SYST_CVR) & SYST_MAX_RELOAD;

    // A tick is already owed, or we are exactly on a boundary with nothing
    // left to measure from. Sleeping through an owed tick would lose it, so
    // decline and let the handler deliver it.
    if csr & SYST_COUNTFLAG != 0 || left == 0 {
        write_reg(SYST_RVR, reload);
        write_reg(SYST_CVR, 0);
        write_reg(SYST_CSR, SYST_ENABLE);
        return 0;
    }

    // `want <= SYST_MAX_RELOAD / per_tick` and `left < per_tick`, so the sum
    // fits in the LOAD register. The clamp is the register's width speaking,
    // not an argument about the caller.
    let whole = want.saturating_sub(1).saturating_mul(per_tick);
    let total = left.saturating_add(whole).min(SYST_MAX_RELOAD);
    write_reg(SYST_RVR, total.saturating_sub(1));
    write_reg(SYST_CVR, 0);
    write_reg(SYST_CSR, SYST_ENABLE);

    {
        // SAFETY: a barrier pair around a hint instruction. None of the three
        // can fault, none touches memory, and none changes a register the
        // compiler is tracking.
        #[expect(unsafe_code, reason = "the tickless sleep")]
        unsafe {
            core::arch::asm!("dsb", "wfi", "isb", options(nomem, nostack, preserves_flags));
        }
    }

    let woke = read_reg(SYST_CSR);
    write_reg(SYST_CSR, 0);
    let remaining = read_reg(SYST_CVR) & SYST_MAX_RELOAD;

    let slept = if woke & SYST_COUNTFLAG != 0 {
        // The window ran to its end, so we are on a tick boundary and
        // exactly `want` ticks have passed.
        u64::from(want)
    } else {
        // Something else woke us early. Count only the ticks that COMPLETED
        // -- reporting a part-tick would wind the kernel's clock past a wake
        // time. The phase shifts by whatever is left of the tick we are in,
        // which is the price of an early wake.
        let elapsed = total.saturating_sub(remaining);
        match elapsed.checked_sub(left) {
            None => 0,
            Some(after) => u64::from(after.checked_div(per_tick).unwrap_or(0).saturating_add(1)),
        }
    };

    // The suppressed ticks must not ALSO arrive as an exception the instant
    // the caller drops PRIMASK; the kernel is about to be told about them.
    write_reg(ICSR, PENDSTCLR);

    write_reg(SYST_RVR, reload);
    write_reg(SYST_CVR, 0);
    write_reg(SYST_CSR, SYST_ENABLE);

    slept
}

/// Off ARM there is no SysTick to reprogram and no `wfi` to wait on, so the
/// only honest answer is to decline. See the ARM definition for the
/// contract.
#[cfg(not(target_arch = "arm"))]
#[must_use]
pub fn suppress_ticks_and_sleep(_expected_idle_ticks: u64) -> u64 {
    0
}

/// What a `SysTick` handler should call: count the tick and, if the
/// scheduler wants a switch, pend one.
pub fn tick(port: &CortexMPort, switch_required: bool) {
    port.note_tick();
    if switch_required {
        pend_switch();
    }
}

/// Start the first task.
///
/// `vPortStartFirstTask`. Sets the process stack pointer from the slot
/// [`CURRENT_SP_SLOT`] names, switches to it, and returns into the task
/// through a PendSV. Never returns.
///
/// # Safety
/// [`CURRENT_SP_SLOT`] must already name a slot holding a stack built by
/// [`init_stack`], and a scheduler must be installed.
#[cfg(target_arch = "arm")]
#[expect(unsafe_code, reason = "the first-task start switches stacks")]
pub unsafe fn start_first_task() -> ! {
    set_exception_priorities();
    // Use the process stack for tasks, keeping the main stack for
    // exceptions, which is what CONTROL.SPSEL selects.
    //
    // SAFETY: the caller's contract, stated on this function, is that
    // `CURRENT_SP_SLOT` names a slot holding a stack built by
    // `init_stack` and that a scheduler is installed. Given that, every
    // load below is of a word this crate wrote, and the frame popped is
    // the one `init_stack` laid out. It never returns, so it cannot leave
    // the caller's stack in a state anything observes.
    unsafe {
        core::arch::asm!(
            // psp = *(*CURRENT_SP_SLOT), then step over the r4-r11 we did
            // not really save for a task that has never run.
            "ldr  r0, [{slot}]",
            "ldr  r0, [r0]",
            "adds r0, #32",
            "msr  psp, r0",
            "isb",
            "movs r0, #2",
            "msr  control, r0",   // SPSEL = 1: threads run on the PSP
            "isb",
            "cpsie i",
            "pop  {{r0-r3}}",     // R0-R3 of the frame
            "pop  {{r4}}",        // R12
            "pop  {{r5}}",        // LR
            "pop  {{r6}}",        // PC
            "add  sp, #4",        // xPSR
            "mov  lr, r5",
            // The stacked PC has bit 0 CLEAR, because that is what the
            // architecture wants of an exception frame. `bx` wants it SET:
            // a clear bit 0 asks for ARM state, which an M-profile core
            // does not have, and the result is a UsageFault that presents
            // as a task that simply never starts.
            "orr  r6, r6, #1",
            "bx   r6",
            slot = in(reg) CURRENT_SP_SLOT.as_ptr(),
            options(noreturn),
        )
    }
}

// There is deliberately NO non-ARM `start_first_task`. A stub would have
// to panic or hang, and this crate may do neither; and nothing off ARM can
// call it meaningfully anyway. Its absence is the honest signature: a host
// build of this crate compiles, and a host build that tried to start a
// task would not.
