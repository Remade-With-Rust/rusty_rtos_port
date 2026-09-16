//! The Kairos **host** port: a task gets a real stack from the operating
//! system.
//!
//! # Why this exists
//!
//! The Kairos kernel is stackless by design. A blocking call keeps its
//! locals in the TCB and answers `Wait::Blocked`, meaning "call again when
//! this task next runs" — which is what lets one kernel run the
//! conformance corpus on a host, on a simulator and on three different
//! chips without a line of assembly in between.
//!
//! A task written in **C** cannot work that way. `vTaskDelay` has to return
//! *later*, on the line after, with its locals intact, and locals in C live
//! on a stack. So the C ABI needs a port that supplies one — and until this
//! crate there were only two kinds: three bare-metal ports, which need an
//! emulator or a chip, and the sim port, which is stackless. That is why
//! K6's kill test ran on QEMU Cortex-M3 before it ran on a host: there was
//! nowhere on a host for a C task to keep its locals.
//!
//! # The shape is FreeRTOS's own
//!
//! `portable/ThirdParty/GCC/Posix` runs one pthread per task and lets
//! exactly one of them run at a time; the MSVC-MinGW port does the same
//! with Win32 threads. This is that: one OS thread per task, a single
//! **run permit**, and a tick thread standing in for the timer interrupt.
//! The OS supplies the stacks and switches them; we supply the policy.
//!
//! # The two ways a switch happens
//!
//! | | who decides | how the outgoing task stops |
//! |---|---|---|
//! | voluntary | the task, through [`pend_switch`] | it waits on its own permit |
//! | preemptive | the tick thread | the OS freezes the thread where it stands |
//!
//! The voluntary path is all a cooperative kernel needs. The preemptive
//! path is not optional: `integer.c` and `flop.c` in the standard demos
//! **never block and never yield** — with `configUSE_PREEMPTION` set they
//! rely on being taken off the CPU — so a host port without it starves
//! every task below them and their neighbours' checkers report failure. A
//! cooperative host port would pass the demos that block and quietly fail
//! the ones that do not, which is the worst of both.
//!
//! # Freezing a thread is safe here, and the reason is narrow
//!
//! Suspending an arbitrary thread is a famous way to deadlock: freeze it
//! inside `malloc` and the next thread that allocates waits forever.
//!
//! It is safe here because of the ORDER in [`Ticker::interrupt`]: the tick
//! thread takes the critical-section lock **first**, and every kernel call
//! and every heap call is inside that lock. A thread that is not holding it
//! is not inside the kernel and not inside our heap, so there is no lock
//! for it to be holding when it freezes.
//!
//! What that argument does NOT cover is a lock we do not own: the C
//! runtime's allocator, or `stdout`. A task that calls `printf` or the
//! system `malloc` directly can still be frozen inside one. The demo task
//! files do neither — they allocate through `pvPortMalloc`, which is the
//! Kairos heap behind our own critical section. A cell that prints from a
//! task must do it inside [`Port::enter_critical`].
//!
//! # Unsafe
//!
//! The workspace denies `unsafe_code`. Every use here is fenced with
//! `#[expect(unsafe_code)]` and a justification, and they are all of one
//! kind: freezing and thawing an OS thread. See `UNSAFE.md`.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;

/// Crate version, for manifests and logs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The most tasks a host cell may have.
///
/// Fixed rather than grown, so the slot table is a `static` and a slot's
/// address never moves — a task thread holds a reference to its own slot
/// for its whole life.
pub const MAX_TASKS: usize = 128;

// ------------------------------------------------------ the run permit --

/// One task's slot: its permit, and how it is currently stopped.
///
/// A task runs only when it holds the single run permit. It can be stopped
/// in two different ways and the resume path has to know which, because
/// thawing a thread that is waiting on a condvar does nothing and
/// signalling a condvar that a frozen thread is not waiting on does nothing
/// either. Getting this wrong is a hang, not an error.
#[derive(Debug)]
struct Slot {
    /// Set when this task may run. The task's own thread waits for it.
    granted: Mutex<bool>,
    /// Signalled when `granted` becomes true.
    wake: Condvar,
    /// The OS thread, once it exists, in whatever form the platform needs
    /// to freeze it. Zero means "no thread yet".
    os_thread: AtomicU64,
    /// True while this task's thread is frozen by the tick thread rather
    /// than parked on its own condvar.
    frozen: AtomicBool,
    /// Which occupant of this slot the thread belongs to.
    ///
    /// A slot outlives its tasks -- `death.c` deletes and recreates them
    /// forever and the kernel hands the freed index straight back -- and a
    /// deleted task's thread is parked inside the C function it was
    /// running, from which it will never return. So the slot takes a NEW
    /// thread and this counter is how the old one knows it is no longer
    /// the occupant: it wakes with the others, sees a generation that is
    /// not its own, and parks again forever.
    ///
    /// Without it the orphan competes for the run permit with the task
    /// that replaced it, and both run.
    generation: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            granted: Mutex::new(false),
            wake: Condvar::new(),
            os_thread: AtomicU64::new(0),
            frozen: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        }
    }
}

/// The slot table. One entry per task index, and the index is the
/// kernel's `TaskHandle::index()`, exactly as on the Cortex-M port.
fn slots() -> &'static [Slot] {
    static SLOTS: OnceLock<Vec<Slot>> = OnceLock::new();
    SLOTS.get_or_init(|| (0..MAX_TASKS).map(|_| Slot::new()).collect())
}

/// Which task index currently holds the run permit.
///
/// This is the host's `pxCurrentTCB`. `usize::MAX` means "nothing is
/// running yet", the state before the first task starts.
pub static CURRENT: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Nobody is running.
pub const NO_TASK: usize = usize::MAX;

// ---------------------------------------------------- the critical lock --

/// The critical section, as a lock the tick thread also takes.
///
/// On a chip `cpsid i` masks the timer interrupt. Here there is no
/// interrupt to mask: the "interrupt" is another OS thread, and the only
/// way to hold it off is a lock it agrees to take. That agreement is the
/// whole of this port's interrupt model, and [`Ticker::interrupt`] is the
/// one place the other side of it lives.
///
/// It is a spin lock rather than a `Mutex` because it must be re-entrant
/// (the kernel nests critical sections freely) and must be releasable from
/// a different call frame than the one that took it, neither of which a
/// `MutexGuard` will do.
#[derive(Debug)]
struct CriticalLock {
    /// The thread that holds it, as its OS id. Zero is free.
    owner: AtomicU64,
    /// How many times that thread has entered without leaving.
    nesting: AtomicU32,
}

