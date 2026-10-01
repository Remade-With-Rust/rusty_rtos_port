//! Every port's initial-stack builder, on stacks of every size and
//! alignment libFuzzer can think of.
//!
//! These are the `unsafe` entry points a firmware hands a raw stack pointer
//! to: `new_task_context` on Xtensa and RISC-V, `init_stack` on Cortex-M.
//! Each documents the window it writes. The stack here is a heap buffer of
//! EXACTLY the fuzzed size, so AddressSanitizer reports any write past either
//! end, and every byte of it starts as a canary, so a write inside the buffer
//! but outside the documented window is caught too. The values the frame is
//! built from are checked to land where the switch will read them.

#![no_main]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use libfuzzer_sys::fuzz_target;

const CANARY: u8 = 0xA5;

extern "C" fn wrapper(_task_fn: usize, _param: usize) -> ! {
    unreachable!("never called: the frame is built, not entered")
}

/// Xtensa's wrapper returns to its trampoline rather than diverging.
extern "C" fn xtensa_wrapper(_task_fn: usize, _param: usize) {}

extern "C" fn entry(_arg: usize) -> ! {
    unreachable!("never called: the frame is built, not entered")
}

/// Every byte of `buf` outside `window` is still the canary.
fn untouched_outside(buf: &[u8], window: core::ops::Range<usize>, what: &str) {
    for (i, b) in buf.iter().enumerate() {
        if !window.contains(&i) {
            assert_eq!(
                *b, CANARY,
                "{what}: byte {i} written outside the window {window:?}"
            );
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 6 {
        return;
    }
    let len = 64 + usize::from(u16::from_le_bytes([data[0], data[1]]) % 4096);
    // How far below the end of the buffer the caller's "top" sits, so the
    // top lands at every alignment and the window at every offset.
    let below = usize::from(data[2]) % 32;
    let task_fn = usize::from(u16::from_le_bytes([data[3], data[4]]));
    let param = usize::from(data[5]);

    // ---- Xtensa: writes the four words at `[top & !15 - 16, top & !15)`.
    {
        let mut buf = vec![CANARY; len];
        let base = buf.as_mut_ptr() as usize;
        let top = base + len - below;
        let aligned = top & !0xf;
        if aligned >= base + 16 {
            // SAFETY: `[aligned - 16, aligned)` is inside `buf`, as the
            // contract requires.
            let ctx = unsafe {
                rusty_rtos_port_xtensa::new_task_context(
                    xtensa_wrapper,
                    task_fn,
                    param,
                    top as *mut u8,
                )
            };
            assert_eq!(
                ctx.A1 as usize, aligned as u32 as usize,
                "xtensa: sp is not top rounded down to 16"
            );
            assert_eq!(
                ctx.A6 as usize, task_fn as u32 as usize,
                "xtensa: task_fn not in A6"
            );
            assert_eq!(
                ctx.A7 as usize, param as u32 as usize,
                "xtensa: param not in A7"
            );
            let window = (aligned - 16 - base)..(aligned - base);
            untouched_outside(&buf, window, "xtensa");
        }
    }

    // ---- RISC-V: builds the context and writes no memory at all.
    {
        let mut buf = vec![CANARY; len];
        let base = buf.as_mut_ptr() as usize;
        let top = base + len - below;
        // SAFETY: `top` is inside `buf`; the builder writes nothing.
        let ctx = unsafe {
            rusty_rtos_port_riscv::new_task_context(wrapper, task_fn, param, top as *mut u8)
        };
        assert_eq!(
            ctx.sp,
            top & !0xf,
            "riscv: sp is not top rounded down to 16"
        );
        assert_eq!(ctx.s[1], task_fn, "riscv: task_fn not in s1");
        assert_eq!(ctx.s[2], param, "riscv: param not in s2");
        untouched_outside(&buf, 0..0, "riscv");
    }

    // ---- Cortex-M: sixteen words below `top & !7`, answered as the new sp.
    {
        let word = core::mem::size_of::<usize>();
        let mut buf = vec![CANARY; len];
        let base = buf.as_mut_ptr() as usize;
        let top = base + len - below;
        let aligned = top & !0x7;
        let frame = 16 * word;
        if aligned >= base + frame && aligned % word == 0 {
            // SAFETY: `[aligned - 16 words, aligned)` is inside `buf` and
            // word-aligned, as the contract requires.
            let sp =
                unsafe { rusty_rtos_port_cortex_m::init_stack(top as *mut usize, entry, param) };
            assert_eq!(
                sp,
                aligned - frame,
                "cortex-m: sp is not sixteen words below the aligned top"
            );
            let window = (aligned - frame - base)..(aligned - base);
            untouched_outside(&buf, window.clone(), "cortex-m");
            let read = |i: usize| {
                let at = aligned - base - i * word;
                usize::from_le_bytes(buf[at..at + word].try_into().unwrap())
            };
            assert_eq!(
                read(1),
                0x0100_0000,
                "cortex-m: xPSR is not the Thumb bit alone"
            );
            assert_eq!(
                read(2),
                (entry as usize) & !1,
                "cortex-m: PC is not the entry"
            );
            assert_eq!(read(8), param, "cortex-m: R0 is not the argument");
        }
    }
});
