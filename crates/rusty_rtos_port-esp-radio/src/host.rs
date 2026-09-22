//! The seam: what this crate needs from a kernel, and nothing more.
//!
//! The adapter used to live inside a firmware and call that firmware's own
//! kernel module directly — `with_kernel(&mut |k: &mut K| k.semaphore_give(s))`
//! and so on, where `K` was one concrete kernel geometry. That is why Janus
//! could not consume it: the glue was correct and passed on silicon, but it
//! was welded to one firmware's task count, priority count and allocator.
//!
//! # Why the seam keeps `with_kernel` instead of flattening it
//!
//! The obvious extraction gives the host one method per kernel call —
//! `host().semaphore_give(s)`. It is wrong, and quietly so. Several places
//! in the adapter do **three or four kernel calls inside one closure**:
//! creating a queue takes two semaphores and a mutex, and deleting one
//! releases four handles. The original held a single borrow of the kernel
//! across each group.
//!
//! One method per call turns one acquisition into four, and opens a window
//! between them where another task can run. Nothing in a type would have
//! caught that; it would have shown up as a rare failure on a board.
//!
//! So the seam is shaped like the thing it replaces: [`RadioHost::with_kernel`]
//! hands out a `&mut dyn` [`KernelOps`] for the duration of one closure, and
//! the atomicity is the host's to provide exactly as it was before.
//!
//! # Why `&'static dyn` and not a generic parameter
//!
//! `esp-radio-rtos-driver` is registered through macros that take a **type
//! path** — `register_semaphore_implementation!(Semaphore)`. A generic
//! `Semaphore<H>` would have to be spelled at that call site, and a macro
//! taking a path is not obliged to accept one with generics. So the five
//! implementation types stay concrete and the kernel arrives through a host
//! installed once at startup.
//!
//! # Why the closure returns nothing
//!
//! A `dyn` trait cannot have a generic method, so `with_kernel` cannot be
//! generic over a return type the way the firmware's free function was.
//! Closures capture instead: write the answer into a local and read it after.
//! That is the one place this seam is less pleasant than what it replaced,
//! and it buys object safety, which is what makes a single installed host
//! possible at all.

use rusty_rtos_core::handle::{QueueHandle, TaskHandle};

/// A semaphore or mutex operation that may have had to block.
///
/// This is the kernel's `Wait<()>` restated at the seam, so the crate does
/// not have to name a kernel type that carries const generics. It has the
/// same **two** states the kernel has and no third: there is no `TimedOut`,
/// because a kernel that parks you answers [`Self::Blocked`] and expects the
/// same call again — the timeout is discovered by the retry, not reported by
/// the first call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocked {
    /// `Wait::Ready(())` — the call completed.
    Completed,
    /// `Wait::Blocked` — the kernel parked the calling task. Leave the
    /// program counter where it is and make the same call again when the
    /// task next runs.
    Blocked,
}

/// The kernel calls this crate makes, as the adapter sees them.
///
/// Every method is the kernel call of the same name with `Result` flattened
/// to `Option`, because the driver has no error channel to carry a reason
/// into: it gets a null pointer or a `false`, and that is all it can act on.
/// **Flattening happens here, not inside the adapter**, so a host that wants
/// to log a refused call has one place to do it.
pub trait KernelOps {
    /// `xTaskGetCurrentTaskHandle`.
    fn current(&mut self) -> TaskHandle;

    /// `xTaskCreate`. `None` if the arena is full.
    fn create_task(&mut self, name: &str, priority: u8) -> Option<TaskHandle>;

    /// `vTaskDelete`. `None` names the calling task, as in the C.
    fn task_delete(&mut self, task: Option<TaskHandle>) -> Option<()>;

    /// `uxTaskPriorityGet`.
    fn task_priority_get(&mut self, task: Option<TaskHandle>) -> Option<u8>;

    /// `vTaskPrioritySet`.
    fn set_priority(&mut self, task: Option<TaskHandle>, priority: u8) -> Option<()>;

    /// `vTaskDelay`, in ticks.
    fn delay(&mut self, ticks: u64) -> Option<()>;

    /// `xSemaphoreCreateCounting`.
    fn semaphore_create_counting(&mut self, max: usize, initial: usize) -> Option<QueueHandle>;

    /// `xSemaphoreTake`, with a tick timeout.
    fn semaphore_take(&mut self, semaphore: QueueHandle, ticks: u64) -> Option<Blocked>;

    /// `xSemaphoreGive`.
    fn semaphore_give(&mut self, semaphore: QueueHandle) -> Option<Blocked>;

    /// `xSemaphoreGiveFromISR`. `true` when a higher-priority task woke.
    fn semaphore_give_from_isr(&mut self, semaphore: QueueHandle) -> Option<bool>;

    /// `uxSemaphoreGetCount`.
    fn semaphore_count(&mut self, semaphore: QueueHandle) -> Option<usize>;

    /// `xSemaphoreCreateMutex`.
    fn mutex_create(&mut self) -> Option<QueueHandle>;

    /// `xSemaphoreCreateRecursiveMutex`.
    fn mutex_create_recursive(&mut self) -> Option<QueueHandle>;

    /// `vQueueDelete`, which also deletes a semaphore or mutex.
    fn queue_delete(&mut self, queue: QueueHandle) -> Option<()>;
}

/// Everything this crate needs from a kernel.
///
/// Implement it on a unit struct beside your kernel instance and hand it to
/// [`install`](crate::install) once, before `esp-radio` starts.
///
/// # Two obligations that no type here can check
///
/// - **`current` must name the task that owns the CPU.** A stackless kernel
///   cannot verify this. If the timer daemon outranks your `main`, `current`
///   names the daemon, every switch declines as `from == to`, and the radio
///   threads never run. The kernel's `start_scheduler` documents it.
/// - **`semaphore_give_from_isr` is called from an ISR** and must use the
///   kernel's from-ISR path, not the task path.
pub trait RadioHost: Sync + 'static {
    /// The largest number of tasks the kernel can hold. Checked against
    /// [`SLOT_CAPACITY`](crate::SLOT_CAPACITY) when the host is installed.
    fn max_tasks(&self) -> usize;

    /// A monotonic microsecond clock. The driver uses it for timeouts; it
    /// need not be the tick.
    fn now_us(&self) -> u64;

    /// Give up the CPU and let the scheduler choose again.
    fn yield_and_switch(&self);

    /// Run `f` with the kernel borrowed, exactly once.
    ///
    /// The borrow must be held for the WHOLE closure — several callers do
    /// three or four operations inside one and rely on no other task running
    /// between them. If the host cannot borrow the kernel right now (it is
    /// already borrowed further up the stack), it must not call `f` at all,
    /// and the caller treats that as a refused operation.
    fn with_kernel(&self, f: &mut dyn FnMut(&mut dyn KernelOps));
}
