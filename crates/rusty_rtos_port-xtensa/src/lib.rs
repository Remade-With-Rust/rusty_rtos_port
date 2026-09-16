#![no_std]
// Xtensa inline asm is nightly-only, and the gate is by TARGET rather than
// by toolchain: an Xtensa build is always the `esp` fork, since there is no
// upstream rustc target, while the host build -- which exists so callers
// keep type-checking on stable -- never sees the attribute.
#![cfg_attr(target_arch = "xtensa", feature(asm_experimental_arch))]
//! `rusty_rtos_port-xtensa` — the Kairos Xtensa port.
//!
//! The Cortex-M port's docs say this family keeps its assembly in the port
//! crates, "the only crates in the family with a fenced `unsafe` block".
//! This is that crate for Xtensa LX6/LX7 — ESP32 and ESP32-S3.
//!
//! # Why this crate exists at all, given the corpus does not need it
//!
//! `rusty_rtos_demo/firmware/xiao-s3-corpus` runs the whole conformance
//! corpus on an ESP32-S3, byte-identical to C FreeRTOS, **with no port**. A
//! Kairos task keeps its locals in the TCB, so it owns no stack and needs no
//! context switch. That result is real, and it is not what this crate is for.
//!
//! What needs a context switch is hosting tasks that are **not** ours. The
//! `esp-radio-rtos-driver` interface (0.4.1) that K5b joins is explicit:
//!
//! ```text
//! /// This function is used to create threads.
//! /// It should allocate the stack.
//! fn task_create(&self, name: &str, task: extern "C" fn(*mut c_void), ...,
//!                task_stack_size: usize) -> ThreadPtr;
//! ```
//!
//! A C function pointer, a stack size, and blocking semaphore waits the
//! radio blob performs from deep inside its own call frames. No stackless
//! kernel can satisfy that, so the port is required by the radio joint and
//! by nothing else the family has met so far.
//!
//! # The switch happens in an INTERRUPT, and that is the whole design
//!
//! An ARM switch saves eight callee-saved registers and swaps `SP`. The
//! equivalent on Xtensa is far worse than it looks: a 64-entry physical
//! register file behind a rotating 16-register window, and a task several
//! calls deep has several live windows sitting in that file. Switch stacks
//! with them live and the hardware later writes them onto whichever stack is
//! current — the *incoming* one — and the corruption surfaces far from its
//! cause.
//!
//! A first version of this crate tried to switch from task context, spilling
//! the windows by hand with `xtensa-lx-rt`'s `SPILL_REGISTERS` sequence. It
//! started a task, ran it, printed from it — and hung the moment that task
//! nested calls deeply enough to need the register file back. That is
//! recorded because the failure is instructive and the fix is not "try
//! harder at the spill":
//!
//! **The switch belongs in an exception handler, where the context is
//! already saved.** `xtensa-lx-rt`'s interrupt entry spills every window and
//! writes the whole machine — `A0`–`A15`, `PC`, `PS`, `SAR`, the loop and
//! MAC registers — into a [`Context`]. A handler holding `&mut Context` can
//! therefore switch tasks by *copying structs*: save the trap frame into the
//! outgoing task's slot, copy the incoming task's slot over the trap frame,
//! and return. The exception exit restores it, windows and all.
//!
//! So a yield here raises a software interrupt rather than calling a
//! switcher — which is what FreeRTOS's Xtensa port and Espressif's own
//! `esp-rtos` both do, and for this reason.
//!
//! # Unsafe
//!
//! Every use is fenced with `#[expect(unsafe_code)]` and a justification.
//! See `UNSAFE.md`.

/// Crate version, for manifests and logs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(target_arch = "xtensa")]
pub use xtensa_lx_rt::exception::Context;

/// The saved machine state of one task.
///
/// On Xtensa this is the interrupt trap frame, because the switch happens
/// inside an interrupt; off Xtensa it is a stand-in carrying the fields this
/// crate sets, so that callers keep type-checking on the host.
#[cfg(not(target_arch = "xtensa"))]
#[derive(Debug, Clone, Copy, Default)]
#[allow(non_snake_case)]
pub struct Context {
    /// Program counter.
    pub PC: u32,
    /// Processor state.
    pub PS: u32,
    /// Return address.
    pub A0: u32,
    /// Stack pointer.
    pub A1: u32,
    /// First windowed argument register used by [`new_task_context`].
    pub A6: u32,
    /// Second windowed argument register used by [`new_task_context`].
    pub A7: u32,
    /// The shift-amount register.
    ///
    /// Not set by this crate, and present for exactly that reason: the real
    /// trap frame carries a dozen registers a fresh task simply wants zeroed,
    /// so [`new_task_context`] finishes with `..Default::default()`. Without
    /// one such field here the host stub would be exhaustively initialised
    /// and that tail would read as dead code on the host build alone.
    pub SAR: u32,
}