static CRITICAL: CriticalLock = CriticalLock {
    owner: AtomicU64::new(0),
    nesting: AtomicU32::new(0),
};

impl CriticalLock {
    /// Take the lock, or count another level if this thread already has it.
    fn enter(&self, me: u64) {
        if self.owner.load(Ordering::Acquire) == me {
            self.nesting.fetch_add(1, Ordering::Relaxed);
            return;
        }
        while self
            .owner
            .compare_exchange_weak(0, me, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            // The holder is either running on another core or about to be
            // scheduled. Either way there is nothing useful to do here.
            std::thread::yield_now();
        }
        self.nesting.store(1, Ordering::Relaxed);
    }

    /// Give up one level, releasing the lock at zero. `true` when it was
    /// actually released.
    fn exit(&self, me: u64) -> bool {
        if self.owner.load(Ordering::Acquire) != me {
            // Not ours to release. A port that "restored" a lock it never
            // took would let the tick thread in halfway through somebody
            // else's critical section.
            return false;
        }
        let left = self.nesting.load(Ordering::Relaxed).saturating_sub(1);
        self.nesting.store(left, Ordering::Relaxed);
        if left == 0 {
            self.owner.store(0, Ordering::Release);
            return true;
        }
        false
    }

    /// Give the lock up entirely, whatever depth this thread was at, and
    /// answer that depth.
    ///
    /// A critical section belongs to the TASK, not to the CPU. FreeRTOS's
    /// own Win32 port saves and restores `uxCriticalNesting` across a
    /// switch for exactly this reason: the kernel yields from inside its
    /// own critical sections, and a task that parked while still holding
    /// this lock would deadlock the task it just handed the CPU to.
    fn release_all(&self, me: u64) -> u32 {
        if self.owner.load(Ordering::Acquire) != me {
            return 0;
        }
        let depth = self.nesting.load(Ordering::Relaxed);
        self.nesting.store(0, Ordering::Relaxed);
        self.owner.store(0, Ordering::Release);
        depth
    }

    /// Take it again at the depth [`release_all`](Self::release_all) gave
    /// back. Nothing to do for a depth of zero.
    fn reacquire(&self, me: u64, depth: u32) {
        if depth == 0 {
            return;
        }
        self.enter(me);
        self.nesting.store(depth, Ordering::Relaxed);
    }
}

// ------------------------------------------------------ the OS backend --

#[cfg(windows)]
mod backend {
    //! Windows: `SuspendThread` / `ResumeThread` freeze a thread where it
    //! stands, which is what makes preemption possible at all.

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{ResumeThread, SuspendThread};

    /// Is preemption available on this platform?
    pub const PREEMPTIVE: bool = true;

    /// The OS handle a `JoinHandle` owns.
    ///
    /// Taken from the spawner so the thread has a handle before it has
    /// run. Borrowed, not owned: the `JoinHandle` must outlive every use,
    /// which is why the caller keeps it.
    #[must_use]
    pub fn thread_handle_of(join: &std::thread::JoinHandle<()>) -> u64 {
        use std::os::windows::io::AsRawHandle as _;
        join.as_raw_handle() as u64
    }

    /// Freeze the thread. `false` if it could not be.
    #[expect(unsafe_code, reason = "suspending a thread is a syscall")]
    pub fn freeze(handle: u64) -> bool {
        if handle == 0 {
            return false;
        }
        // SAFETY: `handle` came from `current_thread` above and is a real
        // thread handle this process owns.
        let previous = unsafe { SuspendThread(handle as HANDLE) };
        previous != u32::MAX
    }

    /// Thaw the thread. `false` if it could not be.
    #[expect(unsafe_code, reason = "resuming a thread is a syscall")]
    pub fn thaw(handle: u64) -> bool {
        if handle == 0 {
            return false;
        }
        // SAFETY: as `freeze`.
        let previous = unsafe { ResumeThread(handle as HANDLE) };
        previous != u32::MAX
    }

    /// Give the handle back to the OS.
    #[expect(unsafe_code, reason = "closing a handle is a syscall")]
    pub fn release(handle: u64) {
        if handle != 0 {
            // SAFETY: as `freeze`; the handle is not used again.
            unsafe { CloseHandle(handle as HANDLE) };
        }
    }
}

#[cfg(unix)]
mod backend {
    //! Unix: a thread cannot be frozen from outside, so it freezes
    //! **itself**.
    //!
    //! There is no `SuspendThread` here. What Unix gives instead is a
    //! signal, and a signal handler is the only code that runs in another
    //! thread's context without that thread's cooperation — which is
    //! exactly what preemption means. So `freeze` sends `SIGUSR1` and the
    //! handler, running in the target, parks the target in `sigsuspend`
    //! until `SIGUSR2` arrives. It is the shape FreeRTOS's own
    //! `portable/ThirdParty/GCC/Posix` port uses.
    //!
    //! # Freezing has to be SYNCHRONOUS, and that is the whole difficulty
    //!
    //! `pthread_kill` returns as soon as the signal is *queued*, not when
    //! it is handled. If `freeze` returned there, the tick thread would
    //! grant the CPU to a new task while the old one was still running on
    //! it — two tasks live at once, which is the one thing a single run
    //! permit exists to prevent. So `freeze` waits until it can observe
    //! the target parked, and answers `false` if it never does, in which
    //! case the caller declines the switch rather than taking it unsafely.
    //!
    //! # The lost-wakeup race, and why `SIGUSR2` is blocked in the handler
    //!
    //! The handler marks itself `PARKED` and then calls `sigsuspend`. A
    //! `thaw` landing in the gap between those two would be delivered to a
    //! thread that is not yet waiting, and the thread would then sleep for
    //! ever on a wake-up that already happened.
    //!
    //! `SIGUSR2` is therefore in the suspend handler's `sa_mask`, so it is
    //! BLOCKED for the whole handler. A `thaw` arriving in that gap stays
    //! *pending*; `sigsuspend` atomically unblocks it, which delivers it
    //! immediately and returns. Nothing is lost, and the loop re-checks
    //! the state rather than trusting the wake-up — a signal handler that
    //! trusts a single wake-up is a signal handler with a bug.
    //!
    //! # What runs inside a handler
    //!
    //! Only async-signal-safe calls: `pthread_self`, `sigfillset`,
    //! `sigdelset`, `sigsuspend`, and atomic loads over a fixed static
    //! table. No allocation, no locks, no thread-locals — a Rust
    //! thread-local can allocate on first touch, which is not safe here.

