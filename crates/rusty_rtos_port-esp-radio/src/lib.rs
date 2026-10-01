//! `esp-radio` on a Kairos kernel: the five `esp-radio-rtos-driver` traits,
//! as a crate you can depend on.
//!
//! [`esp-radio`](https://crates.io/crates/esp-radio) reaches a scheduler
//! through [`esp-radio-rtos-driver`](https://crates.io/crates/esp-radio-rtos-driver),
//! which asks for five implementations: a scheduler, semaphores, queues,
//! timers and wait queues. This crate provides all five against a Kairos
//! kernel, and asks the consumer only for a [`RadioHost`].
//!
//! # Why this crate exists
//!
//! The same code passed on silicon inside
//! `rusty_rtos_port/firmware/xiao-s3-radio` from 2026-09-11 — but a firmware
//! directory is not consumable. Janus said so plainly
//! (`docs/plans/janus-rtos.md` §5b): *"Janus cannot consume ~1,450 lines of
//! `adapter.rs` + `kernel.rs` from inside a firmware directory. If that glue
//! became a crate, the Janus mesh node could take it the day S1's baseline
//! exists, instead of Janus duplicating it and the two copies drifting."*
//!
//! Two copies of a driver adapter that drift is the failure being avoided
//! here. The firmware now depends on this crate rather than carrying its
//! own.
//!
//! # Using it
//!
//! Implement [`RadioHost`] beside your kernel, install it once, and register
//! the five types with the driver's own macros:
//!
//! ```ignore
//! // The kernel has to be WRAPPED: the orphan rule forbids
//! // `impl KernelOps for Kernel<..>`, because neither type is yours. It
//! // borrows, so it costs nothing.
//! struct Ops<'a>(&'a mut MyKernel);
//! impl rusty_rtos_port_esp_radio::KernelOps for Ops<'_> { /* 14 forwards */ }
//!
//! struct MyHost;
//! impl rusty_rtos_port_esp_radio::RadioHost for MyHost {
//!     fn with_kernel(&self, f: &mut dyn FnMut(&mut dyn KernelOps)) {
//!         my_with_kernel(&mut |k| { f(&mut Ops(k)); Some(()) });
//!     }
//!     /* ... and the nine facts about the kernel ... */
//! }
//!
//! static HOST: MyHost = MyHost;
//!
//! fn main() -> ! {
//!     rusty_rtos_port_esp_radio::install(&HOST);
//!     // ... create the kernel, start the scheduler ...
//!     // ... and call `service_timers()` from a task: nothing here has one.
//! }
//!
//! esp_radio_rtos_driver::register_scheduler_implementation!(
//!     rusty_rtos_port_esp_radio::Scheduler
//! );
//! esp_radio_rtos_driver::register_semaphore_implementation!(
//!     rusty_rtos_port_esp_radio::Semaphore
//! );
//! // ... queue, timer, wait_queue the same way
//! ```
//!
//! [`install`] must happen before `esp-radio` starts. It is checked rather
//! than assumed: every entry point calls [`host`], which panics with a
//! named message if nothing was installed, because a null host reached from
//! a C driver is a fault nobody can read.
//!
//! # What this crate is NOT
//!
//! It is not a port. The context switch lives in `rusty_rtos_port-xtensa`
//! and `-riscv`; this is the driver-facing glue above it. It allocates,
//! because the driver hands out raw pointers and takes runtime sizes, so a
//! consumer must have a global allocator.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

// The adapter needs a saved-context type (so, an architecture) and the
// driver's `wait_queue` module (so, `ipc-implementations`). With neither
// architecture selected the rest of the crate — the seam, the host registry,
// the timer table — still compiles, because `cargo check --workspace` builds
// every member with DEFAULT features and a member that cannot do that breaks
// the fleet gate for everyone.
//
// The cost of that is real and is named here: a default-feature check does
// not compile the adapter at all. Both arms are therefore built explicitly,
// and `cargo xtask radio-arms` is what does it — a green fleet gate is not
// evidence about this module.
#[cfg(all(
    any(feature = "xtensa", feature = "riscv"),
    feature = "ipc-implementations"
))]
mod adapter;
mod host;
pub mod port;
#[cfg(all(
    any(feature = "xtensa", feature = "riscv"),
    feature = "ipc-implementations"
))]
mod timers;

#[cfg(any(feature = "xtensa", feature = "riscv"))]
pub use port::{Context, Trampoline, new_task_context, raise_switch};

/// How many tasks the adapter can track.
///
/// A compile-time capacity, because the slot table is a `static` array and
/// a `&'static dyn` host cannot size one. `install` checks the host against
/// it rather than letting an out-of-range index go quiet.
pub const SLOT_CAPACITY: usize = 32;

#[cfg(all(
    any(feature = "xtensa", feature = "riscv"),
    feature = "ipc-implementations"
))]
pub use adapter::{Queue, Scheduler, Semaphore, Timer, WaitQueue};

