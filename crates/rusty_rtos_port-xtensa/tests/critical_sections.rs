//! What the Xtensa port's critical sections do, checked on the host.
//!
//! The two instructions — `rsil` to mask and `wsr.ps` to restore — are
//! stubbed off Xtensa, and nothing here pretends otherwise. What is NOT
//! stubbed is the part that actually goes wrong: the nesting count, which
//! `PS` gets kept, and when an outermost exit is counted. That logic is
//! plain Rust and runs anywhere.
//!
//! It is worth testing on the host precisely because the board is not
//! always plugged in, and a port crate that can only be exercised with
//! hardware attached is a crate that rots between sessions.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Building a task stack is pointer work by nature; the crate's own fences
// carry the justifications, and these two are the test handing it memory.
#![expect(unsafe_code, reason = "the initial-frame test hands the port a stack")]

use rusty_rtos_core::port::Port;
use rusty_rtos_port_xtensa::XtensaPort;

#[test]
fn only_the_outermost_exit_counts_as_sim_time() {
    let port = XtensaPort::new();
    assert_eq!(port.exits(), 0);

    // One section, one exit.
    port.enter_critical();
    port.exit_critical();
    assert_eq!(port.exits(), 1, "an outermost exit is one exit");

    // Three deep is still ONE exit, and only on the way out of the last.
    port.enter_critical();
    port.enter_critical();
    port.enter_critical();
    assert_eq!(port.exits(), 1, "entering counts nothing");
    port.exit_critical();
    assert_eq!(port.exits(), 1, "an inner exit is not outermost");
    port.exit_critical();
    assert_eq!(port.exits(), 1, "still nested");
    port.exit_critical();
    assert_eq!(port.exits(), 2, "now it is outermost");
}

#[test]
fn an_unbalanced_exit_cannot_drive_the_count_backwards() {
    // `saturating_sub` in `exit_critical` is deliberate: a stray exit must
    // not wrap the nesting count to `u32::MAX` and leave interrupts masked
    // for the rest of the run. It is the kind of defect that presents as
    // "the board stopped ticking" hours later.
    let port = XtensaPort::new();
    port.exit_critical();
    port.exit_critical();

    // The port is still usable, and still counts correctly.
    port.enter_critical();
    port.exit_critical();
    assert!(
        port.exits() >= 1,
        "the port still works after a stray exit: {}",
        port.exits()
    );
}

/// `count_yield` counts; `yield_now` raises. They are deliberately not the
/// same call.
///
/// This test used to assert that `yield_now` incremented the counter, and it
/// caught the day that changed. `Kernel::port_yield` calls `count_yield` and
/// then `yield_now`, so a port that counted in both would double every yield
/// the kernel asked for — and on a port with `COMMITS_SWITCH` set, `yield_now`
/// is the ONLY way the kernel asks, so the double would be every yield there
/// is.
#[test]
fn counting_a_yield_is_separate_from_raising_one() {
    let port = XtensaPort::new();
    assert_eq!(port.yield_count(), 0);

    // Raising does not count.
    port.yield_now();
    port.yield_now();
    assert_eq!(
        port.yield_count(),
        0,
        "`yield_now` must not count: the kernel counts separately, and          counting in both places doubles every yield"
    );

    // Counting does.
    port.count_yield();
    port.count_yield();
    assert_eq!(port.yield_count(), 2);
}

#[test]
fn ticks_are_counted_by_whoever_owns_the_timer() {
    // The port does not own a timer. A firmware wires one up and calls
    // this beside `Kernel::increment_tick`, exactly as the Cortex-M cell
    // does from `SysTick`.
    let port = XtensaPort::new();
    assert_eq!(port.tick_count(), 0);
    for _ in 0..10 {
        port.note_tick();
    }
    assert_eq!(port.tick_count(), 10);
}

/// The initial frame's arithmetic, which is the same on every target.
///
/// `new_task_context` is `unsafe` because it writes below `stack_top`; the
/// arithmetic it does with that pointer is not, and getting it wrong is how
/// a task starts on a misaligned stack and faults on its first `entry`.
#[test]
fn a_fresh_context_starts_on_an_aligned_stack() {
    extern "C" fn wrapper(_task_fn: usize, _param: usize) {}
    extern "C" fn body(_a: usize, _b: usize) {}

    // Deliberately over-sized and deliberately MISALIGNED at the top, so
    // the rounding is exercised rather than trivially satisfied.
    let mut stack = vec![0u8; 4096];
    // SAFETY: 4093 is inside the 4096-byte allocation, so the offset stays
    // within the same object -- which is what `add` requires.
    let raw_top = unsafe { stack.as_mut_ptr().add(4093) };

    // SAFETY: the 4 KiB behind `raw_top` is ours and outlives the call.
    let context = unsafe {
        rusty_rtos_port_xtensa::new_task_context(wrapper, body as *const () as usize, 7, raw_top)
    };

    // Alignment survives the narrowing to `u32` -- truncation keeps the low
    // bits -- so this is meaningful on the host even though the rest of the
    // address is not. On a 32-bit target `A1` IS the address.
    assert_eq!(context.A1 % 16, 0, "SP must be 16-byte aligned");
    assert!(
        (context.A1 as usize) <= (raw_top as usize) & 0xffff_ffff,
        "the stack pointer must be at or below the top it was given"
    );

    // The four ABI words really were written, and to the aligned top rather
    // than the raw one. Reading them back is what proves the writes landed
    // where the arithmetic said, which is the half a type cannot check.
    let aligned = (raw_top as usize) & !0xf;
    // SAFETY: these are the words `new_task_context` just wrote, inside the
    // stack this test owns.
    let (sp_word, above) = unsafe {
        (
            ((aligned - 12) as *const u32).read_volatile(),
            ((aligned - 4) as *const u32).read_volatile(),
        )
    };
    assert_eq!(
        sp_word, context.A1,
        "the frame's own stack-pointer word must match A1"
    );
    assert_eq!(above, 0, "the rest of the initial frame is zeroed");
    assert_eq!(
        context.PC,
        (wrapper as *const ()) as usize as u32,
        "a fresh task enters through the wrapper, not the body"
    );
    assert_eq!(
        context.A6, body as *const () as usize as u32,
        "the body travels in A6"
    );
    assert_eq!(context.A7, 7, "its parameter travels in A7");
    assert_ne!(context.PS & 0x0004_0000, 0, "PS.WOE must be set");
}