    use std::sync::Once;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    /// Is preemption available on this platform?
    pub const PREEMPTIVE: bool = true;

    /// How many task threads can be registered at once.
    ///
    /// An entry is freed by [`release`], which the port calls when a task
    /// slot is reused or a task is deleted, so this bounds LIVE threads
    /// and not threads over the run. The C ABI cell runs 60.
    const MAX_THREADS: usize = 256;

    /// Neither asked to park nor parked.
    const RUNNING: u32 = 0;
    /// Signalled, but not yet seen to be parked.
    const ASKED: u32 = 1;
    /// Sitting in `sigsuspend`, inside the handler.
    const PARKED: u32 = 2;

    /// How many times [`freeze`] looks for the target to park before it
    /// gives up and answers `false`.
    ///
    /// This is a bound on a handoff between two runnable threads, not a
    /// wait for work, so it is generous rather than tuned: the cost of
    /// being wrong in one direction is a declined switch, and in the other
    /// it is two tasks running at once.
    const PARK_SPINS: u32 = 200_000;

    struct Entry {
        /// The `pthread_t`, widened to a `usize`.
        ///
        /// Zero means the entry is free. No Unix gives out a null
        /// `pthread_t` — on Linux it is a pointer to the thread's own
        /// descriptor — so zero is safe to spend as the empty marker.
        tid: AtomicUsize,
        /// One of [`RUNNING`], [`ASKED`], [`PARKED`].
        state: AtomicU32,
    }

    static TABLE: [Entry; MAX_THREADS] = [const {
        Entry {
            tid: AtomicUsize::new(0),
            state: AtomicU32::new(RUNNING),
        }
    }; MAX_THREADS];

    /// The entry a handle names, or `None`.
    ///
    /// The handle is the table index plus one, so that a handle of zero
    /// keeps meaning "no thread" the way the Windows backend's does.
    fn entry_at(handle: u64) -> Option<&'static Entry> {
        let index = usize::try_from(handle).ok()?.checked_sub(1)?;
        TABLE.get(index)
    }

    /// The entry belonging to a `pthread_t`, or `None`.
    ///
    /// A linear scan of atomic loads, which is what makes it callable from
    /// a signal handler. 256 relaxed loads is nothing next to the context
    /// switch that is about to happen.
    fn entry_of(tid: usize) -> Option<&'static Entry> {
        TABLE.iter().find(|e| e.tid.load(Ordering::Acquire) == tid)
    }

    /// Park this thread until someone thaws it. `SIGUSR1`.
    #[expect(
        unsafe_code,
        reason = "a signal handler is the only code that runs in another thread without its cooperation"
    )]
    extern "C" fn on_suspend(_signal: core::ffi::c_int) {
        // SAFETY: `pthread_self` reads the calling thread's own id and is
        // async-signal-safe.
        let me = unsafe { libc::pthread_self() } as usize;
        let Some(entry) = entry_of(me) else {
            // Not a task thread, or its entry was released while the
            // signal was in flight. Either way there is nobody to wait
            // for a thaw that will never come.
            return;
        };
        if entry
            .state
            .compare_exchange(ASKED, PARKED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // `freeze` gave up before this handler ran and put the state
            // back. Parking now would be parking with nobody left to thaw.
            return;
        }

        // Everything blocked except the thaw signal, so `sigsuspend` can
        // only be woken by the thing that is meant to wake it.
        //
        // SAFETY: `mask` is a real `sigset_t` about to be initialised by
        // `sigfillset`; all four calls are async-signal-safe.
        unsafe {
            let mut mask: libc::sigset_t = core::mem::zeroed();
            libc::sigfillset(&raw mut mask);
            libc::sigdelset(&raw mut mask, libc::SIGUSR2);
            while entry.state.load(Ordering::Acquire) == PARKED {
                libc::sigsuspend(&raw const mask);
            }
        }
    }

    /// Wake a parked thread. `SIGUSR2`.
    ///
    /// Deliberately empty: the work is done by `sigsuspend` RETURNING. A
    /// handler that did anything here would be doing it in the wrong
    /// thread's context for no reason.
    extern "C" fn on_resume(_signal: core::ffi::c_int) {}

    /// Install both handlers, once for the process.
    #[expect(unsafe_code, reason = "installing a signal handler is a syscall")]
    fn install_handlers() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // SAFETY: both structures are zeroed, then filled with a real
            // handler and a real mask before `sigaction` reads them, and
            // the handlers have C ABI and the right signature.
            unsafe {
                let mut suspend: libc::sigaction = core::mem::zeroed();
                suspend.sa_sigaction = on_suspend as *const () as libc::sighandler_t;
                libc::sigemptyset(&raw mut suspend.sa_mask);
                // The lost-wakeup guard: see this module's header.
                libc::sigaddset(&raw mut suspend.sa_mask, libc::SIGUSR2);
                // A task frozen mid-`read` should resume the read, not see
                // `EINTR` it never asked about.
                suspend.sa_flags = libc::SA_RESTART;
                libc::sigaction(libc::SIGUSR1, &raw const suspend, core::ptr::null_mut());

                let mut resume: libc::sigaction = core::mem::zeroed();
                resume.sa_sigaction = on_resume as *const () as libc::sighandler_t;
                libc::sigemptyset(&raw mut resume.sa_mask);
                resume.sa_flags = libc::SA_RESTART;
                libc::sigaction(libc::SIGUSR2, &raw const resume, core::ptr::null_mut());
            }
        });
    }

    /// The OS handle a `JoinHandle` owns, registered so it can be frozen.
    ///
    /// Taken from the spawner so the thread has a handle before it has
    /// run, exactly as on Windows: a thread that registered itself would
    /// have a window in which it holds the run permit and cannot be
    /// frozen.
    #[must_use]
    pub fn thread_handle_of(join: &std::thread::JoinHandle<()>) -> u64 {
        use std::os::unix::thread::JoinHandleExt as _;

        install_handlers();
        let tid = join.as_pthread_t() as usize;
        if tid == 0 {
            return 0;
        }
        for (index, entry) in TABLE.iter().enumerate() {
            // Claim the entry with the store that publishes it, so two
            // threads spawning at once cannot take the same one.
            if entry
                .tid
                .compare_exchange(0, tid, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                entry.state.store(RUNNING, Ordering::Release);
                return index.saturating_add(1) as u64;
            }
        }
        // Full. Zero is the port's "no thread", so it declines to freeze
        // this one rather than freezing somebody else.
        0
    }

    /// Freeze the thread, and do not answer until it really is frozen.
    #[expect(unsafe_code, reason = "signalling a thread is a syscall")]
    pub fn freeze(handle: u64) -> bool {
        let Some(entry) = entry_at(handle) else {
            return false;
        };
        let tid = entry.tid.load(Ordering::Acquire);
        if tid == 0 {
            return false;
        }
        if entry
            .state
            .compare_exchange(RUNNING, ASKED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Already asked or already parked. Not this caller's to take.
            return false;
        }
        // SAFETY: `tid` was registered by `thread_handle_of` from a live
        // `JoinHandle` the port still owns, and cleared by `release`
        // before that thread's slot is reused.
        if unsafe { libc::pthread_kill(tid as libc::pthread_t, libc::SIGUSR1) } != 0 {
            entry.state.store(RUNNING, Ordering::Release);
            return false;
        }
        for _ in 0..PARK_SPINS {
            if entry.state.load(Ordering::Acquire) == PARKED {
                return true;
            }
            std::thread::yield_now();
        }
        // It never parked. Put the state back: the handler, if it runs at
        // all now, will find its `ASKED -> PARKED` swap refused and return
        // without parking, which is the unwind this depends on.
        entry.state.store(RUNNING, Ordering::Release);
        false
    }

    /// Thaw the thread. `false` if it was not parked.
    #[expect(unsafe_code, reason = "signalling a thread is a syscall")]
    pub fn thaw(handle: u64) -> bool {
        let Some(entry) = entry_at(handle) else {
            return false;
        };
        let tid = entry.tid.load(Ordering::Acquire);
        if tid == 0 {
            return false;
        }
        // The state goes back FIRST, so the handler's loop sees `RUNNING`
        // when the signal wakes it. The other order is a thread that wakes
        // up, re-reads `PARKED`, and suspends itself again for ever.
        if entry.state.swap(RUNNING, Ordering::AcqRel) != PARKED {
            return false;
        }
        // SAFETY: as `freeze`.
        unsafe { libc::pthread_kill(tid as libc::pthread_t, libc::SIGUSR2) == 0 }
    }

    /// Give the entry back. There is no OS handle to close on Unix.
    pub fn release(handle: u64) {
        let Some(entry) = entry_at(handle) else {
            return;
        };
        entry.state.store(RUNNING, Ordering::Release);
        entry.tid.store(0, Ordering::Release);
    }
}

