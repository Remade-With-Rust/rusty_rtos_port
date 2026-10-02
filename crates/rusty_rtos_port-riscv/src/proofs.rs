//! Kani proof of [`new_task_context`](crate::new_task_context) (hardening
//! gate H-30).
//!
//! The RISC-V builder keeps the whole initial frame in the returned
//! `Context` and writes NO stack memory, so its soundness does not depend on
//! the caller at all: this proves that for EVERY `stack_top`, including a
//! dangling or null one, it touches no memory (Kani would flag any access)
//! and answers a 16-byte-aligned stack pointer at or below the top, with the
//! entry point and both arguments where the trampoline reads them.
//!
//! The switch itself is assembly, which a model checker cannot see; the
//! QEMU `virt` cells and their poisonings are its evidence (threat model R-4).
//!
//! `cargo kani -p rusty_rtos_port-riscv` (Linux/macOS; a WSL command here).

extern "C" fn wrapper(_task_fn: usize, _param: usize) -> ! {
    loop {}
}

#[kani::proof]
#[expect(
    unsafe_code,
    reason = "calls the unsafe fn with an unconstrained pointer"
)]
fn new_task_context_touches_no_memory_for_any_top() {
    let top: usize = kani::any();
    let task_fn: usize = kani::any();
    let param: usize = kani::any();
    // SAFETY: deliberately NOT the documented contract -- any address at
    // all. The function must not dereference it, and Kani proves it does not.
    let ctx = unsafe { crate::new_task_context(wrapper, task_fn, param, top as *mut u8) };
    assert_eq!(ctx.sp, top & !0xf);
    assert!(ctx.sp <= top);
    assert_eq!(ctx.sp % 16, 0);
    assert_eq!(ctx.s[1], task_fn);
    assert_eq!(ctx.s[2], param);
}
