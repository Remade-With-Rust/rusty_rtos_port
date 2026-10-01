//! ONE heap for this binary: Kairos `heap_4`, serving both the Rust global
//! allocator and the radio blob's C `malloc` family.
//!
//! # Why `heap_4` and not `rusty_alloc` here
//!
//! The first board runs used `rusty_rtos_alloc` (rusty_alloc's small-metal
//! profile) for both. The scan transmitted and heard nothing, and the cause
//! was the heap: 29 of 88 blob allocations came back NULL, every one a
//! 96-byte request, with the 196,608-byte region reporting `free=0`. That
//! allocator's floor is roughly one page per size class touched, independent
//! of the bytes asked for; the blob touches many classes, and three segments
//! were gone before its scan list could take a record. `esp-rtos` with
//! `esp-alloc` found 15 access points from a 160 KiB heap on the same board.
//!
//! FreeRTOS gives the Wi-Fi driver `heap_4` -- first fit, coalescing, an
//! 8-byte header -- and `rusty_rtos_heap-core` is that algorithm, proven
//! byte-for-byte against `heap_4.c` (K4). Its cost is the bytes asked for
//! plus a header, which is the shape this workload needs.
//!
//! # The bridge
//!
//! `Heap4` speaks offsets into a private arena whose address is not promised
//! to be aligned. So every request is padded, the pointer handed out is
//! aligned UP inside the block, and the 8 bytes below it hold the block's
//! `heap_4` offset and the size asked for. `free` reads the offset back;
//! `realloc` reads the size. Both C and Rust callers go through the same two
//! functions, so a block is freed the same way whoever allocated it.
//!
//! # Interrupt safety
//!
//! Every heap operation runs with interrupts masked to the kernel's level,
//! as `esp-alloc`'s do: the blob may allocate from a context that an
//! interrupt can preempt, and a free list is not reentrant.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::Ordering::Relaxed;

use rusty_rtos_heap_core::Heap4;

/// Arena bytes. The `heap_4` header and the bridge's 8 bytes come out of it.
pub const ARENA: usize = 192 * 1024;

/// `portBYTE_ALIGNMENT` = 8 and `sizeof(BlockLink_t)` = 8 on a 32-bit part.
type H = Heap4<ARENA, 8, 8>;

/// What sits below every pointer handed out.
const BRIDGE: usize = 8;

struct Cell(UnsafeCell<H>);
// SAFETY: every access is under `masked`, and there is one core.
unsafe impl Sync for Cell {}
static HEAP: Cell = Cell(UnsafeCell::new(H::new()));

/// Diagnostics: calls, NULL answers, calls from interrupt level, frees.
pub mod stats {
    use core::sync::atomic::AtomicU32;
    pub static CALLS: AtomicU32 = AtomicU32::new(0);
    pub static NULLS: AtomicU32 = AtomicU32::new(0);
    pub static FROM_ISR: AtomicU32 = AtomicU32::new(0);
    pub static FREES: AtomicU32 = AtomicU32::new(0);
}

fn masked<R>(f: impl FnOnce(&mut H) -> R) -> R {
    let ps = mask();
    // SAFETY: interrupts are masked to the kernel's level and there is one
    // core, so this is the only live borrow.
    let out = f(unsafe { &mut *HEAP.0.get() });
    unmask(ps);
    out
}

#[cfg(target_arch = "xtensa")]
fn mask() -> u32 {
    let ps: u32;
    // SAFETY: raises INTLEVEL; touches no memory.
    unsafe { core::arch::asm!("rsil {0}, 3", out(reg) ps, options(nostack)) };
    ps
}
#[cfg(target_arch = "xtensa")]
fn unmask(ps: u32) {
    // SAFETY: restores a state this core was already in.
    unsafe { core::arch::asm!("wsr.ps {0}", "rsync", in(reg) ps, options(nostack)) };
}
#[cfg(not(target_arch = "xtensa"))]
fn mask() -> u32 {
    0
}
#[cfg(not(target_arch = "xtensa"))]
fn unmask(_: u32) {}

/// Whether PS says an interrupt or exception is being handled.
fn in_interrupt() -> bool {
    #[cfg(target_arch = "xtensa")]
    {
        let ps: u32;
        // SAFETY: reads PS.
        unsafe { core::arch::asm!("rsr.ps {0}", out(reg) ps, options(nostack)) };
        ps & 0xf != 0 || ps & 0x10 != 0
    }
    #[cfg(not(target_arch = "xtensa"))]
    false
}