#[cfg(not(any(windows, unix)))]
mod backend {
    //! Everywhere else: **cooperative only**, and it says so rather than
    //! pretending.
    //!
    //! Windows and Unix both have a way to stop a thread that is not
    //! cooperating. A platform that is neither does not necessarily, and
    //! guessing at one would be worse than reporting the truth: on this
    //! platform a task that never blocks and never yields will not be
    //! taken off the CPU, and the demos that rely on preemption
    //! (`integer.c`, `flop.c`) will starve their neighbours.
    //!
    //! [`super::PREEMPTIVE`] is the honest report of that, and a cell
    //! should check it rather than assume.

    /// Is preemption available on this platform?
    pub const PREEMPTIVE: bool = false;

    /// No handle to take.
    #[must_use]
    pub fn thread_handle_of(_join: &std::thread::JoinHandle<()>) -> u64 {
        0
    }

    /// Cannot freeze a thread here.
    pub fn freeze(_handle: u64) -> bool {
        false
    }

    /// Nothing was frozen, so nothing thaws.
    pub fn thaw(_handle: u64) -> bool {
        false
    }

    /// Nothing to release.
    pub fn release(_handle: u64) {}
}

/// Whether this platform can take a task off the CPU that does not
/// cooperate.
///
/// A cell that runs the standard demo files should refuse to report a pass
/// when this is false and `integer`-shaped tasks are in the mix: they
/// never block, so without preemption everything below them starves and
/// the failure looks like somebody else's bug.
pub const PREEMPTIVE: bool = backend::PREEMPTIVE;

// --------------------------------------------------------- the switcher --

/// What the port calls to choose the next task.
///
/// It must set [`CURRENT`] to the incoming task's index; whatever it
/// leaves there is what runs next. The same contract as the Cortex-M
/// port's `CURRENT_SP_SLOT`, one level up: there the scheduler names a
/// stack pointer, here it names a thread.
pub type Scheduler = extern "C" fn();

static SCHEDULER: AtomicUsize = AtomicUsize::new(0);

/// Install the function the port calls to pick the next task.
pub fn set_scheduler(f: Scheduler) {
    SCHEDULER.store(f as usize, Ordering::SeqCst);
}

fn ask_scheduler() {
    let f = SCHEDULER.load(Ordering::SeqCst);
    if f != 0 {
        // SAFETY: `f` was stored by `set_scheduler` from a `Scheduler`,
        // and nothing else ever writes this.
        #[expect(unsafe_code, reason = "the installed scheduler is a fn pointer")]
        let f: Scheduler = unsafe { core::mem::transmute::<usize, Scheduler>(f) };
        f();
    }
}

/// Permits handed to each slot, so a cell can ask who actually got the CPU.
#[allow(clippy::declare_interior_mutable_const)]
const NO_GRANTS: AtomicU64 = AtomicU64::new(0);
static GRANTS: [AtomicU64; MAX_TASKS] = [NO_GRANTS; MAX_TASKS];

/// How many times slot `index` was handed the run permit.
#[must_use]
pub fn grants_to(index: usize) -> u64 {
    GRANTS.get(index).map_or(0, |c| c.load(Ordering::Relaxed))
}

/// Give `index` the run permit.
fn grant(index: usize) {
    if let Some(c) = GRANTS.get(index) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    let Some(slot) = slots().get(index) else {
        return;
    };
    if slot.frozen.swap(false, Ordering::SeqCst) {
        // It was taken off the CPU by the tick thread, not parked, so it
        // is not waiting on anything and must be thawed instead.
        backend::thaw(slot.os_thread.load(Ordering::SeqCst));
        return;
    }
    let Ok(mut granted) = slot.granted.lock() else {
        return;
    };
    *granted = true;
    // `notify_all`, not `notify_one`: an orphaned thread from an earlier
    // occupant of this slot may be waiting on the same condvar, and
    // waking only one of them could wake the wrong one. Each checks its
    // own generation and the loser goes back to sleep for good.
    slot.wake.notify_all();
}