/// `PS.WOE` — window overflow enabled.
///
/// A task started without it would run, and the first interrupt taken in it
/// would spill nothing, quietly corrupting whichever stack came next.
const PS_WOE: u32 = 0x0004_0000;

/// `PS.CALLINC` = 1: the task is entered as though it had been `call4`-ed.
///
/// This is what puts the entry point's two arguments in `A6`/`A7`: on
/// `entry` the window rotates by four, so the callee sees them as its own
/// `a2`/`a3`.
const PS_CALLINC_CALL4: u32 = 1 << 16;

/// How many bytes below the stack top the initial frame occupies.
pub const INITIAL_FRAME_BYTES: usize = 16;

/// Build the saved context of a task that has never run.
///
/// `stack_top` is one past the highest usable byte of the task's stack; it is
/// rounded down to the 16-byte alignment the ABI requires of `SP`. When the
/// task is first switched to, `wrapper` runs with `task_fn` and `param` as
/// its two arguments.
///
/// The wrapper exists because a task body must not simply `return`: there is
/// no frame beneath it to return into. A caller supplies one that ends the
/// task instead.
///
/// # Safety
///
/// `stack_top` must be one past a writable stack that stays mapped for as
/// long as the task can run, with at least [`INITIAL_FRAME_BYTES`] plus
/// whatever the task body needs below it.
#[expect(unsafe_code, reason = "writing the initial frame")]
#[must_use]
pub unsafe fn new_task_context(
    wrapper: extern "C" fn(task_fn: usize, param: usize),
    task_fn: usize,
    param: usize,
    stack_top: *mut u8,
) -> Context {
    // The address arithmetic is done at POINTER width and narrowed only on
    // the way into the register fields. Doing it in `u32` throughout costs
    // nothing on a 32-bit target and writes through a TRUNCATED address on
    // any other — which on the 64-bit host build is an access violation the
    // first time a test calls this. Found exactly that way.
    let top = (stack_top as usize) & !0xf;

    // The four words the ABI expects below a frame's stack pointer. The one
    // at `top - 12` is the frame's own stack pointer, which is what a window
    // underflow follows when the task's first `entry` unwinds.
    // SAFETY: the caller guarantees `[top - 16, top)` is writable.
    unsafe {
        (top.wrapping_sub(4) as *mut u32).write_volatile(0);
        (top.wrapping_sub(8) as *mut u32).write_volatile(0);
        (top.wrapping_sub(12) as *mut u32).write_volatile(top as u32);
        (top.wrapping_sub(16) as *mut u32).write_volatile(0);
    }

    // `..Default::default()` and not a full literal: the Xtensa `Context` is
    // the whole trap frame -- SAR, the loop registers, the MAC accumulators --
    // and a fresh task wants zero in every one of them.
    Context {
        PC: (wrapper as *const ()) as usize as u32,
        A0: 0,
        A1: top as u32,
        A6: task_fn as u32,
        A7: param as u32,
        PS: PS_WOE | PS_CALLINC_CALL4,
        ..Default::default()
    }
}

/// Swap the running task.
///
/// Called from inside the switching interrupt, with the trap frame the
/// handler was given. `current` is where the outgoing task's state is kept —
/// `None` when the outgoing task is being discarded rather than paused — and
/// `next` is the task to resume.
///
/// # Safety
///
/// `trap_frame` must be the frame the interrupt handler was handed, and the
/// two contexts must be valid for writing and reading respectively.
#[expect(unsafe_code, reason = "the context switch itself")]
pub unsafe fn switch_context(
    current: Option<*mut Context>,
    next: *const Context,
    trap_frame: &mut Context,
) {
    // SAFETY: the contract above. These are plain `Copy` structs, and the
    // exception exit restores whatever is left in `trap_frame`.
    unsafe {
        if let Some(current) = current {
            core::ptr::copy_nonoverlapping(core::ptr::from_mut(trap_frame), current, 1);
        }
        core::ptr::copy_nonoverlapping(next, core::ptr::from_mut(trap_frame), 1);
    }
}

