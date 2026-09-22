//! The Kairos kernel behind `esp-radio-rtos-driver` 0.4.1.
//!
//! Five traits, and they divide sharply into two halves.
//!
//! # The half our kernel supplies directly
//!
//! `SchedulerImplementation` is what the radio actually wants replaced:
//! tasks, priorities, yields, sleeps and a clock. Every one of those is a
//! `Kernel` call, and the switch is `rusty_rtos_port-xtensa`'s.
//!
//! # The half the adapter has to build
//!
//! Queues and semaphores cannot be mapped onto the kernel's, and the reason
//! is structural rather than a missing feature:
//!
//! | | the driver wants | the kernel has |
//! |---|---|---|
//! | item | `*const u8`, arbitrary `item_size` | `u64` — `slots: [u64; SLOTS]` |
//! | handle | a `NonNull` the impl allocates | an index into a fixed arena |
//! | count | created on demand | `MAX_QUEUES`, a const generic |
//!
//! So the payload lives in a heap buffer this module owns, and the kernel
//! supplies only the **blocking**: a bounded buffer guarded by two counting
//! semaphores, which is the textbook construction and uses nothing the
//! corpus does not already prove against C FreeRTOS every day.
//!
//! # How a STACKED task blocks on a STACKLESS kernel
//!
//! This is the join that looked impossible and is not. `Kernel::semaphore_take`
//! answers `Wait::Blocked`, documented as *"leave the program counter where
//! it is and make the same call again when the task next runs."* That is a
//! **retry** protocol, and a retry protocol serves both shapes of task:
//!
//! * a stackless corpus task returns to its runner, which re-enters it;
//! * a radio task — which owns a stack — loops and yields, and the context
//!   switch resumes it inside the very same call.
//!
//! [`block_on`] is that loop. The kernel's own bookkeeping carries the
//! timeout across retries, exactly as it does for the corpus.

use core::ffi::c_void;
use core::ptr::NonNull;

use alloc::alloc::{alloc, dealloc};
use alloc::boxed::Box;
use core::alloc::Layout;

use esp_radio_rtos_driver::queue::{QueueImplementation, QueuePtr};
use esp_radio_rtos_driver::semaphore::{SemaphoreImplementation, SemaphoreKind, SemaphorePtr};
use esp_radio_rtos_driver::timer::{TimerImplementation, TimerPtr};
use esp_radio_rtos_driver::wait_queue::{WaitQueueImplementation, WaitQueuePtr};
use esp_radio_rtos_driver::{SchedulerImplementation, ThreadPtr};

use rusty_rtos_core::handle::{QueueHandle, TaskHandle};

use crate::{host, Blocked};
use crate::SLOT_CAPACITY as MAX_TASKS;

/// The tick is 1 kHz, so one tick is 1,000 microseconds.
const US_PER_TICK: u64 = 1_000;

/// Microseconds to ticks, rounding **up**.
///
/// Up, because a radio that asked to wait 1,500 µs and was woken at 1,000
/// has been given a wrong answer, where one woken at 2,000 has merely been
/// given a slow one. The rounding is a real fidelity limit of a 1 kHz tick
/// and is recorded in the cell's README rather than hidden here.
fn ticks_from_us(us: u64) -> u64 {
    us.div_ceil(US_PER_TICK)
}

/// A deadline in absolute microseconds turned into a relative tick count.
fn ticks_until(deadline_us: Option<u64>) -> u64 {
    match deadline_us {
        None => u64::MAX,
        Some(deadline) => ticks_from_us(deadline.saturating_sub(now_us())),
    }
}

/// Drive a kernel call that may block until it completes or fails.
///
/// See the module docs: `Wait::Blocked` means "call me again when this task
/// next runs", so a stacked task yields and calls again.
fn block_on<F>(mut call: F) -> bool
where
    F: FnMut(&mut K) -> Option<Wait<()>>,
{
    loop {
        // The kernel lock is NEVER held across the yield. Taking it, making
        // one call and dropping it is what keeps the switch legal.
        match with_kernel(&mut call) {
            Some(Wait::Ready(())) => return true,
            Some(Wait::Blocked) => yield_and_switch(),
            None => return false,
        }
    }
}