/// Wait until this task has the run permit, or forever if this thread is
/// no longer the slot's occupant.
fn await_permit(index: usize, generation: u64) {
    let Some(slot) = slots().get(index) else {
        return;
    };
    let Ok(granted) = slot.granted.lock() else {
        return;
    };
    take_permit(slot, generation, granted);
}

/// The waiting half, given a guard the caller already holds.
///
/// `pend_switch` must take this lock BEFORE it lets anything else run, so
/// the wait cannot be a self-contained function -- it has to be handed the
/// guard.
fn take_permit(slot: &Slot, generation: u64, mut granted: std::sync::MutexGuard<'_, bool>) {
    loop {
        if slot.generation.load(Ordering::SeqCst) != generation {
            // A later task has this slot. This thread belongs to a task
            // that no longer exists and must never run again.
            let Ok(next) = slot.wake.wait(granted) else {
                return;
            };
            granted = next;
            continue;
        }
        if *granted {
            *granted = false;
            return;
        }
        let Ok(next) = slot.wake.wait(granted) else {
            return;
        };
        granted = next;
    }
}

/// `portYIELD()`: ask the scheduler who is next and hand the CPU over.
///
/// Returns when this task is next given the permit, which is what makes a
/// blocking C call possible: the locals are on this thread's own stack and
/// are still there when it comes back.
pub fn pend_switch() {
    let who = me();
    if CRITICAL.owner.load(Ordering::Acquire) == who {
        // We are inside a critical section, which on this port means
        // inside a kernel call. This is `PendSV` being PENDED, not taken:
        // record it, and `exit_critical` runs it on the way out.
        //
        // Taking it here instead re-enters the kernel while an outer call
        // still holds a `&mut` to it -- and the kernel's idea of the
        // current task then moves under the feet of the call in progress,
        // so every API that means "the calling task" names the wrong one.
        // `GenQTest` ends a test with `vTaskPrioritySet( NULL,
        // genqMUTEX_LOW_PRIORITY )`, and the task set to low priority was
        // the HIGH priority one.
        PENDING.store(true, Ordering::SeqCst);
        return;
    }
    switch_now(who);
}

/// Set when a switch has been asked for and not yet taken. One flag,
/// because there is one CPU: this is `ICSR.PENDSVSET`.
static PENDING: AtomicBool = AtomicBool::new(false);

/// Take the switch. The caller must NOT hold the critical section.
fn switch_now(who: u64) {
    // The decision and the handover happen with the tick thread locked
    // out. Without this the two interleave: a task asks the scheduler, the
    // tick thread freezes it mid-answer and hands the CPU to somebody
    // else, and when the frozen task is later thawed it finishes a
    // handover whose answer is now stale -- granting the permit to a task
    // that should not have it, so two run at once.
    //
    // That race is not theoretical and it is not loud. It shows up as
    // `eTaskGetState( x ) == eBlocked` failing three statements after a
    // `vTaskResume`, which reads as a scheduler bug rather than a port
    // one.
    CRITICAL.enter(who);
    let mine = CURRENT.load(Ordering::SeqCst);
    ask_scheduler();
    let next = CURRENT.load(Ordering::SeqCst);
    if next == mine || next == NO_TASK || mine == NO_TASK {
        CRITICAL.exit(who);
        return;
    }
    let Some(slot) = slots().get(mine) else {
        CRITICAL.exit(who);
        return;
    };
    // Our own permit lock, taken BEFORE anything else can run, so a grant
    // aimed at us cannot land between the handover and the wait.
    let Ok(granted) = slot.granted.lock() else {
        CRITICAL.exit(who);
        return;
    };
    let generation = slot.generation.load(Ordering::SeqCst);
    grant(next);

    // Put the critical section down completely while parked, and pick it
    // up at the same depth on the way back. See `release_all`.
    let depth = CRITICAL.release_all(who);

    take_permit(slot, generation, granted);

    CRITICAL.reacquire(who, depth);
    // ...and undo the level this function took on the way in.
    CRITICAL.exit(who);
}

/// Give task `index` a thread, parked until it is granted the permit.
///
/// `entry` is handed the task index, exactly as `init_stack` does on the
/// Cortex-M port, so one trampoline can serve every task.
pub fn init_task(index: usize, entry: extern "C" fn(usize) -> !) {
    let Some(slot) = slots().get(index) else {
        return;
    };
    // A slot that already had a thread gets a NEW one, and the old one is
    // orphaned rather than reused: it is parked inside the C function of a
    // task that no longer exists, and there is no safe way to bring it
    // back to the top. Bumping the generation is what stops it competing
    // for the permit.
    let generation = slot
        .generation
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    if generation > 1 {
        ORPHANS.fetch_add(1, Ordering::Relaxed);
        // Wake the outgoing occupant so it can see its generation has
        // passed and settle; and give its handle back.
        if let Ok(granted) = slot.granted.lock() {
            drop(granted);
            slot.wake.notify_all();
        }
        backend::release(slot.os_thread.swap(0, Ordering::SeqCst));
    }
    let spawned = std::thread::Builder::new()
        .name(format!("kairos-task-{index}.{generation}"))
        .stack_size(STACK_BYTES)
        .spawn(move || {
            MY_INDEX.set(Some(index));
            await_permit(index, generation);
            entry(index);
        });

    // Register the thread from HERE, not from inside it.
    //
    // A thread that names itself has a window in which it exists, may hold
    // the run permit, and has no handle -- and a tick landing in that
    // window cannot freeze it. That is not a missed tick: it is the one
    // case where the port would have to decline a switch, which is the
    // path that used to leave the kernel and the port disagreeing about
    // who was running.
    //
    // The `JoinHandle` owns the OS thread handle, so it is kept: dropping
    // it would close the handle and every later freeze would fail.
    if let Ok(handle) = spawned {
        if let Some(slot) = slots().get(index) {
            slot.os_thread
                .store(backend::thread_handle_of(&handle), Ordering::SeqCst);
        }
        if let Ok(mut live) = threads().lock() {
            live.push(handle);
        }
    }
}

