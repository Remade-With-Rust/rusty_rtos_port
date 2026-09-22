//! Which architecture's stack context this build uses.
//!
//! A task's saved context is the one genuinely architecture-shaped thing the
//! adapter touches, so it is selected by feature rather than hidden behind
//! the [`RadioHost`](crate::RadioHost) seam: a `&'static dyn` cannot carry a
//! `Sized` associated type into the `static` array that holds the slots.
//!
//! Exactly one of `xtensa` or `riscv` must be on. Both or neither is a
//! compile error rather than a confusing type error fifty lines later.
//!
//! **Janus needs `riscv`** — the mesh node is a C6 — and the cell this crate
//! was extracted from is `xtensa`. Both are built in CI for that reason.

#[cfg(all(feature = "xtensa", feature = "riscv"))]
compile_error!(
    "rusty_rtos_port-esp-radio: enable exactly one of `xtensa` or `riscv`, \
     not both — a build has one stack layout"
);

// With NEITHER architecture on, this module is simply empty and the rest of
// the crate — the seam, the host registry — still compiles. That is
// deliberate: `cargo check --workspace` builds every member with default
// features, and a member that cannot compile without a feature is a member
// that breaks the fleet gate for everyone.
//
// A consumer that forgets to pick an architecture does not get silence: the
// adapter types are gated on the same features, so the registration macros
// fail on a missing type with the architecture named in this module's docs.

#[cfg(feature = "xtensa")]
pub use rusty_rtos_port_xtensa::Context;
#[cfg(feature = "riscv")]
pub use rusty_rtos_port_riscv::Context;

/// The entry trampoline a fresh task is built around.
///
/// Declared `-> !` because a task body never returns: the two ports disagree
/// about whether to say so in the type (`-riscv` writes `-> !`, `-xtensa`
/// writes `()`), and this crate takes the honest one and adapts.
pub type Trampoline = extern "C" fn(task_fn: usize, param: usize) -> !;

/// `new_task_context`, over whichever port is selected.
///
/// # Safety
/// As the underlying port's: `stack_top` must be the top of a live, suitably
/// aligned stack that outlives the task, and must not be shared with another
/// task.
#[cfg(feature = "riscv")]
pub unsafe fn new_task_context(
    wrapper: Trampoline,
    task_fn: usize,
    param: usize,
    stack_top: *mut u8,
) -> Context {
    // SAFETY: forwarded unchanged; the caller carries the port's contract.
    unsafe { rusty_rtos_port_riscv::new_task_context(wrapper, task_fn, param, stack_top) }
}

/// `new_task_context`, over whichever port is selected.
///
/// # Safety
/// As the underlying port's: `stack_top` must be the top of a live, suitably
/// aligned stack that outlives the task, and must not be shared with another
/// task.
#[cfg(feature = "xtensa")]
pub unsafe fn new_task_context(
    wrapper: Trampoline,
    task_fn: usize,
    param: usize,
    stack_top: *mut u8,
) -> Context {
    // The Xtensa port types the trampoline as returning `()`. A function
    // that never returns satisfies a caller expecting one that does — the
    // difference is in what the type PROMISES, not in the calling
    // convention, and both are `extern "C"` with identical arguments.
    //
    // SAFETY: same ABI, same argument types, and the pointee never returns,
    // so the `()` the caller believes it may receive cannot be produced.
    let wrapper: extern "C" fn(usize, usize) = unsafe { core::mem::transmute(wrapper) };
    // SAFETY: forwarded unchanged; the caller carries the port's contract.
    unsafe { rusty_rtos_port_xtensa::new_task_context(wrapper, task_fn, param, stack_top) }
}