// ----------------------------------------------------------------- tasks --

/// What a `ThreadPtr` points at.
pub struct TaskSlot {
    /// The saved machine state, swapped by the switching interrupt.
    pub context: Context,
    /// The kernel's own handle for this task.
    pub handle: TaskHandle,
    /// The stack this task owns, and the layout to free it with.
    stack: *mut u8,
    layout: Layout,
    /// `current_task_thread_semaphore`: one semaphore per thread, made on
    /// demand and owned for the life of the task.
    thread_semaphore: Option<SemaphorePtr>,
}

/// Task slots by kernel task index, so the switching interrupt can find a
/// context from whatever the scheduler chose.
static mut SLOTS: [*mut TaskSlot; MAX_TASKS] = [core::ptr::null_mut(); MAX_TASKS];

/// Record a slot against its kernel index.
fn register_slot(handle: TaskHandle, slot: *mut TaskSlot) {
    let index = usize::from(handle.index());
    // SAFETY: single core; every writer masks interrupts through
    // `with_kernel`, and `index` is below `MAX_TASKS` because the kernel
    // refuses to create more tasks than that.
    unsafe {
        if let Some(cell) = (&raw mut SLOTS).cast::<*mut TaskSlot>().add(index).as_mut() {
            *cell = slot;
        }
    }
}

/// The context of whatever task the kernel says is current.
///
/// `None` for a kernel task with no slot — the idle and timer tasks, which
/// this cell never gives stacks to because nothing ever switches to them.
pub fn context_of(handle: TaskHandle) -> Option<*mut Context> {
    let index = usize::from(handle.index());
    // SAFETY: as `register_slot`.
    unsafe {
        let slot = *(&raw const SLOTS).cast::<*mut TaskSlot>().add(index);
        if slot.is_null() {
            None
        } else {
            Some(&raw mut (*slot).context)
        }
    }
}

/// Give `main` a slot so the scheduler can switch away from it.
///
/// Its context is zeroed and its stack is the one the runtime already gave
/// `main`; the first switch away fills the context in. It owns no heap
/// stack, so nothing here frees one.
pub fn register_main(handle: TaskHandle) {
    let slot = Box::into_raw(Box::new(TaskSlot {
        context: Context::default(),
        handle,
        stack: core::ptr::null_mut(),
        layout: Layout::new::<u8>(),
        thread_semaphore: None,
    }));
    register_slot(handle, slot);
}

/// The handle for a task index, if this cell gave that index a context.
pub fn handle_for(index: u32) -> Option<TaskHandle> {
    let index = index as usize;
    if index >= MAX_TASKS {
        return None;
    }
    // SAFETY: as `register_slot`.
    unsafe {
        let slot = *(&raw const SLOTS).cast::<*mut TaskSlot>().add(index);
        if slot.is_null() {
            None
        } else {
            Some((*slot).handle)
        }
    }
}

/// The frame a radio task starts in.
///
/// The blob's entry point is `extern "C" fn(*mut c_void)` and must not
/// return; if it ever does, the task deletes itself rather than running off
/// the end of a stack that has nothing beneath it.
extern "C" fn task_entry(task_fn: usize, param: usize) {
    // SAFETY: `task_fn` is the pointer the radio handed `task_create`, whose
    // type the driver fixes as `extern "C" fn(*mut c_void)`.
    let entry: extern "C" fn(*mut c_void) = unsafe { core::mem::transmute(task_fn) };
    entry(param as *mut c_void);
    Scheduler.schedule_task_deletion(None);
    // `schedule_task_deletion(None)` yields and never comes back.
    loop {
        core::hint::spin_loop();
    }
}

/// The scheduler the radio runs on.
pub struct Scheduler;

impl SchedulerImplementation for Scheduler {
    fn initialized(&self) -> bool {
        crate::kernel::started()
    }

    fn yield_task(&self) {
        yield_and_switch();
    }

    fn yield_task_from_isr(&self) {
        // Already in an interrupt: ask for the switch, and it happens on the
        // way out rather than re-entering the switcher from inside itself.
        rusty_rtos_port_xtensa::yield_now();
    }