thread_local! {
    /// Which task this thread IS, as opposed to which task the kernel
    /// believes is running.
    ///
    /// On a chip those cannot differ: there is one CPU and the kernel's
    /// `current` is definitionally who is on it. Here they are two
    /// different facts kept in two places, so they CAN differ -- and when
    /// they do, every API that means "the calling task" quietly names
    /// somebody else. Having the thread able to state its own identity is
    /// what makes that checkable rather than arguable.
    static MY_INDEX: core::cell::Cell<Option<usize>> = const { core::cell::Cell::new(None) };
}

/// Is a context switch outstanding but not yet taken? (`ICSR.PENDSVSET`.)
#[must_use]
pub fn switch_pending() -> bool {
    PENDING.load(Ordering::SeqCst)
}

/// How deep the calling thread is inside critical sections, and whether it
/// is the holder at all.
#[must_use]
pub fn critical_depth() -> (bool, u32) {
    let mine = CRITICAL.owner.load(Ordering::Acquire) == me();
    (mine, CRITICAL.nesting.load(Ordering::Relaxed))
}

/// Which task the CALLING THREAD is, if it is a task thread at all.
#[must_use]
pub fn my_index() -> Option<usize> {
    MY_INDEX.with(core::cell::Cell::get)
}

/// What the tick thread did, tick by tick.
///
/// A port that declines to switch reports the same thing as a port that
/// was never asked, and those are different bugs -- so it counts both.
static TICKS_SEEN: AtomicU64 = AtomicU64::new(0);
static TICKS_WANTED_SWITCH: AtomicU64 = AtomicU64::new(0);
static TICKS_COULD_NOT_FREEZE: AtomicU64 = AtomicU64::new(0);
static TICKS_SWITCHED: AtomicU64 = AtomicU64::new(0);

/// `(ticks, wanted a switch, could not freeze, actually switched)`.
#[must_use]
pub fn tick_stats() -> (u64, u64, u64, u64) {
    (
        TICKS_SEEN.load(Ordering::Relaxed),
        TICKS_WANTED_SWITCH.load(Ordering::Relaxed),
        TICKS_COULD_NOT_FREEZE.load(Ordering::Relaxed),
        TICKS_SWITCHED.load(Ordering::Relaxed),
    )
}

/// Bytes of OS stack per task thread.
///
/// Every orphaned thread keeps one until the process ends, and `death.c`
/// makes orphans on purpose, so this is not purely a depth question. The
/// deepest a demo task reached on the Cortex-M3 cell, measured by stack
/// painting, was 122 words; a host frame is wider but not by three orders
/// of magnitude.
const STACK_BYTES: usize = 128 * 1024;

/// How many task threads have been orphaned by a slot being reused.
///
/// Reported rather than hidden: each one holds [`STACK_BYTES`] of address
/// space for the life of the process, and a cell that makes thousands of
/// them should be able to see that it is doing so. Terminating them
/// instead is what FreeRTOS's Posix port does with `pthread_cancel`, and
/// is the obvious next step for this port.
static ORPHANS: AtomicU64 = AtomicU64::new(0);

/// How many task threads have been orphaned by a slot being reused.
#[must_use]
pub fn orphaned_threads() -> u64 {
    ORPHANS.load(Ordering::Relaxed)
}

/// The spawned task threads, kept alive so their OS handles stay valid.
fn threads() -> &'static Mutex<Vec<std::thread::JoinHandle<()>>> {
    static THREADS: OnceLock<Mutex<Vec<std::thread::JoinHandle<()>>>> = OnceLock::new();
    THREADS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Start the first task and never come back.
///
/// The calling thread becomes the idle of last resort: it parks, because
/// every task now has its own thread and this one has nothing left to do.
pub fn start_first_task(index: usize) -> ! {
    CURRENT.store(index, Ordering::SeqCst);
    grant(index);
    loop {
        // Long enough that this thread costs nothing, short enough that a
        // cell which exits can do so promptly.
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ------------------------------------------------------------ the tick --

/// What the tick thread calls once per period.
///
/// It stands where `SysTick` does on a chip: it is called with the
/// critical-section lock HELD and the running task frozen, so it may
/// touch the kernel freely. `true` asks for a context switch, exactly as
/// `xTaskIncrementTick` answering `pdTRUE` does.
pub type TickFn = extern "C" fn() -> bool;

/// The tick thread's own handle on the system.
#[derive(Debug)]
pub struct Ticker {
    period: Duration,
    on_tick: TickFn,
}

impl Ticker {
    /// A ticker that calls `on_tick` every `period`.
    #[must_use]
    pub const fn new(period: Duration, on_tick: TickFn) -> Self {
        Self { period, on_tick }
    }

    /// Run the ticker on its own thread, forever.
    pub fn spawn(self) {
        let _ = std::thread::Builder::new()
            .name("kairos-tick".to_owned())
            .spawn(move || {
                loop {
                    std::thread::sleep(self.period);
                    self.interrupt();
                }
            });
    }

    /// One tick, in the order that makes freezing a thread safe.
    ///
    /// 1. Take the critical-section lock. This WAITS for the running task
    ///    to leave any critical section, so when it is held the running
    ///    task is outside the kernel and outside our heap.
    /// 2. Freeze the running task. It cannot be holding a lock of ours,
    ///    by step 1.
    /// 3. Do the kernel's tick work and ask who is next.
    /// 4. Release the lock, then thaw or grant the incoming task.
    ///
    /// Steps 1 and 2 in the other order is the classic deadlock: freeze a
    /// task inside `pvPortMalloc` and the next task to allocate waits for
    /// a lock whose holder is never going to run again.
    fn interrupt(&self) {
        let me = me();
        CRITICAL.enter(me);
        IN_TICK.store(true, Ordering::SeqCst);

        TICKS_SEEN.fetch_add(1, Ordering::Relaxed);
        let running = CURRENT.load(Ordering::SeqCst);
        let frozen = self.freeze_running(running);
        if !frozen {
            TICKS_COULD_NOT_FREEZE.fetch_add(1, Ordering::Relaxed);
        }

        let want_switch = (self.on_tick)();
        if want_switch {
            TICKS_WANTED_SWITCH.fetch_add(1, Ordering::Relaxed);
        }

        // Ask the scheduler ONLY when its answer can be honoured.
        //
        // The scheduler is not a query: `switch_context` MOVES the
        // kernel's idea of the current task. Asking it and then declining
        // to switch -- because the running thread could not be frozen --
        // leaves the kernel and this port disagreeing about who is
        // running, and the running task keeps executing under somebody
        // else's identity. Every API that means "the calling task" then
        // names the wrong one: `vTaskPrioritySet( NULL, x )` sets a task
        // that is not the caller, `uxTaskPriorityGet( NULL )` answers for
        // a task that is not asking.
        //
        // `GenQTest` catches it precisely, because it ends a test with
        // `vTaskPrioritySet( NULL, genqMUTEX_LOW_PRIORITY )` -- and the
        // task that was wrongly set to low priority was the HIGH priority
        // one, three hundred lines from the assertion that noticed.
        let next = if want_switch && frozen {
            ask_scheduler();
            CURRENT.load(Ordering::SeqCst)
        } else {
            running
        };

        IN_TICK.store(false, Ordering::SeqCst);
        // A switch the kernel asked for during the tick has been taken
        // here, or declined here; either way it is not still outstanding.
        PENDING.store(false, Ordering::SeqCst);
        CRITICAL.exit(me);

        if next == running || next == NO_TASK {
            // Nothing to change. Put the task back exactly as it was.
            //
            // A switch the tick wanted and could not take is not lost, only
            // deferred: the running task's next kernel call asks the
            // scheduler again, and the ready higher-priority task is still
            // ready.
            if frozen {
                if let Some(slot) = slots().get(running) {
                    slot.frozen.store(false, Ordering::SeqCst);
                    backend::thaw(slot.os_thread.load(Ordering::SeqCst));
                }
            }
            return;
        }
        TICKS_SWITCHED.fetch_add(1, Ordering::Relaxed);
        grant(next);
    }

    /// Freeze the running task, and record that it was frozen rather than
    /// parked so [`grant`] thaws it instead of signalling a condvar
    /// nobody is waiting on.
    fn freeze_running(&self, running: usize) -> bool {
        if running == NO_TASK || !preemption_enabled() {
            return false;
        }
        let Some(slot) = slots().get(running) else {
            return false;
        };
        let handle = slot.os_thread.load(Ordering::SeqCst);
        if !backend::freeze(handle) {
            return false;
        }
        slot.frozen.store(true, Ordering::SeqCst);
        true
    }
}

/// Turn preemption off at runtime, with `KAIROS_HOST_NO_PREEMPT=1`.
///
/// Two uses, and both are about not having to take this port's word for
/// anything:
///
/// * **The poison test.** A cell that claims "a task that never yields was
///   taken off the CPU" has to be able to show the claim FAILING when the
///   mechanism is removed. Without that, the check might be passing for
///   some other reason and nobody would know.
/// * **Standing in for the platforms that do not have it.** Windows and
///   Unix both preempt; a platform that is neither reports `PREEMPTIVE`
///   false, and this is how that is reproduced on the machine you have
///   rather than discovered on the machine you do not.
fn preemption_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("KAIROS_HOST_NO_PREEMPT").as_deref() != Ok("1"))
}