/// The software interrupt the switch runs in.
///
/// Software0 everywhere except the original ESP32, which reserves it for the
/// Bluetooth stack and uses Software1 — the same choice `esp-rtos` makes. It
/// has to agree with the handler a firmware exports.
pub const SW_INTERRUPT_MASK: u32 = if cfg!(feature = "esp32") {
    1 << 29
} else {
    1 << 7
};

/// Enable the switching interrupt. Call once, before the first switch.
#[cfg(target_arch = "xtensa")]
#[expect(unsafe_code, reason = "enabling an interrupt is a register write")]
pub fn enable_switching() {
    // SAFETY: enables one software interrupt, whose handler the firmware
    // supplies; it can do nothing until `yield_now` raises it.
    unsafe {
        xtensa_lx::interrupt::enable_mask(SW_INTERRUPT_MASK);
    }
}

/// `portYIELD()`: raise the switching interrupt.
///
/// It does not switch, it *asks*. The switch happens when the interrupt is
/// taken, which is the only context in which the machine state is saved.
#[cfg(target_arch = "xtensa")]
#[expect(unsafe_code, reason = "raising an interrupt is a register write")]
#[inline]
pub fn yield_now() {
    // SAFETY: sets the software interrupt this port owns.
    unsafe {
        xtensa_lx::interrupt::set(SW_INTERRUPT_MASK);
    }
}

/// Clear the switching interrupt. The handler must do this first.
#[cfg(target_arch = "xtensa")]
#[expect(unsafe_code, reason = "clearing an interrupt is a register write")]
#[inline]
pub fn clear_switch_request() {
    // SAFETY: clears the software interrupt this port owns.
    unsafe {
        xtensa_lx::interrupt::clear(SW_INTERRUPT_MASK);
    }
}

/// Off Xtensa this is a stub, so every caller keeps type-checking on the
/// host build. That is what stops this crate rotting between board runs.
#[cfg(not(target_arch = "xtensa"))]
pub fn enable_switching() {}

/// As [`enable_switching`].
#[cfg(not(target_arch = "xtensa"))]
#[inline]
pub fn yield_now() {}

/// As [`enable_switching`].
#[cfg(not(target_arch = "xtensa"))]
#[inline]
pub fn clear_switch_request() {}

// ------------------------------------------------------------- the port --

use core::sync::atomic::{AtomicU32, Ordering};

use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;

/// `PS.INTLEVEL` a critical section masks to.
///
/// Three, not fifteen: level 4 and above on this ISA are non-maskable and
/// debug-level interrupts that a critical section has no business holding
/// off, and the radio blob's own handlers live below it. It is also the
/// level `esp-hal`'s `critical-section` implementation uses, which matters
/// because the two will nest inside one another in a radio firmware.
#[cfg(target_arch = "xtensa")]
const CRITICAL_INTLEVEL: u32 = 3;

/// The Kairos Xtensa port.
///
/// Critical sections mask interrupts by raising `PS.INTLEVEL`; the yield
/// raises the software interrupt the switch runs in; the tick arrives from
/// whatever timer the firmware wires up, through [`XtensaPort::note_tick`].
#[derive(Debug, Default)]
pub struct XtensaPort {
    nesting: AtomicU32,
    /// The `PS` saved by the OUTERMOST `enter_critical`, restored by the
    /// matching exit. Only the outermost value is kept, because only the
    /// outermost exit unmasks.
    saved_ps: AtomicU32,
    yields: AtomicU32,
    ticks: AtomicU32,
    exits: AtomicU32,
}

impl XtensaPort {
    /// A port with nothing counted yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            nesting: AtomicU32::new(0),
            saved_ps: AtomicU32::new(0),
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

    /// Count one tick. A firmware calls this from its timer interrupt,
    /// beside `Kernel::increment_tick`.
    pub fn note_tick(&self) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
    }

    /// `portYIELD()` calls made.
    #[must_use]
    pub fn yield_count(&self) -> u64 {
        u64::from(self.yields.load(Ordering::Relaxed))
    }
}