    fn current_task(&self) -> ThreadPtr {
        let handle = with_kernel(&mut |k: &mut K| Some(k.current())).unwrap_or_default();
        let index = usize::from(handle.index());
        // SAFETY: as `register_slot`.
        let slot = unsafe { *(&raw const SLOTS).cast::<*mut TaskSlot>().add(index) };
        NonNull::new(slot.cast::<()>()).unwrap_or(NonNull::dangling())
    }

    fn max_task_priority(&self) -> u32 {
        // One below the configured ceiling: the top priority belongs to the
        // timer daemon, and a radio task that outranked it would starve the
        // software timers the radio itself arms.
        u32::from(crate::kernel::MAX_PRIORITIES) - 2
    }

    fn task_create(
        &self,
        name: &str,
        task: extern "C" fn(*mut c_void),
        param: *mut c_void,
        priority: u32,
        _core_id: Option<u32>,
        task_stack_size: usize,
    ) -> ThreadPtr {
        // The stack is the adapter's to allocate -- the trait says so, and
        // it is the whole reason a stackless kernel cannot host the radio.
        let size = task_stack_size.max(2048);
        let layout = match Layout::from_size_align(size, 16) {
            Ok(l) => l,
            Err(_) => return NonNull::dangling(),
        };
        // SAFETY: a non-zero layout; the pointer is checked below.
        let stack = unsafe { alloc(layout) };
        if stack.is_null() {
            return NonNull::dangling();
        }

        let clamped = (priority as u8).min(crate::kernel::MAX_PRIORITIES.saturating_sub(2));

        // Everything that does not need a handle is done FIRST, so that
        // creating the task and registering its slot can happen under one
        // interrupt mask.
        //
        // Order matters here and cost a board run to learn. `create_task`
        // can preempt: a new task that outranks the caller makes the kernel
        // raise the switching exception, and that exception fires the
        // instant `with_kernel` unmasks. If the slot is registered after
        // that, the exception finds no context for the task the scheduler
        // just chose, declines the swap, and leaves `current` naming a task
        // the CPU is not running — the very drift this port was fixed to
        // prevent.
        //
        // SAFETY: the stack is `size` bytes at `stack`, so one past its end
        // is `stack + size`, and it lives until `delete` frees it.
        let context = unsafe {
            new_task_context(
                task_entry,
                task as *const () as usize,
                param as usize,
                stack.add(size),
            )
        };
        let slot = Box::into_raw(Box::new(TaskSlot {
            context,
            handle: TaskHandle::NULL,
            stack,
            layout,
            thread_semaphore: None,
        }));

        let made = with_kernel(&mut |k: &mut K| {
            let handle = k.create_task(name, clamped).ok()?;
            // SAFETY: `slot` was just leaked from a `Box` and nothing else
            // holds it yet.
            unsafe {
                (*slot).handle = handle;
            }
            register_slot(handle, slot);
            Some(handle)
        });
        if made.is_none() {
            // SAFETY: the slot and stack are ours; no task was created.
            unsafe {
                drop(Box::from_raw(slot));
                dealloc(stack, layout);
            }
            return NonNull::dangling();
        }
        NonNull::new(slot.cast::<()>()).unwrap_or(NonNull::dangling())
    }

    fn schedule_task_deletion(&self, task_handle: Option<ThreadPtr>) {
        let slot = match task_handle {
            Some(ptr) => ptr.as_ptr().cast::<TaskSlot>(),
            None => self.current_task().as_ptr().cast::<TaskSlot>(),
        };
        if slot.is_null() {
            return;
        }
        // SAFETY: every live `ThreadPtr` this adapter issued points at a
        // `TaskSlot` it leaked from a `Box`.
        let handle = unsafe { (*slot).handle };
        let deleting_self = with_kernel(&mut |k: &mut K| {
            let me = k.current();
            let _ = k.task_delete(Some(handle));
            Some(me == handle)
        })
        .unwrap_or(false);

        if deleting_self {
            // The kernel has taken this task off every list; the switch
            // below never returns, so the slot and its stack are left for
            // the next `task_create` to reuse the index. Freeing the stack
            // we are standing on is the one thing that must not happen here.
            yield_and_switch();
            return;
        }
        register_slot(handle, core::ptr::null_mut());
        // SAFETY: the task is deleted and not running, so its stack and slot
        // are ours to release.
        unsafe {
            let owned = Box::from_raw(slot);
            dealloc(owned.stack, owned.layout);
        }
    }