/// True while the tick thread is inside [`Ticker::interrupt`].
///
/// This port's only interrupt context. It is a flag rather than a magic
/// owner id in the critical lock because the lock's identity has to be the
/// SAME function everywhere -- `pend_switch` compares against it to decide
/// whether it is being pended or taken, and a tick thread that held the
/// lock under a different name would take a switch from inside the tick.
static IN_TICK: AtomicBool = AtomicBool::new(false);

/// The OS thread backing task `index`, or zero if it has none yet.
///
/// A cell reports this the way a bare-metal cell reports a saved stack
/// pointer: a slot with an entry point and no thread is a task that is
/// SCHEDULABLE WITH NOWHERE TO RUN, which is a real bug shape and one that
/// presents a long way from its cause.
#[must_use]
pub fn thread_of(index: usize) -> u64 {
    slots()
        .get(index)
        .map_or(0, |s| s.os_thread.load(Ordering::SeqCst))
}

/// Hand a task slot's OS handle back. For a cell that deletes tasks.
pub fn release_task(index: usize) {
    if let Some(slot) = slots().get(index) {
        backend::release(slot.os_thread.swap(0, Ordering::SeqCst));
    }
}

// -------------------------------------------------------------- the port --

/// The host [`Port`].
#[derive(Debug, Default)]
pub struct HostPort {
    yields: AtomicU64,
    ticks: AtomicU64,
}

impl HostPort {
    /// A port with nothing counted yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            yields: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
        }
    }

    /// Ticks delivered so far.
    #[must_use]
    pub fn tick_count(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }

    /// `portYIELD()` calls so far.
    #[must_use]
    pub fn yield_count(&self) -> u64 {
        self.yields.load(Ordering::Relaxed)
    }

    /// Count a tick the ticker delivered.
    pub fn note_tick(&self) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
    }
}

/// The identity the critical lock uses for the calling thread.
///
/// A thread id, not a handle: it only has to be unique and non-zero, and
/// `ThreadId` is neither a number nor stable across runs, so it is hashed
/// into one.
fn me() -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    // Zero means "the lock is free", so it is the one value that may not
    // be handed out.
    match h.finish() {
        0 => 1,
        other => other,
    }
}

impl Port for HostPort {
    /// This port COMMITS the switch.
    ///
    /// `pend_switch` does not merely note a request: it asks the scheduler,
    /// hands the run permit over and parks the caller, so the decision and
    /// the change of running task are one step -- exactly what every
    /// bare-metal port here declares, for the same reason.
    ///
    /// Leaving it `false` was a real defect and a quiet one. The kernel
    /// then treats itself as STACKLESS and moves `current` inside
    /// `port_yield` by calling `switch_context` directly; the port then
    /// switches AGAIN on the way out. Two selections per yield, and the
    /// round robin advances twice -- which on a ready list of two is the
    /// same task every time, for ever.
    ///
    /// That is how `EventGroups` and `dynamic` stopped dead in the C ABI
    /// host cell while a busier demo's equal-priority tasks took a
    /// thousandfold share, and how one of two never-yielding tasks in
    /// `firmware/host-kernel` got 13.5 million laps against ZERO.
    ///
    /// The trait's own doc names the other half of the cost: between the
    /// kernel moving `current` and the port catching up, "code runs as a
    /// task the kernel no longer thinks is current".
    const COMMITS_SWITCH: bool = true;

    fn yield_now(&self) {
        self.yields.fetch_add(1, Ordering::Relaxed);
        pend_switch();
    }

