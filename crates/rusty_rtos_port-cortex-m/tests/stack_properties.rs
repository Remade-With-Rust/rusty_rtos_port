//! Property tests (hardening gate H-28): `init_stack`, the Cortex-M stack builder, for many
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

extern "C" fn entry(_arg: usize) -> ! {
    unreachable!("never called: the frame is built, not entered")
}

#[test]
fn init_stack_writes_only_its_window_and_builds_the_frame() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let word = core::mem::size_of::<usize>();
    let frame = 16 * word;
    let mut built = 0;
    for case in 0..CASES {
        let len = 64 + (rng.next() % 2048) as usize;
        let below = (rng.next() % 32) as usize;
        let param = rng.next() as usize;
        let mut buf = vec![CANARY; len];
        let base = buf.as_mut_ptr() as usize;
        let top = base + len - below;
        let aligned = top & !0x7;
        if aligned < base + frame || aligned % word != 0 {
            continue;
        }
        // SAFETY: `[aligned - 16 words, aligned)` is inside `buf`, which this
        // test owns and nothing else uses, and it is word-aligned.
        let sp = unsafe { rusty_rtos_port_cortex_m::init_stack(top as *mut usize, entry, param) };
        built += 1;
        assert_eq!(
            sp,
            aligned - frame,
            "case {case}: sp is not sixteen words below the aligned top"
        );
        assert_eq!(
            sp % 8,
            0,
            "case {case}: the frame is not 8-byte aligned (AAPCS)"
        );
        untouched_outside(&buf, (aligned - frame - base)..(aligned - base), case);
        let read = |i: usize| {
            let at = aligned - base - i * word;
            usize::from_le_bytes(buf[at..at + word].try_into().unwrap())
        };
        assert_eq!(
            read(1),
            0x0100_0000,
            "case {case}: xPSR is not the Thumb bit alone"
        );
        assert_eq!(
            read(2),
            (entry as extern "C" fn(usize) -> ! as usize) & !1,
            "case {case}: PC is not the entry"
        );
        assert_eq!(read(8), param, "case {case}: R0 is not the argument");
        for i in 9..=16 {
            assert_eq!(read(i), 0, "case {case}: r4-r11 are not zeroed");
        }
    }
    assert!(built > CASES / 2, "only {built} cases built a frame");
}