    fn current_task_thread_semaphore(&self) -> SemaphorePtr {
        let slot = self.current_task().as_ptr().cast::<TaskSlot>();
        if slot.is_null() {
            return NonNull::dangling();
        }
        // SAFETY: as `schedule_task_deletion`.
        unsafe {
            if let Some(existing) = (*slot).thread_semaphore {
                return existing;
            }
            let made = Semaphore::create(SemaphoreKind::Counting { max: 1, initial: 0 });
            (*slot).thread_semaphore = Some(made);
            made
        }
    }

    unsafe fn task_priority(&self, task: ThreadPtr) -> u32 {
        let slot = task.as_ptr().cast::<TaskSlot>();
        // SAFETY: the caller guarantees the pointer came from `task_create`.
        let handle = unsafe { (*slot).handle };
        with_kernel(&mut |k: &mut K| k.task_priority_get(Some(handle)).ok().map(u32::from))
            .unwrap_or(0)
    }

    unsafe fn set_task_priority(&self, task: ThreadPtr, priority: u32) {
        let slot = task.as_ptr().cast::<TaskSlot>();
        // SAFETY: as `task_priority`.
        let handle = unsafe { (*slot).handle };
        let clamped = (priority as u8).min(crate::kernel::MAX_PRIORITIES.saturating_sub(2));
        with_kernel(&mut |k: &mut K| {
            let _ = k.set_priority(Some(handle), clamped);
            Some(())
        });
    }

    fn usleep(&self, us: u32) {
        let ticks = ticks_from_us(u64::from(us));
        with_kernel(&mut |k: &mut K| {
            let _ = k.delay(ticks);
            Some(())
        });
        yield_and_switch();
    }

    fn usleep_until(&self, target: u64) {
        let now = now_us();
        if target > now {
            self.usleep((target - now).min(u64::from(u32::MAX)) as u32);
        }
    }

    fn now(&self) -> u64 {
        now_us()
    }
}

// ------------------------------------------------------------ semaphores --

/// A semaphore the radio owns.
struct Sem {
    handle: QueueHandle,
}

pub struct Semaphore;

impl SemaphoreImplementation for Semaphore {
    fn create(kind: SemaphoreKind) -> SemaphorePtr {
        let handle = with_kernel(&mut |k: &mut K| match kind {
            SemaphoreKind::Counting { max, initial } => k
                .semaphore_create_counting(max as usize, initial as usize)
                .ok(),
            SemaphoreKind::Mutex => k.mutex_create().ok(),
            SemaphoreKind::RecursiveMutex => k.mutex_create_recursive().ok(),
        });
        let Some(handle) = handle else {
            return NonNull::dangling();
        };
        let boxed = Box::into_raw(Box::new(Sem { handle }));
        NonNull::new(boxed.cast::<()>()).unwrap_or(NonNull::dangling())
    }

    unsafe fn delete(semaphore: SemaphorePtr) {
        // SAFETY: the caller guarantees this came from `create`.
        let owned = unsafe { Box::from_raw(semaphore.as_ptr().cast::<Sem>()) };
        with_kernel(&mut |k: &mut K| {
            let _ = k.queue_delete(owned.handle);
            Some(())
        });
    }

    unsafe fn take(semaphore: SemaphorePtr, timeout_us: Option<u32>) -> bool {
        let ticks = timeout_us.map_or(u64::MAX, |us| ticks_from_us(u64::from(us)));
        // SAFETY: as `delete`.
        let handle = unsafe { (*semaphore.as_ptr().cast::<Sem>()).handle };
        block_on(move |k: &mut K| k.semaphore_take(handle, ticks).ok())
    }

    unsafe fn take_with_deadline(semaphore: SemaphorePtr, deadline_instant: Option<u64>) -> bool {
        let ticks = ticks_until(deadline_instant);
        // SAFETY: as `delete`.
        let handle = unsafe { (*semaphore.as_ptr().cast::<Sem>()).handle };
        block_on(move |k: &mut K| k.semaphore_take(handle, ticks).ok())
    }

