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
//! struct MyHost;
//! impl rusty_rtos_port_esp_radio::RadioHost for MyHost { /* ... */ }
//!
//! static HOST: MyHost = MyHost;
//!
//! fn main() -> ! {
//!     rusty_rtos_port_esp_radio::install(&HOST);
//!     // ... create the kernel, start the scheduler ...
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

mod host;
pub mod port;

#[cfg(any(feature = "xtensa", feature = "riscv"))]
pub use port::{new_task_context, Context, Trampoline};

// NOT YET WIRED: `adapter.rs` is in this crate and is not compiled.
//
// It is the 895-line body lifted from `firmware/xiao-s3-radio`, and porting
// it to the seam above is 16 call sites, 8 of them multi-line closures that
// return a value the seam can no longer return generically. That is a
// semantic edit each, in glue that currently PASSES ON SILICON, so it is
// being done deliberately rather than in one sweep. The seam is landed and
// stable so a consumer can write their `RadioHost` against it today.
//
// Remaining, exactly: 34 `K` -> `dyn KernelOps`, 21 `with_kernel`, 9
// `crate::kernel::`, 5 `Wait` -> `Blocked`, 4 `yield_and_switch`, 3
// `now_us`, 3 `Context`.

/// How many tasks the adapter can track.
///
/// A compile-time capacity, because the slot table is a `static` array and
/// a `&'static dyn` host cannot size one. `install` checks the host against
/// it rather than letting an out-of-range index go quiet.
pub const SLOT_CAPACITY: usize = 32;

pub use host::{Blocked, KernelOps, RadioHost};

use core::sync::atomic::{AtomicPtr, Ordering};

static HOST: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the kernel this crate will drive. Call once, before `esp-radio`
/// starts.
///
/// Installing twice is allowed and the last one wins, which is what makes a
/// test that builds a fresh kernel per case possible.
pub fn install(host: &'static dyn RadioHost) {
    assert!(
        host.max_tasks() <= SLOT_CAPACITY,
        "rusty_rtos_port-esp-radio: the host holds more tasks than SLOT_CAPACITY"
    );
    // A `&dyn` is a fat pointer and does not fit an `AtomicPtr`, so the
    // reference to the reference is what is stored. The inner reference is
    // `'static`, so the box is never freed and the pointer stays valid.
    let boxed: &'static &'static dyn RadioHost = alloc::boxed::Box::leak(
        alloc::boxed::Box::new(host),
    );
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