// The switching seam. A host's switch interrupt asks `context_of` for the two
// contexts to swap, and `register_main` gives the task the runtime started in
// a slot so the scheduler can switch AWAY from it — without that, `main` has
// nowhere to save its machine state and the first switch loses it.
#[cfg(all(
    any(feature = "xtensa", feature = "riscv"),
    feature = "ipc-implementations"
))]
pub use adapter::{
    TaskSlot, context_of, handle_for, isr_stats, register_kernel_task, register_main,
};
pub use host::{Blocked, KernelOps, RadioHost};
#[cfg(all(
    any(feature = "xtensa", feature = "riscv"),
    feature = "ipc-implementations"
))]
pub use timers::service_timers;

/// Borrow the kernel for one closure and take its answer back out.
///
/// [`RadioHost::with_kernel`] is a `dyn` method, so it cannot be generic over
/// a return type — it writes through a capture. This is the generic form
/// built on top of it, with the same signature the firmware's free function
/// had, so the adapter's call sites read as expressions.
///
/// `None` means one of two things, and every caller treats them the same: the
/// closure declined, or the host could not lend the kernel at all. Both are a
/// refused operation, and a driver that gets a null pointer or a `false`
/// cannot tell them apart anyway.
///
/// The closure must not block or yield. The host holds a lock — usually an
/// interrupt mask — for its whole duration, and yielding under one would run
/// another task with interrupts off. Every blocking path in the adapter takes
/// this once per attempt and drops it before yielding.
#[cfg(all(
    any(feature = "xtensa", feature = "riscv"),
    feature = "ipc-implementations"
))]
pub(crate) fn with_kernel<R>(f: &mut dyn FnMut(&mut dyn KernelOps) -> Option<R>) -> Option<R> {
    let mut out = None;
    host().with_kernel(&mut |k| out = f(k));
    out
}

/// A masked region, unmasked when it drops.
///
/// The seam exposes [`RadioHost::enter_critical`] and
/// [`RadioHost::exit_critical`] as a raw pair because a `dyn` method cannot
/// be generic over a closure's return. This pairs them so nothing inside the
/// crate can take one without the other — including on an early return, which
/// is how the raw form would eventually be got wrong.
#[cfg(all(
    any(feature = "xtensa", feature = "riscv"),
    feature = "ipc-implementations"
))]
pub(crate) struct Critical(u32);

#[cfg(all(
    any(feature = "xtensa", feature = "riscv"),
    feature = "ipc-implementations"
))]
impl Critical {
    #[inline]
    pub(crate) fn enter() -> Self {
        Critical(host().enter_critical())
    }
}

#[cfg(all(
    any(feature = "xtensa", feature = "riscv"),
    feature = "ipc-implementations"
))]
impl Drop for Critical {
    #[inline]
    fn drop(&mut self) {
        host().exit_critical(self.0);
    }
}

use core::sync::atomic::{AtomicPtr, Ordering};

static HOST: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the kernel this crate will drive. Call once, before `esp-radio`
/// starts.
///
/// Installing twice is allowed and the last one wins, which is what makes a
/// test that builds a fresh kernel per case possible.
///
/// # ★ Forgetting this does not fail to build, and does not fail to link
///
/// It deletes the crate. With LTO on, a program that never calls `install`
/// gives the compiler a [`HOST`] that is provably null, so [`host`] folds to
/// an unconditional panic and every adapter function behind it becomes
/// unreachable code the linker drops.
///
/// Measured on the `xiao-s3-radio` cell, 2026-09-21, as a before/after of the
/// same firmware: **`.text` 50,449 → 31,265 bytes** and all thirteen kernel
/// queue and semaphore symbols gone, with a clean build and a clean clippy.
/// On the board it is a panic on the first driver call.
///
/// There is no type that catches it — the driver reaches this crate from
/// `extern "C"` shims that carry no state — so the check is a build-time
/// comparison, not a compiler error. If the adapter seems to do nothing, look
/// for this call first.
pub fn install(host: &'static dyn RadioHost) {
    assert!(
        host.max_tasks() <= SLOT_CAPACITY,
        "rusty_rtos_port-esp-radio: the host holds more tasks than SLOT_CAPACITY"
    );
    // A `&dyn` is a fat pointer and does not fit an `AtomicPtr`, so the
    // reference to the reference is what is stored. The inner reference is
    // `'static`, so the box is never freed and the pointer stays valid.
    let boxed: &'static &'static dyn RadioHost =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(host));
    HOST.store(
        (boxed as *const &'static dyn RadioHost) as *mut (),
        Ordering::Release,
    );
}

/// The installed host.
///
/// # Panics
/// If [`install`] has not been called. That is deliberate: this is reached
/// from a C driver that has no error channel, so the alternative is a null
/// dereference inside `esp-radio` with no hint of the cause.
#[inline]
pub fn host() -> &'static dyn RadioHost {
    let p = HOST.load(Ordering::Acquire);
    assert!(
        !p.is_null(),
        "rusty_rtos_port-esp-radio: no RadioHost installed — call install() \
         before esp-radio starts"
    );
    // SAFETY: the pointer was produced by `install` from a leaked
    // `&'static &'static dyn RadioHost`, so it is non-null, aligned, and
    // points at a live value for the rest of the program.
    *unsafe { &*(p as *const &'static dyn RadioHost) }
}

/// Whether a host has been installed, for a consumer that wants to check
/// rather than find out from a panic.
pub fn is_installed() -> bool {
    !HOST.load(Ordering::Acquire).is_null()
}