    unsafe fn give(semaphore: SemaphorePtr) -> bool {
        // SAFETY: as `delete`.
        let handle = unsafe { (*semaphore.as_ptr().cast::<Sem>()).handle };
        block_on(move |k: &mut K| k.semaphore_give(handle).ok())
    }

    unsafe fn try_give_from_isr(
        semaphore: SemaphorePtr,
        higher_prio_task_waken: Option<&mut bool>,
    ) -> bool {
        // SAFETY: as `delete`.
        let handle = unsafe { (*semaphore.as_ptr().cast::<Sem>()).handle };
        let woken = with_kernel(&mut |k: &mut K| k.semaphore_give_from_isr(handle).ok());
        match woken {
            Some(w) => {
                if let Some(flag) = higher_prio_task_waken {
                    *flag = w == rusty_rtos_core::isr::Woken::YES;
                }
                true
            }
            None => false,
        }
    }

    unsafe fn current_count(semaphore: SemaphorePtr) -> u32 {
        // SAFETY: as `delete`.
        let handle = unsafe { (*semaphore.as_ptr().cast::<Sem>()).handle };
        with_kernel(&mut |k: &mut K| k.semaphore_count(handle).ok().map(|c| c as u32)).unwrap_or(0)
    }

    unsafe fn try_take(semaphore: SemaphorePtr) -> bool {
        // SAFETY: as `delete`.
        let handle = unsafe { (*semaphore.as_ptr().cast::<Sem>()).handle };
        // Zero ticks: the kernel answers `Ready` or fails, never `Blocked`.
        matches!(
            with_kernel(&mut |k: &mut K| k.semaphore_take(handle, 0).ok()),
            Some(Wait::Ready(()))
        )
    }

    unsafe fn try_take_from_isr(
        semaphore: SemaphorePtr,
        _higher_prio_task_waken: Option<&mut bool>,
    ) -> bool {
        // SAFETY: as `delete`.
        let handle = unsafe { (*semaphore.as_ptr().cast::<Sem>()).handle };
        matches!(
            with_kernel(&mut |k: &mut K| k.semaphore_take(handle, 0).ok()),
            Some(Wait::Ready(()))
        )
    }
}

// ---------------------------------------------------------------- queues --

/// A bounded buffer with the payload in a heap ring and the blocking in two
/// counting semaphores. See the module docs for why it cannot simply be one
/// of the kernel's queues.
struct Q {
    storage: *mut u8,
    layout: Layout,
    capacity: usize,
    item_size: usize,
    /// Guards the ring against two tasks writing at once.
    lock: QueueHandle,
    /// Counts items present; a receiver takes it.
    filled: QueueHandle,
    /// Counts free slots; a sender takes it.
    empty: QueueHandle,
    head: usize,
    tail: usize,
    len: usize,
}

pub struct Queue;

impl Queue {
    /// Copy one item into the ring at the front or the back.
    ///
    /// # Safety
    /// `q` must be live and `item` must point at `item_size` readable bytes.
    unsafe fn push(q: *mut Q, item: *const u8, front: bool) {
        // SAFETY: the caller's contract.
        unsafe {
            let cap = (*q).capacity;
            let size = (*q).item_size;
            let index = if front {
                (*q).head = ((*q).head + cap - 1) % cap;
                (*q).head
            } else {
                let at = (*q).tail;
                (*q).tail = (at + 1) % cap;
                at
            };
            core::ptr::copy_nonoverlapping(
                (*q).storage.add(index * size),
                (*q).storage.add(index * size),
                0,
            );
            core::ptr::copy_nonoverlapping(item, (*q).storage.add(index * size), size);
            (*q).len += 1;
        }
    }

    /// Copy one item out of the ring.
    ///
    /// # Safety
    /// As [`Queue::push`], and `item` must be writable for `item_size`.
    unsafe fn pop(q: *mut Q, item: *mut u8) {
        // SAFETY: the caller's contract.
        unsafe {
            let cap = (*q).capacity;
            let size = (*q).item_size;
            let at = (*q).head;
            (*q).head = (at + 1) % cap;
            core::ptr::copy_nonoverlapping((*q).storage.add(at * size), item, size);
            (*q).len -= 1;
        }
    }
}