    fn yield_from_isr(&self, woken: Woken) {
        if woken.needed() {
            self.yields.fetch_add(1, Ordering::Relaxed);
            pend_switch();
        }
    }

    fn enter_critical(&self) {
        CRITICAL.enter(me());
    }

    fn exit_critical(&self) {
        let who = me();
        // Released, not merely counted down: this is the outermost exit,
        // which on a chip is where `cpsie i` lets the pending `PendSV`
        // finally run. Same place, same reason.
        if CRITICAL.exit(who)
            && !IN_TICK.load(Ordering::SeqCst)
            && PENDING.swap(false, Ordering::SeqCst)
        {
            switch_now(who);
        }
    }

    fn set_interrupt_mask_from_isr(&self) -> u32 {
        let who = me();
        let already = CRITICAL.owner.load(Ordering::Acquire) == who;
        if !already {
            CRITICAL.enter(who);
        }
        u32::from(already)
    }

    fn clear_interrupt_mask_from_isr(&self, saved: u32) {
        // Restore, do not release: a mask taken inside somebody else's
        // critical section must leave that section standing. The Cortex-M
        // port learned this the hard way -- an unconditional release there
        // ended a caller's critical section halfway through and let a
        // half-created task be scheduled.
        if saved == 0 {
            CRITICAL.exit(me());
        }
    }

    fn in_isr(&self) -> bool {
        // The tick thread is this port's only interrupt context, and only
        // one context runs at a time -- so if the tick is inside, the
        // caller IS the tick.
        IN_TICK.load(Ordering::SeqCst)
    }

    fn count_tick(&self) {
        self.note_tick();
    }

    fn count_yield(&self) {
        self.yields.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_critical_section_nests_and_only_the_outermost_exit_releases() {
        let who = 7;
        CRITICAL.enter(who);
        assert_ne!(CRITICAL.owner.load(Ordering::Acquire), 0);
        CRITICAL.enter(who);
        assert!(!CRITICAL.exit(who), "the inner exit must not release");
        assert_ne!(CRITICAL.owner.load(Ordering::Acquire), 0);
        assert!(CRITICAL.exit(who), "the outer exit releases");
        assert_eq!(CRITICAL.owner.load(Ordering::Acquire), 0);
    }

    #[test]
    fn a_thread_cannot_release_a_critical_section_it_does_not_hold() {
        CRITICAL.enter(11);
        assert!(!CRITICAL.exit(12), "12 does not hold it");
        assert_ne!(CRITICAL.owner.load(Ordering::Acquire), 0, "11 still does");
        assert!(CRITICAL.exit(11));
    }

    #[test]
    fn the_thread_identity_is_never_the_free_marker() {
        assert_ne!(me(), 0, "zero means the lock is free");
    }

    /// A yield asked for from inside a critical section must be PENDED,
    /// not taken -- that is the whole of what `PendSV` being the
    /// lowest-priority exception buys on a chip.
    #[test]
    fn a_yield_inside_a_critical_section_is_pended_rather_than_taken() {
        let who = me();
        PENDING.store(false, Ordering::SeqCst);
        CRITICAL.enter(who);
        pend_switch();
        assert!(
            PENDING.load(Ordering::SeqCst),
            "the switch should be outstanding, not taken"
        );
        PENDING.store(false, Ordering::SeqCst);
        assert!(CRITICAL.exit(who));
    }

    #[test]
    fn the_slot_table_is_the_size_it_says() {
        assert_eq!(slots().len(), MAX_TASKS);
    }

    /// The port must not claim preemption it does not have. A cell reads
    /// this to decide whether a demo that never yields can be trusted.
    #[test]
    fn preemption_is_reported_honestly_for_this_platform() {
        assert_eq!(PREEMPTIVE, cfg!(any(windows, unix)));
    }

    /// Freezing a thread that is **running** is the whole of preemption,
    /// so it gets a test that needs no kernel, no cell and no emulator.
    ///
    /// The subject does nothing but increment a counter, which is the
    /// point: it never blocks, never yields and never takes a lock, so
    /// the ONLY thing that can stop it is the port. If the counter stops
    /// moving while it is frozen and moves again after the thaw, the
    /// mechanism works; if `freeze` returned `true` without actually
    /// stopping it, this fails — and that is the failure worth catching,
    /// because a `freeze` that answers before the thread has stopped puts
    /// two tasks on the CPU at once and the damage shows up somewhere
    /// else entirely.
    #[test]
    fn a_running_thread_is_frozen_where_it_stands_and_thaws_again() {
        if !PREEMPTIVE {
            // Nothing to assert on a platform that says it cannot.
            return;
        }
        if cfg!(miri) {
            // Miri interprets Rust; it does not run `SuspendThread` and it
            // does not deliver signals. This test IS the foreign call, so
            // under Miri there is nothing here it can check -- and the
            // package's Miri row is a real gate, not one to break with a
            // test that was never going to run under it.
            return;
        }
        static SPINS: AtomicU64 = AtomicU64::new(0);
        static STOP: AtomicBool = AtomicBool::new(false);

        SPINS.store(0, Ordering::SeqCst);
        STOP.store(false, Ordering::SeqCst);

        let spinner = std::thread::spawn(|| {
            while !STOP.load(Ordering::Relaxed) {
                SPINS.fetch_add(1, Ordering::Relaxed);
                std::hint::spin_loop();
            }
        });
        let handle = backend::thread_handle_of(&spinner);
        assert_ne!(handle, 0, "a spawned thread must have a usable handle");

        // Wait for it to be genuinely running, so that what gets frozen
        // is a thread in its loop rather than one that has not started.
        let mut started = false;
        for _ in 0..1_000 {
            if SPINS.load(Ordering::SeqCst) > 0 {
                started = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(started, "the spinner never ran");

        assert!(backend::freeze(handle), "freeze refused a running thread");
        let at_freeze = SPINS.load(Ordering::SeqCst);
        // Long enough that a thread still on the CPU would move the
        // counter by millions, so this is not a close-run thing.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            SPINS.load(Ordering::SeqCst),
            at_freeze,
            "a frozen thread kept running"
        );

        assert!(backend::thaw(handle), "thaw refused a frozen thread");
        let mut resumed = false;
        for _ in 0..1_000 {
            if SPINS.load(Ordering::SeqCst) > at_freeze {
                resumed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(resumed, "a thawed thread never ran again");

        STOP.store(true, Ordering::SeqCst);
        spinner.join().expect("the spinner should end");
        backend::release(handle);
    }
}
