//! Property tests (hardening gate H-28): `new_task_context`, the Xtensa stack builder, for many
//! stack sizes and tops, against its documented contract.
//!
//! The same assertions `fuzz/task_stacks` makes, from a seeded generator so
//! they run in `cargo test` on every push rather than only under libFuzzer.
//! The contract is what makes the `unsafe` sound (`UNSAFE.md`): every byte
//! written lands inside the documented window below the aligned top, and
//! the frame holds what the first switch-in will load.

#![allow(
    unsafe_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "a property test of an unsafe fn: it calls it inside a buffer it owns, and asserts by panicking"
)]

const CANARY: u8 = 0xA5;
const CASES: u64 = 20_000;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn untouched_outside(buf: &[u8], window: core::ops::Range<usize>, case: u64) {
    for (i, b) in buf.iter().enumerate() {
        if !window.contains(&i) {
            assert_eq!(
                *b, CANARY,
                "case {case}: byte {i} written outside {window:?}"
            );
        }
    }
}

extern "C" fn wrapper(_task_fn: usize, _param: usize) {}

#[test]
fn new_task_context_writes_only_its_window_and_aligns_the_stack() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut built = 0;
    for case in 0..CASES {
        let len = 64 + (rng.next() % 2048) as usize;
        let below = (rng.next() % 32) as usize;
        let task_fn = (rng.next() as u32) as usize;
        let param = (rng.next() as u32) as usize;
        let mut buf = vec![CANARY; len];
        let base = buf.as_mut_ptr() as usize;
        let top = base + len - below;
        let aligned = top & !0xf;
        if aligned < base + 16 {
            continue;
        }
        // SAFETY: the 16 bytes below the aligned top are inside `buf`, which
        // this test owns and nothing else uses.
        let ctx = unsafe {
            rusty_rtos_port_xtensa::new_task_context(wrapper, task_fn, param, top as *mut u8)
        };
        built += 1;
        assert_eq!(
            ctx.A1 as usize, aligned as u32 as usize,
            "case {case}: sp is not top rounded down to 16"
        );
        assert_eq!(
            ctx.A6 as usize, task_fn,
            "case {case}: task_fn is not in A6"
        );
        assert_eq!(ctx.A7 as usize, param, "case {case}: param is not in A7");
        untouched_outside(&buf, (aligned - 16 - base)..(aligned - base), case);
    }
    assert!(built > CASES / 2, "only {built} cases built a context");
}