impl QueueImplementation for Queue {
    fn create(capacity: usize, item_size: usize) -> QueuePtr {
        let bytes = capacity.saturating_mul(item_size).max(1);
        let Ok(layout) = Layout::from_size_align(bytes, 8) else {
            return NonNull::dangling();
        };
        // SAFETY: a non-zero layout; checked below.
        let storage = unsafe { alloc(layout) };
        if storage.is_null() {
            return NonNull::dangling();
        }
        let made = with_kernel(&mut |k: &mut K| {
            let lock = k.mutex_create().ok()?;
            let filled = k.semaphore_create_counting(capacity.max(1), 0).ok()?;
            let empty = k
                .semaphore_create_counting(capacity.max(1), capacity)
                .ok()?;
            Some((lock, filled, empty))
        });
        let Some((lock, filled, empty)) = made else {
            // SAFETY: `storage` came from `alloc` with this layout.
            unsafe { dealloc(storage, layout) };
            return NonNull::dangling();
        };
        let boxed = Box::into_raw(Box::new(Q {
            storage,
            layout,
            capacity: capacity.max(1),
            item_size,
            lock,
            filled,
            empty,
            head: 0,
            tail: 0,
            len: 0,
        }));
        NonNull::new(boxed.cast::<()>()).unwrap_or(NonNull::dangling())
    }

    unsafe fn delete(queue: QueuePtr) {
        // SAFETY: the caller guarantees this came from `create`.
        let owned = unsafe { Box::from_raw(queue.as_ptr().cast::<Q>()) };
        with_kernel(&mut |k: &mut K| {
            let _ = k.queue_delete(owned.lock);
            let _ = k.queue_delete(owned.filled);
            let _ = k.queue_delete(owned.empty);
            Some(())
        });
        // SAFETY: the storage came from `create`.
        unsafe { dealloc(owned.storage, owned.layout) };
    }

    unsafe fn send_to_front(queue: QueuePtr, item: *const u8, timeout_us: Option<u32>) -> bool {
        let ticks = timeout_us.map_or(u64::MAX, |us| ticks_from_us(u64::from(us)));
        // SAFETY: the caller's contract.
        unsafe { send(queue, item, ticks, true) }
    }

    unsafe fn send_to_front_with_deadline(
        queue: QueuePtr,
        item: *const u8,
        deadline_instant: Option<u64>,
    ) -> bool {
        // SAFETY: the caller's contract.
        unsafe { send(queue, item, ticks_until(deadline_instant), true) }
    }

    unsafe fn send_to_back(queue: QueuePtr, item: *const u8, timeout_us: Option<u32>) -> bool {
        let ticks = timeout_us.map_or(u64::MAX, |us| ticks_from_us(u64::from(us)));
        // SAFETY: the caller's contract.
        unsafe { send(queue, item, ticks, false) }
    }

    unsafe fn send_to_back_with_deadline(
        queue: QueuePtr,
        item: *const u8,
        deadline_instant: Option<u64>,
    ) -> bool {
        // SAFETY: the caller's contract.
        unsafe { send(queue, item, ticks_until(deadline_instant), false) }
    }

    unsafe fn try_send_to_back_from_isr(
        queue: QueuePtr,
        item: *const u8,
        higher_prio_task_waken: Option<&mut bool>,
    ) -> bool {
        let q = queue.as_ptr().cast::<Q>();
        // SAFETY: the caller's contract. From an ISR nothing may block, so
        // a full queue is refused rather than waited on.
        unsafe {
            if (*q).len >= (*q).capacity {
                return false;
            }
            Queue::push(q, item, false);
            let handle = (*q).filled;
            let woken = with_kernel(&mut |k: &mut K| k.semaphore_give_from_isr(handle).ok());
            if let (Some(w), Some(flag)) = (woken, higher_prio_task_waken) {
                *flag = w == rusty_rtos_core::isr::Woken::YES;
            }
            true
        }
    }

    unsafe fn receive(queue: QueuePtr, item: *mut u8, timeout_us: Option<u32>) -> bool {
        let ticks = timeout_us.map_or(u64::MAX, |us| ticks_from_us(u64::from(us)));
        // SAFETY: the caller's contract.
        unsafe { receive_inner(queue, item, ticks) }
    }