/// Allocate `size` bytes aligned to `align` (a power of two; at least 8).
fn raw_alloc(size: usize, align: usize, zeroed: bool) -> *mut u8 {
    stats::CALLS.fetch_add(1, Relaxed);
    if in_interrupt() {
        stats::FROM_ISR.fetch_add(1, Relaxed);
    }
    let align = align.max(8);
    let total = size.checked_add(align).and_then(|t| t.checked_add(BRIDGE));
    let (Some(total), Ok(size32)) = (total, u32::try_from(size)) else {
        stats::NULLS.fetch_add(1, Relaxed);
        return core::ptr::null_mut();
    };
    let got = masked(|h| {
        let block = h.alloc(total)?;
        let base = h.address_of(block.offset())?;
        Some((block.offset(), base.as_ptr()))
    });
    let Some((offset, base)) = got else {
        let n = stats::NULLS.fetch_add(1, Relaxed);
        if n < 4 {
            esp_println::println!(
                "MALLOC NULL size={size} align={align} heap_free={}",
                free_bytes()
            );
        }
        return core::ptr::null_mut();
    };
    // The user pointer: at least BRIDGE bytes in, aligned up. It stays inside
    // the block because the block carries `align + BRIDGE` bytes of slack.
    let skip = (base as usize + BRIDGE).next_multiple_of(align) - base as usize;
    let user = base.wrapping_add(skip);
    // SAFETY: `user - 8 .. user + size` lies inside the block just allocated.
    unsafe {
        user.sub(8).cast::<u32>().write_unaligned(offset as u32);
        user.sub(4).cast::<u32>().write_unaligned(size32);
        if zeroed {
            core::ptr::write_bytes(user, 0, size);
        }
    }
    user
}

/// # Safety
/// `p` is null or came from [`raw_alloc`] and has not been freed.
unsafe fn raw_free(p: *mut u8) {
    if p.is_null() {
        return;
    }
    stats::FREES.fetch_add(1, Relaxed);
    // SAFETY: per the contract, the bridge sits just below `p`.
    let offset = unsafe { p.sub(8).cast::<u32>().read_unaligned() };
    // A refused free (a pointer this heap never handed out, or a double
    // free) is dropped rather than corrupting the list: `free_raw` checks.
    let _ = masked(|h| h.free_raw(u64::from(offset)));
}

/// # Safety
/// As [`raw_free`], and `p` non-null.
unsafe fn raw_size(p: *mut u8) -> usize {
    // SAFETY: as `raw_free`.
    unsafe { p.sub(4).cast::<u32>().read_unaligned() as usize }
}

/// # Safety
/// As [`raw_free`].
unsafe fn raw_realloc(p: *mut u8, size: usize) -> *mut u8 {
    if p.is_null() {
        return raw_alloc(size, 8, false);
    }
    if size == 0 {
        // SAFETY: forwarded.
        unsafe { raw_free(p) };
        return core::ptr::null_mut();
    }
    let fresh = raw_alloc(size, 8, false);
    if !fresh.is_null() {
        // SAFETY: `p` is live with its size in the bridge; `fresh` holds `size`.
        unsafe {
            core::ptr::copy_nonoverlapping(p, fresh, raw_size(p).min(size));
            raw_free(p);
        }
    }
    fresh
}

/// Free arena bytes, as `heap_4` counts them.
pub fn free_bytes() -> usize {
    masked(|h| h.free_bytes())
}

/// `xMinimumEverFreeBytesRemaining`.
pub fn minimum_ever_free() -> usize {
    masked(|h| h.minimum_ever_free_bytes())
}

// ------------------------------------------------------------- Rust side --

/// The global allocator, over the same arena.
pub struct Heap4Alloc;

// SAFETY: `raw_alloc` returns a block of at least `layout.size()` bytes at
// `layout.align()`, or null; `dealloc` only ever receives those.
unsafe impl GlobalAlloc for Heap4Alloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        raw_alloc(layout.size(), layout.align(), false)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        raw_alloc(layout.size(), layout.align(), true)
    }
    unsafe fn dealloc(&self, p: *mut u8, _layout: Layout) {
        // SAFETY: the trait's contract.
        unsafe { raw_free(p) }
    }
}

// ---------------------------------------------------------------- C side --

#[unsafe(no_mangle)]
extern "C" fn malloc(size: usize) -> *mut u8 {
    raw_alloc(size, 8, false)
}

#[unsafe(no_mangle)]
extern "C" fn malloc_internal(size: usize) -> *mut u8 {
    raw_alloc(size, 8, false)
}

/// # Safety
/// C's contract: `p` is null or a live block from this heap.
#[unsafe(no_mangle)]
unsafe extern "C" fn free(p: *mut u8) {
    // SAFETY: forwarded.
    unsafe { raw_free(p) }
}

/// # Safety
/// As [`free`].
#[unsafe(no_mangle)]
unsafe extern "C" fn free_internal(p: *mut u8) {
    // SAFETY: forwarded.
    unsafe { raw_free(p) }
}

/// # Safety
/// As [`free`].
#[unsafe(no_mangle)]
unsafe extern "C" fn realloc(p: *mut u8, size: usize) -> *mut u8 {
    // SAFETY: forwarded.
    unsafe { raw_realloc(p, size) }
}

/// # Safety
/// As [`free`].
#[unsafe(no_mangle)]
unsafe extern "C" fn realloc_internal(p: *mut u8, size: usize) -> *mut u8 {
    // SAFETY: forwarded.
    unsafe { raw_realloc(p, size) }
}

#[unsafe(no_mangle)]
extern "C" fn calloc(number: u32, size: usize) -> *mut u8 {
    match (number as usize).checked_mul(size) {
        Some(total) => raw_alloc(total, 8, true),
        None => core::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
extern "C" fn calloc_internal(number: u32, size: usize) -> *mut u8 {
    calloc(number, size)
}

#[unsafe(no_mangle)]
extern "C" fn get_free_internal_heap_size() -> usize {
    free_bytes()
}
