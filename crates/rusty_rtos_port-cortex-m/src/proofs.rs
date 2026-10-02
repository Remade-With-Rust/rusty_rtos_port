//! Kani proof of [`init_stack`](crate::init_stack) (hardening gate H-30).
//!
//! The one `unsafe fn` in this crate whose memory safety depends on the
//! CALLER: it writes sixteen words below a pointer it cannot check. The
//! property tests sample tops and sizes; this proves, for EVERY top inside a
//! buffer that meets the documented contract, that every write lands inside
//! the buffer (Kani checks each `write_volatile` for bounds and alignment)
//! and only inside the sixteen words below the 8-aligned top, and that the
//! frame holds what the first switch-in loads.
//!
//! The other `unsafe` here -- the PendSV switch and the first-task start, in
//! assembly, and the System Control Space register writes -- is outside what
//! a model checker can see: there is no assembly model, and the registers are
//! fixed hardware addresses. The QEMU cells, each with a poisoning that makes
//! them fail, are that code's evidence (threat model R-4).
//!
//! `cargo kani -p rusty_rtos_port-cortex-m` (Linux/macOS; a WSL command here).

extern "C" fn entry(_arg: usize) -> ! {
    loop {}
}

const WORDS: usize = 24;

#[kani::proof]
#[kani::unwind(26)]
#[expect(
    unsafe_code,
    reason = "calls the unsafe fn under its documented contract"
)]
fn init_stack_writes_only_its_window() {
    const SENTINEL: usize = 0x5A5A;
    let mut buf = [SENTINEL; WORDS];
    let word = core::mem::size_of::<usize>();
    // Any top from sixteen words in to one past the end, at any byte skew:
    // the contract is that the 8-aligned top has sixteen writable words
    // beneath it, which every such top meets.
    let words: usize = kani::any();
    kani::assume((16..=WORDS).contains(&words));
    let skew: usize = kani::any();
    kani::assume(skew < word && (words < WORDS || skew == 0));
    let arg: usize = kani::any();
    let base = buf.as_mut_ptr();
    let top = base
        .cast::<u8>()
        .wrapping_add(words * word + skew)
        .cast::<usize>();
    let aligned = (top as usize) & !0x7;
    // SAFETY: the 8-aligned top lies inside `buf` with at least sixteen
    // words beneath it -- the documented contract.
    let sp = unsafe { crate::init_stack(top, entry, arg) };
    assert_eq!(sp, aligned - 16 * word);
    let first = (sp - base as usize) / word;
    for (i, w) in buf.iter().enumerate() {
        if i < first || i >= first + 16 {
            assert_eq!(*w, SENTINEL, "a word outside the window was written");
        }
    }
    assert_eq!(buf[first + 16 - 8], arg, "R0 is the argument");
    assert_eq!(
        buf[first + 16 - 1],
        0x0100_0000,
        "xPSR is the Thumb bit alone"
    );
}