    unsafe fn receive_with_deadline(
        queue: QueuePtr,
        item: *mut u8,
        deadline_instant: Option<u64>,
    ) -> bool {
        // SAFETY: the caller's contract.
        unsafe { receive_inner(queue, item, ticks_until(deadline_instant)) }
    }

    unsafe fn try_receive_from_isr(
        queue: QueuePtr,
        item: *mut u8,
        _higher_prio_task_waken: Option<&mut bool>,
    ) -> bool {
        let q = queue.as_ptr().cast::<Q>();
        // SAFETY: the caller's contract.
        unsafe {
            if (*q).len == 0 {
                return false;
            }
            Queue::pop(q, item);
            let handle = (*q).empty;
            let _ = with_kernel(&mut |k: &mut K| k.semaphore_give_from_isr(handle).ok());
            true
        }
    }

    unsafe fn remove(queue: QueuePtr, _item: *const u8) {
        // `remove` drops the item at the head. The driver uses it to discard
        // a message it has already peeked, which is the only shape this
        // adapter can honour without scanning by value -- an item is
        // `item_size` opaque bytes and nothing here may interpret them.
        let q = queue.as_ptr().cast::<Q>();
        // SAFETY: the caller's contract.
        unsafe {
            if (*q).len == 0 {
                return;
            }
            (*q).head = ((*q).head + 1) % (*q).capacity;
            (*q).len -= 1;
            let handle = (*q).empty;
            let _ = with_kernel(&mut |k: &mut K| k.semaphore_give(handle).ok());
        }
    }

    fn messages_waiting(queue: QueuePtr) -> usize {
        let q = queue.as_ptr().cast::<Q>();
        // SAFETY: a live queue pointer from `create`.
        unsafe { (*q).len }
    }
}

/// The blocking send both `send_to_*` forms share.
///
/// # Safety
/// `queue` must be live and `item` must point at `item_size` readable bytes.
unsafe fn send(queue: QueuePtr, item: *const u8, ticks: u64, front: bool) -> bool {
    let q = queue.as_ptr().cast::<Q>();
    // SAFETY: the caller's contract.
    let (empty, filled, lock) = unsafe { ((*q).empty, (*q).filled, (*q).lock) };
    if !block_on(move |k: &mut K| k.semaphore_take(empty, ticks).ok()) {
        return false;
    }
    if !block_on(move |k: &mut K| k.semaphore_take(lock, u64::MAX).ok()) {
        return false;
    }
    // SAFETY: the lock is held, so the ring is ours.
    unsafe { Queue::push(q, item, front) };
    block_on(move |k: &mut K| k.semaphore_give(lock).ok());
    block_on(move |k: &mut K| k.semaphore_give(filled).ok());
    true
}

/// The blocking receive both `receive*` forms share.
///
/// # Safety
/// `queue` must be live and `item` writable for `item_size` bytes.
unsafe fn receive_inner(queue: QueuePtr, item: *mut u8, ticks: u64) -> bool {
    let q = queue.as_ptr().cast::<Q>();
    // SAFETY: the caller's contract.
    let (empty, filled, lock) = unsafe { ((*q).empty, (*q).filled, (*q).lock) };
    if !block_on(move |k: &mut K| k.semaphore_take(filled, ticks).ok()) {
        return false;
    }
    if !block_on(move |k: &mut K| k.semaphore_take(lock, u64::MAX).ok()) {
        return false;
    }
    // SAFETY: the lock is held.
    unsafe { Queue::pop(q, item) };
    block_on(move |k: &mut K| k.semaphore_give(lock).ok());
    block_on(move |k: &mut K| k.semaphore_give(empty).ok());
    true
}

// ----------------------------------------------------------- wait queues --

/// A wait queue: everyone waiting is released by one `notify`.
///
/// Built from a counting semaphore plus the number of waiters, because
/// "wake all" is not something a semaphore does on its own.
struct WQ {
    sem: QueueHandle,
    waiters: usize,
}

pub struct WaitQueue;

