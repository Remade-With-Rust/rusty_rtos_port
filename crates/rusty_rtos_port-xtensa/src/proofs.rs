//! Kani proof of [`new_task_context`](crate::new_task_context) (hardening
//! gate H-30).
//!
//! The Xtensa builder writes the four words the windowed ABI expects below
//! the 16-aligned top, through a pointer it cannot check. This proves, for
//! EVERY top inside a buffer that meets the documented contract
//! ([`INITIAL_FRAME_BYTES`](crate::INITIAL_FRAME_BYTES) writable below the
//! aligned top), that every write is in bounds and aligned (Kani checks each
//! `write_volatile`), that nothing outside those sixteen bytes changes, and
//! that the context holds the aligned stack pointer and both arguments.
//!
//! The switch is the trap frame `xtensa-lx-rt` saves and restores, plus the
//! cells' assembly; neither is visible to a model checker, and the S3 runs
//! with their poisonings are that code's evidence (threat model R-4).
//!
//! `cargo kani -p rusty_rtos_port-xtensa` (Linux/macOS; a WSL command here).

extern "C" fn wrapper(_task_fn: usize, _param: usize) {}

const BYTES: usize = 64;

#[kani::proof]
#[kani::unwind(66)]
#[expect(
    unsafe_code,
    reason = "calls the unsafe fn under its documented contract"
)]
fn new_task_context_writes_only_its_window() {
    const SENTINEL: u8 = 0xA5;
    // `u32` words, so the buffer is 4-aligned like a real stack.
    let mut words = [u32::from_ne_bytes([SENTINEL; 4]); BYTES / 4];
    let base = words.as_mut_ptr().cast::<u8>();
    let offset: usize = kani::any();
    kani::assume((16..=BYTES).contains(&offset));
    let task_fn: u32 = kani::any();
    let param: u32 = kani::any();
    let top = base.wrapping_add(offset);
    let aligned = (top as usize) & !0xf;
    kani::assume(aligned >= base as usize + 16);
    // SAFETY: the 16-aligned top has sixteen writable bytes beneath it,
    // inside `words` -- the documented contract.
    let ctx = unsafe { crate::new_task_context(wrapper, task_fn as usize, param as usize, top) };
    assert_eq!(ctx.A1 as usize, aligned as u32 as usize);
    assert_eq!(ctx.A6, task_fn);
    assert_eq!(ctx.A7, param);
    let lo = aligned - 16 - base as usize;
    for (i, w) in words.iter().enumerate() {
        let at = i * 4;
        if at < lo || at >= lo + 16 {
            assert_eq!(
                *w,
                u32::from_ne_bytes([SENTINEL; 4]),
                "a word outside the window was written"
            );
        }
    }
}
