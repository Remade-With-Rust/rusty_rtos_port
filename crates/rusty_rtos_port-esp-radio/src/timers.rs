//! The radio's software timers: a fixed table, a clock, and a service call.
//!
//! # Why this is in the crate and not behind the host seam
//!
//! It touches no kernel state at all. A radio timer is a callback, a data
//! pointer, a period and a due time; firing one is a comparison against
//! [`RadioHost::now_us`](crate::RadioHost::now_us). The firmware this was
//! lifted from kept the table beside its kernel, which made it look like
//! kernel state, and that is exactly why the glue could not be consumed:
//! nothing here depends on a kernel geometry, so nothing here needed to.
//!
//! What it does need is mutual exclusion against the interrupt that fires a
//! timer, and that comes from [`Critical`] — the host's mask, not the
//! kernel's lock. The two are separate on purpose: arming a timer must work
//! while the kernel is borrowed further up the stack.
//!
//! # The callback runs OUTSIDE the mask
//!
//! [`service_timers`] reads a due timer under the mask, drops it, and only
//! then calls back. A radio callback may take a semaphore, and holding an
//! interrupt mask across one would deadlock the first time it blocked. The
//! table is re-read per slot rather than iterated under one lock for the
//! same reason.

use core::ffi::c_void;

use crate::Critical;

/// How many timers the radio may have at once.
///
/// `esp-radio` creates a handful — one per connection state machine and a
/// few for the scan — so sixteen is slack rather than a budget. A
/// seventeenth is refused with [`usize::MAX`], which
/// [`TimerImplementation::create`](crate::Timer) turns into a null handle,
/// and the radio treats that as it treats any allocation failure.
const MAX_RADIO_TIMERS: usize = 16;

#[derive(Clone, Copy)]
struct RadioTimer {
    used: bool,
    active: bool,
    periodic: bool,
    period_us: u64,
    due_us: u64,
    callback: Option<unsafe extern "C" fn(*mut c_void)>,
    data: *mut c_void,
}

const EMPTY: RadioTimer = RadioTimer {
    used: false,
    active: false,
    periodic: false,
    period_us: 0,
    due_us: 0,
    callback: None,
    data: core::ptr::null_mut(),
};

static mut TIMERS: [RadioTimer; MAX_RADIO_TIMERS] = [EMPTY; MAX_RADIO_TIMERS];

/// The table, borrowed under a mask.
///
/// # Safety
/// The caller must hold a live [`Critical`] for as long as the returned
/// pointer is used, and there must be one core. Both hold everywhere in this
/// module: every caller takes the guard on its first line.
#[inline]
unsafe fn table() -> *mut RadioTimer {
    (&raw mut TIMERS).cast::<RadioTimer>()
}

/// Claim a slot for a radio timer; the index is its identity.
///
/// [`usize::MAX`] when the table is full, which is the one value that is
/// never a valid index, so a caller cannot mistake it for one.
pub(crate) fn remember(callback: unsafe extern "C" fn(*mut c_void), data: *mut c_void) -> usize {
    let _guard = Critical::enter();
    // SAFETY: the guard is live for this whole block and there is one core,
    // so this is the only borrow of the table.
    unsafe {
        let table = table();
        for i in 0..MAX_RADIO_TIMERS {
            if !(*table.add(i)).used {
                *table.add(i) = RadioTimer {
                    used: true,
                    active: false,
                    periodic: false,
                    period_us: 0,
                    due_us: 0,
                    callback: Some(callback),
                    data,
                };
                return i;
            }
        }
    }
    usize::MAX
}

/// Release a slot. An out-of-range index is ignored rather than trapping:
/// the driver deletes timers it may never have successfully created.
pub(crate) fn forget(index: usize) {
    if index >= MAX_RADIO_TIMERS {
        return;
    }
    let _guard = Critical::enter();
    // SAFETY: as `remember`.
    unsafe {
        let table = table();
        (*table.add(index)).used = false;
        (*table.add(index)).active = false;
    }
}

/// Arm a slot to fire `timeout_us` from now, once or repeatedly.
pub(crate) fn arm(index: usize, timeout_us: u64, periodic: bool) {
    if index >= MAX_RADIO_TIMERS {
        return;
    }
    // Read the clock BEFORE masking. `now_us` is the host's and may do work;
    // the mask is only needed for the store.
    let due = crate::host().now_us().saturating_add(timeout_us);
    let _guard = Critical::enter();
    // SAFETY: as `remember`.
    unsafe {
        let t = table().add(index);
        (*t).active = true;
        (*t).periodic = periodic;
        (*t).period_us = timeout_us;
        (*t).due_us = due;
    }
}

/// Disarm a slot without releasing it.
pub(crate) fn disarm(index: usize) {
    if index >= MAX_RADIO_TIMERS {
        return;
    }
    let _guard = Critical::enter();
    // SAFETY: as `remember`.
    unsafe {
        (*table().add(index)).active = false;
    }
}

/// Whether a slot is armed.
pub(crate) fn active(index: usize) -> bool {
    if index >= MAX_RADIO_TIMERS {
        return false;
    }
    let _guard = Critical::enter();
    // SAFETY: as `remember`.
    unsafe { (*table().add(index)).active }
}

/// Fire every timer that is due, and answer how many fired.
///
/// **The consumer must call this**, from a task or a tick hook. Nothing in
/// this crate has a thread of its own, so an unserviced table simply never
/// fires and the radio's retransmits and scan timeouts stop happening — a
/// fault that looks like a dead radio rather than a missing call.
///
/// A periodic timer's next due time is set from *now*, not from the previous
/// due time, so a late service does not then fire a burst catching up. That
/// matches what the C driver's timer task does.
pub fn service_timers() -> u32 {
    let mut fired = 0;
    let now = crate::host().now_us();
    for i in 0..MAX_RADIO_TIMERS {
        let due = {
            let _guard = Critical::enter();
            // SAFETY: as `remember`.
            unsafe {
                let t = table().add(i);
                if (*t).used && (*t).active && now >= (*t).due_us {
                    if (*t).periodic {
                        (*t).due_us = now.saturating_add((*t).period_us);
                    } else {
                        (*t).active = false;
                    }
                    (*t).callback.map(|cb| (cb, (*t).data))
                } else {
                    None
                }
            }
        };
        // Outside the mask: see the module docs.
        if let Some((cb, data)) = due {
            // SAFETY: the pair came from `TimerImplementation::create`, and
            // the radio keeps both valid until it deletes the timer — which
            // clears `used` under the same mask that read them.
            unsafe { cb(data) };
            fired += 1;
        }
    }
    fired
}