impl WaitQueueImplementation for WaitQueue {
    fn create() -> WaitQueuePtr {
        let Some(sem) = with_kernel(&mut |k: &mut K| k.semaphore_create_counting(64, 0).ok())
        else {
            return NonNull::dangling();
        };
        let boxed = Box::into_raw(Box::new(WQ { sem, waiters: 0 }));
        NonNull::new(boxed.cast::<()>()).unwrap_or(NonNull::dangling())
    }

    unsafe fn delete(queue: WaitQueuePtr) {
        // SAFETY: the caller guarantees this came from `create`.
        let owned = unsafe { Box::from_raw(queue.as_ptr().cast::<WQ>()) };
        with_kernel(&mut |k: &mut K| {
            let _ = k.queue_delete(owned.sem);
            Some(())
        });
    }

    unsafe fn wait_until(queue: WaitQueuePtr, deadline_instant: Option<u64>) {
        let wq = queue.as_ptr().cast::<WQ>();
        // SAFETY: a live pointer from `create`.
        let sem = unsafe {
            (*wq).waiters += 1;
            (*wq).sem
        };
        let ticks = ticks_until(deadline_instant);
        block_on(move |k: &mut K| k.semaphore_take(sem, ticks).ok());
        // SAFETY: as above.
        unsafe {
            (*wq).waiters = (*wq).waiters.saturating_sub(1);
        }
    }

    unsafe fn notify(queue: WaitQueuePtr) {
        let wq = queue.as_ptr().cast::<WQ>();
        // SAFETY: a live pointer from `create`.
        let (sem, waiters) = unsafe { ((*wq).sem, (*wq).waiters) };
        // Release every current waiter: this is a condition variable's
        // broadcast, not a semaphore's signal.
        for _ in 0..waiters {
            block_on(move |k: &mut K| k.semaphore_give(sem).ok());
        }
    }

    unsafe fn notify_from_isr(queue: WaitQueuePtr, higher_prio_task_waken: Option<&mut bool>) {
        let wq = queue.as_ptr().cast::<WQ>();
        // SAFETY: a live pointer from `create`.
        let (sem, waiters) = unsafe { ((*wq).sem, (*wq).waiters) };
        let mut any = false;
        for _ in 0..waiters {
            if let Some(w) = with_kernel(&mut |k: &mut K| k.semaphore_give_from_isr(sem).ok()) {
                any |= w == rusty_rtos_core::isr::Woken::YES;
            }
        }
        if let Some(flag) = higher_prio_task_waken {
            *flag = any;
        }
    }
}

// ---------------------------------------------------------------- timers --

/// A radio timer.
///
/// NOT one of the kernel's software timers: those dispatch by a `u16`
/// callback index through the timer daemon, which cannot carry a C function
/// pointer handed over at runtime. The radio's timers live in the cell's own
/// table and are serviced by a task built on nothing but `delay` and a
/// microsecond clock.
struct T {
    index: usize,
}

pub struct Timer;

impl TimerImplementation for Timer {
    fn create(function: unsafe extern "C" fn(*mut c_void), data: *mut c_void) -> TimerPtr {
        let index = crate::kernel::remember_timer(function, data);
        if index == usize::MAX {
            return NonNull::dangling();
        }
        let boxed = Box::into_raw(Box::new(T { index }));
        NonNull::new(boxed.cast::<()>()).unwrap_or(NonNull::dangling())
    }

    unsafe fn delete(timer: TimerPtr) {
        // SAFETY: the caller guarantees this came from `create`.
        let owned = unsafe { Box::from_raw(timer.as_ptr().cast::<T>()) };
        crate::kernel::forget_timer(owned.index);
    }

    unsafe fn arm(timer: TimerPtr, timeout: u64, periodic: bool) {
        // SAFETY: a live pointer from `create`.
        let index = unsafe { (*timer.as_ptr().cast::<T>()).index };
        crate::kernel::arm_timer(index, timeout, periodic);
    }

    unsafe fn is_active(timer: TimerPtr) -> bool {
        // SAFETY: a live pointer from `create`.
        let index = unsafe { (*timer.as_ptr().cast::<T>()).index };
        crate::kernel::timer_active(index)
    }

    unsafe fn disarm(timer: TimerPtr) {
        // SAFETY: a live pointer from `create`.
        let index = unsafe { (*timer.as_ptr().cast::<T>()).index };
        crate::kernel::disarm_timer(index);
    }
}