/// Raise `PS.INTLEVEL` and answer the old `PS`.
#[cfg(target_arch = "xtensa")]
#[expect(unsafe_code, reason = "the critical section is a PS write")]
#[inline(always)]
fn mask_interrupts() -> u32 {
    let ps: u32;
    // SAFETY: `rsil` reads PS and raises INTLEVEL in one instruction. It
    // touches no memory and cannot fault.
    unsafe {
        core::arch::asm!(
            "rsil {0}, {1}",
            out(reg) ps,
            const CRITICAL_INTLEVEL,
            options(nostack),
        );
    }
    ps
}

/// Restore a `PS` saved by [`mask_interrupts`].
#[cfg(target_arch = "xtensa")]
#[expect(unsafe_code, reason = "the critical section is a PS write")]
#[inline(always)]
fn restore_interrupts(ps: u32) {
    // SAFETY: `ps` came from `rsil` on this core, so it is a state this
    // core was already in. `rsync` is required before the new PS is
    // guaranteed visible to the instructions that follow.
    unsafe {
        core::arch::asm!(
            "wsr.ps {0}",
            "rsync",
            in(reg) ps,
            options(nostack),
        );
    }
}

#[cfg(not(target_arch = "xtensa"))]
#[inline(always)]
fn mask_interrupts() -> u32 {
    0
}

#[cfg(not(target_arch = "xtensa"))]
#[inline(always)]
fn restore_interrupts(_ps: u32) {}

impl Port for XtensaPort {
    /// This port SWITCHES STACKS, so the kernel must not commit a switch at
    /// the point of the yield — only the switching exception can move the
    /// registers, and anything running between a commit and that exception
    /// would be running as a task the kernel had already moved on from.
    const COMMITS_SWITCH: bool = true;

    fn yield_now(&self) {
        // No counter here: `Kernel::port_yield` calls `count_yield` just
        // before this, and counting in both places would double every yield
        // the kernel asks for.
        yield_now();
    }

    fn yield_from_isr(&self, woken: Woken) {
        if woken == Woken::YES {
            self.yields.fetch_add(1, Ordering::Relaxed);
            yield_now();
        }
    }

    fn enter_critical(&self) {
        let ps = mask_interrupts();
        // Only the OUTERMOST section's PS is kept: an inner `rsil` returns
        // a PS that is already masked, and restoring that on the way out
        // would leave interrupts off for good.
        if self.nesting.fetch_add(1, Ordering::Relaxed) == 0 {
            self.saved_ps.store(ps, Ordering::Relaxed);
        }
    }

    fn exit_critical(&self) {
        // Interrupts are masked here, so this read-modify-write needs no
        // atomicity of its own.
        let n = self.nesting.load(Ordering::Relaxed).saturating_sub(1);
        self.nesting.store(n, Ordering::Relaxed);
        if n == 0 {
            self.exits.fetch_add(1, Ordering::Relaxed);
            restore_interrupts(self.saved_ps.load(Ordering::Relaxed));
        }
    }

    fn set_interrupt_mask_from_isr(&self) -> u32 {
        mask_interrupts()
    }

    fn clear_interrupt_mask_from_isr(&self, saved: u32) {
        restore_interrupts(saved);
    }

    fn in_isr(&self) -> bool {
        // `PS.INTLEVEL` above zero means an interrupt is being serviced.
        // This is weaker than ARM's `IPSR`: a task inside a critical
        // section also reads non-zero. Callers in this family use it to
        // decide which API half to take, and a task in a critical section
        // taking the from-ISR half is safe -- the reverse would not be.
        mask_intlevel() != 0
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

/// The current `PS.INTLEVEL`.
#[cfg(target_arch = "xtensa")]
#[expect(unsafe_code, reason = "reading PS")]
#[inline]
fn mask_intlevel() -> u32 {
    let ps: u32;
    // SAFETY: reads a register, touches no memory.
    unsafe {
        core::arch::asm!("rsr.ps {0}", out(reg) ps, options(nomem, nostack));
    }
    ps & 0xf
}

#[cfg(not(target_arch = "xtensa"))]
#[inline]
fn mask_intlevel() -> u32 {
    0
}

/// `waiti 0`: sleep until the next interrupt. The C port's idle.
#[cfg(target_arch = "xtensa")]
#[expect(unsafe_code, reason = "the idle instruction")]
#[inline]
fn idle_wait() {
    // SAFETY: `waiti` is a hint; it cannot fault. It returns on any
    // interrupt at level 0 or above, which includes the tick.
    unsafe {
        core::arch::asm!("waiti 0", options(nomem, nostack));
    }
}

#[cfg(not(target_arch = "xtensa"))]
#[inline]
fn idle_wait() {}
